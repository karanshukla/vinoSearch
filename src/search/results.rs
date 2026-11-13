//! `Control1.Search`: every file a query finds, for the search window (#111).
//! KRunner and GNOME Shell show the best 5 of each kind; the window shows up
//! to `results::MAX` and filters them itself.

use crate::index::{Hit, Kind};
use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;
use vinosearch_shared::dbus::results;

/// One file as `Control1.Search` returns it: path, kind (`results::*`),
/// score, snippet, unit ("line", "page", "slide", ... or empty), the match's
/// number in that unit (0 for none), and mtime in Unix seconds. A media
/// file's unit is "second", counted from 0.
pub type Found = (String, String, f64, String, String, u32, i64);

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
        // An HTML page's lines are of its extracted text: no use to a reader.
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
    Some((
        h.path,
        kind.to_owned(),
        f64::from(h.score),
        snippet,
        unit.to_owned(),
        at,
        mtime,
    ))
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

    #[test]
    fn found_names_the_unit_and_drops_missing_files() {
        let dir = std::env::temp_dir().join(format!("vinosearch-results-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("note.md");
        std::fs::write(&note, "x").unwrap();
        let photo = dir.join("photo.png");
        std::fs::write(&photo, "x").unwrap();

        let (path, kind, _, snippet, unit, at, mtime) =
            found(hit(Kind::Text, 0.8, note.to_str().unwrap())).unwrap();
        assert_eq!(path, note.to_str().unwrap());
        assert_eq!((kind.as_str(), unit.as_str(), at), ("text", "line", 3));
        assert_eq!(snippet, "some matching text");
        assert!(mtime > 0);

        // Text read in a photo: an image, with no line to name.
        let (_, kind, _, snippet, unit, at, _) =
            found(hit(Kind::Document, 0.8, photo.to_str().unwrap())).unwrap();
        assert_eq!((kind.as_str(), unit.as_str(), at), ("image", "", 0));
        assert_eq!(snippet, "some matching text");

        // A frame's number is past FRAME_LINES; the window gets its second.
        let clip = dir.join("clip.mkv");
        std::fs::write(&clip, "x").unwrap();
        let mut seen = hit(Kind::Media, 0.8, clip.to_str().unwrap());
        seen.line = crate::extract::media::FRAME_LINES as i64 + 754;
        let (_, kind, _, snippet, unit, at, _) = found(seen).unwrap();
        assert_eq!((kind.as_str(), unit.as_str(), at), ("media", "second", 754));
        assert_eq!(snippet, "");

        assert!(found(hit(Kind::Text, 0.8, "/nonexistent/gone.md")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
