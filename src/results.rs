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
/// number in that unit (0 for none), and mtime in Unix seconds.
pub type Found = (String, String, f64, String, String, u32, i64);

/// The window's kind for a hit: text OCR read in an image is an image, as
/// KRunner groups it.
fn kind(h: &Hit) -> &'static str {
    match h.kind {
        Kind::Text => results::TEXT,
        Kind::Code => results::CODE,
        Kind::Session => results::SESSION,
        Kind::Document if crate::extract::is_document(Path::new(&h.path)) => results::DOCUMENT,
        Kind::Document | Kind::Image => results::IMAGE,
    }
}

/// Best chunk per file, files at or above their kind's `floor`, at most
/// `limit` files of each of the window's kinds: text, then documents, then
/// sessions, then images, best first within each.
pub fn every_file(hits: Vec<Hit>, floor: impl Fn(Kind) -> f32, limit: usize) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut groups: [Vec<Hit>; 5] = Default::default();
    for h in hits {
        if h.score < floor(h.kind) || !seen.insert(h.path.clone()) {
            continue;
        }
        let group = &mut groups[match kind(&h) {
            results::TEXT => 0,
            results::DOCUMENT => 1,
            results::SESSION => 2,
            results::IMAGE => 3,
            _ => 4,
        }];
        group.push(h);
    }
    for group in &mut groups {
        group.sort_by(|a, b| b.score.total_cmp(&a.score));
        group.truncate(limit);
    }
    groups.into_iter().flatten().collect()
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
        _ => "",
    };
    let at = if unit.is_empty() {
        0
    } else {
        u32::try_from(h.line).unwrap_or(0)
    };
    let snippet = match h.kind {
        Kind::Image => String::new(),
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
        }
    }

    fn floor(kind: Kind) -> f32 {
        match kind {
            Kind::Image => 0.6,
            _ => 0.65,
        }
    }

    #[test]
    fn one_hit_per_file_above_its_floor_grouped_by_the_window_kind() {
        let hits = vec![
            hit(Kind::Text, 0.70, "/n/a.md"),
            hit(Kind::Text, 0.80, "/n/b.md"),
            hit(Kind::Text, 0.75, "/n/a.md"),
            hit(Kind::Text, 0.60, "/n/low.md"),
            hit(Kind::Document, 0.72, "/d/scan.pdf"),
            hit(Kind::Document, 0.71, "/p/shot.png"),
            hit(Kind::Image, 0.62, "/p/cat.jpg"),
            hit(Kind::Image, 0.66, "/p/shot.png"),
            hit(Kind::Image, 0.55, "/p/dim.jpg"),
        ];
        let paths: Vec<_> = every_file(hits, floor, 10)
            .into_iter()
            .map(|h| h.path)
            .collect();
        assert_eq!(
            paths,
            [
                "/n/b.md",
                "/n/a.md",
                "/d/scan.pdf",
                "/p/shot.png",
                "/p/cat.jpg"
            ]
        );
    }

    #[test]
    fn at_most_limit_of_each_kind() {
        let hits = (0..5)
            .map(|i| hit(Kind::Text, 0.9 - i as f32 * 0.01, &format!("/n/{i}.md")))
            .chain((0..5).map(|i| hit(Kind::Image, 0.7, &format!("/p/{i}.jpg"))))
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

        assert!(found(hit(Kind::Text, 0.8, "/nonexistent/gone.md")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
