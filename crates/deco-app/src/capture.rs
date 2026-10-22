//! Cattura degli schermi (uno alla volta, selezionabile) e codifica (AV1 a fasce, oppure JPEG).

use std::{
    io::ErrorKind,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Result};
use deco_codec::{Chroma, SlicedEncoder};
use deco_proto::msg::{Av1Band, ScreenInfo, VideoCodec};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use scrap::{Capturer, Display};
use tokio::sync::mpsc;
use tracing::warn;

const JPEG_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const AV1_FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Frame pronto da inviare.
pub enum Encoded {
    Jpeg { width: u32, height: u32, jpeg: Vec<u8> },
    /// Solo le fasce cambiate.
    Av1 { width: u32, height: u32, bands: Vec<Av1Band> },
}

/// Parametri di codifica modificabili durante la sessione (dal viewer).
pub struct Controls {
    quality: AtomicU8,
    codec: AtomicU8,
    force_key: AtomicBool,
}

impl Controls {
    pub fn new(quality: u8) -> Arc<Self> {
        Arc::new(Self {
            quality: AtomicU8::new(quality),
            // Si parte in JPEG: il primo frame arriva subito, poi il viewer sceglie AV1.
            codec: AtomicU8::new(0),
            force_key: AtomicBool::new(false),
        })
    }

    pub fn set_quality(&self, q: u8) {
        self.quality.store(q.clamp(20, 95), Ordering::Relaxed);
    }

    pub fn set_codec(&self, c: VideoCodec) {
        self.codec.store(matches!(c, VideoCodec::Av1) as u8, Ordering::Relaxed);
    }

    /// Chiede che il prossimo frame AV1 contenga tutte le fasce come keyframe.
    pub fn request_keyframe(&self) {
        self.force_key.store(true, Ordering::Relaxed);
    }

    fn codec(&self) -> VideoCodec {
        if self.codec.load(Ordering::Relaxed) == 1 { VideoCodec::Av1 } else { VideoCodec::Jpeg }
    }
}

/// Rettangolo di uno schermo nel desktop virtuale, in pixel fisici.
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Schermo attualmente trasmesso, condiviso tra thread di cattura e di input.
pub struct Screens {
    index: AtomicUsize,
    rect: Mutex<Rect>,
}

impl Screens {
    pub fn new(index: usize) -> Arc<Self> {
        Arc::new(Self { index: AtomicUsize::new(index), rect: Mutex::new(Rect::default()) })
    }

    pub fn select(&self, index: usize) {
        self.index.store(index, Ordering::SeqCst);
    }

    pub fn index(&self) -> usize {
        self.index.load(Ordering::SeqCst)
    }

    /// Posizione e dimensione dello schermo in uso (aggiornata dal thread di cattura).
    pub fn rect(&self) -> Rect {
        *self.rect.lock().unwrap()
    }
}

/// Elenco degli schermi e indice di quello principale (origine 0,0).
pub fn list() -> Result<(Vec<ScreenInfo>, usize)> {
    let displays = Display::all().map_err(|e| anyhow!("impossibile elencare gli schermi: {e}"))?;
    if displays.is_empty() {
        bail!("nessuno schermo disponibile");
    }
    let mut primary_idx = 0;
    let screens = displays
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let primary = d.origin() == (0, 0);
            if primary {
                primary_idx = i;
            }
            ScreenInfo { width: d.width() as u32, height: d.height() as u32, primary }
        })
        .collect();
    Ok((screens, primary_idx))
}

enum Exit {
    /// Sessione chiusa o fermata.
    Done,
    /// Il viewer ha scelto un altro schermo.
    Switched,
}

/// Avvia il thread di cattura. Il canale ha capacità 1: la cattura procede solo quando
/// il frame precedente è stato preso in carico, così non si accodano frame vecchi.
pub fn spawn(tx: mpsc::Sender<Encoded>, controls: Arc<Controls>, stop: Arc<AtomicBool>, screens: Arc<Screens>) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match run(&tx, &controls, &stop, &screens) {
                Ok(Exit::Done) => return,
                Ok(Exit::Switched) => {}
                Err(e) => {
                    // Cambio risoluzione, desktop sicuro (UAC), schermo spento: riprova.
                    warn!("cattura interrotta: {e:#}");
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });
}

