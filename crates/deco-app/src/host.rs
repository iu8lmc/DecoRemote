//! Lato "host": si registra sul server e condivide lo schermo con chi si collega.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use deco_net::{Direct, DIRECT_ACCEPT_TIMEOUT};
use deco_proto::{
    msg::{ClientHello, HostMsg, PathMsg, ServerMsg, VideoCodec, ViewerMsg},
    net::{recv_plain, send_plain},
    secure::{derive_psk, handshake_responder, BoxR, BoxW, SecureReader, SecureWriter, VIDEO_LANE_NONCE_BASE},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, Notify},
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    capture::{self, Encoded},
    clip, cursor, input,
    share::{self, Role, Share},
};

const SERVER_SILENCE_LIMIT: Duration = Duration::from_secs(45);
const DEFAULT_QUALITY: u8 = 60;
/// Tempo concesso all'utente dell'host per consentire o rifiutare una connessione.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Password errate tollerate in `LOCKOUT` prima di rifiutare ogni nuova richiesta (protegge l'accesso non vigilato).
const MAX_FAILED_LOGINS: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(10 * 60);

struct Pending {
    since: Instant,
    reply: oneshot::Sender<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostStatus {
    Connecting,
    Online,
    Offline(String),
}

pub struct HostShared {
    pub id: String,
    pub status: Mutex<HostStatus>,
    /// Una richiesta di connessione è in corso (anche se in attesa di conferma): blocca le altre.
    pub in_session: AtomicBool,
    /// Un viewer è stato accettato e sta controllando questo computer.
    pub active: AtomicBool,
    pub require_confirmation: AtomicBool,
    pending: Mutex<Option<Pending>>,
    /// Istanti delle password errate recenti.
    failed_logins: Mutex<Vec<Instant>>,
    pub server: Mutex<String>,
    pub password: Mutex<String>,
    /// Chat e file della sessione in corso (None fuori sessione).
    pub share: Mutex<Option<Share>>,
    /// Forza la riconnessione al server (es. dopo aver cambiato indirizzo).
    pub reconnect: Notify,
    /// Chiude la sessione in corso.
    pub stop_session: Notify,
    ctx: egui::Context,
}

impl HostShared {
    pub fn new(
        id: String,
        server: String,
        password: String,
        require_confirmation: bool,
        ctx: egui::Context,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            status: Mutex::new(HostStatus::Connecting),
            in_session: AtomicBool::new(false),
            active: AtomicBool::new(false),
            require_confirmation: AtomicBool::new(require_confirmation),
            pending: Mutex::new(None),
            failed_logins: Mutex::new(Vec::new()),
            server: Mutex::new(server),
            password: Mutex::new(password),
            share: Mutex::new(None),
            reconnect: Notify::new(),
            stop_session: Notify::new(),
            ctx,
        })
    }

    pub fn status(&self) -> HostStatus {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, s: HostStatus) {
        *self.status.lock().unwrap() = s;
        self.ctx.request_repaint();
    }

    /// Da quanto tempo c'è una richiesta in attesa di risposta, se c'è.
    pub fn pending_since(&self) -> Option<Instant> {
        self.pending.lock().unwrap().as_ref().map(|p| p.since)
    }

    /// Risposta dell'utente dell'host alla richiesta in sospeso.
    pub fn answer(&self, allow: bool) {
        if let Some(p) = self.pending.lock().unwrap().take() {
            let _ = p.reply.send(allow);
        }
        self.ctx.request_repaint();
    }

    /// Mostra la richiesta nella UI e attende la risposta (rifiuto automatico allo scadere del tempo).
    async fn ask_approval(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        *self.pending.lock().unwrap() = Some(Pending { since: Instant::now(), reply });
        // L'app può essere minimizzata o in secondo piano: va portata in primo piano.
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
        self.ctx.request_repaint();
        matches!(timeout(APPROVAL_TIMEOUT, rx).await, Ok(Ok(true)))
    }

    fn clear_pending(&self) {
        *self.pending.lock().unwrap() = None;
        self.ctx.request_repaint();
    }

    /// `true` se ci sono stati troppi tentativi falliti da poco.
    fn locked_out(&self) -> bool {
        let mut failed = self.failed_logins.lock().unwrap();
        failed.retain(|t| t.elapsed() < LOCKOUT);
        failed.len() >= MAX_FAILED_LOGINS
    }

    fn server(&self) -> String {
        self.server.lock().unwrap().clone()
    }
}

