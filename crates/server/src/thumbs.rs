//! Image thumbnails, so a 12 MB photo isn't downloaded in full to draw a
//! 40px tile. One thumb per upload, generated at upload time and lazily for
//! anything that predates this, stored beside the original as `thumb.jpg`.
//!
//! This decodes user-supplied images, so decoding runs under explicit
//! limits: a malicious file can claim enormous dimensions and blow up
//! memory long before anything looks at the pixels.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// Longest edge of a generated thumbnail. This is a budget for the LONGEST
/// edge, so the short edge of a tall image gets whatever the aspect ratio
/// leaves it: at 400 a 9:20 phone screenshot came out 180px wide, which is
/// less than half the width the phone draws it at and looked like mush. 800
/// gives that same screenshot 360px — sharp at the size it is rendered, and
/// still a fraction of a multi-megabyte original.
const THUMB_EDGE: u32 = 800;
const THUMB_QUALITY: u8 = 78;
/// Refuse to decode anything claiming more pixels than this (~40 MP).
const MAX_PIXELS: u64 = 40_000_000;
const MAX_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

pub fn is_thumbable(name: &str) -> bool {
    matches!(
        name.rsplit('.').next().unwrap_or_default().to_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif"
    )
}

/// Where the thumbnail for an original lives: beside it, so deleting the
/// upload directory (retention) takes the thumb with it. The leading dot
/// makes collision impossible — sanitize_filename trims leading dots, so no
/// upload can ever be named this and have its thumb overwrite it.
pub fn thumb_path(original: &Path) -> Option<PathBuf> {
    Some(original.parent()?.join(".thumb.jpg"))
}

/// Decode, scale, and JPEG-encode. Returns None when the file isn't a
/// decodable image or is too large to be worth trusting.
fn render(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_ALLOC_BYTES);
    reader.limits(limits);

    let (w, h) = reader.into_dimensions().ok()?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    // Already small enough: no thumb, callers fall back to the original.
    if w <= THUMB_EDGE && h <= THUMB_EDGE {
        return None;
    }

    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_ALLOC_BYTES);
    reader.limits(limits);
    let image = reader.decode().ok()?;

    let thumb = image.thumbnail(THUMB_EDGE, THUMB_EDGE).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY)
        .encode(thumb.as_raw(), thumb.width(), thumb.height(), image::ExtendedColorType::Rgb8)
        .ok()?;
    // A "thumbnail" bigger than the file it stands in for is worse than none:
    // it costs disk to store and more bytes to send. Flat-coloured PNGs — a
    // screenshot of a chat app, say — hit this readily now that the budget is
    // 800px. Callers already fall back to the original when this is None.
    if out.len() >= bytes.len() {
        return None;
    }
    Some(out)
}

/// Width and height of every original asked about so far. A page of history
/// asks about fifty files at once and the same files every time it is
/// opened; the header read is cheap but not free, and an upload never
/// changes size. Bounded by starting over once it is large — simpler than
/// an LRU and never wrong, only occasionally slow again.
static DIMENSIONS: LazyLock<Mutex<HashMap<PathBuf, (u32, u32)>>> = LazyLock::new(Default::default);
const DIMENSIONS_CAP: usize = 4096;

/// Width and height of an uploaded image, from its header alone — no decode,
/// so nothing a malicious file claims about itself costs more than a few
/// bytes to read. None for anything that isn't an image we can read.
pub async fn dimensions(original: PathBuf) -> Option<(u32, u32)> {
    let name = original.file_name()?.to_string_lossy().to_ascii_lowercase();
    if !is_thumbable(&name) {
        return None;
    }
    if let Some(known) = DIMENSIONS.lock().unwrap().get(&original) {
        return Some(*known);
    }
    let path = original.clone();
    let dims = tokio::task::spawn_blocking(move || {
        image::ImageReader::open(&path).ok()?.with_guessed_format().ok()?.into_dimensions().ok()
    })
    .await
    .ok()??;
    if dims.0 == 0 || dims.1 == 0 {
        return None;
    }
    let mut cache = DIMENSIONS.lock().unwrap();
    if cache.len() >= DIMENSIONS_CAP {
        cache.clear();
    }
    cache.insert(original, dims);
    Some(dims)
}

