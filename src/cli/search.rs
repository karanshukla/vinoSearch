//! `search` and `similar`.

use crate::extract::{self, media, session};
use crate::index::{self, Hit, Kind};
use crate::ml::vision;
use crate::{Settings, embedder, snippet};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub fn search(query: &str, n: usize, openvino_dir: Option<PathBuf>, s: &Settings) -> Result<bool> {
    let mut emb = embedder(openvino_dir, &s.device)?;
    let conn = index::open(&index::default_db()?)?;
    index::purge(&conn, &s.excludes)?;
    index::purge_types(&conn, &s.types)?;
    index::purge_other_models(&conn)?;
    let t0 = Instant::now();
    let (text, kinds) = index::scope(query);
    let q = emb.query_for(text, kinds)?;
    let t1 = Instant::now();
    let vectors = index::Vectors::load(&conn)?;
    let words = index::Words::find(&conn, text);
    let mut hits = Vec::new();
    for kind in kinds {
        hits.extend(index::search(
            &conn,
            &vectors,
            &q,
            words.as_ref(),
            *kind,
            n,
        )?);
    }
    eprintln!(
        "query {:.1} ms, search {:.1} ms",
        (t1 - t0).as_secs_f64() * 1e3,
        t1.elapsed().as_secs_f64() * 1e3
    );
    for hit in hits {
        if hit.kind == Kind::Image {
            println!("{:.3}  {}  (image)", hit.score, hit.path);
            continue;
        }
        println!(
            "{:.3}  {}{}\n       {}",
            hit.score,
            hit.path,
            place_in_file(&hit),
            snippet(&hit.text)
        );
        print_copies(&hit);
    }
    Ok(true)
}

/// Where in its file a hit is, as the text printed straight after the path:
/// `:12`, `, page 3`, `, heard at 04:05`.
fn place_in_file(hit: &Hit) -> String {
    let path = Path::new(&hit.path);
    match hit.kind {
        Kind::Document if !extract::is_document(path) => " (text in the image)".to_owned(),
        Kind::Document => format!(", {} {}", extract::unit(path), hit.line),
        Kind::Session => match session::Part::of(hit.line) {
            (session::Part::Transcript, line) => format!(", transcript:{line}"),
            (session::Part::Summary, line) => format!(", summary:{line}"),
        },
        Kind::Media => match media::Track::of(hit.line) {
            (media::Track::Sound, seconds) => format!(", heard at {}", media::stamp(seconds)),
            (media::Track::Picture, seconds) => format!(", seen at {}", media::stamp(seconds)),
        },
        _ => format!(":{}", hit.line),
    }
}

fn print_copies(hit: &Hit) {
    if !hit.copies.is_empty() {
        println!("       same as: {}", hit.copies.join(", "));
    }
}

pub fn similar(
    image: PathBuf,
    n: usize,
    min_score: Option<f32>,
    openvino_dir: Option<PathBuf>,
    s: &Settings,
) -> Result<bool> {
    let conn = index::open(&index::default_db()?)?;
    let path = std::fs::canonicalize(&image).unwrap_or(image);
    let key = path.to_string_lossy().into_owned();
    let t0 = Instant::now();
    let q = match index::image_vector(&conn, &key)? {
        Some(v) => v,
        None => {
            let bytes = std::fs::read(&path)?;
            let pixels = vision::prepare(&vision::load(&bytes)?)?;
            let mut emb = embedder(openvino_dir, &s.device)?;
            emb.image(&pixels)?
        }
    };
    let t1 = Instant::now();
    let vectors = index::Vectors::load(&conn)?;
    let with_the_image_itself = n + 1;
    let hits = index::search(
        &conn,
        &vectors,
        &q,
        None,
        Kind::Image,
        with_the_image_itself,
    )?;
    eprintln!(
        "vector {:.1} ms, search {:.1} ms",
        (t1 - t0).as_secs_f64() * 1e3,
        t1.elapsed().as_secs_f64() * 1e3
    );
    let shown = hits
        .into_iter()
        .filter(|h| h.path != key && min_score.is_none_or(|m| h.score >= m))
        .take(n);
    for hit in shown {
        println!("{:.3}  {}", hit.score, hit.path);
        print_copies(&hit);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: Kind, path: &str, line: i64) -> Hit {
        Hit {
            kind,
            score: 0.9,
            path: path.into(),
            line,
            text: String::new(),
            copies: Vec::new(),
            vector: None,
            similar: Vec::new(),
        }
    }

    #[test]
    fn text_and_code_are_placed_by_line() {
        assert_eq!(place_in_file(&hit(Kind::Text, "/h/notes.md", 12)), ":12");
        assert_eq!(place_in_file(&hit(Kind::Code, "/h/main.rs", 7)), ":7");
    }

    #[test]
    fn a_document_is_placed_by_its_own_unit() {
        assert_eq!(
            place_in_file(&hit(Kind::Document, "/h/tax.pdf", 3)),
            ", page 3"
        );
        assert_eq!(
            place_in_file(&hit(Kind::Document, "/h/budget.csv", 40)),
            ", row 40"
        );
    }

    #[test]
    fn text_read_from_a_picture_has_no_place() {
        assert_eq!(
            place_in_file(&hit(Kind::Document, "/h/screenshot.png", 1)),
            " (text in the image)"
        );
    }

    #[test]
    fn a_session_names_the_part_that_matched() {
        let transcript = hit(Kind::Session, "/h/talk.vwsession", 5);
        assert_eq!(place_in_file(&transcript), ", transcript:5");
        let summary_line = session::SUMMARY_LINES as i64 + 2;
        let summary = hit(Kind::Session, "/h/talk.vwsession", summary_line);
        assert_eq!(place_in_file(&summary), ", summary:2");
    }

    #[test]
    fn media_is_placed_by_time_on_the_track_that_matched() {
        let heard = hit(Kind::Media, "/h/film.mkv", 245);
        assert_eq!(place_in_file(&heard), ", heard at 04:05");
        let seen = hit(Kind::Media, "/h/film.mkv", media::FRAME_LINES as i64 + 3725);
        assert_eq!(place_in_file(&seen), ", seen at 1:02:05");
    }
}
