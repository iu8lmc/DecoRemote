//! Codec video AV1 (libaom in modalità real-time, ottimizzato per contenuti da schermo).
//!
//! Scelte di progetto per la bassa latenza:
//! - `AOM_USAGE_REALTIME` con CBR, nessun lag e nessun frame di riordino: ogni frame esce subito;
//! - `tune_content = SCREEN` abilita palette e IntraBC, che sul testo e sulla grafica piatta
//!   rendono molto più dei tool pensati per il video naturale;
//! - crominanza 4:4:4 opzionale, per testo colorato senza sbavature.

use std::{mem::MaybeUninit, os::raw::c_int, ptr, time::Instant};

mod sliced;

pub use sliced::{BandPacket, SlicedDecoder, SlicedEncoder, SlicedOptions};

use anyhow::{anyhow, bail, Result};
use libaom_sys as aom;
use yuv::{
    YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

/// Spazio colore usato dai due lati: BT.709, range pieno.
const MATRIX: YuvStandardMatrix = YuvStandardMatrix::Bt709;
const RANGE: YuvRange = YuvRange::Full;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

fn check(code: aom::aom_codec_err_t, what: &str) -> Result<()> {
    if code == aom::AOM_CODEC_OK {
        Ok(())
    } else {
        bail!("libaom: {what} fallito (codice {code})")
    }
}

/// Parametri di rate control per uno slider di qualità 20..=95.
///
/// Restituisce (kbps, min_q, max_q); i quantizzatori di libaom vanno da 0 (migliore) a 63.
fn rate_for_quality(quality: u8, width: u32, height: u32) -> (u32, u32, u32) {
    let q = quality.clamp(20, 95) as f32;
    let t = (q - 20.0) / 75.0; // 0..1
    let mbit_per_mpx = 2.0 + 18.0 * t; // 2..20 Mbit/s per megapixel
    let mpx = (width * height) as f32 / 1.0e6;
    let kbps = (mbit_per_mpx * mpx * 1000.0).max(100.0) as u32;
    let max_q = (56.0 - 44.0 * t).round() as u32; // 56..12
    (kbps, max_q / 3, max_q)
}

/// Parametri di velocità/parallelismo dell'encoder.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub threads: u32,
    /// log2 del numero di colonne di tile (0..=6).
    pub tile_cols_log2: i32,
    /// log2 del numero di righe di tile (0..=6).
    pub tile_rows_log2: i32,
    /// 7..=10 per il real-time: più alto = più veloce, meno efficiente.
    pub cpu_used: i32,
    pub cdef: bool,
    pub restoration: bool,
    /// 0 = deblocking disattivato, 1 = attivo.
    pub loop_filter: i32,
}

impl Tuning {
    pub fn for_size(width: u32, height: u32, threads: u32) -> Self {
        Self {
            threads,
            tile_cols_log2: if width >= 3000 { 3 } else if width >= 1200 { 2 } else { 1 },
            tile_rows_log2: if height >= 1000 { 1 } else { 0 },
            cpu_used: 10,
            cdef: true,
            restoration: false,
            loop_filter: 1,
        }
    }
}

/// Tempi dell'ultima codifica, per diagnostica.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub convert_ms: f64,
    pub encode_ms: f64,
}

pub struct Av1Encoder {
    pub timings: Timings,
    ctx: aom::aom_codec_ctx_t,
    cfg: aom::aom_codec_enc_cfg_t,
    img: aom::aom_image_t,
    width: u32,
    height: u32,
    chroma: Chroma,
    started: Instant,
    last_pts: i64,
}

// SAFETY: il contesto libaom e l'immagine appartengono solo a questa struttura e non sono
// condivisi; si può spostare l'encoder tra thread purché venga usato da uno alla volta.
unsafe impl Send for Av1Encoder {}

impl Av1Encoder {
    pub fn new(width: u32, height: u32, quality: u8, chroma: Chroma, threads: u32) -> Result<Self> {
        Self::with_tuning(width, height, quality, chroma, Tuning::for_size(width, height, threads))
    }

