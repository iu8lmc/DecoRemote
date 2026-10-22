//! Lato "viewer": si collega a un host, mostra lo schermo e inoltra l'input.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc, Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use deco_codec::{BandPacket, SlicedDecoder};
use deco_net::{Direct, DIRECT_CONNECT_TIMEOUT};
use deco_proto::{
    msg::{Av1Band, ClientHello, HostMsg, PathMsg, ScreenInfo, ServerMsg, VideoCodec, ViewerMsg},
    net::{recv_plain, send_plain},
    secure::{derive_psk, handshake_initiator, BoxR, BoxW, VIDEO_LANE_NONCE_BASE},
};
use tokio::{
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{self, UnboundedSender},
    time::timeout,
};
use tracing::{info, warn};

use crate::clip;

/// Frame accodati oltre i quali si rinuncia a recuperare e si chiede un keyframe (~0,4 s a 60 fps).
const MAX_BACKLOG: usize = 24;

#[derive(Clone, Debug)]
pub enum ViewerStatus {
    Connecting,
    /// Password corretta, in attesa che l'utente dell'host consenta l'accesso.
    WaitingApproval,
    Connected { host_name: String, screens: Vec<ScreenInfo>, current: usize },
    Closed(Option<String>),
}

/// Contatori per mostrare codec, fps e banda.
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub av1: AtomicBool,
    /// Collegamento diretto QUIC (true) o relay TCP (false).
    pub direct: AtomicBool,
    /// RTT misurato da QUIC, in ms (solo con il collegamento diretto).
    pub rtt_ms: std::sync::atomic::AtomicU32,
}

/// Forma del cursore dell'host, pronta per diventare una texture.
pub struct CursorShape {
    pub hot_x: i32,
    pub hot_y: i32,
    pub image: egui::ColorImage,
}

/// Ultimo stato noto del cursore dell'host.
#[derive(Default)]
pub struct CursorState {
    pub shapes: HashMap<u64, Arc<CursorShape>>,
    /// Posizione normalizzata (0..1) sullo schermo trasmesso.
    pub pos: (f32, f32),
    pub shape: u64,
    pub visible: bool,
}

/// Aggiornamento dell'immagine remota, in attesa di essere caricato come texture.
pub struct TextureUpdate {
    pub size: [usize; 2],
    /// Immagine intera (JPEG, o cambio di risoluzione): sostituisce tutto.
    pub full: Option<egui::ColorImage>,
    /// Fasce aggiornate, per prima riga. Una fascia più recente sostituisce la precedente.
    pub parts: BTreeMap<usize, egui::ColorImage>,
}

enum Incoming {
    Jpeg(Vec<u8>),
    Av1 { width: u32, height: u32, bands: Vec<Av1Band> },
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Incoming>,
    closed: bool,
    /// Dopo una perdita di sincronia si scartano i frame AV1 finché non arriva un keyframe completo.
    waiting_key: bool,
}

/// Coda ordinata dei frame da decodificare. Il JPEG si può saltare (ogni frame è indipendente);
/// l'AV1 no, perché ogni frame dipende dai precedenti.
#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
}

enum Pushed {
    Ok,
    /// La decodifica è troppo indietro: serve un keyframe.
    Overflow,
}

impl Queue {
    fn push(&self, item: Incoming) -> Pushed {
        let mut st = self.state.lock().unwrap();
        let mut result = Pushed::Ok;
        match item {
            Incoming::Jpeg(_) => {
                // Conta solo l'ultima immagine: sostituisce un JPEG ancora in coda.
                if matches!(st.items.back(), Some(Incoming::Jpeg(_))) {
                    st.items.pop_back();
                }
                st.items.push_back(item);
            }
            Incoming::Av1 { height, ref bands, .. } => {
                let full_key = bands.iter().all(|b| b.key) && bands.iter().map(|b| b.h).sum::<u32>() == height;
                if st.waiting_key {
                    if !full_key {
                        return Pushed::Ok; // scartato, si attende il keyframe
                    }
                    st.waiting_key = false;
                    st.items.clear();
                }
                st.items.push_back(item);
                if st.items.len() > MAX_BACKLOG {
                    st.items.clear();
                    st.waiting_key = true;
                    result = Pushed::Overflow;
                }
            }
        }
        self.cv.notify_one();
        result
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.cv.notify_one();
    }

