//! Image thumbnails, so a 12 MB photo isn't downloaded in full to draw a
//! 40px tile. One thumb per upload, generated at upload time and lazily for
//! anything that predates this, stored beside the original as `thumb.jpg`.
//!
//! This decodes user-supplied images, so decoding runs under explicit
//! limits: a malicious file can claim enormous dimensions and blow up
//! memory long before anything looks at the pixels.

use std::path::{Path, PathBuf};

/// Longest edge of a generated thumbnail. Comfortably covers both the Files
/// panel's tiles and inline chat images at their rendered size.
const THUMB_EDGE: u32 = 400;
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
    Some(out)
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
        let big = png(1600, 1200);
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

    #[test]
    fn small_images_are_left_alone() {
        // Already tile-sized: making a "thumbnail" would only waste space.
        assert!(render(&png(320, 240)).is_none());
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
