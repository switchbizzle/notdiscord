//! Feasibility check: can we push live video INTO the webview by encoding
//! frames as JPEG fast enough? Times encoding at a few sizes.

use std::time::Instant;

fn frame(w: u32, h: u32) -> Vec<u8> {
    // Photo-ish content (gradients + noise), not a flat colour, so the
    // encoder does realistic work.
    let mut buf = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 3) as usize;
            let n = ((x * 7 + y * 13) % 37) as u8;
            buf[i] = (x * 255 / w) as u8 ^ n;
            buf[i + 1] = (y * 255 / h) as u8;
            buf[i + 2] = 128u8.wrapping_add(n);
        }
    }
    buf
}

fn bench(w: u32, h: u32, quality: u8) {
    let raw = frame(w, h);
    let mut ms = Vec::new();
    let mut bytes = 0usize;
    for _ in 0..12 {
        let start = Instant::now();
        let mut out = Vec::with_capacity(120_000);
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        enc.encode(&raw, w, h, image::ExtendedColorType::Rgb8).expect("encode");
        ms.push(start.elapsed().as_secs_f64() * 1000.0);
        bytes = out.len();
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = ms[ms.len() / 2];
    println!(
        "{w}x{h} q{quality}: {:.1} ms/frame · {:.0} KB · headroom {:.0} fps · {:.1} MB/s at 15fps",
        median,
        bytes as f64 / 1024.0,
        1000.0 / median,
        bytes as f64 * 15.0 / 1_048_576.0
    );
}

fn main() {
    println!("JPEG encode cost (single thread):");
    bench(1280, 720, 70);
    bench(960, 540, 70);
    bench(640, 360, 70);
    bench(640, 360, 55);
}