    fn resync(&self) {
        let mut st = self.state.lock().unwrap();
        st.items.clear();
        st.waiting_key = true;
    }

    fn pop(&self) -> Option<Incoming> {
        let mut st = self.state.lock().unwrap();
        loop {
            if let Some(item) = st.items.pop_front() {
                return Some(item);
            }
            if st.closed {
                return None;
            }
            st = self.cv.wait(st).unwrap();
        }
    }
}

pub struct Viewer {
    pub status: Arc<Mutex<ViewerStatus>>,
    /// Ultimo aggiornamento decodificato, non ancora caricato come texture.
    pub frame: Arc<Mutex<Option<TextureUpdate>>>,
    pub stats: Arc<Stats>,
    pub cursor: Arc<Mutex<CursorState>>,
    pub tx: UnboundedSender<ViewerMsg>,
}

impl Viewer {
    pub fn connect(rt: &Handle, ctx: egui::Context, server: String, target: String, password: String) -> Self {
        let status = Arc::new(Mutex::new(ViewerStatus::Connecting));
        let frame = Arc::new(Mutex::new(None));
        let stats = Arc::new(Stats::default());
        let cursor = Arc::new(Mutex::new(CursorState::default()));
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = Arc::new(Queue::default());

        spawn_decoder(queue.clone(), frame.clone(), ctx.clone(), tx.clone());

        let (status_task, ctx_task, stats_task, tx_task) = (status.clone(), ctx, stats.clone(), tx.clone());
        let cursor_task = cursor.clone();
        rt.spawn(async move {
            let link = Link {
                status: &status_task,
                queue: &queue,
                stats: &stats_task,
                cursor: &cursor_task,
                ctx: &ctx_task,
                tx: tx_task,
            };
            let result = run(&server, &target, &password, rx, link).await;
            queue.close();
            let reason = result.err().map(|e| format!("{e:#}"));
            *status_task.lock().unwrap() = ViewerStatus::Closed(reason);
            ctx_task.request_repaint();
        });

        Self { status, frame, stats, cursor, tx }
    }

    pub fn status(&self) -> ViewerStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn send(&self, msg: ViewerMsg) {
        let _ = self.tx.send(msg);
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.tx.send(ViewerMsg::Bye);
    }
}

fn merge(slot: &Mutex<Option<TextureUpdate>>, update: TextureUpdate) {
    let mut slot = slot.lock().unwrap();
    match slot.as_mut() {
        Some(cur) if update.full.is_none() && cur.size == update.size => cur.parts.extend(update.parts),
        _ => *slot = Some(update),
    }
}

fn spawn_decoder(
    queue: Arc<Queue>,
    out: Arc<Mutex<Option<TextureUpdate>>>,
    ctx: egui::Context,
    tx: UnboundedSender<ViewerMsg>,
) {
    thread::spawn(move || {
        let mut av1 = SlicedDecoder::new();
        // Schermo ricostruito per intero: le fasce aggiornano solo le proprie righe.
        let mut composite: Vec<u8> = Vec::new();
        let mut comp_size = (0u32, 0u32);

        while let Some(item) = queue.pop() {
            match item {
                Incoming::Jpeg(jpeg) => match image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg) {
                    Ok(img) => {
                        let rgb = img.to_rgb8();
                        let size = [rgb.width() as usize, rgb.height() as usize];
                        let full = egui::ColorImage::from_rgb(size, rgb.as_raw());
                        merge(&out, TextureUpdate { size, full: Some(full), parts: BTreeMap::new() });
                        ctx.request_repaint();
                    }
                    Err(e) => warn!("frame JPEG non decodificabile: {e}"),
                },
                Incoming::Av1 { width, height, bands } => {
                    if comp_size != (width, height) {
                        composite = vec![0u8; width as usize * height as usize * 4];
                        comp_size = (width, height);
                        av1 = SlicedDecoder::new();
                    }
                    let packets: Vec<BandPacket> = bands
                        .into_iter()
                        .map(|b| BandPacket { index: b.index, y: b.y, h: b.h, key: b.key, data: b.data })
                        .collect();
                    if let Err(e) = av1.decode(&packets, width, height, &mut composite) {
                        warn!("frame AV1 non decodificabile: {e:#}");
                        queue.resync();
                        let _ = tx.send(ViewerMsg::RequestKeyframe);
                        continue;
                    }
                    let stride = width as usize * 4;
                    let size = [width as usize, height as usize];
                    let mut parts = BTreeMap::new();
                    for p in &packets {
                        let rows = &composite[p.y as usize * stride..(p.y + p.h) as usize * stride];
                        let pixels = bytemuck::cast_slice::<u8, egui::Color32>(rows).to_vec();
                        parts.insert(p.y as usize, egui::ColorImage::new([width as usize, p.h as usize], pixels));
                    }
                    merge(&out, TextureUpdate { size, full: None, parts });
                    ctx.request_repaint();
                }
            }
        }
    });
}

