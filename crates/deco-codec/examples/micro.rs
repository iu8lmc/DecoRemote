//! Micro-benchmark: costo fisso per chiamata di libaom su immagini piccole.
//! Uso: micro LARGHEZZA ALTEZZA THREAD [cpu_used]

use std::time::Instant;

use deco_codec::{Av1Encoder, Chroma, Tuning};

fn main() {
    let a: Vec<u32> = std::env::args().skip(1).filter_map(|s| s.parse().ok()).collect();
    let (w, h, threads) = (a[0] as usize, a[1] as usize, a[2]);
    let mut tuning = Tuning::for_size(w as u32, h as u32, threads);
    tuning.tile_rows_log2 = 0;
    tuning.tile_cols_log2 = 0;
    if let Some(c) = a.get(3) {
        tuning.cpu_used = *c as i32;
    }
    let mut enc = Av1Encoder::with_tuning(w as u32, h as u32, 70, Chroma::Yuv444, tuning).unwrap();
    let mut frame = vec![250u8; w * h * 4];
    for (i, px) in frame.chunks_mut(4).enumerate() {
        if (i / w / 10) % 2 == 0 && (i % w) % 7 < 3 {
            px[..3].copy_from_slice(&[30, 30, 30]);
        }
    }
    // CTRL="43=0,104=0": coppie id=valore di controlli libaom da applicare dopo l'init.
    if let Ok(list) = std::env::var("CTRL") {
        for pair in list.split(',').filter(|p| !p.is_empty()) {
            let (id, val) = pair.split_once('=').unwrap();
            enc.control(id.parse().unwrap(), val.parse().unwrap()).unwrap();
        }
    }
    let mut times = Vec::new();
    for i in 0..60 {
        // Cambia un solo piccolo pixel (4x4) per frame.
        let k = ((i % 50) * 4 + 8) * 4;
        for r in 0..4 {
            for c in 0..4 {
                frame[(r * w * 4) + k + c * 4] = (i * 5) as u8;
            }
        }
        let mut out = Vec::new();
        let t = Instant::now();
        enc.encode_bgra(&frame, w * 4, false, &mut out).unwrap();
        times.push((t.elapsed().as_secs_f64() * 1000.0, enc.timings.convert_ms, enc.timings.encode_ms, out.len()));
    }
    let n = times.len() - 1;
    let avg = |f: fn(&(f64, f64, f64, usize)) -> f64| times[1..].iter().map(f).sum::<f64>() / n as f64;
    println!(
        "{w}x{h} thread {threads} cpu_used {}: totale {:.2} ms (conversione {:.2}, libaom {:.2}), {:.0} B/frame",
        tuning.cpu_used,
        avg(|t| t.0),
        avg(|t| t.1),
        avg(|t| t.2),
        avg(|t| t.3 as f64)
    );
}
