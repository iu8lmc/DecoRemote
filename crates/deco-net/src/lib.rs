//! Collegamento diretto fra viewer e host via QUIC (UDP), con hole punching.
//!
//! La sicurezza non dipende dal TLS di QUIC (il certificato è autofirmato e non verificato):
//! sopra le stream gira comunque il canale Noise con PSK derivata dalla password, che autentica
//! l'host e cifra tutto. TLS serve solo perché QUIC lo richiede.
//!
//! Vantaggi rispetto al relay TCP: percorso diretto (un salto in meno), controllo di congestione
//! BBR (code corte), stream indipendenti (un pacchetto perso nel video non blocca il cursore).

use std::{
    net::{SocketAddr, UdpSocket},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use deco_proto::{
    msg::UdpOffer,
    secure::{BoxR, BoxW},
};
use quinn::{
    congestion::BbrConfig,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
    ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TransportConfig,
};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{ring, CryptoProvider},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use tokio::task::JoinSet;
use tracing::{debug, info};

const ALPN: &[u8] = b"decoremote/1";
const SERVER_NAME: &str = "decoremote";
const PUNCH_PACKET: &[u8] = b"DRPUNCH-0000000";
/// Pacchetti di "perforazione" prima di avviare QUIC, e intervallo fra l'uno e l'altro.
const PUNCH_COUNT: u32 = 6;
const PUNCH_INTERVAL: Duration = Duration::from_millis(60);

/// Finestra massima per stabilire il collegamento diretto, prima di ripiegare sul relay TCP.
pub const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_millis(2500);
/// Lato host: un po' più lunga di quella del viewer, che decide.
pub const DIRECT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(4);

/// Prepara il collegamento diretto: socket UDP e indirizzi con cui è raggiungibile.
/// Restituisce `None` se il server non risponde via UDP o se è disattivato con `DECOREMOTE_NO_UDP`.
pub async fn prepare(server: &str) -> Option<(UdpSocket, UdpOffer)> {
    if std::env::var_os("DECOREMOTE_NO_UDP").is_some() {
        return None;
    }
    let server = server.to_string();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
        let addr = deco_proto::udp::resolve_v4(&server)?;
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        let offer = deco_proto::udp::make_offer(&sock, addr)?;
        Ok((sock, offer))
    })
    .await
    .ok()?;
    match result {
        Ok(found) => Some(found),
        Err(e) => {
            debug!("collegamento diretto non disponibile: {e}");
            None
        }
    }
}

/// Una connessione QUIC stabilita; l'endpoint resta vivo finché esiste questo valore.
pub struct Direct {
    pub conn: Connection,
    _endpoint: Endpoint,
}

impl Direct {
    pub fn remote(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Tempo di andata e ritorno stimato da QUIC.
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"fine sessione");
    }

    /// Lato viewer: apre la stream di controllo. Il primo dato scritto la rende visibile all'host.
    pub async fn open_control(&self) -> Result<(BoxR, BoxW)> {
        let (send, recv) = self.conn.open_bi().await?;
        Ok((Box::new(recv), Box::new(send)))
    }

    /// Lato host: attende la stream di controllo aperta dal viewer.
    pub async fn accept_control(&self) -> Result<(BoxR, BoxW)> {
        let (send, recv) = self.conn.accept_bi().await?;
        Ok((Box::new(recv), Box::new(send)))
    }

    /// Lato host: apre la stream del video (unidirezionale, host -> viewer).
    pub async fn open_video(&self) -> Result<BoxW> {
        Ok(Box::new(self.conn.open_uni().await?))
    }

    /// Lato viewer: attende la stream del video.
    pub async fn accept_video(&self) -> Result<BoxR> {
        Ok(Box::new(self.conn.accept_uni().await?))
    }
}

#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn transport() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.congestion_controller_factory(Arc::new(BbrConfig::default()));
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.max_idle_timeout(Some(Duration::from_secs(15).try_into().expect("timeout valido")));
    // Il primo scambio fa anche da hole punching: ritrasmettere presto invece dei 333 ms predefiniti.
    t.initial_rtt(Duration::from_millis(100));
    t.datagram_receive_buffer_size(None);
    Arc::new(t)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

