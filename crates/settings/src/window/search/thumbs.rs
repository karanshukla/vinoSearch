//! Thumbnails for the photo grid. The freedesktop thumbnail cache first
//! (`~/.cache/thumbnails`, which file managers fill), so a photo seen in
//! Dolphin or Files is never decoded again; otherwise the photo is decoded
//! here, on a thread of its own, one at a time, under the indexer's limits.
//! Nothing is written to the shared cache.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use eframe::egui;
use image::{DynamicImage, ImageDecoder, ImageReader};
use md5::{Digest, Md5};
use vinosearch_shared::limits::{MAX_DECODED_BYTES, MAX_IMAGE_BYTES};

/// Longest side of a thumbnail decoded here: the cache's "large".
pub const SIZE: u32 = 256;

pub enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    /// Not an image this build reads, too big, or gone.
    Failed,
}

type Done = Arc<Mutex<Vec<(String, Option<egui::ColorImage>)>>>;

pub struct Thumbs {
    asks: Sender<String>,
    done: Done,
    pub(super) shown: HashMap<String, Thumb>,
    ctx: Arc<OnceLock<egui::Context>>,
}

impl Default for Thumbs {
    fn default() -> Self {
        let (asks, rx) = mpsc::channel::<String>();
        let done: Done = Arc::default();
        let ctx: Arc<OnceLock<egui::Context>> = Arc::default();
        let (out, wake) = (Arc::clone(&done), Arc::clone(&ctx));
        std::thread::spawn(move || {
            let cache = cache_dir();
            while let Ok(path) = rx.recv() {
                let image = load(cache.as_deref(), Path::new(&path)).ok();
                out.lock().unwrap().push((path, image));
                if let Some(ctx) = wake.get() {
                    ctx.request_repaint();
                }
            }
        });
        Thumbs {
            asks,
            done,
            shown: HashMap::new(),
            ctx,
        }
    }
}

impl Thumbs {
    /// `path`'s thumbnail, asked for the first time it is wanted.
    pub fn get(&mut self, ctx: &egui::Context, path: &str) -> &Thumb {
        let _ = self.ctx.set(ctx.clone());
        for (done, image) in self.done.lock().unwrap().drain(..) {
            let thumb = match image {
                Some(image) => Thumb::Ready(ctx.load_texture(
                    format!("thumb:{done}"),
                    image,
                    egui::TextureOptions::LINEAR,
                )),
                None => Thumb::Failed,
            };
            self.shown.insert(done, thumb);
        }
        self.shown.entry(path.to_owned()).or_insert_with(|| {
            let _ = self.asks.send(path.to_owned());
            Thumb::Loading
        })
    }
}

/// `$XDG_CACHE_HOME/thumbnails`, else `~/.cache/thumbnails`.
fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("thumbnails"))
}