    pub fn with_tuning(width: u32, height: u32, quality: u8, chroma: Chroma, tuning: Tuning) -> Result<Self> {
        if width == 0 || height == 0 || width > 16384 || height > 16384 {
            bail!("dimensioni non valide: {width}x{height}");
        }
        // SAFETY: strutture C plain-old-data inizializzate da libaom stessa.
        unsafe {
            let mut cfg = MaybeUninit::<aom::aom_codec_enc_cfg_t>::zeroed().assume_init();
            check(
                aom::aom_codec_enc_config_default(aom::aom_codec_av1_cx(), &mut cfg, aom::AOM_USAGE_REALTIME),
                "config di default",
            )?;
            let (kbps, min_q, max_q) = rate_for_quality(quality, width, height);
            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_threads = tuning.threads.max(1);
            cfg.g_profile = if chroma == Chroma::Yuv444 { 1 } else { 0 };
            cfg.g_timebase = aom::aom_rational { num: 1, den: 1000 };
            cfg.g_lag_in_frames = 0;
            cfg.g_error_resilient = 0;
            cfg.rc_end_usage = aom::AOM_CBR;
            cfg.rc_target_bitrate = kbps;
            cfg.rc_min_quantizer = min_q;
            cfg.rc_max_quantizer = max_q;
            cfg.rc_undershoot_pct = 50;
            cfg.rc_overshoot_pct = 50;
            // Buffer corto (ms): un solo frame grosso non deve accumulare latenza.
            cfg.rc_buf_sz = 200;
            cfg.rc_buf_initial_sz = 100;
            cfg.rc_buf_optimal_sz = 100;
            cfg.kf_mode = aom::AOM_KF_AUTO;
            cfg.kf_min_dist = 0;
            cfg.kf_max_dist = 3000;

            let mut ctx = MaybeUninit::<aom::aom_codec_ctx_t>::zeroed().assume_init();
            check(
                aom::aom_codec_enc_init_ver(&mut ctx, aom::aom_codec_av1_cx(), &cfg, 0, aom::AOM_ENCODER_ABI_VERSION as c_int),
                "inizializzazione encoder",
            )?;

            let mut ctl = |id: u32, value: c_int, name: &str| -> Result<()> {
                check(aom::aom_codec_control(&mut ctx as *mut _, id as c_int, value), name)
            };
            // Velocità massima del profilo real-time.
            ctl(aom::AOME_SET_CPUUSED, tuning.cpu_used, "cpu-used")?;
            ctl(aom::AV1E_SET_TUNE_CONTENT, aom::AOM_CONTENT_SCREEN as c_int, "tune-content")?;
            ctl(aom::AV1E_SET_ROW_MT, 1, "row-mt")?;
            ctl(aom::AV1E_SET_TILE_COLUMNS, tuning.tile_cols_log2, "tile-columns")?;
            ctl(aom::AV1E_SET_TILE_ROWS, tuning.tile_rows_log2, "tile-rows")?;
            ctl(aom::AV1E_SET_AQ_MODE, 3, "aq-mode")?;
            ctl(aom::AV1E_SET_DELTAQ_MODE, 0, "deltaq-mode")?;
            ctl(aom::AV1E_SET_ENABLE_TPL_MODEL, 0, "tpl")?;
            ctl(aom::AV1E_SET_COEFF_COST_UPD_FREQ, 3, "coeff-cost-upd")?;
            ctl(aom::AV1E_SET_ENABLE_CDEF, tuning.cdef as c_int, "cdef")?;
            ctl(aom::AV1E_SET_ENABLE_RESTORATION, tuning.restoration as c_int, "restoration")?;
            ctl(aom::AV1E_SET_LOOPFILTER_CONTROL, tuning.loop_filter, "loop-filter")?;
            ctl(aom::AV1E_SET_ENABLE_PALETTE, 1, "palette")?;
            ctl(aom::AV1E_SET_ENABLE_INTRABC, 1, "intrabc")?;
            ctl(aom::AV1E_SET_COLOR_RANGE, aom::AOM_CR_FULL_RANGE as c_int, "color-range")?;
            ctl(aom::AV1E_SET_COLOR_PRIMARIES, 1, "primaries")?;
            ctl(aom::AV1E_SET_TRANSFER_CHARACTERISTICS, 13, "transfer")?;
            ctl(aom::AV1E_SET_MATRIX_COEFFICIENTS, aom::AOM_CICP_MC_BT_709 as c_int, "matrix")?;

            let mut img = MaybeUninit::<aom::aom_image_t>::zeroed().assume_init();
            let fmt = match chroma {
                Chroma::Yuv420 => aom::AOM_IMG_FMT_I420,
                Chroma::Yuv444 => aom::AOM_IMG_FMT_I444,
            };
            if aom::aom_img_alloc(&mut img, fmt, width, height, 32).is_null() {
                aom::aom_codec_destroy(&mut ctx);
                bail!("libaom: allocazione immagine fallita");
            }
            img.range = aom::AOM_CR_FULL_RANGE;
            img.mc = aom::AOM_CICP_MC_BT_709;
            img.cp = 1;
            img.tc = 13;

            Ok(Self { timings: Timings::default(), ctx, cfg, img, width, height, chroma, started: Instant::now(), last_pts: -1 })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn chroma(&self) -> Chroma {
        self.chroma
    }

    /// Imposta un controllo libaom grezzo (per sperimentazione e benchmark).
    pub fn control(&mut self, id: u32, value: i32) -> Result<()> {
        // SAFETY: ctx valido per tutta la vita di `self`; il valore è un intero come richiedono questi controlli.
        unsafe { check(aom::aom_codec_control(&mut self.ctx, id as c_int, value as c_int), "controllo") }
    }

    /// Cambia al volo bitrate e quantizzatori, senza ricreare l'encoder né inserire un keyframe.
    pub fn set_quality(&mut self, quality: u8) -> Result<()> {
        let (kbps, min_q, max_q) = rate_for_quality(quality, self.width, self.height);
        self.cfg.rc_target_bitrate = kbps;
        self.cfg.rc_min_quantizer = min_q;
        self.cfg.rc_max_quantizer = max_q;
        // SAFETY: ctx e cfg sono validi per tutta la vita di `self`.
        unsafe { check(aom::aom_codec_enc_config_set(&mut self.ctx, &self.cfg), "cambio qualità") }
    }

    /// Codifica un frame BGRA (`stride` in byte) e accoda il bitstream a `out`.
    /// Restituisce `Some(is_keyframe)`, oppure `None` se l'encoder ha scartato il frame.
    pub fn encode_bgra(&mut self, bgra: &[u8], stride: usize, force_key: bool, out: &mut Vec<u8>) -> Result<Option<bool>> {
        let (w, h) = (self.width as usize, self.height as usize);
        if stride < w * 4 || bgra.len() < stride * (h - 1) + w * 4 {
            bail!("buffer BGRA troppo piccolo");
        }
        let t0 = Instant::now();
        self.convert(bgra, stride as u32)?;
        self.timings.convert_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = Instant::now();

        let pts = (self.started.elapsed().as_millis() as i64).max(self.last_pts + 1);
        let duration = (pts - self.last_pts).clamp(1, 1000) as u64;
        self.last_pts = pts;
        let flags = if force_key { aom::AOM_EFLAG_FORCE_KF as i64 } else { 0 };
        // SAFETY: `img` è un'immagine valida allocata da libaom; ctx è inizializzato.
        unsafe {
            check(aom::aom_codec_encode(&mut self.ctx, &self.img, pts, duration as _, flags as _), "encode")?;
            let mut iter: aom::aom_codec_iter_t = ptr::null();
            let mut key = None;
            loop {
                let pkt = aom::aom_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                let pkt = &*pkt;
                if pkt.kind == aom::AOM_CODEC_CX_FRAME_PKT {
                    let f = &pkt.data.frame;
                    out.extend_from_slice(std::slice::from_raw_parts(f.buf as *const u8, f.sz));
                    key = Some(f.flags & aom::AOM_FRAME_IS_KEY != 0);
                }
            }
            self.timings.encode_ms = t1.elapsed().as_secs_f64() * 1000.0;
            Ok(key)
        }
    }

    fn convert(&mut self, bgra: &[u8], stride: u32) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let chroma_h = match self.chroma {
            Chroma::Yuv420 => h.div_ceil(2),
            Chroma::Yuv444 => h,
        };
        // SAFETY: i piani sono allocati da aom_img_alloc con stride e altezza indicati; le slice
        // vivono solo per questa conversione, durante la quale l'immagine non viene toccata altrove.
        let mut planar = unsafe {
            let y = std::slice::from_raw_parts_mut(self.img.planes[0], self.img.stride[0] as usize * h as usize);
            let u = std::slice::from_raw_parts_mut(self.img.planes[1], self.img.stride[1] as usize * chroma_h as usize);
            let v = std::slice::from_raw_parts_mut(self.img.planes[2], self.img.stride[2] as usize * chroma_h as usize);
            YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(y),
                y_stride: self.img.stride[0] as u32,
                u_plane: yuv::BufferStoreMut::Borrowed(u),
                u_stride: self.img.stride[1] as u32,
                v_plane: yuv::BufferStoreMut::Borrowed(v),
                v_stride: self.img.stride[2] as u32,
                width: w,
                height: h,
            }
        };
        let mode = YuvConversionMode::Balanced;
        match self.chroma {
            Chroma::Yuv420 => yuv::bgra_to_yuv420(&mut planar, bgra, stride, RANGE, MATRIX, mode),
            Chroma::Yuv444 => yuv::bgra_to_yuv444(&mut planar, bgra, stride, RANGE, MATRIX, mode),
        }
        .map_err(|e| anyhow!("conversione colore: {e:?}"))
    }
}