fn server_config() -> Result<ServerConfig> {
    let certified = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
    let cert = CertificateDer::from(certified.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(transport());
    config.migration(false);
    Ok(config)
}

fn client_config() -> Result<ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(transport());
    Ok(config)
}

/// Invia qualche pacchetto verso ogni indirizzo candidato per aprire il NAT in uscita.
async fn punch(sock: &UdpSocket, targets: &[SocketAddr]) {
    for _ in 0..PUNCH_COUNT {
        for t in targets {
            let _ = sock.send_to(PUNCH_PACKET, t);
        }
        tokio::time::sleep(PUNCH_INTERVAL).await;
    }
}

fn endpoint(sock: UdpSocket, server: Option<ServerConfig>) -> Result<Endpoint> {
    sock.set_nonblocking(true)?;
    Endpoint::new(EndpointConfig::default(), server, sock, Arc::new(quinn::TokioRuntime))
        .context("creazione endpoint QUIC")
}

/// Lato viewer: raggiunge l'host provando tutti i suoi indirizzi in parallelo.
///
/// `sock` è il socket già usato per la scoperta dell'indirizzo, così la porta (e il mapping NAT) è la stessa.
pub async fn connect_to_host(sock: UdpSocket, host: &UdpOffer, timeout: Duration) -> Result<Direct> {
    if host.addrs.is_empty() {
        bail!("l'host non ha indirizzi UDP");
    }
    punch(&sock, &host.addrs).await;
    let mut ep = endpoint(sock, None)?;
    ep.set_default_client_config(client_config()?);

    let mut attempts = JoinSet::new();
    for &addr in &host.addrs {
        let connecting = ep.connect(addr, SERVER_NAME)?;
        attempts.spawn(async move { connecting.await.map_err(|e| anyhow!("{addr}: {e}")).map(|c| (addr, c)) });
    }
    let first = tokio::time::timeout(timeout, async {
        let mut last_err = anyhow!("nessun indirizzo raggiungibile");
        while let Some(joined) = attempts.join_next().await {
            match joined {
                Ok(Ok(found)) => return Ok(found),
                Ok(Err(e)) => last_err = e,
                Err(e) => last_err = anyhow!(e),
            }
        }
        Err(last_err)
    })
    .await
    .map_err(|_| anyhow!("collegamento diretto scaduto"))??;

    let (addr, conn) = first;
    info!("collegamento diretto QUIC verso {addr}");
    attempts.abort_all();
    Ok(Direct { conn, _endpoint: ep })
}

/// Lato host: attende il viewer, accettando solo connessioni provenienti dai suoi indirizzi.
pub async fn accept_from_viewer(sock: UdpSocket, viewer: &UdpOffer, timeout: Duration) -> Result<Direct> {
    punch(&sock, &viewer.addrs).await;
    let ep = endpoint(sock, Some(server_config()?))?;
    let allowed: Vec<_> = viewer.addrs.iter().map(|a| a.ip()).collect();

    let conn = tokio::time::timeout(timeout, async {
        while let Some(incoming) = ep.accept().await {
            if !allowed.contains(&incoming.remote_address().ip()) {
                debug!("connessione QUIC da {} rifiutata: non è il viewer atteso", incoming.remote_address());
                incoming.refuse();
                continue;
            }
            match incoming.await {
                Ok(conn) => return Ok(conn),
                Err(e) => debug!("handshake QUIC fallito: {e}"),
            }
        }
        Err(anyhow!("endpoint chiuso"))
    })
    .await
    .map_err(|_| anyhow!("collegamento diretto scaduto"))??;
    info!("collegamento diretto QUIC da {}", conn.remote_address());
    Ok(Direct { conn, _endpoint: ep })
}

#[cfg(test)]
mod tests {
    use deco_proto::secure::{derive_psk, handshake_initiator, handshake_responder, VIDEO_LANE_NONCE_BASE};
    use deco_proto::msg::{HostMsg, ViewerMsg};

