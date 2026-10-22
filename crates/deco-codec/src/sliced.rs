//! Schermo diviso in fasce orizzontali, ciascuna con il proprio flusso AV1 indipendente.
//!
//! Perché: libaom non scala oltre ~25 ms a frame su uno schermo ultrawide, per via delle parti
//! seriali del codificatore. Con fasce indipendenti:
//! - le fasce si codificano e si decodificano in parallelo, ognuna su thread propri;
//! - le fasce che non sono cambiate non costano nulla (né CPU né banda);
//! - il viewer sa esattamente quali righe aggiornare.

use std::thread;

use anyhow::{bail, Result};
use xxhash_rust::xxh3::Xxh3;

use crate::{Av1Decoder, Av1Encoder, Chroma, Tuning};

/// Parametri della suddivisione in fasce.
#[derive(Clone, Copy, Debug)]
pub struct SlicedOptions {
    /// Altezza ideale di una fascia, in righe.
    pub band_height: u32,
    pub max_bands: u32,
    /// Thread libaom per ogni fascia (le fasce girano comunque in parallelo fra loro).
    pub threads_per_band: u32,
    pub tile_cols_log2: i32,
    pub cdef: bool,
    pub loop_filter: i32,
}

impl Default for SlicedOptions {
    /// Valori scelti col benchmark su 3440x1440 (vedi `examples/bench.rs`): 8 fasce da 180 righe,
    /// ognuna con fino a 4 thread, danno il miglior compromesso fra schermo fermo, piccole modifiche
    /// e scorrimento a tutto schermo.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(8);
        Self {
            band_height: 180,
            max_bands: 8,
            threads_per_band: (cores / 8).clamp(1, 4),
            tile_cols_log2: 2,
            cdef: true,
            loop_filter: 1,
        }
    }
}

/// Bitstream di una fascia per un dato frame.
#[derive(Debug, Clone)]
pub struct BandPacket {
    pub index: u16,
    /// Prima riga della fascia e sua altezza.
    pub y: u32,
    pub h: u32,
    pub key: bool,
    pub data: Vec<u8>,
}

struct Band {
    y: u32,
    h: u32,
    enc: Av1Encoder,
    last_hash: Option<u64>,
}

pub struct SlicedEncoder {
    width: u32,
    height: u32,
    bands: Vec<Band>,
}

fn band_layout(height: u32, opts: &SlicedOptions) -> Vec<(u32, u32)> {
    let n = (height / opts.band_height.max(16)).clamp(1, opts.max_bands.max(1));
    let (base, extra) = (height / n, height % n);
    let mut y = 0;
    (0..n)
        .map(|i| {
            let h = base + u32::from(i < extra);
            let band = (y, h);
            y += h;
            band
        })
        .collect()
}

impl SlicedEncoder {
    pub fn new(width: u32, height: u32, quality: u8, chroma: Chroma) -> Result<Self> {
        Self::with_options(width, height, quality, chroma, SlicedOptions::default())
    }

