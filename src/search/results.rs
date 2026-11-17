//! `Control1.Search`: every file a query finds, for the search window (#111).
//! KRunner and GNOME Shell show the best 5 of each kind; the window shows up
//! to `results::MAX` and filters them itself.

use crate::index::{Hit, Kind};
use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;
use vinosearch_shared::dbus::results;

/// One file as `Control1.Search` returns it. The fields' order is the
/// D-Bus struct the window reads (pinned by
/// `tests::found_goes_over_d_bus_as_the_struct_ssdssux`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, zbus::zvariant::Type)]
pub struct Found {
    pub path: String,
    /// One of `results::*`.
    pub kind: String,
    pub score: f64,
    pub snippet: String,
    /// "line", "page", "slide", "second" (media, counted from 0), ... or empty.
    pub unit: String,
    /// The match's number in `unit`; 0 for none.
    pub at: u32,
    /// Unix seconds.
    pub mtime: i64,
}

/// The window's kind for a hit: text OCR read in an image is an image, as
/// KRunner groups it.
fn kind(h: &Hit) -> &'static str {
    match h.kind {
        _ if picture(h) => results::IMAGE,
        Kind::Text => results::TEXT,
        Kind::Code => results::CODE,
        Kind::Session => results::SESSION,
        Kind::Media => results::MEDIA,
        Kind::Document | Kind::Image => results::DOCUMENT,
    }
}

/// An image, by what's in it or by the text OCR read in it: held to the
/// image floor and GAP both. OCR's text alone scored up to 0.67 for
/// off-topic words ("valorant" found KDE doc screenshots at 0.656).
pub fn picture(h: &Hit) -> bool {
    match h.kind {
        Kind::Image => true,
        Kind::Document => !crate::extract::is_document(Path::new(&h.path)),
        _ => false,
    }
}

/// Audio and video further than this below the best text, document or
/// session result are left out, and images (and text read in them) further
/// below the best of those or audio and video. Their floors alone let them through for queries text
/// answers well: "bills", a document at 0.704 and a photo at 0.632;
/// "valorant", sessions at 0.79 and unrelated videos and photos at 0.66.
/// Not yet checked against a query where text and a photo are both right.
pub const GAP: f32 = 0.06;

/// Leaves out the audio, video and images in `hits` more than GAP below the
/// best of the kinds shown before them. Alone, they need only their floor.
pub fn drop_far_below(hits: &mut Vec<Hit>) {
    let best = |hits: &[Hit], kinds: &[Kind]| {
        hits.iter()
            .filter(|h| kinds.contains(&h.kind) && !picture(h))
            .map(|h| h.score)
            .fold(f32::NEG_INFINITY, f32::max)
    };
    let words = best(hits, &[Kind::Text, Kind::Document, Kind::Session]);
    hits.retain(|h| h.kind != Kind::Media || h.score >= words - GAP);
    let others = best(
        hits,
        &[Kind::Text, Kind::Document, Kind::Session, Kind::Media],
    );
    hits.retain(|h| !picture(h) || h.score >= others - GAP);
}

/// Best chunk per file, files at or above their kind's `floor`, at most
/// `limit` files of each of the window's kinds: text, then documents, then
/// sessions, then audio and video, then images, best first within each,
/// less those [`drop_far_below`] leaves out.
pub fn every_file(hits: Vec<Hit>, floor: impl Fn(&Hit) -> f32, limit: usize) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut groups: [Vec<Hit>; 6] = Default::default();
    for h in hits {
        if h.score < floor(&h) || !seen.insert(h.path.clone()) {
            continue;
        }
        let group = &mut groups[match kind(&h) {
            results::TEXT => 0,
            results::DOCUMENT => 1,
            results::SESSION => 2,
            results::MEDIA => 3,
            results::IMAGE => 4,
            _ => 5,
        }];
        group.push(h);
    }
    for group in &mut groups {
        group.sort_by(|a, b| b.score.total_cmp(&a.score));
        group.truncate(limit);
    }
    let mut kept = groups.into_iter().flatten().collect();
    drop_far_below(&mut kept);
    kept
}