    use super::*;

    fn offer_for(sock: &UdpSocket) -> UdpOffer {
        UdpOffer { addrs: vec![sock.local_addr().unwrap()] }
    }

    #[tokio::test]
    async fn direct_session_with_noise_and_video_lane() {
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let viewer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (host_offer, viewer_offer) = (offer_for(&host_sock), offer_for(&viewer_sock));

        let host = tokio::spawn(async move {
            let direct = accept_from_viewer(host_sock, &viewer_offer, Duration::from_secs(5)).await?;
            let (r, w) = direct.accept_control().await?;
            let psk = derive_psk("pw", "123456789")?;
            let (mut r, w) = handshake_responder(r, w, &psk).await?;
            let video = direct.open_video().await?;
            let mut video = w.lane(video, VIDEO_LANE_NONCE_BASE);
            let mut w = w;
            // Un frame "grande" sulla corsia video, un messaggio di controllo sull'altra.
            video.send(&HostMsg::Frame { width: 1, height: 1, jpeg: vec![9; 200_000] }).await?;
            w.send(&HostMsg::Clipboard("ciao".into())).await?;
            match r.recv::<ViewerMsg>().await? {
                ViewerMsg::Text(t) if t == "dal viewer" => {}
                other => bail!("inatteso: {other:?}"),
            }
            anyhow::Ok(direct)
        });

        let direct = connect_to_host(viewer_sock, &host_offer, Duration::from_secs(5)).await.unwrap();
        let (r, w) = direct.open_control().await.unwrap();
        let psk = derive_psk("pw", "123456789").unwrap();
        let (mut r, mut w) = handshake_initiator(r, w, &psk).await.unwrap();
        w.send(&ViewerMsg::Text("dal viewer".into())).await.unwrap();

        let video = direct.accept_video().await.unwrap();
        let mut video = r.lane(video, VIDEO_LANE_NONCE_BASE);
        match video.recv::<HostMsg>().await.unwrap() {
            HostMsg::Frame { jpeg, .. } => assert_eq!(jpeg.len(), 200_000),
            other => panic!("inatteso: {other:?}"),
        }
        match r.recv::<HostMsg>().await.unwrap() {
            HostMsg::Clipboard(t) => assert_eq!(t, "ciao"),
            other => panic!("inatteso: {other:?}"),
        }
        let host_direct = host.await.unwrap().unwrap();
        assert!(direct.rtt() < Duration::from_secs(1));
        host_direct.close();
    }