impl Drop for Av1Encoder {
    fn drop(&mut self) {
        // SAFETY: risorse create in `new` e rilasciate una sola volta.
        unsafe {
            aom::aom_img_free(&mut self.img);
            aom::aom_codec_destroy(&mut self.ctx);
        }
    }
}

pub struct Av1Decoder {
    ctx: aom::aom_codec_ctx_t,
}

// SAFETY: vedi Av1Encoder.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new(threads: u32) -> Result<Self> {
        // SAFETY: strutture C inizializzate prima dell'uso; il contesto è distrutto in Drop.
        unsafe {
            let cfg = aom::aom_codec_dec_cfg { threads: threads.max(1), w: 0, h: 0, allow_lowbitdepth: 1 };
            let mut ctx = MaybeUninit::<aom::aom_codec_ctx_t>::zeroed().assume_init();
            check(
                aom::aom_codec_dec_init_ver(&mut ctx, aom::aom_codec_av1_dx(), &cfg, 0, aom::AOM_DECODER_ABI_VERSION as c_int),
                "inizializzazione decoder",
            )?;
            Ok(Self { ctx })
        }
    }

    /// Decodifica un frame e scrive i pixel come RGBA (8 bit) in `out`.
    /// Restituisce `(larghezza, altezza)` se è uscita un'immagine.
    pub fn decode_rgba(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<Option<(u32, u32)>> {
        let mut size = None;
        let r = self.decode_with(data, |w, h| {
            size = Some((w, h));
            out.resize(w as usize * h as usize * 4, 0);
            Ok((&mut out[..], w as usize * 4))
        })?;
        Ok(r.then_some(size).flatten())
    }

    /// Decodifica un frame direttamente in `dst` (RGBA, `stride` byte per riga), che deve avere
    /// esattamente le dimensioni `width`x`height` attese. Restituisce `false` se non è uscita
    /// nessuna immagine.
    pub fn decode_into(&mut self, data: &[u8], width: u32, height: u32, dst: &mut [u8], stride: usize) -> Result<bool> {
        self.decode_with(data, |w, h| {
            if (w, h) != (width, height) {
                bail!("dimensioni decodificate {w}x{h} diverse da quelle attese {width}x{height}");
            }
            if dst.len() < stride * (h as usize - 1) + w as usize * 4 {
                bail!("buffer di destinazione troppo piccolo");
            }
            Ok((&mut dst[..], stride))
        })
    }

    fn decode_with<'a>(
        &mut self,
        data: &[u8],
        dst: impl FnOnce(u32, u32) -> Result<(&'a mut [u8], usize)>,
    ) -> Result<bool> {
        // SAFETY: il puntatore restituito da get_frame vale fino alla chiamata successiva su ctx.
        unsafe {
            check(aom::aom_codec_decode(&mut self.ctx, data.as_ptr(), data.len(), ptr::null_mut()), "decode")?;
            let mut iter: aom::aom_codec_iter_t = ptr::null();
            let mut last: *const aom::aom_image_t = ptr::null();
            loop {
                let img = aom::aom_codec_get_frame(&mut self.ctx, &mut iter);
                if img.is_null() {
                    break;
                }
                last = img;
            }
            if last.is_null() {
                return Ok(false);
            }
            let img = &*last;
            let (w, h) = (img.d_w, img.d_h);
            let chroma_h = ((h + img.y_chroma_shift) >> img.y_chroma_shift) as usize;
            let planar = YuvPlanarImage {
                y_plane: std::slice::from_raw_parts(img.planes[0], img.stride[0] as usize * h as usize),
                y_stride: img.stride[0] as u32,
                u_plane: std::slice::from_raw_parts(img.planes[1], img.stride[1] as usize * chroma_h),
                u_stride: img.stride[1] as u32,
                v_plane: std::slice::from_raw_parts(img.planes[2], img.stride[2] as usize * chroma_h),
                v_stride: img.stride[2] as u32,
                width: w,
                height: h,
            };
            if img.bit_depth != 8 {
                bail!("profondità colore non supportata: {}", img.bit_depth);
            }
            let (out, stride) = dst(w, h)?;
            match img.fmt {
                f if f == aom::AOM_IMG_FMT_I420 => yuv::yuv420_to_rgba(&planar, out, stride as u32, RANGE, MATRIX),
                f if f == aom::AOM_IMG_FMT_I444 => yuv::yuv444_to_rgba(&planar, out, stride as u32, RANGE, MATRIX),
                f => bail!("formato immagine non supportato: {f}"),
            }
            .map_err(|e| anyhow!("conversione colore: {e:?}"))?;
            Ok(true)
        }
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        // SAFETY: contesto creato in `new`.
        unsafe {
            aom::aom_codec_destroy(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Immagine sintetica "da desktop": sfondo piatto, righe di testo simulate, un gradiente.
    fn desktop_frame(w: usize, h: usize, shift: usize) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let sy = (y + shift) % h;
                let text = (sy / 14) % 2 == 0 && (x / 6 + sy / 14) % 3 != 0 && sy % 14 < 9;
                let (b, g, r) = if text {
                    (30, 30, 200)
                } else if x < 80 {
                    ((x * 3) as u8, 120, (sy % 256) as u8)
                } else {
                    (250, 250, 250)
                };
                let i = (y * w + x) * 4;
                f[i..i + 4].copy_from_slice(&[b, g, r, 255]);
            }
        }
        f
    }

    fn roundtrip(chroma: Chroma) {
        let (w, h) = (640, 360);
        let mut enc = Av1Encoder::new(w as u32, h as u32, 80, chroma, 4).unwrap();
        let mut dec = Av1Decoder::new(4).unwrap();
        let mut rgba = Vec::new();
        for i in 0..6 {
            let frame = desktop_frame(w, h, i * 3);
            let mut bits = Vec::new();
            let key = enc.encode_bgra(&frame, w * 4, false, &mut bits).unwrap();
            assert!(!bits.is_empty(), "frame {i} senza bitstream");
            assert_eq!(key, Some(i == 0), "solo il primo frame deve essere keyframe");
            let (dw, dh) = dec.decode_rgba(&bits, &mut rgba).unwrap().expect("frame decodificato");
            assert_eq!((dw as usize, dh as usize), (w, h));
            // Errore medio per canale: deve essere basso a qualità alta.
            let mut err = 0u64;
            for p in 0..w * h {
                err += (frame[p * 4] as i64 - rgba[p * 4 + 2] as i64).unsigned_abs();
                err += (frame[p * 4 + 1] as i64 - rgba[p * 4 + 1] as i64).unsigned_abs();
                err += (frame[p * 4 + 2] as i64 - rgba[p * 4] as i64).unsigned_abs();
            }
            let mean = err as f64 / (w * h * 3) as f64;
            assert!(mean < 6.0, "{chroma:?} frame {i}: errore medio {mean:.2}");
        }
    }

    #[test]
    fn roundtrip_420() {
        roundtrip(Chroma::Yuv420);
    }

    #[test]
    fn roundtrip_444() {
        roundtrip(Chroma::Yuv444);
    }

    #[test]
    fn forced_keyframe_and_quality_change() {
        let (w, h) = (320, 200);
        let mut enc = Av1Encoder::new(w as u32, h as u32, 60, Chroma::Yuv420, 2).unwrap();
        let mut bits = Vec::new();
        enc.encode_bgra(&desktop_frame(w, h, 0), w * 4, false, &mut bits).unwrap();
        enc.set_quality(90).unwrap();
        bits.clear();
        enc.encode_bgra(&desktop_frame(w, h, 2), w * 4, false, &mut bits).unwrap();
        bits.clear();
        let key = enc.encode_bgra(&desktop_frame(w, h, 4), w * 4, true, &mut bits).unwrap();
        assert_eq!(key, Some(true));
    }
}
