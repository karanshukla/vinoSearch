//! Near-duplicate images collapsed into one result (#78): a burst, an edit,
//! a resized or recompressed copy. Byte-identical files already share one
//! result as `copies`; these come back as `similar` on the best-scoring one.
//! Grouped at query time among the hits only, nothing stored, after Orchard
//! Photos' rules (chaseleantj/orchard-photos, `server/groups.py`) with
//! thresholds measured on EmbeddingGemma 2's vision vectors
//! (`bench/near_duplicates.py`).

use crate::index::{Hit, Kind};
use image::{ImageDecoder, ImageReader};
use std::path::Path;

/// Cosine to a group's centre (not to any one member, so a slow series
/// can't chain) at which a photo joins it. On 200 Flickr30k photos, each
/// one's closest other photo scored 0.70 to 0.90; half-size, cropped to
/// 85%, brightened and shifted copies 0.93 to 0.99 to their original, and
/// every variant 0.92 to 0.997 to its group's centre. A JPEG saved again at
/// quality 35 scored 0.89 to 0.98, so some of those stay apart.
const ALIKE: f32 = 0.92;
/// The lower bar for a name that differs from a member's only by a size or
/// copy suffix (`-800x400`, ` (1)`, ` copy`) in the same folder: a crop.
/// The middle half of a photo scored 0.81 to 0.98 to the whole.
const ALIKE_NAMED: f32 = 0.80;
/// Screenshots of one app score as copies whatever they show: two windows
/// of a made-up app with different text 0.90 to 0.97, two different apps up
/// to 0.955. So an image with no camera EXIF, the same shape as the other,
/// also has to match pixel for pixel: at most this share of a THUMB-square
/// grey thumbnail more than PIXEL_STEP apart. Orchard's 64x64 and 3% can't
/// see text change: different text differed in 0 to 0.6% of pixels. At
/// 256x256, a changed clock differed in at most 0.02%, different text in
/// at least 2.1%.
const MAX_DIFFERING: f32 = 0.005;
const PIXEL_STEP: i16 = 16;
const THUMB: u32 = 256;
/// Shapes this close are the same, for a copy resized whole.
const SAME_SHAPE: f64 = 0.01;

/// What decides a pair without its pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Look {
    size: (u32, u32),
    /// EXIF names a camera maker or model.
    camera: bool,
}

/// `hits` with each near-duplicate image folded into the best-scoring one
/// of its group, in the order they came. Other kinds pass through.
pub fn collapse(hits: Vec<Hit>) -> Vec<Hit> {
    collapse_with(hits, look, differing)
}

struct Group {
    /// Where its front hit is in the output.
    at: usize,
    sum: Vec<f32>,
    members: Vec<String>,
}

fn collapse_with(
    hits: Vec<Hit>,
    look: impl Fn(&Path) -> Option<Look>,
    differing: impl Fn(&Path, &Path) -> Option<f32>,
) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::with_capacity(hits.len());
    let mut groups: Vec<Group> = Vec::new();
    for h in hits {
        let Some(v) = h.vector.as_ref().filter(|_| h.kind == Kind::Image) else {
            out.push(h);
            continue;
        };
        // Which group, and whether by its vector alone.
        let joins = groups.iter().enumerate().find_map(|(i, g)| {
            let c = cosine(&g.sum, v);
            let named = || g.members.iter().any(|m| same_but_suffix(m, &h.path));
            let alike = c >= ALIKE;
            ((alike || (c >= ALIKE_NAMED && named()))
                && g.members
                    .iter()
                    .all(|m| pixels_agree(m, &h.path, &look, &differing)))
            .then_some((i, alike))
        });
        match joins {
            Some((i, alike)) => {
                let g = &mut groups[i];
                // A crop let in by its name doesn't move the centre, or
                // other crops would follow it in.
                if alike {
                    g.sum.iter_mut().zip(v).for_each(|(s, x)| *s += x);
                }
                g.members.push(h.path.clone());
                let front = &mut out[g.at];
                front.similar.push(h.path);
                front.similar.extend(h.copies);
            }
            None => {
                groups.push(Group {
                    at: out.len(),
                    sum: v.clone(),
                    members: vec![h.path.clone()],
                });
                out.push(h);
            }
        }
    }
    out
}