    pub fn with_options(width: u32, height: u32, quality: u8, chroma: Chroma, opts: SlicedOptions) -> Result<Self> {
        let layout = band_layout(height, &opts);
        let bands = layout
            .into_iter()
            .map(|(y, h)| {
                let mut tuning = Tuning::for_size(width, h, opts.threads_per_band);
                tuning.tile_rows_log2 = 0;
                tuning.tile_cols_log2 = opts.tile_cols_log2;
                tuning.cdef = opts.cdef;
                tuning.loop_filter = opts.loop_filter;
                Ok(Band { y, h, enc: Av1Encoder::with_tuning(width, h, quality, chroma, tuning)?, last_hash: None })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { width, height, bands })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    pub fn set_quality(&mut self, quality: u8) -> Result<()> {
        self.bands.iter_mut().try_for_each(|b| b.enc.set_quality(quality))
    }

    /// Codifica solo le fasce cambiate rispetto al frame precedente (tutte se `force_key`).
    /// Restituisce i pacchetti in ordine di fascia; vuoto se lo schermo è identico.
    pub fn encode_bgra(&mut self, bgra: &[u8], stride: usize, force_key: bool) -> Result<Vec<BandPacket>> {
        let width = self.width as usize;
        let results: Vec<Result<Option<BandPacket>>> = thread::scope(|scope| {
            let handles: Vec<_> = self
                .bands
                .iter_mut()
                .enumerate()
                .map(|(i, band)| {
                    scope.spawn(move || -> Result<Option<BandPacket>> {
                        let (y, h) = (band.y as usize, band.h as usize);
                        let start = y * stride;
                        let rows = &bgra[start..start + (h - 1) * stride + width * 4];

                        let mut hasher = Xxh3::new();
                        for r in 0..h {
                            hasher.update(&rows[r * stride..r * stride + width * 4]);
                        }
                        let hash = hasher.digest();
                        if !force_key && band.last_hash == Some(hash) {
                            return Ok(None);
                        }

                        let mut data = Vec::with_capacity(32 * 1024);
                        let key = band.enc.encode_bgra(rows, stride, force_key, &mut data)?;
                        band.last_hash = Some(hash);
                        Ok(key.map(|key| BandPacket { index: i as u16, y: band.y, h: band.h, key, data }))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread di codifica in panic")).collect()
        });
        let mut out = Vec::new();
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }
}

pub struct SlicedDecoder {
    decoders: Vec<Option<Av1Decoder>>,
}

impl SlicedDecoder {
    pub fn new() -> Self {
        Self { decoders: Vec::new() }
    }

    /// Decodifica i pacchetti (in parallelo) direttamente nel frame RGBA completo `frame`
    /// (`width`x`height`, righe contigue). Le fasce non presenti restano com'erano.
    ///
    /// I pacchetti devono essere ordinati per `y`, senza sovrapposizioni.
    pub fn decode(&mut self, packets: &[BandPacket], width: u32, height: u32, frame: &mut [u8]) -> Result<()> {
        let stride = width as usize * 4;
        if frame.len() != stride * height as usize {
            bail!("buffer del frame di dimensione errata");
        }
        let max_index = packets.iter().map(|p| p.index as usize + 1).max().unwrap_or(0);
        if self.decoders.len() < max_index {
            self.decoders.resize_with(max_index, || None);
        }
        for p in packets {
            if self.decoders[p.index as usize].is_none() {
                self.decoders[p.index as usize] = Some(Av1Decoder::new(2)?);
            }
        }

        // Ogni fascia scrive in un intervallo di righe distinto: si divide il buffer in fette
        // disgiunte, così i thread non condividono memoria mutabile.
        let mut slices = Vec::with_capacity(packets.len());
        let mut rest = frame;
        let mut consumed = 0u32;
        for p in packets {
            if p.y < consumed || p.h == 0 || p.y + p.h > height {
                bail!("fascia {} fuori posto (y={}, h={})", p.index, p.y, p.h);
            }
            let (_, tail) = rest.split_at_mut((p.y - consumed) as usize * stride);
            let (mine, tail) = tail.split_at_mut(p.h as usize * stride);
            slices.push(mine);
            rest = tail;
            consumed = p.y + p.h;
        }

        let mut slots: Vec<Option<&mut Av1Decoder>> = self.decoders.iter_mut().map(|d| d.as_mut()).collect();
        let mut jobs = Vec::with_capacity(packets.len());
        for (p, dst) in packets.iter().zip(slices) {
            let Some(dec) = slots[p.index as usize].take() else {
                bail!("fascia {} duplicata nello stesso frame", p.index);
            };
            jobs.push((p, dec, dst));
        }

        let results: Vec<Result<()>> = thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .into_iter()
                .map(|(p, dec, dst)| {
                    scope.spawn(move || -> Result<()> {
                        if !dec.decode_into(&p.data, width, p.h, dst, stride)? {
                            bail!("la fascia {} non ha prodotto un'immagine", p.index);
                        }
                        Ok(())
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread di decodifica in panic")).collect()
        });
        results.into_iter().collect()
    }
}

impl Default for SlicedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, seed: u8) -> Vec<u8> {
        let mut f = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                if (y / 12 + seed as usize) % 3 == 0 && x % 9 < 5 {
                    let i = (y * w + x) * 4;
                    f[i..i + 3].copy_from_slice(&[20, 20, 20]);
                }
            }
        }
        f
    }

    #[test]
    fn layout_covers_screen() {
        for h in [100, 180, 720, 1080, 1440, 2160] {
            let opts = SlicedOptions::default();
            let l = band_layout(h, &opts);
            assert_eq!(l.iter().map(|b| b.1).sum::<u32>(), h);
            assert!(l.windows(2).all(|w| w[0].0 + w[0].1 == w[1].0));
            assert!(l.len() <= opts.max_bands as usize);
        }
    }

    #[test]
    fn unchanged_bands_are_skipped_and_decode_matches() {
        let (w, h) = (640usize, 720usize);
        let mut enc = SlicedEncoder::new(w as u32, h as u32, 80, Chroma::Yuv444).unwrap();
        assert!(enc.band_count() > 1);
        let mut dec = SlicedDecoder::new();
        let mut shown = vec![0u8; w * h * 4];

        let f0 = frame(w, h, 0);
        let p = enc.encode_bgra(&f0, w * 4, false).unwrap();
        assert_eq!(p.len(), enc.band_count(), "primo frame: tutte le fasce");
        assert!(p.iter().all(|b| b.key));
        dec.decode(&p, w as u32, h as u32, &mut shown).unwrap();

        // Frame identico: niente da inviare.
        assert!(enc.encode_bgra(&f0, w * 4, false).unwrap().is_empty());

        // Cambia solo un rettangolo in alto: deve arrivare solo la prima fascia (o poche).
        let mut f1 = f0.clone();
        for y in 0..40 {
            for x in 0..200 {
                let i = (y * w + x) * 4;
                f1[i..i + 3].copy_from_slice(&[0, 0, 220]);
            }
        }
        let p = enc.encode_bgra(&f1, w * 4, false).unwrap();
        assert!(!p.is_empty() && p.len() < enc.band_count(), "{} fasce inviate", p.len());
        dec.decode(&p, w as u32, h as u32, &mut shown).unwrap();

        // Il frame mostrato deve coincidere (entro la compressione) con l'originale.
        let mut err = 0u64;
        for px in 0..w * h {
            for c in 0..3 {
                err += (f1[px * 4 + 2 - c] as i64 - shown[px * 4 + c] as i64).unsigned_abs();
            }
        }
        let mean = err as f64 / (w * h * 3) as f64;
        assert!(mean < 3.0, "errore medio {mean:.2}");

        // Keyframe forzato: tornano tutte.
        let p = enc.encode_bgra(&f1, w * 4, true).unwrap();
        assert_eq!(p.len(), enc.band_count());
    }
}
