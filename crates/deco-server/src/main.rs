//! Server di rendezvous + relay di DecoRemote.
//!
//! Gli host registrano il proprio ID con una connessione di controllo; quando un
//! viewer chiede di collegarsi, il server avvisa l'host, che apre una seconda
//! connessione. Le due connessioni vengono poi unite (relay). Il traffico tra host
//! e viewer è cifrato end-to-end, quindi il server non vede mai il contenuto.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Result};
use deco_proto::{
    msg::{ClientHello, ServerMsg, UdpOffer},
    net::{recv_plain, send_plain},
    udp::serve_discovery,
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    time::{interval, timeout},
};
use tracing::{info, warn};

const DEFAULT_ADDR: &str = "0.0.0.0:21116";
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_TIMEOUT: Duration = Duration::from_secs(15);
const PING_EVERY: Duration = Duration::from_secs(15);

struct HostEntry {
    session: u64,
    tx: mpsc::UnboundedSender<ServerMsg>,
}

#[derive(Default)]
struct State {
    hosts: Mutex<HashMap<String, HostEntry>>,
    /// Viewer in attesa della connessione dati dell'host (con gli indirizzi UDP di quest'ultimo).
    pending: Mutex<HashMap<u64, oneshot::Sender<(TcpStream, Option<UdpOffer>)>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let addr = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let listener = TcpListener::bind(&addr).await?;
    info!("DecoRemote server in ascolto su {addr} (TCP) e UDP sulla stessa porta");

    // Scoperta dell'indirizzo UDP pubblico, per il collegamento diretto fra peer.
    match tokio::net::UdpSocket::bind(&addr).await {
        Ok(udp) => {
            tokio::spawn(async move {
                if let Err(e) = serve_discovery(udp).await {
                    warn!("servizio UDP terminato: {e}");
                }
            });
        }
        Err(e) => warn!("UDP non disponibile ({e}): i collegamenti passeranno solo dal relay"),
    }
    // Seconda porta UDP (porta + 1): permette ai client di verificare il tipo di NAT.
    if let Ok(mut second) = addr.parse::<std::net::SocketAddr>() {
        second.set_port(second.port().wrapping_add(1));
        match tokio::net::UdpSocket::bind(second).await {
            Ok(udp) => {
                tokio::spawn(async move {
                    let _ = serve_discovery(udp).await;
                });
            }
            Err(e) => warn!("seconda porta UDP {second} non disponibile ({e}): la verifica del NAT non sarà possibile"),
        }
    }

    let state = Arc::new(State::default());
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true).ok();
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, state).await {
                warn!("{peer}: {e:#}");
            }
        });
    }
}

fn valid_id(id: &str) -> bool {
    (6..=16).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

async fn handle(mut stream: TcpStream, state: Arc<State>) -> Result<()> {
    let hello: ClientHello = timeout(HELLO_TIMEOUT, recv_plain(&mut stream)).await??;
    match hello {
        ClientHello::Host { id } => host_control(stream, state, id).await,
        ClientHello::Connect { target, udp } => viewer(stream, state, target, udp).await,
        ClientHello::Accept { token, udp } => {
            let waiting = state.pending.lock().unwrap().remove(&token);
            match waiting {
                // Il task del viewer prosegue con questo stream e fa da relay.
                Some(tx) => {
                    let _ = tx.send((stream, udp));
                    Ok(())
                }
                None => {
                    send_plain(&mut stream, &ServerMsg::Error("richiesta scaduta".into())).await?;
                    Ok(())
                }
            }
        }
    }
}

async fn host_control(mut stream: TcpStream, state: Arc<State>, id: String) -> Result<()> {
    if !valid_id(&id) {
        send_plain(&mut stream, &ServerMsg::Error("ID non valido".into())).await?;
        return Ok(());
    }
    let session: u64 = rand::random();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let taken = {
        let mut hosts = state.hosts.lock().unwrap();
        let taken = hosts.get(&id).is_some_and(|h| !h.tx.is_closed());
        if !taken {
            hosts.insert(id.clone(), HostEntry { session, tx });
        }
        taken
    };
    if taken {
        send_plain(&mut stream, &ServerMsg::Error("ID già in uso".into())).await?;
        return Ok(());
    }
    info!("host {id} registrato");

    let result = host_loop(&mut stream, &mut rx).await;

    let mut hosts = state.hosts.lock().unwrap();
    if hosts.get(&id).is_some_and(|h| h.session == session) {
        hosts.remove(&id);
    }
    info!("host {id} disconnesso");
    result
}

async fn host_loop(
    stream: &mut TcpStream,
    rx: &mut mpsc::UnboundedReceiver<ServerMsg>,
) -> Result<()> {
    send_plain(stream, &ServerMsg::Ok).await?;
    let (mut reader, mut writer) = stream.split();
    let mut ping = interval(PING_EVERY);
    ping.tick().await; // la prima tick è immediata
    let mut scratch = [0u8; 64];
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => send_plain(&mut writer, &m).await?,
                None => return Ok(()),
            },
            _ = ping.tick() => send_plain(&mut writer, &ServerMsg::Ping).await?,
            // L'host non invia nulla dopo la registrazione: qualsiasi lettura serve solo a vedere la chiusura.
            n = reader.read(&mut scratch) => {
                if n? == 0 {
                    return Ok(());
                }
            }
        }
    }
}

async fn viewer(
    mut stream: TcpStream,
    state: Arc<State>,
    target: String,
    viewer_udp: Option<UdpOffer>,
) -> Result<()> {
    let token: u64 = rand::random();
    let (tx, rx) = oneshot::channel();
    // Va registrato prima di avvisare l'host: la sua Accept potrebbe arrivare subito.
    state.pending.lock().unwrap().insert(token, tx);
    let notified = {
        let hosts = state.hosts.lock().unwrap();
        hosts
            .get(&target)
            .map(|h| h.tx.send(ServerMsg::Incoming { token }).is_ok())
            .unwrap_or(false)
    };
    if !notified {
        state.pending.lock().unwrap().remove(&token);
        send_plain(&mut stream, &ServerMsg::Error("ID non trovato o host offline".into())).await?;
        return Ok(());
    }

    let (mut host_stream, host_udp) = match timeout(PAIR_TIMEOUT, rx).await {
        Ok(Ok(accepted)) => accepted,
        _ => {
            state.pending.lock().unwrap().remove(&token);
            send_plain(&mut stream, &ServerMsg::Error("l'host non ha risposto".into())).await?;
            bail!("timeout in attesa dell'host {target}");
        }
    };

    // Ognuno riceve gli indirizzi UDP dell'altro, per tentare il collegamento diretto.
    send_plain(&mut host_stream, &ServerMsg::Paired { peer_udp: viewer_udp }).await?;
    send_plain(&mut stream, &ServerMsg::Paired { peer_udp: host_udp }).await?;
    info!("sessione avviata verso {target}");
    let (up, down) = tokio::io::copy_bidirectional(&mut stream, &mut host_stream).await?;
    info!("sessione verso {target} chiusa (↑{up} B ↓{down} B)");
    Ok(())
}
