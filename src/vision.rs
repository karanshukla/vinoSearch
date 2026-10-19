//! Images to the vision encoder's input, as EmbeddingGemma 2's processor
//! (transformers' Gemma4ImageProcessor) makes it.
//!
//! The image is resized, keeping its aspect ratio, to the largest size with
//! at most `PATCHES` 16x16 patches and sides that are multiples of 48 (a
//! soft token pools 3x3 patches), then cut into patches and padded to
//! `PATCHES`, which is the static vision encoder's input shape (#2).

use anyhow::{Context, Result, bail};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage, imageops::FilterType};
use std::io::Cursor;

/// Soft tokens per image. 140 retrieves within 1.8 recall@1 of the
/// processor's 280 on Flickr30k (84.5 against 86.3, recall@10 98.6 against
/// 98.7) in half the NPU time and energy: 1.3 s an image on the NPU.
pub const SOFT_TOKENS: usize = 140;
const PATCH: usize = 16;
const POOL: usize = 3;
pub const PATCHES: usize = SOFT_TOKENS * POOL * POOL;
pub const PATCH_DIM: usize = PATCH * PATCH * 3;
/// Decoding a huge image can take far more memory than the file. Refused
/// past this many bytes held at once: the decoded pixels, a copy if its
/// orientation turns it, and the 8-bit RGB copy (a 100-megapixel RGB photo
/// is 3 x 300 MB).
const MAX_DECODED_BYTES: u64 = 1 << 30;

/// One image, patched and padded for the vision encoder.
pub struct Pixels {
    /// `[PATCHES, PATCH_DIM]`: each patch's rows, then columns, then RGB, in
    /// 0..1. Padding patches are 0.
    pub values: Vec<f32>,
    /// `[PATCHES, 2]`: each patch's (column, row); padding is (-1, -1).
    pub positions: Vec<i64>,
    /// Soft tokens the image fills, so the rows of the encoder's output that
    /// are the image: (rows / 3) * (columns / 3) of its patch grid.
    pub tokens: usize,
}

/// Gemma4ImageProcessor's `get_aspect_ratio_preserving_size`, as (height, width).
fn target_size(height: u32, width: u32) -> Result<(u32, u32)> {
    let side = (POOL * PATCH) as f64;
    let factor = ((PATCHES * PATCH * PATCH) as f64 / (f64::from(height) * f64::from(width))).sqrt();
    let mut h = (factor * f64::from(height) / side).floor() * side;
    let mut w = (factor * f64::from(width) / side).floor() * side;
    let longest = (PATCHES / (POOL * POOL)) as f64 * side;
    if h == 0.0 && w == 0.0 {
        bail!("{width}x{height} is too small to resize");
    } else if h == 0.0 {
        h = side;
        w = ((f64::from(width) / f64::from(height)).floor() * side).min(longest);
    } else if w == 0.0 {
        w = side;
        h = ((f64::from(height) / f64::from(width)).floor() * side).min(longest);
    }
    Ok((h as u32, w as u32))
}

/// Decode an image file's bytes, turned upright by its EXIF orientation.
/// Animations give their first frame.
pub fn load(bytes: &[u8]) -> Result<RgbImage> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .context("not an image this build reads")?;
    // Everything the steps below hold, judged from the header before decoding.
    let (w, h) = decoder.dimensions();
    let needed = 2 * decoder.total_bytes() + u64::from(w) * u64::from(h) * 3;
    if needed > MAX_DECODED_BYTES {
        bail!(
            "{w}x{h} needs {} MB to decode, more than {}",
            needed >> 20,
            MAX_DECODED_BYTES >> 20
        );
    }
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    // Alpha is dropped, not composited, as the processor's convert("RGB") does.
    Ok(img.to_rgb8())
}

pub fn prepare(img: &RgbImage) -> Result<Pixels> {
    let (h, w) = target_size(img.height(), img.width())?;
    // Catmull-Rom is bicubic with a = -0.5, which is what torchvision's
    // antialiased bicubic resize uses; image's filter widens when shrinking,
    // as antialiasing does.
    let resized;
    let img = if (h, w) == (img.height(), img.width()) {
        img
    } else {
        resized = image::imageops::resize(img, w, h, FilterType::CatmullRom);
        &resized
    };
    patchify(img)
}