/// What the window shows of `h`; None for a file gone since it was indexed.
pub fn found(h: Hit) -> Option<Found> {
    let path = Path::new(&h.path);
    let mtime = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let kind = kind(&h);
    let unit = match kind {
        // An HTML page's lines are of its extracted text, no use to a reader.
        results::TEXT if !crate::extract::is_html(path) => "line",
        results::CODE => "line",
        results::DOCUMENT => crate::extract::unit(path),
        results::MEDIA => "second",
        _ => "",
    };
    let at = match h.kind {
        Kind::Media => u32::try_from(crate::extract::media::Track::of(h.line).1).unwrap_or(0),
        _ if unit.is_empty() => 0,
        _ => u32::try_from(h.line).unwrap_or(0),
    };
    let snippet = match h.kind {
        Kind::Image | Kind::Media => String::new(),
        _ => crate::snippet(&h.text),
    };
    Some(Found {
        path: h.path,
        kind: kind.to_owned(),
        score: f64::from(h.score),
        snippet,
        unit: unit.to_owned(),
        at,
        mtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: Kind, score: f32, path: &str) -> Hit {
        Hit {
            kind,
            score,
            path: path.to_owned(),
            line: 3,
            text: "some   matching\ntext".into(),
            copies: Vec::new(),
            vector: None,
            similar: Vec::new(),
        }
    }

    fn floor(h: &Hit) -> f32 {
        if picture(h) { 0.6 } else { 0.65 }
    }

    #[test]
    fn one_hit_per_file_above_its_floor_grouped_by_the_window_kind() {
        let hits = vec![
            hit(Kind::Text, 0.70, "/n/a.md"),
            hit(Kind::Text, 0.80, "/n/b.md"),
            hit(Kind::Text, 0.75, "/n/a.md"),
            hit(Kind::Text, 0.60, "/n/low.md"),
            hit(Kind::Document, 0.72, "/d/scan.pdf"),
            hit(Kind::Document, 0.76, "/p/shot.png"),
            hit(Kind::Image, 0.62, "/p/cat.jpg"),
            hit(Kind::Image, 0.66, "/p/shot.png"),
            hit(Kind::Image, 0.55, "/p/dim.jpg"),
        ];
        let paths: Vec<_> = every_file(hits, floor, 10)
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(paths, ["/n/b.md", "/n/a.md", "/d/scan.pdf", "/p/shot.png",]);
    }

    #[test]
    fn audio_video_and_images_far_below_a_session_are_left_out() {
        let hits = vec![
            hit(Kind::Session, 0.79, "/s/match.vwsession"),
            hit(Kind::Media, 0.75, "/v/close.mp4"),
            hit(Kind::Media, 0.66, "/v/clip.mp4"),
            hit(Kind::Image, 0.66, "/p/photo.png"),
        ];
        let paths: Vec<_> = every_file(hits, floor, 10)
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(paths, ["/s/match.vwsession", "/v/close.mp4"]);
    }

    #[test]
    fn images_alone_need_only_their_floor() {
        let hits = vec![
            hit(Kind::Image, 0.80, "/p/best.jpg"),
            hit(Kind::Image, 0.62, "/p/cat.jpg"),
        ];
        assert_eq!(every_file(hits, floor, 10).len(), 2);
    }

    #[test]
    fn at_most_limit_of_each_kind() {
        let hits = (0..5)
            .map(|i| hit(Kind::Text, 0.9 - i as f32 * 0.01, &format!("/n/{i}.md")))
            .chain((0..5).map(|i| hit(Kind::Image, 0.88, &format!("/p/{i}.jpg"))))
            .collect();
        let kept = every_file(hits, floor, 2);
        assert_eq!(kept.len(), 4);
        assert_eq!(kept[0].path, "/n/0.md");
    }

    /// A file `name` in a temp dir of its own, removed when dropped.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("vinosearch-results-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            TempFile(path)
        }

        fn hit(&self, kind: Kind) -> Hit {
            hit(kind, 0.8, self.0.to_str().unwrap())
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn kind_unit_at(f: &Found) -> (&str, &str, u32) {
        (f.kind.as_str(), f.unit.as_str(), f.at)
    }

    #[test]
    fn found_names_a_text_files_line() {
        let note = TempFile::new("note.md");
        let f = found(note.hit(Kind::Text)).unwrap();
        assert_eq!(f.path, note.0.to_str().unwrap());
        assert_eq!(kind_unit_at(&f), ("text", "line", 3));
        assert_eq!(f.snippet, "some matching text");
        assert!(f.mtime > 0);
    }

    #[test]
    fn text_read_in_a_photo_is_an_image_with_no_line_to_name() {
        let photo = TempFile::new("photo.png");
        let f = found(photo.hit(Kind::Document)).unwrap();
        assert_eq!(kind_unit_at(&f), ("image", "", 0));
        assert_eq!(f.snippet, "some matching text");
    }

    #[test]
    fn a_frame_is_found_at_its_second() {
        let clip = TempFile::new("clip.mkv");
        let mut seen = clip.hit(Kind::Media);
        seen.line = crate::extract::media::FRAME_LINES as i64 + 754;
        let f = found(seen).unwrap();
        assert_eq!(kind_unit_at(&f), ("media", "second", 754));
        assert_eq!(f.snippet, "");
    }

    #[test]
    fn a_file_gone_since_it_was_indexed_is_not_found() {
        assert!(found(hit(Kind::Text, 0.8, "/nonexistent/gone.md")).is_none());
    }

    /// The window reads the fields by position, as the tuple they were.
    #[test]
    fn found_goes_over_d_bus_as_the_struct_ssdssux() {
        use zbus::zvariant::Type;
        type AsTuple = (String, String, f64, String, String, u32, i64);
        assert_eq!(Found::SIGNATURE.to_string(), "(ssdssux)");
        assert_eq!(Found::SIGNATURE, AsTuple::SIGNATURE);
    }
}