fn run(tx: &mpsc::Sender<Encoded>, controls: &Controls, stop: &AtomicBool, screens: &Screens) -> Result<Exit> {
    let displays = Display::all().map_err(|e| anyhow!("{e}"))?;
    if displays.is_empty() {
        bail!("nessuno schermo disponibile");
    }
    // Se il monitor scelto è sparito (scollegato) si ripiega sull'ultimo disponibile.
    let wanted = screens.index();
    let idx = wanted.min(displays.len() - 1);
    // compare_exchange: non sovrascrive una scelta più recente del viewer.
    let _ = screens.index.compare_exchange(wanted, idx, Ordering::SeqCst, Ordering::SeqCst);
    let display = displays.into_iter().nth(idx).expect("indice verificato");
    let (w, h) = (display.width(), display.height());
    let (x, y) = display.origin();
    *screens.rect.lock().unwrap() = Rect { x, y, w: w as u32, h: h as u32 };
    let mut capturer = Capturer::new(display).map_err(|e| anyhow!("{e}"))?;
    let mut tight = Vec::new();
    let mut last_frame = Instant::now() - JPEG_FRAME_INTERVAL;
    let mut av1: Option<SlicedEncoder> = None;
    let mut av1_quality = controls.quality.load(Ordering::Relaxed);

    while !stop.load(Ordering::Relaxed) {
        if screens.index() != idx {
            return Ok(Exit::Switched);
        }
        let codec = controls.codec();
        let interval = if codec == VideoCodec::Av1 { AV1_FRAME_INTERVAL } else { JPEG_FRAME_INTERVAL };
        if let Some(wait) = interval.checked_sub(last_frame.elapsed()) {
            thread::sleep(wait);
        }
        match capturer.frame() {
            Ok(buf) => {
                let stride = buf.len() / h;
                let quality = controls.quality.load(Ordering::Relaxed);
                let (width, height) = (w as u32, h as u32);
                let encoded = match codec {
                    VideoCodec::Av1 => {
                        if av1.is_none() {
                            // Un encoder nuovo emette tutte le fasce come keyframe.
                            av1_quality = quality;
                            av1 = Some(SlicedEncoder::new(width, height, quality, Chroma::Yuv444)?);
                        }
                        let enc = av1.as_mut().expect("creato sopra");
                        if quality != av1_quality {
                            enc.set_quality(quality)?;
                            av1_quality = quality;
                        }
                        let force = controls.force_key.swap(false, Ordering::Relaxed);
                        let packets = enc.encode_bgra(&buf, stride, force)?;
                        // Schermo identico: niente da inviare.
                        (!packets.is_empty()).then(|| Encoded::Av1 {
                            width,
                            height,
                            bands: packets
                                .into_iter()
                                .map(|p| Av1Band { index: p.index, y: p.y, h: p.h, key: p.key, data: p.data })
                                .collect(),
                        })
                    }
                    VideoCodec::Jpeg => {
                        av1 = None;
                        let mut jpeg = Vec::with_capacity(128 * 1024);
                        encode_jpeg(&buf, w, h, stride, quality, &mut tight, &mut jpeg)?;
                        Some(Encoded::Jpeg { width, height, jpeg })
                    }
                };
                last_frame = Instant::now();
                if let Some(frame) = encoded {
                    if tx.blocking_send(frame).is_err() {
                        return Ok(Exit::Done); // sessione chiusa
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(anyhow!("{e}")),
        }
    }
    Ok(Exit::Done)
}

fn encode_jpeg(
    buf: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    quality: u8,
    tight: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> Result<()> {
    let row = w * 4;
    let data: &[u8] = if stride == row {
        &buf[..row * h]
    } else {
        tight.clear();
        for y in 0..h {
            tight.extend_from_slice(&buf[y * stride..y * stride + row]);
        }
        tight
    };
    let mut encoder = Encoder::new(out, quality.clamp(20, 95));
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder.encode(data, w as u16, h as u16, ColorType::Bgra)?;
    Ok(())
}