/// Cut an image whose sides are multiples of 48 into patches, padded to `PATCHES`.
fn patchify(img: &RgbImage) -> Result<Pixels> {
    let (h, w) = (img.height(), img.width());
    let (rows, cols) = (h as usize / PATCH, w as usize / PATCH);
    let n = rows * cols;
    // At least one whole soft token, so the image has a row of features.
    if n > PATCHES || rows < POOL || cols < POOL {
        bail!("{w}x{h} is {rows}x{cols} patches, not 3x3 to {PATCHES}");
    }
    let mut values = vec![0f32; PATCHES * PATCH_DIM];
    let mut positions = vec![-1i64; PATCHES * 2];
    let raw = img.as_raw();
    let stride = w as usize * 3;
    for r in 0..rows {
        for c in 0..cols {
            let p = r * cols + c;
            positions[p * 2] = c as i64;
            positions[p * 2 + 1] = r as i64;
            let out = &mut values[p * PATCH_DIM..(p + 1) * PATCH_DIM];
            for y in 0..PATCH {
                let start = (r * PATCH + y) * stride + c * PATCH * 3;
                for (o, &v) in out[y * PATCH * 3..(y + 1) * PATCH * 3]
                    .iter_mut()
                    .zip(&raw[start..start + PATCH * 3])
                {
                    *o = f32::from(v) / 255.0;
                }
            }
        }
    }
    Ok(Pixels {
        values,
        positions,
        tokens: (rows / POOL) * (cols / POOL),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_the_processor() {
        // get_aspect_ratio_preserving_size(h, w, 16, 1260, 3), transformers 5.20.1.
        for ((h, w), want) in [
            ((1956, 4096), (384, 816)),
            ((500, 580), (480, 576)),
            ((1000, 1000), (528, 528)),
            ((767, 515), (672, 432)),
            ((501, 1501), (288, 960)),
            ((2160, 3840), (384, 720)),
        ] {
            let got = target_size(h, w).unwrap();
            assert_eq!(got, want, "{h}x{w}");
            let (gh, gw) = got;
            assert!((gh as usize / PATCH) * (gw as usize / PATCH) <= PATCHES);
        }
    }

    #[test]
    fn a_sliver_gets_one_soft_token_row() {
        assert_eq!(target_size(10, 4000).unwrap(), (48, 6720));
    }

    #[test]
    fn an_image_too_big_to_decode_is_refused_from_its_header() {
        // A 40,000 x 40,000 24-bit BMP, 4.8 GB decoded: its header alone.
        let (w, h) = (40_000i32, 40_000i32);
        let mut bmp = b"BM".to_vec();
        bmp.extend(54u32.to_le_bytes()); // file size, not checked
        bmp.extend([0; 4]);
        bmp.extend(54u32.to_le_bytes()); // pixel data offset
        bmp.extend(40u32.to_le_bytes()); // BITMAPINFOHEADER
        bmp.extend(w.to_le_bytes());
        bmp.extend(h.to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0; 24]);
        let err = load(&bmp).unwrap_err().to_string();
        assert!(err.contains("needs"), "{err}");
    }

    #[test]
    fn a_grid_without_a_whole_soft_token_is_refused() {
        assert!(patchify(&RgbImage::new(480, 32)).is_err());
    }

    #[test]
    fn patches_are_rows_then_columns_then_channels() {
        let img = RgbImage::from_fn(48 * 2, 48, |x, y| image::Rgb([x as u8, y as u8, 7]));
        let px = patchify(&img).unwrap();
        // 6 columns by 3 rows of patches.
        assert_eq!(px.tokens, 2);
        let patch = 6 + 2; // row 1, column 2
        assert_eq!(&px.positions[patch * 2..patch * 2 + 2], &[2, 1]);
        let v = &px.values[patch * PATCH_DIM..];
        // Pixel (x 32, y 16) first, then (x 33, y 16).
        assert_eq!(v[0], 32.0 / 255.0);
        assert_eq!(v[1], 16.0 / 255.0);
        assert_eq!(v[2], 7.0 / 255.0);
        assert_eq!(v[3], 33.0 / 255.0);
        assert_eq!(v[PATCH * 3 + 1], 17.0 / 255.0);
        assert_eq!(px.positions[18 * 2], -1);
        assert!(px.values[18 * PATCH_DIM..].iter().all(|&x| x == 0.0));
    }
}