/// Tutto ciò che serve al task di rete per parlare con UI e decoder.
struct Link<'a> {
    status: &'a Mutex<ViewerStatus>,
    queue: &'a Queue,
    stats: &'a Stats,
    cursor: &'a Mutex<CursorState>,
    ctx: &'a egui::Context,
    tx: UnboundedSender<ViewerMsg>,
}

async fn run(
    server: &str,
    target: &str,
    password: &str,
    mut rx: mpsc::UnboundedReceiver<ViewerMsg>,
    link: Link<'_>,
) -> Result<()> {
    let mut stream = timeout(Duration::from_secs(10), TcpStream::connect(server))
        .await
        .map_err(|_| anyhow!("timeout connettendo a {server}"))??;
    stream.set_nodelay(true)?;
    // Indirizzi UDP per il collegamento diretto (None se UDP non è disponibile).
    let udp = deco_net::prepare(server).await;
    let offer = udp.as_ref().map(|(_, offer)| offer.clone());
    send_plain(&mut stream, &ClientHello::Connect { target: target.to_string(), udp: offer }).await?;
    let peer_udp = match timeout(Duration::from_secs(20), recv_plain::<_, ServerMsg>(&mut stream))
        .await
        .map_err(|_| anyhow!("il server non risponde"))??
    {
        ServerMsg::Paired { peer_udp } => peer_udp,
        ServerMsg::Error(e) => bail!("{e}"),
        other => bail!("risposta inattesa dal server: {other:?}"),
    };

    let (id, pw) = (target.to_string(), password.to_string());
    let psk = tokio::task::spawn_blocking(move || derive_psk(&pw, &id)).await??;

    // Si prova prima il collegamento diretto QUIC; se non riesce si resta sul relay TCP.
    // La scelta viene comunicata all'host in chiaro sul relay (se la manomette, al peggio si ripiega sul TCP).
    let (read, write, direct): (BoxR, BoxW, Option<Direct>) = match (udp, peer_udp) {
        (Some((sock, _)), Some(peer)) => match deco_net::connect_to_host(sock, &peer, DIRECT_CONNECT_TIMEOUT).await {
            Ok(direct) => {
                send_plain(&mut stream, &PathMsg::UseQuic).await?;
                drop(stream);
                let (r, w) = direct.open_control().await?;
                (r, w, Some(direct))
            }
            Err(e) => {
                info!("collegamento diretto non riuscito ({e:#}): uso il relay");
                send_plain(&mut stream, &PathMsg::UseTcp).await?;
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w), None)
            }
        },
        _ => {
            let (r, w) = stream.into_split();
            (Box::new(r), Box::new(w), None)
        }
    };
    link.stats.direct.store(direct.is_some(), Ordering::Relaxed);
    info!("percorso dati: {}", if direct.is_some() { "QUIC diretto" } else { "relay TCP" });

    let (mut reader, mut writer) = timeout(Duration::from_secs(15), handshake_initiator(read, write, &psk))
        .await
        .map_err(|_| anyhow!("handshake scaduto: password errata?"))??;
    let lanes = reader.lanes();

    let stop = Arc::new(AtomicBool::new(false));
    // Finché l'host non accetta non si invia nulla (vedi la nota nel lato host).
    let approved = AtomicBool::new(false);
    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel();
    let (clip_in_tx, clip_in_rx) = std_mpsc::channel();
    clip::spawn(clip_out_tx, clip_in_rx, stop.clone());

    let send_loop = async {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(ViewerMsg::Bye) | None => {
                        let _ = writer.send(&ViewerMsg::Bye).await;
                        return Ok::<(), anyhow::Error>(());
                    }
                    Some(m) => writer.send(&m).await?,
                },
                Some(text) = clip_out_rx.recv(), if approved.load(Ordering::SeqCst) => {
                    writer.send(&ViewerMsg::Clipboard(text)).await?
                }
            }
        }
    };

    // Gestisce un messaggio dell'host, arrivi dalla stream di controllo o da quella video.
    // Restituisce `true` quando la sessione è finita.
    let handle = |msg: HostMsg| -> Result<bool> {
        match msg {
            HostMsg::AwaitingApproval => {
                *link.status.lock().unwrap() = ViewerStatus::WaitingApproval;
                link.ctx.request_repaint();
            }
            HostMsg::Denied => bail!("L'utente dell'host ha rifiutato la connessione."),
            HostMsg::Hello { host_name, screens, current, codecs } => {
                approved.store(true, Ordering::SeqCst);
                if codecs.contains(&VideoCodec::Av1) {
                    let _ = link.tx.send(ViewerMsg::SetCodec(VideoCodec::Av1));
                }
                *link.status.lock().unwrap() = ViewerStatus::Connected { host_name, screens, current: current as usize };
                link.ctx.request_repaint();
            }
            HostMsg::ScreenChanged { index } => {
                if let ViewerStatus::Connected { current, .. } = &mut *link.status.lock().unwrap() {
                    *current = index as usize;
                }
                link.ctx.request_repaint();
            }
            HostMsg::Frame { jpeg, .. } => {
                link.stats.frames.fetch_add(1, Ordering::Relaxed);
                link.stats.bytes.fetch_add(jpeg.len() as u64, Ordering::Relaxed);
                link.stats.av1.store(false, Ordering::Relaxed);
                link.queue.push(Incoming::Jpeg(jpeg));
            }
            HostMsg::Av1Frame { width, height, bands } => {
                link.stats.frames.fetch_add(1, Ordering::Relaxed);
                let bytes: usize = bands.iter().map(|b| b.data.len()).sum();
                link.stats.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                link.stats.av1.store(true, Ordering::Relaxed);
                if let Pushed::Overflow = link.queue.push(Incoming::Av1 { width, height, bands }) {
                    warn!("decodifica in ritardo: richiedo un keyframe");
                    let _ = link.tx.send(ViewerMsg::RequestKeyframe);
                }
            }
            HostMsg::Clipboard(text) => {
                let _ = clip_in_tx.send(text);
            }
            HostMsg::CursorShape { id, width, height, hot_x, hot_y, rgba } => {
                let (w, h) = (width as usize, height as usize);
                // Dati dalla rete: si scartano forme incoerenti invece di andare in panic.
                if w > 0 && h > 0 && w <= 256 && h <= 256 && rgba.len() == w * h * 4 {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    let mut cs = link.cursor.lock().unwrap();
                    cs.shapes.insert(id, Arc::new(CursorShape { hot_x, hot_y, image }));
                    if cs.shapes.len() > 128 {
                        let oldest = cs.shapes.keys().min().copied();
                        if let Some(oldest) = oldest {
                            cs.shapes.remove(&oldest);
                        }
                    }
                }
            }
            HostMsg::Cursor { x, y, shape, visible } => {
                {
                    let mut cs = link.cursor.lock().unwrap();
                    cs.pos = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
                    cs.shape = shape;
                    cs.visible = visible;
                }
                link.ctx.request_repaint();
            }
            HostMsg::Bye => return Ok(true),
        }
        Ok(false)
    };

    let recv_loop = async {
        loop {
            if handle(reader.recv::<HostMsg>().await?)? {
                return Ok::<(), anyhow::Error>(());
            }
        }
    };

    // Stream video separata (solo con il collegamento diretto).
    let video_loop = async {
        let Some(d) = direct.as_ref() else { return std::future::pending::<Result<()>>().await };
        let mut video = lanes.reader(d.accept_video().await?, VIDEO_LANE_NONCE_BASE);
        loop {
            if handle(video.recv::<HostMsg>().await?)? {
                return Ok(());
            }
        }
    };

    let rtt_loop = async {
        let Some(d) = direct.as_ref() else { return std::future::pending::<Result<()>>().await };
        loop {
            link.stats.rtt_ms.store(d.rtt().as_millis() as u32, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };

    let result = tokio::select! {
        r = send_loop => r,
        r = recv_loop => r,
        r = video_loop => r,
        r = rtt_loop => r,
    };
    stop.store(true, Ordering::Relaxed);
    if let Some(d) = &direct {
        d.close();
    }
    result
}