/// Mantiene la registrazione sul server, riconnettendosi con backoff.
pub async fn run(shared: Arc<HostShared>) {
    let mut backoff = 1u64;
    loop {
        shared.set_status(HostStatus::Connecting);
        match control_loop(&shared).await {
            Ok(()) => backoff = 1,
            Err(e) => {
                warn!("server: {e:#}");
                shared.set_status(HostStatus::Offline(format!("{e:#}")));
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

async fn control_loop(shared: &Arc<HostShared>) -> Result<()> {
    let server = shared.server();
    let mut stream = timeout(Duration::from_secs(10), TcpStream::connect(&server))
        .await
        .map_err(|_| anyhow!("timeout connettendo a {server}"))??;
    stream.set_nodelay(true)?;
    send_plain(&mut stream, &ClientHello::Host { id: shared.id.clone() }).await?;
    match recv_plain::<_, ServerMsg>(&mut stream).await? {
        ServerMsg::Ok => {}
        ServerMsg::Error(e) => bail!("{e}"),
        other => bail!("risposta inattesa dal server: {other:?}"),
    }
    info!("registrato come {} su {server}", shared.id);
    shared.set_status(HostStatus::Online);

    loop {
        let msg = tokio::select! {
            m = timeout(SERVER_SILENCE_LIMIT, recv_plain::<_, ServerMsg>(&mut stream)) => {
                m.map_err(|_| anyhow!("il server non risponde"))??
            }
            _ = shared.reconnect.notified() => return Ok(()),
        };
        match msg {
            ServerMsg::Incoming { token } => {
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = session(&shared, token).await {
                        warn!("sessione: {e:#}");
                    }
                    shared.ctx.request_repaint();
                });
            }
            ServerMsg::Error(e) => bail!("{e}"),
            ServerMsg::Ping | ServerMsg::Ok | ServerMsg::Paired { .. } => {}
        }
    }
}

struct SessionGuard<'a>(&'a HostShared);

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.0.in_session.store(false, Ordering::SeqCst);
        self.0.active.store(false, Ordering::SeqCst);
        self.0.clear_pending();
    }
}