/// Cosine between `sum` (any length) and the unit vector `v`.
fn cosine(sum: &[f32], v: &[f32]) -> f32 {
    let dot: f32 = sum.iter().zip(v).map(|(a, b)| a * b).sum();
    let norm = sum.iter().map(|a| a * a).sum::<f32>().sqrt();
    if norm == 0.0 { 0.0 } else { dot / norm }
}

/// Whether two images alike by their vectors are alike enough: photos with
/// camera EXIF always are, as are images of different shapes (a crop); a
/// screenshot or render the same shape as the other must match pixel for
/// pixel. Unreadable counts as alike.
fn pixels_agree(
    a: &str,
    b: &str,
    look: &impl Fn(&Path) -> Option<Look>,
    differing: &impl Fn(&Path, &Path) -> Option<f32>,
) -> bool {
    let (a, b) = (Path::new(a), Path::new(b));
    let (Some(la), Some(lb)) = (look(a), look(b)) else {
        return true;
    };
    let aspect = |(w, h): (u32, u32)| f64::from(w) / f64::from(h.max(1));
    if (la.camera && lb.camera) || (aspect(la.size) / aspect(lb.size) - 1.0).abs() > SAME_SHAPE {
        return true;
    }
    differing(a, b).is_none_or(|d| d <= MAX_DIFFERING)
}

/// `a` and `b` in one folder, named alike but for a size or copy suffix:
/// `beach.jpg` and `beach-800x400.jpg`, `beach (1).jpg`, `beach copy.png`.
fn same_but_suffix(a: &str, b: &str) -> bool {
    let (a, b) = (Path::new(a), Path::new(b));
    if a.parent() != b.parent() {
        return false;
    }
    let stem = |p: &Path| base(&p.file_stem().unwrap_or_default().to_string_lossy());
    stem(a) == stem(b)
}

/// A file stem less any trailing size or copy suffixes, lowercased.
fn base(stem: &str) -> String {
    let mut s = stem.trim().to_lowercase();
    loop {
        let before = s.len();
        for suffix in [" copy", "-copy", "_copy"] {
            if let Some(rest) = s.strip_suffix(suffix) {
                s = rest.trim_end().to_owned();
            }
        }
        // " (1)", "-1x" are left alone, only "(n)".
        if s.ends_with(')')
            && let Some(open) = s.rfind('(')
            && s[open + 1..s.len() - 1].bytes().all(|c| c.is_ascii_digit())
            && open + 2 < s.len()
        {
            s = s[..open].trim_end().to_owned();
        }
        // -800x400, _1920x1080, -scaled
        if let Some(i) = s.rfind(['-', '_']) {
            let tail = &s[i + 1..];
            let dims = tail.split_once('x').is_some_and(|(w, h)| {
                !w.is_empty()
                    && !h.is_empty()
                    && w.bytes().all(|c| c.is_ascii_digit())
                    && h.bytes().all(|c| c.is_ascii_digit())
            });
            if dims || tail == "scaled" {
                s.truncate(i);
            }
        }
        if s.len() == before {
            return s;
        }
    }
}

/// An image's size and whether its EXIF names a camera, from its header
/// alone.
fn look(path: &Path) -> Option<Look> {
    let mut reader = ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    // As indexing decodes them: these are the same files, in the same process.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(vinosearch_shared::limits::MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let size = decoder.dimensions();
    let camera = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .is_some_and(|e| names_camera(&e));
    Some(Look { size, camera })
}

