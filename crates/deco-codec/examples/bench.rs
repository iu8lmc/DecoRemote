//! Benchmark con codec a fasce: cattura lo schermo reale una volta, poi simula tre scenari.
//! Uso: cargo run --release -p deco-codec --example bench [420|444] [qualità] [thread]

use std::time::Instant;

use deco_codec::{Chroma, SlicedDecoder, SlicedEncoder, SlicedOptions};
use scrap::{Capturer, Display};

fn grab() -> (Vec<u8>, usize, usize) {
    let display = Display::primary().expect("schermo");
    let (w, h) = (display.width(), display.height());
    let mut cap = Capturer::new(display).expect("capturer");
    for _ in 0..200 {
        if let Ok(buf) = cap.frame() {
            let stride = buf.len() / h;
            let mut tight = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                tight.extend_from_slice(&buf[y * stride..y * stride + w * 4]);
            }
            return (tight, w, h);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("nessun frame catturato");
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = s.iter().sum::<f64>() / s.len().max(1) as f64;
    (avg, s[((s.len() as f64 - 1.0) * 0.95) as usize], *s.last().unwrap())
}

fn error_vs(frame: &[u8], shown: &[u8], w: usize, h: usize) -> (f64, f64) {
    let (mut exact, mut err) = (0usize, 0u64);
    for p in 0..w * h {
        let d = [
            (frame[p * 4 + 2] as i32 - shown[p * 4] as i32).unsigned_abs(),
            (frame[p * 4 + 1] as i32 - shown[p * 4 + 1] as i32).unsigned_abs(),
            (frame[p * 4] as i32 - shown[p * 4 + 2] as i32).unsigned_abs(),
        ];
        err += d.iter().map(|&x| x as u64).sum::<u64>();
        exact += d.iter().all(|&x| x <= 3) as usize;
    }
    (err as f64 / (w * h * 3) as f64, exact as f64 * 100.0 / (w * h) as f64)
}

fn main() {
    let chroma = if std::env::args().nth(1).as_deref() == Some("420") { Chroma::Yuv420 } else { Chroma::Yuv444 };
    let quality: u8 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(70);
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<i32>().ok());
    let mut opts = SlicedOptions::default();
    if let Some(v) = env("BAND_H") { opts.band_height = v as u32; }
    if let Some(v) = env("MAX_BANDS") { opts.max_bands = v as u32; }
    if let Some(v) = env("PER_BAND") { opts.threads_per_band = v as u32; }
    if let Some(v) = env("TILE_COLS") { opts.tile_cols_log2 = v; }
    if let Some(v) = env("CDEF") { opts.cdef = v != 0; }
    if let Some(v) = env("LF") { opts.loop_filter = v; }
    
    let (base, w, h) = grab();
    let mut enc = SlicedEncoder::with_options(w as u32, h as u32, quality, chroma, opts).unwrap();
    println!("schermo {w}x{h}  {chroma:?}  qualità {quality}  fasce {}  {opts:?}", enc.band_count());
    let mut dec = SlicedDecoder::new();
    let mut shown = vec![0u8; w * h * 4];

    // Keyframe iniziale.
    let t = Instant::now();
    let p = enc.encode_bgra(&base, w * 4, false).unwrap();
    let enc_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    dec.decode(&p, w as u32, h as u32, &mut shown).unwrap();
    println!(
        "keyframe (tutte le fasce): codifica {enc_ms:.1} ms, decodifica {:.1} ms, {:.0} KB",
        t.elapsed().as_secs_f64() * 1000.0,
        p.iter().map(|b| b.data.len()).sum::<usize>() as f64 / 1024.0
    );

    for scenario in ["scorrimento (tutto cambia)", "digitazione (area piccola)", "schermo fermo"] {
        let mut frame = base.clone();
        let (mut te, mut td, mut sz, mut nb) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in 0..90usize {
            if scenario.starts_with("scorr") {
                frame.rotate_left(4 * w * 4);
            } else if scenario.starts_with("digit") {
                // Riquadro 400x60 che cambia a ogni frame, in alto a sinistra.
                for y in 0..60 {
                    for x in 0..400 {
                        let k = (y * w + x) * 4;
                        frame[k] = (i * 7 + x) as u8;
                        frame[k + 1] = (i * 3 + y) as u8;
                    }
                }
            }
            let t = Instant::now();
            let p = enc.encode_bgra(&frame, w * 4, false).unwrap();
            te.push(t.elapsed().as_secs_f64() * 1000.0);
            nb.push(p.len() as f64);
            sz.push(p.iter().map(|b| b.data.len()).sum::<usize>() as f64);
            let t = Instant::now();
            dec.decode(&p, w as u32, h as u32, &mut shown).unwrap();
            td.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let (ea, e95, emax) = stats(&te);
        let (da, d95, _) = stats(&td);
        let kb = sz.iter().sum::<f64>() / sz.len() as f64 / 1024.0;
        let bands = nb.iter().sum::<f64>() / nb.len() as f64;
        let (mean, exact) = error_vs(&frame, &shown, w, h);
        println!("\n[{scenario}]");
        println!("  codifica  media {ea:.1} ms  p95 {e95:.1}  max {emax:.1}   (fasce attive in media: {bands:.1})");
        println!("  decodifica media {da:.1} ms  p95 {d95:.1}");
        println!("  {kb:.1} KB/frame  ->  {:.1} Mbit/s a 60 fps", kb * 1024.0 * 8.0 * 60.0 / 1e6);
        println!("  qualità: errore medio {mean:.2}/255, pixel entro ±3: {exact:.1}%");
    }
}