async fn session(shared: &Arc<HostShared>, token: u64) -> Result<()> {
    // Una sola sessione alla volta: le altre richieste scadono lato server.
    if shared.in_session.swap(true, Ordering::SeqCst) {
        bail!("sessione già attiva, richiesta ignorata");
    }
    let _guard = SessionGuard(shared);
    shared.ctx.request_repaint();
    if shared.locked_out() {
        bail!("troppe password errate: richieste rifiutate per {} minuti", LOCKOUT.as_secs() / 60);
    }

    let server = shared.server();
    // Indirizzi UDP per il collegamento diretto (None se UDP non è disponibile).
    let udp = deco_net::prepare(&server).await;
    let mut stream = TcpStream::connect(&server).await?;
    stream.set_nodelay(true)?;
    let offer = udp.as_ref().map(|(_, offer)| offer.clone());
    send_plain(&mut stream, &ClientHello::Accept { token, udp: offer }).await?;
    let peer_udp = match recv_plain::<_, ServerMsg>(&mut stream).await? {
        ServerMsg::Paired { peer_udp } => peer_udp,
        ServerMsg::Error(e) => bail!("{e}"),
        other => bail!("risposta inattesa dal server: {other:?}"),
    };

    let (id, password) = (shared.id.clone(), shared.password.lock().unwrap().clone());
    let psk = tokio::task::spawn_blocking(move || derive_psk(&password, &id)).await??;
    // Il viewer decide il percorso e lo comunica sul relay: se il collegamento diretto riesce
    // l'handshake prosegue su QUIC, altrimenti sul relay TCP.
    let (read, write, direct): (BoxR, BoxW, Option<Direct>) = match (udp, peer_udp) {
        (Some((sock, _)), Some(peer)) => {
            let quic = tokio::spawn(async move { deco_net::accept_from_viewer(sock, &peer, DIRECT_ACCEPT_TIMEOUT).await });
            let path: PathMsg = timeout(DIRECT_ACCEPT_TIMEOUT + Duration::from_secs(5), recv_plain(&mut stream))
                .await
                .map_err(|_| anyhow!("il viewer non ha scelto il percorso"))??;
            match path {
                PathMsg::UseQuic => {
                    let direct = quic.await??;
                    drop(stream); // il relay non serve più
                    let (r, w) = timeout(Duration::from_secs(10), direct.accept_control())
                        .await
                        .map_err(|_| anyhow!("il viewer non ha aperto la stream di controllo"))??;
                    (r, w, Some(direct))
                }
                PathMsg::UseTcp => {
                    quic.abort();
                    let (r, w) = stream.into_split();
                    (Box::new(r), Box::new(w), None)
                }
            }
        }
        _ => {
            let (r, w) = stream.into_split();
            (Box::new(r), Box::new(w), None)
        }
    };
    info!("percorso dati: {}", if direct.is_some() { "QUIC diretto" } else { "relay TCP" });
    let handshake = timeout(Duration::from_secs(15), handshake_responder(read, write, &psk))
        .await
        .map_err(|_| anyhow!("handshake scaduto"))?;
    let (mut reader, mut writer) = match handshake {
        Ok(channel) => channel,
        Err(e) => {
            if format!("{e:#}").contains("password errata") {
                shared.failed_logins.lock().unwrap().push(Instant::now());
                warn!("password errata da un viewer");
            }
            return Err(e);
        }
    };
    shared.failed_logins.lock().unwrap().clear();
    info!("viewer autenticato");

    if shared.require_confirmation.load(Ordering::SeqCst) {
        writer.send(&HostMsg::AwaitingApproval).await?;
        // Il viewer non invia nulla finché non è accettato: qualsiasi cosa arrivi (o la chiusura) annulla la richiesta.
        let allowed = tokio::select! {
            allowed = shared.ask_approval() => allowed,
            _ = reader.recv::<ViewerMsg>() => {
                info!("il viewer ha annullato la richiesta");
                return Ok(());
            }
        };
        shared.clear_pending();
        if !allowed {
            info!("connessione rifiutata");
            let _ = writer.send(&HostMsg::Denied).await;
            return Ok(());
        }
    }
    shared.active.store(true, Ordering::SeqCst);

    let result = stream_screen(shared, reader, writer, direct.as_ref()).await;
    if let Some(d) = &direct {
        d.close();
    }
    result
}