    /// Inoltra datagrammi fra viewer e host perdendone una parte e ritardando gli altri.
    async fn lossy_proxy(viewer: SocketAddr, host: SocketAddr, loss_percent: u64, one_way: Duration) -> SocketAddr {
        let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut rng = 0x9E37_79B9_7F4A_7C15u64;
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if rng % 100 < loss_percent {
                    continue;
                }
                let to = if from == viewer { host } else { viewer };
                let (data, out) = (buf[..n].to_vec(), sock.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(one_way).await;
                    let _ = out.send_to(&data, to).await;
                });
            }
        });
        addr
    }

    /// Con perdite di pacchetti e un video che satura il collegamento, i messaggi di controllo
    /// (cursore, comandi) devono restare reattivi: ecco il vantaggio delle stream QUIC indipendenti.
    #[tokio::test]
    async fn control_lane_stays_responsive_under_loss_and_video_load() {
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let viewer_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (host_addr, viewer_addr) = (host_sock.local_addr().unwrap(), viewer_sock.local_addr().unwrap());
        // Il viewer crede che l'host sia al proxy; l'host vede arrivare il viewer dal proxy (stesso IP).
        let proxy = lossy_proxy(viewer_addr, host_addr, 3, Duration::from_millis(15)).await;
        let host_offer_for_viewer = UdpOffer { addrs: vec![proxy] };
        let viewer_offer_for_host = UdpOffer { addrs: vec![proxy] };

        let host = tokio::spawn(async move {
            let direct = accept_from_viewer(host_sock, &viewer_offer_for_host, Duration::from_secs(8)).await?;
            let (r, w) = direct.accept_control().await?;
            let psk = derive_psk("pw", "123456789")?;
            let (mut r, mut w) = handshake_responder(r, w, &psk).await?;
            let lanes = w.lanes();
            let mut video = lanes.writer(direct.open_video().await?, VIDEO_LANE_NONCE_BASE);
            // Il video spinge frame da 120 KB senza sosta finché il viewer non ha finito.
            let pusher = tokio::spawn(async move {
                let mut sent = 0u32;
                while video.send(&HostMsg::Frame { width: sent, height: 1, jpeg: vec![5; 120_000] }).await.is_ok() {
                    sent += 1;
                }
                sent
            });
            // Eco dei messaggi di controllo.
            while let Ok(ViewerMsg::Text(t)) = r.recv::<ViewerMsg>().await {
                if t == "stop" {
                    break;
                }
                w.send(&HostMsg::Clipboard(t)).await?;
            }
            pusher.abort();
            anyhow::Ok(direct)
        });

        let direct = connect_to_host(viewer_sock, &host_offer_for_viewer, Duration::from_secs(8)).await.unwrap();
        let (r, w) = direct.open_control().await.unwrap();
        let psk = derive_psk("pw", "123456789").unwrap();
        let (mut r, mut w) = handshake_initiator(r, w, &psk).await.unwrap();
        let lanes = r.lanes();

        // Riceve il video in background, verificando l'integrità dei frame.
        let video_rx = {
            let video = direct.accept_video().await.unwrap();
            let mut video = lanes.reader(video, VIDEO_LANE_NONCE_BASE);
            tokio::spawn(async move {
                let mut frames = 0u32;
                while let Ok(HostMsg::Frame { jpeg, .. }) = video.recv::<HostMsg>().await {
                    assert_eq!(jpeg.len(), 120_000);
                    assert!(jpeg.iter().all(|&b| b == 5), "frame corrotto");
                    frames += 1;
                }
                frames
            })
        };

        let mut rtts = Vec::new();
        for i in 0..50 {
            let t = std::time::Instant::now();
            w.send(&ViewerMsg::Text(format!("ping {i}"))).await.unwrap();
            match r.recv::<HostMsg>().await.unwrap() {
                HostMsg::Clipboard(text) => assert_eq!(text, format!("ping {i}")),
                other => panic!("inatteso: {other:?}"),
            }
            rtts.push(t.elapsed());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        w.send(&ViewerMsg::Text("stop".into())).await.unwrap();
        rtts.sort();
        let p50 = rtts[rtts.len() / 2];
        let p95 = rtts[rtts.len() * 95 / 100];
        let host_direct = host.await.unwrap().unwrap();
        host_direct.close();
        let frames = tokio::time::timeout(Duration::from_secs(2), video_rx).await.map(|r| r.unwrap()).unwrap_or(0);
        eprintln!("controllo con 3% di perdite e video a piena velocità: p50 {p50:?}, p95 {p95:?}; frame video ricevuti: {frames}");
        assert!(frames >= 5, "il video deve scorrere: {frames} frame");
        assert!(p95 < Duration::from_millis(400), "controllo troppo lento: p95 {p95:?}");
    }

    #[tokio::test]
    async fn unexpected_peer_is_refused() {
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        // L'host si aspetta un viewer da un altro IP: la connessione da 127.0.0.1 va rifiutata.
        let expected = UdpOffer { addrs: vec!["203.0.113.7:4000".parse().unwrap()] };
        let host = tokio::spawn(async move { accept_from_viewer(host_sock, &expected, Duration::from_millis(1500)).await });

        let intruder = UdpSocket::bind("127.0.0.1:0").unwrap();
        let res = connect_to_host(intruder, &UdpOffer { addrs: vec![host_addr] }, Duration::from_millis(1200)).await;
        assert!(res.is_err());
        assert!(host.await.unwrap().is_err());
    }
}