/// Make the thumbnail for `original` if it doesn't exist yet. Returns the
/// thumb path when one is available. Decoding happens on a blocking thread —
/// a big JPEG is hundreds of milliseconds of CPU.
pub async fn ensure(original: PathBuf) -> Option<PathBuf> {
    let name = original.file_name()?.to_string_lossy().to_ascii_lowercase();
    if !is_thumbable(&name) {
        return None;
    }
    let path = thumb_path(&original)?;
    if tokio::fs::metadata(&path).await.is_ok() {
        return Some(path);
    }
    let bytes = tokio::fs::read(&original).await.ok()?;
    let rendered = tokio::task::spawn_blocking(move || render(&bytes)).await.ok()??;
    // A concurrent request may have written it first; either way it exists.
    tokio::fs::write(&path, &rendered).await.ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Photographic noise, which is what JPEG is good at and PNG is not —
    /// the shape of file a thumbnail is actually for.
    fn photo(w: u32, h: u32) -> Vec<u8> {
        let mut seed: u32 = 0x9e3779b9;
        let buf = image::RgbImage::from_fn(w, h, |_, _| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = (seed >> 16) as u8;
            image::Rgb([v, v.wrapping_add(40), v.wrapping_add(90)])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(buf)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(buf)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn shrinks_big_images() {
        let big = photo(1600, 1200);
        let thumb = render(&big).expect("thumbnail");
        assert!(thumb.starts_with(&[0xFF, 0xD8, 0xFF]), "not a JPEG");
        // Byte savings are what matter in production (a 12 MB photo becomes
        // tens of KB), but a synthetic gradient is already tiny as PNG — so
        // assert the dimensions, which is the property that guarantees it.
        assert!(thumb.len() < big.len(), "thumb {} vs source {}", thumb.len(), big.len());
        let (w, h) = image::ImageReader::new(std::io::Cursor::new(&thumb))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert!(w.max(h) <= THUMB_EDGE, "{w}x{h} exceeds the edge budget");
        // Aspect ratio survives.
        assert!((w as f32 / h as f32 - 4.0 / 3.0).abs() < 0.05);
    }

    /// The case that made switchb say "images don't preview well": a phone
    /// screenshot. The long edge is the budget, so what matters is that the
    /// SHORT edge is still wide enough to draw at the width a phone gives it.
    #[test]
    fn a_phone_screenshot_keeps_a_usable_width() {
        let shot = photo(1080, 2400);
        let thumb = render(&shot).expect("thumbnail");
        let (w, h) = image::ImageReader::new(std::io::Cursor::new(&thumb))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((w, h), (360, 800), "9:20 screenshot should fill the long-edge budget");
        // The phone draws inline images up to ~309px wide; anything narrower
        // than that is being upscaled on screen.
        assert!(w >= 309, "{w}px wide is narrower than the phone renders it");
    }

    #[tokio::test]
    async fn dimensions_come_from_the_header() {
        let dir = std::env::temp_dir().join(format!("nd-dims-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, photo(1080, 2400)).unwrap();
        assert_eq!(dimensions(path.clone()).await, Some((1080, 2400)));
        // Second time is the cache; still the same answer.
        assert_eq!(dimensions(path).await, Some((1080, 2400)));
        // Not an image, and not a file at all.
        let text = dir.join("notes.txt");
        std::fs::write(&text, b"hello").unwrap();
        assert_eq!(dimensions(text).await, None);
        assert_eq!(dimensions(dir.join("missing.png")).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_images_are_left_alone() {
        // Already tile-sized: making a "thumbnail" would only waste space.
        assert!(render(&png(320, 240)).is_none());
    }

    /// A flat PNG that JPEG cannot beat keeps no thumbnail at all, and the
    /// caller serves the original — which is both smaller and sharper.
    #[test]
    fn a_thumbnail_that_saves_nothing_is_not_kept() {
        // A smooth gradient: trivial for PNG, expensive for JPEG.
        assert!(render(&png(1600, 1200)).is_none());
    }

    #[test]
    fn junk_is_not_an_image() {
        assert!(render(b"this is not an image at all, just bytes").is_none());
        assert!(render(&[]).is_none());
    }

    #[test]
    fn thumbable_by_extension() {
        assert!(is_thumbable("photo.PNG"));
        assert!(is_thumbable("a.jpeg"));
        assert!(!is_thumbable("clip.mp4"));
        assert!(!is_thumbable("notes.txt"));
    }
}