/// Whether EXIF (a TIFF structure, optionally after `Exif\0\0`) has Make
/// (0x010F) or Model (0x0110) in its first IFD.
fn names_camera(exif: &[u8]) -> bool {
    let t = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
    let le = match t.get(..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return false,
    };
    let u16_at = |i: usize| {
        t.get(i..i + 2).map(|b| {
            let b = [b[0], b[1]];
            if le {
                u16::from_le_bytes(b)
            } else {
                u16::from_be_bytes(b)
            }
        })
    };
    let u32_at = |i: usize| {
        t.get(i..i + 4).map(|b| {
            let b = [b[0], b[1], b[2], b[3]];
            if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            }
        })
    };
    let Some(ifd) = u32_at(4).map(|o| o as usize) else {
        return false;
    };
    let Some(n) = u16_at(ifd) else { return false };
    (0..usize::from(n)).any(|i| matches!(u16_at(ifd + 2 + 12 * i), Some(0x010F | 0x0110)))
}

/// The share of two images' THUMB-square grey thumbnails more than
/// PIXEL_STEP apart; None if either doesn't decode.
fn differing(a: &Path, b: &Path) -> Option<f32> {
    let thumb = |p: &Path| -> Option<image::GrayImage> {
        let bytes = std::fs::read(p).ok()?;
        let rgb = crate::vision::load(&bytes).ok()?;
        let grey = image::DynamicImage::ImageRgb8(rgb).into_luma8();
        Some(image::imageops::resize(
            &grey,
            THUMB,
            THUMB,
            image::imageops::FilterType::Triangle,
        ))
    };
    let (a, b) = (thumb(a)?, thumb(b)?);
    let apart = a
        .pixels()
        .zip(b.pixels())
        .filter(|(x, y)| (i16::from(x.0[0]) - i16::from(y.0[0])).abs() > PIXEL_STEP)
        .count();
    Some(apart as f32 / (THUMB * THUMB) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    fn image(path: &str, score: f32, v: &[f32]) -> Hit {
        Hit {
            kind: Kind::Image,
            score,
            path: path.to_owned(),
            line: 0,
            text: String::new(),
            copies: Vec::new(),
            vector: Some(unit(v)),
            similar: Vec::new(),
        }
    }

    const PHOTO: Look = Look {
        size: (4032, 3024),
        camera: true,
    };

    fn photos(hits: Vec<Hit>) -> Vec<(String, Vec<String>)> {
        collapse_with(hits, |_| Some(PHOTO), |_, _| None)
            .into_iter()
            .map(|h| (h.path, h.similar))
            .collect()
    }

    #[test]
    fn a_burst_is_one_result_under_its_best() {
        let got = photos(vec![
            image("/p/a1.jpg", 0.70, &[1.0, 0.0, 0.0]),
            image("/p/cat.jpg", 0.68, &[0.0, 1.0, 0.0]),
            image("/p/a2.jpg", 0.66, &[1.0, 0.05, 0.0]),
            image("/p/a3.jpg", 0.65, &[1.0, 0.0, 0.05]),
        ]);
        assert_eq!(
            got,
            [
                (
                    "/p/a1.jpg".into(),
                    vec!["/p/a2.jpg".into(), "/p/a3.jpg".into()]
                ),
                ("/p/cat.jpg".into(), vec![])
            ]
        );
    }

    #[test]
    fn a_slow_series_does_not_chain() {
        // Each frame close to the last, the last far from the first: the
        // centre moves less than a chain would.
        let frames: Vec<Hit> = (0..8)
            .map(|i| {
                let t = i as f32 * 0.12;
                image(&format!("/p/f{i}.jpg"), 0.7, &[t.cos(), t.sin(), 0.0])
            })
            .collect();
        let got = photos(frames);
        assert!(got.len() > 1, "{got:?}");
    }

    #[test]
    fn a_crop_joins_only_by_a_copy_name_in_the_same_folder() {
        let v = [1.0, 0.0, 0.0];
        let crop = [1.0, 0.55, 0.0]; // cosine 0.876
        let got = photos(vec![
            image("/p/beach.jpg", 0.7, &v),
            image("/p/beach-800x400.jpg", 0.6, &crop),
            image("/q/beach-800x400.jpg", 0.6, &crop),
            image("/p/sunset.jpg", 0.6, &crop),
        ]);
        assert_eq!(
            got,
            [
                ("/p/beach.jpg".into(), vec!["/p/beach-800x400.jpg".into()]),
                ("/q/beach-800x400.jpg".into(), vec!["/p/sunset.jpg".into()]),
            ]
        );
    }

    #[test]
    fn screenshots_must_match_pixel_for_pixel() {
        let shot = Look {
            size: (1280, 800),
            camera: false,
        };
        let hits = || {
            vec![
                image("/s/a.png", 0.7, &[1.0, 0.0]),
                image("/s/b.png", 0.69, &[1.0, 0.01]),
            ]
        };
        let n = |d: f32| collapse_with(hits(), |_| Some(shot), move |_, _| Some(d)).len();
        assert_eq!(n(0.02), 2);
        assert_eq!(n(0.001), 1);
        // Different shapes: not the same screen, the vectors decide.
        let got = collapse_with(
            hits(),
            |p| {
                Some(Look {
                    size: if p.ends_with("a.png") {
                        (1280, 800)
                    } else {
                        (800, 800)
                    },
                    camera: false,
                })
            },
            |_, _| Some(0.5),
        );
        assert_eq!(got.len(), 1);
        // Half the size, the same shape: still compared pixel for pixel.
        let got = collapse_with(
            hits(),
            |p| {
                Some(Look {
                    size: if p.ends_with("a.png") {
                        (1280, 800)
                    } else {
                        (640, 400)
                    },
                    camera: false,
                })
            },
            |_, _| Some(0.5),
        );
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn other_kinds_and_copies_pass_through() {
        let mut a = image("/p/a.jpg", 0.7, &[1.0, 0.0]);
        let mut b = image("/p/b.jpg", 0.6, &[1.0, 0.0]);
        b.copies = vec!["/backup/b.jpg".into()];
        let mut text = image("/n.md", 0.8, &[1.0, 0.0]);
        text.kind = Kind::Text;
        a.copies = vec!["/backup/a.jpg".into()];
        let got = collapse_with(vec![text, a, b], |_| Some(PHOTO), |_, _| None);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].copies, ["/backup/a.jpg"]);
        assert_eq!(got[1].similar, ["/p/b.jpg", "/backup/b.jpg"]);
    }

    #[test]
    fn copy_suffixes_come_off_the_name() {
        for (a, b) in [
            ("beach", "beach-800x400"),
            ("beach", "Beach (1)"),
            ("beach", "beach copy"),
            ("beach", "beach copy (2)"),
            ("IMG_2041", "IMG_2041_1920x1080"),
            ("photo", "photo-scaled"),
        ] {
            assert_eq!(base(a), base(b), "{a} / {b}");
        }
        assert_ne!(base("beach"), base("beach2"));
        assert_ne!(base("IMG_2041"), base("IMG_2042"));
        assert_ne!(base("plan (draft)"), base("plan"));
    }

    #[test]
    fn exif_naming_a_camera() {
        // Little-endian TIFF, one IFD entry: Make, ASCII.
        let make = b"Exif\0\0II*\0\x08\0\0\0\x01\0\x0f\x01\x02\0\x06\0\0\0\x1a\0\0\0";
        assert!(names_camera(make));
        // Only Orientation.
        let orientation = b"II*\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0";
        assert!(!names_camera(orientation));
        assert!(!names_camera(b"Exif\0\0junk"));
        assert!(!names_camera(b"II*\0\xff\xff\0\0"));
    }
}