async fn stream_screen(
    shared: &Arc<HostShared>,
    mut reader: SecureReader<BoxR>,
    mut writer: SecureWriter<BoxW>,
    direct: Option<&Direct>,
) -> Result<()> {
    let (screen_list, primary) = capture::list()?;
    let screens = capture::Screens::new(primary);
    let stop = Arc::new(AtomicBool::new(false));
    let controls = capture::Controls::new(DEFAULT_QUALITY);

    let (frame_tx, mut frame_rx) = mpsc::channel(1);
    capture::spawn(frame_tx, controls.clone(), stop.clone(), screens.clone());

    let (input_tx, input_rx) = std_mpsc::channel::<ViewerMsg>();

    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel();
    let (clip_in_tx, clip_in_rx) = std_mpsc::channel();
    clip::spawn(clip_out_tx, clip_in_rx, stop.clone());

    // Messaggi generati dal lato ricezione (es. conferma del cambio schermo) da inoltrare al viewer.
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<HostMsg>();
    let screen_count = screen_list.len();
    cursor::spawn(reply_tx.clone(), screens.clone(), stop.clone());

    let host_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "host".into());

    // Chat e file: se l'host è ridotto a icona un messaggio lo riporta in vista (senza rubare il focus).
    let (share, mut share_out) = {
        let (repaint, notify) = (shared.ctx.clone(), shared.ctx.clone());
        share::start(
            Role::Host,
            share::default_recv_dir("Ricevuti"),
            move || repaint.request_repaint(),
            move || {
                notify.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                notify.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
                notify.request_repaint();
            },
        )
    };
    *shared.share.lock().unwrap() = Some(share.clone());

    // Se Windows rifiuta l'input, il viewer lo scopre dalla chat invece di vedere comandi che non rispondono.
    let reporter = share.clone();
    input::spawn(input_rx, screens.clone(), move |text| reporter.chat(&text));
    share.chat(&format!(
        "Host DecoRemote v{}, {}.",
        env!("CARGO_PKG_VERSION"),
        if input::is_elevated() { "avviato come amministratore" } else { "utente normale" }
    ));

    // Con il collegamento diretto il video viaggia su una stream propria: un pacchetto perso nel video
    // non può ritardare cursore, appunti o risposte ai comandi.
    let mut video_lane = match direct {
        Some(d) => Some(writer.lanes().writer(d.open_video().await?, VIDEO_LANE_NONCE_BASE)),
        None => None,
    };
    writer
        .send(&HostMsg::Hello {
            host_name,
            screens: screen_list,
            current: primary as u32,
            codecs: vec![VideoCodec::Av1, VideoCodec::Jpeg],
        })
        .await?;
    let ctrl = tokio::sync::Mutex::new(writer);

    let video_loop = async {
        loop {
            let f = frame_rx.recv().await.ok_or_else(|| anyhow!("cattura terminata"))?;
            let msg = match f {
                Encoded::Jpeg { width, height, jpeg } => HostMsg::Frame { width, height, jpeg },
                Encoded::Av1 { width, height, bands } => HostMsg::Av1Frame { width, height, bands },
            };
            match video_lane.as_mut() {
                Some(lane) => lane.send(&msg).await?,
                None => ctrl.lock().await.send(&msg).await?, // relay TCP: unica stream
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    let ctrl_loop = async {
        loop {
            tokio::select! {
                Some(text) = clip_out_rx.recv() => ctrl.lock().await.send(&HostMsg::Clipboard(text)).await?,
                Some(msg) = reply_rx.recv() => ctrl.lock().await.send(&msg).await?,
                Some(m) = share_out.recv() => ctrl.lock().await.send(&HostMsg::Share(m)).await?,
                else => return Ok::<(), anyhow::Error>(()),
            }
        }
    };

    let recv_loop = async {
        loop {
            match reader.recv::<ViewerMsg>().await? {
                ViewerMsg::Bye => return Ok::<(), anyhow::Error>(()),
                ViewerMsg::SetQuality(q) => controls.set_quality(q),
                ViewerMsg::SetCodec(c) => controls.set_codec(c),
                ViewerMsg::RequestKeyframe => controls.request_keyframe(),
                ViewerMsg::Share(m) => share.incoming(m),
                ViewerMsg::Clipboard(text) => {
                    let _ = clip_in_tx.send(text);
                }
                ViewerMsg::SelectScreen(i) => {
                    let i = i as usize;
                    if i < screen_count {
                        screens.select(i);
                        let _ = reply_tx.send(HostMsg::ScreenChanged { index: i as u32 });
                    }
                }
                other => {
                    let _ = input_tx.send(other);
                }
            }
        }
    };

    let result = tokio::select! {
        r = video_loop => r,
        r = ctrl_loop => r,
        r = recv_loop => r,
        _ = shared.stop_session.notified() => Ok(()),
    };
    stop.store(true, Ordering::Relaxed);
    *shared.share.lock().unwrap() = None;
    let _ = ctrl.lock().await.send(&HostMsg::Bye).await;
    if direct.is_some() {
        // Lascia il tempo al Bye di partire prima che la connessione venga chiusa.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    info!("sessione terminata");
    result
}