fn load(cache: Option<&Path>, path: &Path) -> Result<egui::ColorImage, String> {
    let image = match cache.and_then(|c| cached(c, path)) {
        Some(image) => image,
        None => decode(path)?,
    };
    let rgba = image.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

/// `path` as a `file://` URI, escaped as GLib's `g_filename_to_uri` does,
/// which the thumbnail spec hashes. Qt escapes a few more characters
/// (`[`, `]`), so a thumbnail Dolphin wrote for such a name is missed and the
/// photo decoded instead.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,=:@/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Where the spec puts `path`'s thumbnail in `size` (normal, large, ...).
fn cache_file(cache: &Path, size: &str, path: &Path) -> PathBuf {
    let digest = Md5::digest(file_uri(path).as_bytes());
    let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    cache.join(size).join(format!("{name}.png"))
}

/// The biggest cached thumbnail of `path` that is still current: the spec's
/// `Thumb::MTime` matching the file's.
fn cached(cache: &Path, path: &Path) -> Option<DynamicImage> {
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    ["xx-large", "x-large", "large", "normal"]
        .iter()
        .find_map(|size| {
            let bytes = std::fs::read(cache_file(cache, size, path)).ok()?;
            let stamp: u64 = png_text(&bytes, "Thumb::MTime")?.trim().parse().ok()?;
            if stamp != mtime {
                return None;
            }
            image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()
        })
}

/// A PNG's `tEXt` value for `key`.
fn png_text(png: &[u8], key: &str) -> Option<String> {
    let mut rest = png.strip_prefix(b"\x89PNG\r\n\x1a\n")?;
    while rest.len() >= 12 {
        let len = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        let data = rest.get(8..8 + len)?;
        if kind == b"tEXt"
            && let Some(nul) = data.iter().position(|&b| b == 0)
            && &data[..nul] == key.as_bytes()
        {
            // Latin-1, which is ASCII for the keys the spec defines.
            return Some(data[nul + 1..].iter().map(|&b| b as char).collect());
        }
        rest = rest.get(12 + len..)?;
    }
    None
}

/// Decode `path` and shrink it to SIZE, under the indexer's limits: the file
/// at most MAX_IMAGE_BYTES, its pixels at most MAX_DECODED_BYTES.
fn decode(path: &Path) -> Result<DynamicImage, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_IMAGE_BYTES {
        return Err(format!("{size} bytes, more than {MAX_IMAGE_BYTES}"));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let (w, h) = decoder.dimensions();
    if decoder.total_bytes() > MAX_DECODED_BYTES {
        return Err(format!("{w}x{h} is too big to decode"));
    }
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    image.apply_orientation(orientation);
    Ok(image.thumbnail(SIZE, SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vinosearch-thumbs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn png(w: u32, h: u32, colour: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(w, h, image::Rgb(colour));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// `png` with a tEXt chunk after its header, as a thumbnailer writes it.
    fn with_text(png: &[u8], key: &str, value: &str) -> Vec<u8> {
        let ihdr_end = 8 + 12 + 13;
        let mut data = key.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(value.as_bytes());
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        let mut typed = b"tEXt".to_vec();
        typed.extend_from_slice(&data);
        chunk.extend_from_slice(&typed);
        chunk.extend_from_slice(&crc32(&typed).to_be_bytes());
        [&png[..ihdr_end], &chunk, &png[ihdr_end..]].concat()
    }

    #[test]
    fn uris_are_escaped_as_glib_does() {
        assert_eq!(
            file_uri(Path::new("/home/u/My Photos/a&b (1)#2.jpg")),
            "file:///home/u/My%20Photos/a&b%20(1)%232.jpg"
        );
        assert_eq!(
            file_uri(Path::new("/p/caf\u{e9}.png")),
            "file:///p/caf%C3%A9.png"
        );
        // The spec's own example.
        let digest = Md5::digest(b"file:///home/jens/photos/me.png");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "c6ee772d9e49320e97ec29a7eb5b1697");
        assert!(
            cache_file(
                Path::new("/c"),
                "large",
                Path::new("/home/jens/photos/me.png")
            )
            .ends_with("large/c6ee772d9e49320e97ec29a7eb5b1697.png")
        );
    }

    #[test]
    fn a_current_cached_thumbnail_is_used_and_a_stale_one_is_not() {
        let dir = temp("cache");
        let photo = dir.join("photo.png");
        std::fs::write(&photo, png(600, 400, [200, 0, 0])).unwrap();
        let mtime = std::fs::metadata(&photo)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cache = dir.join("thumbnails");
        let thumb = cache_file(&cache, "normal", &photo);
        std::fs::create_dir_all(thumb.parent().unwrap()).unwrap();

        std::fs::write(
            &thumb,
            with_text(
                &png(128, 85, [0, 0, 200]),
                "Thumb::MTime",
                &mtime.to_string(),
            ),
        )
        .unwrap();
        let image = load(Some(&cache), &photo).unwrap();
        assert_eq!(image.size, [128, 85]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(0, 0, 200));

        // Stale: the photo is decoded instead.
        std::fs::write(
            &thumb,
            with_text(&png(128, 85, [0, 0, 200]), "Thumb::MTime", "12"),
        )
        .unwrap();
        let image = load(Some(&cache), &photo).unwrap();
        assert_eq!(image.size, [256, 171]);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(200, 0, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decoding_stays_under_the_indexers_limits() {
        let dir = temp("limits");
        // A header claiming 60000 x 60000 RGB: 10 GB of pixels, refused
        // before any are decoded.
        let mut huge = png(1, 1, [0, 0, 0]);
        huge[16..20].copy_from_slice(&60000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&60000u32.to_be_bytes());
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        let path = dir.join("huge.png");
        std::fs::write(&path, &huge).unwrap();
        assert!(decode(&path).is_err());

        assert!(decode(&dir.join("missing.png")).is_err());
        std::fs::write(dir.join("note.png"), "not a png").unwrap();
        assert!(decode(&dir.join("note.png")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
