use super::{FileTypes, Kind, compact, media, remove, wanted};
use crate::config::Excludes;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

/// Drop everything indexed under an excluded path. Returns files removed.
pub fn purge(conn: &Connection, ex: &Excludes) -> Result<usize> {
    let mut n = 0;
    for p in ex.paths() {
        n += remove(conn, p)?;
    }
    if n > 0 {
        compact(conn)?;
    }
    Ok(n)
}

/// Drop every indexed file `types` no longer takes (images, once turned
/// off), and hold media to the longest it now allows (`media::fit`).
/// Returns files removed.
pub fn purge_types(conn: &Connection, types: &FileTypes) -> Result<usize> {
    let paths: Vec<String> = conn
        .prepare("SELECT path FROM files")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let tx = conn.unchecked_transaction()?;
    let mut n = 0;
    for p in paths.iter().filter(|p| !wanted(Path::new(p), types)) {
        n += tx.execute("DELETE FROM files WHERE path = ?1", [p])?;
    }
    tx.commit()?;
    let left_out = if types.media {
        media::fit(conn, types.max_media_seconds)?.left_out
    } else {
        0
    };
    if n + left_out > 0 {
        compact(conn)?;
    }
    Ok(n)
}

/// Drop the vectors of every kind made by a model other than this build's,
/// and record this build's. The files holding them go too (with their
/// copies), so the next check of each root indexes them afresh. An index
/// with no record for a kind is taken as this build's and tagged: nothing
/// else has shipped for it. Returns (kind, files dropped) per kind purged.
pub fn purge_other_models(conn: &Connection) -> Result<Vec<(Kind, usize)>> {
    let tags: Vec<(Kind, String)> = Kind::ALL.iter().map(|k| (*k, k.model_tag())).collect();
    purge_models_but(conn, &tags)
}

fn purge_models_but(conn: &Connection, tags: &[(Kind, String)]) -> Result<Vec<(Kind, usize)>> {
    let mut out = Vec::new();
    let tx = conn.unchecked_transaction()?;
    for (kind, tag) in tags {
        let was: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [kind.meta_key()],
                |r| r.get(0),
            )
            .optional()?;
        if was.as_deref().is_some_and(|w| w != tag) {
            out.push((*kind, drop_kind(&tx, *kind)?));
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![kind.meta_key(), tag],
        )?;
    }
    tx.commit()?;
    if out.iter().any(|(_, n)| *n > 0) {
        compact(conn)?;
    }
    Ok(out)
}

/// Every chunk of `kind`, and the files holding them and their copies.
/// Returns files removed. The chunks go first: deleting a file with chunks
/// hands them to a copy (`hand_over`), and these must go (pinned by
/// `tests::vectors_from_another_model_are_dropped_with_their_files`).
fn drop_kind(tx: &Connection, kind: Kind) -> Result<usize> {
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS dropped (id INTEGER, sha BLOB); DELETE FROM dropped;",
    )?;
    tx.execute(
        "INSERT INTO dropped SELECT DISTINCT f.id, f.sha256 FROM files f
         JOIN chunks c ON c.file_id = f.id WHERE c.kind = ?1",
        [kind as i64],
    )?;
    tx.execute("DELETE FROM chunks WHERE kind = ?1", [kind as i64])?;
    let n = tx.execute(
        "DELETE FROM files WHERE id IN (SELECT id FROM dropped)
         OR sha256 IN (SELECT sha FROM dropped WHERE sha IS NOT NULL)",
        [],
    )?;
    tx.execute_batch("DROP TABLE dropped")?;
    Ok(n)
}

/// `LIKE` patterns for files with a HEIF extension.
fn heif_patterns() -> Vec<String> {
    crate::ml::vision::HEIF_EXTENSIONS
        .iter()
        .map(|e| format!("%.{e}"))
        .collect()
}

/// Images recorded without chunks for want of a HEIF decoder: by extension,
/// leaving out copies of a file that did decode. What the Index page counts
/// while heif-convert is missing.
pub fn undecodable(conn: &Connection) -> Result<i64> {
    let mut n = 0;
    for pattern in heif_patterns() {
        n += conn.query_row(
            "SELECT count(*) FROM files f
             WHERE lower(f.path) LIKE ?1
               AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = f.id)
               AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                               WHERE g.sha256 = f.sha256)",
            [pattern],
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(n)
}

/// Forget the images recorded as undecodable once a decoder is there, so the
/// next check of each root indexes them. Returns files dropped.
pub fn retry_undecodable(conn: &Connection) -> Result<usize> {
    if crate::ml::vision::heif_convert().is_none() {
        return Ok(0);
    }
    let mut n = 0;
    for pattern in heif_patterns() {
        n += conn.execute(
            "DELETE FROM files WHERE lower(path) LIKE ?1
               AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = files.id)
               AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                               WHERE g.sha256 = files.sha256)",
            [pattern],
        )?;
    }
    Ok(n)
}

/// Forget the PDFs recorded with no text once OCR could read them as scans:
/// with this OCR model, and poppler installed. Recorded in `meta`, so
/// it happens once for each, not at every start. Returns files dropped.
pub fn retry_scans(conn: &Connection, ocr: bool) -> Result<usize> {
    if !ocr || !crate::ml::ocr::poppler() {
        return Ok(0);
    }
    let key = "reading.scans";
    let now = format!(
        "{}/{}",
        crate::ml::model::OCR_REVISION,
        crate::ml::ocr::READING
    );
    let now = now.as_str();
    let was: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?;
    if was.as_deref() == Some(now) {
        return Ok(0);
    }
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "DELETE FROM files WHERE lower(path) LIKE '%.pdf'
           AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.file_id = files.id)
           AND NOT EXISTS (SELECT 1 FROM files g JOIN chunks c ON c.file_id = g.id
                           WHERE g.sha256 = files.sha256)",
        [],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key, now],
    )?;
    tx.commit()?;
    Ok(n)
}

/// A format whose text is read differently now. Mtime and size don't show
/// the change, so files read the old way are dropped once instead.
struct Reading {
    format: &'static str,
    extensions: &'static [&'static str],
    version: u32,
}

/// HTML through `extract::html` since 1; PDFs without their headers, footers
/// and page numbers since 1.
const READINGS: &[Reading] = &[
    Reading {
        format: "html",
        extensions: &["html", "htm"],
        version: 1,
    },
    Reading {
        format: "pdf",
        extensions: &["pdf"],
        version: 1,
    },
];

/// Forget the files of each format in `READINGS` indexed under an older
/// reading, so the next check of each root indexes them again. Returns files
/// dropped.
pub fn reread_formats(conn: &Connection) -> Result<usize> {
    let mut n = 0;
    let tx = conn.unchecked_transaction()?;
    for Reading {
        format,
        extensions,
        version,
    } in READINGS
    {
        let key = format!("reading.{format}");
        let was: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if was == Some(version.to_string()) {
            continue;
        }
        for e in *extensions {
            n += tx.execute(
                "DELETE FROM files WHERE lower(path) LIKE ?1",
                [format!("%.{e}")],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, version.to_string()],
        )?;
    }
    tx.commit()?;
    if n > 0 {
        compact(conn)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{add_file, axis, file_with};
    use crate::index::{counts, is_indexed, open};
    use std::fs;

    #[test]
    fn pdfs_with_no_text_are_read_again_once_ocr_can() {
        let dir = std::env::temp_dir().join(format!("vinosearch-scans-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        file_with(&conn, "/scan.PDF", &[]);
        file_with(&conn, "/text.pdf", &[(Kind::Document, 1)]);
        file_with(&conn, "/broken.jpg", &[]);
        let paths = |conn: &Connection| -> Vec<String> {
            conn.prepare("SELECT path FROM files ORDER BY path")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(retry_scans(&conn, false).unwrap(), 0);
        if crate::ml::ocr::poppler() {
            assert_eq!(retry_scans(&conn, true).unwrap(), 1);
            assert_eq!(paths(&conn), ["/broken.jpg", "/text.pdf"]);
            file_with(&conn, "/scan.PDF", &[]);
            assert_eq!(
                retry_scans(&conn, true).unwrap(),
                0,
                "once per OCR model, not at every start"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_indexed_before_it_was_read_as_html_is_indexed_again_once() {
        let dir = std::env::temp_dir().join(format!("vinosearch-reread-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        for p in [
            "/site/index.html",
            "/site/OLD.HTM",
            "/notes.md",
            "/docs/a.pdf",
        ] {
            add_file(&conn, p, &[0]);
        }
        assert_eq!(reread_formats(&conn).unwrap(), 3);
        assert!(is_indexed(&conn, "/notes.md").unwrap());
        assert!(!is_indexed(&conn, "/site/index.html").unwrap());
        add_file(&conn, "/site/index.html", &[0]);
        assert_eq!(reread_formats(&conn).unwrap(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn undecodable_photos_are_counted_but_not_their_decoded_copies() {
        let dir = std::env::temp_dir().join(format!("vinosearch-heif-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let file = |path: &str, hash: &[u8]| {
            conn.execute(
                "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, ?2)",
                params![path, hash],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        file("/p/IMG_1.HEIC", b"a");
        file("/p/IMG_2.heic", b"b");
        file("/p/render.avif", b"c");
        let decoded = file("/p/IMG_3.heic", b"d");
        conn.execute(
            "INSERT INTO chunks (file_id, start_line, text, embedding, kind) VALUES (?1, 0, '', ?2, 1)",
            params![decoded, axis(0)],
        )
        .unwrap();
        let _copy_of_decoded = file("/p/copy.heic", b"d");
        let _not_heif = file("/p/scan.pdf", b"e");
        assert_eq!(undecodable(&conn).unwrap(), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A byte-identical copy of `of` at `path`, holding no chunks of its own.
    fn copy_holding_no_chunks(conn: &Connection, of: &str, path: &str) {
        conn.execute("UPDATE files SET sha256 = x'01' WHERE path = ?1", [of])
            .unwrap();
        conn.execute(
            "INSERT INTO files (path, mtime, size, sha256) VALUES (?1, 0, 0, x'01')",
            [path],
        )
        .unwrap();
    }

    #[test]
    fn vectors_from_another_model_are_dropped_with_their_files() {
        let dir = std::env::temp_dir().join(format!("vinosearch-model-{}", std::process::id()));
        let conn = open(&dir.join("i.sqlite")).unwrap();
        let tag = |kind: Kind, t: &str| (kind, t.to_owned());
        let v1 = [
            tag(Kind::Text, "text-1"),
            tag(Kind::Document, "text-1"),
            tag(Kind::Image, "text-1+vision-1/140"),
        ];
        assert!(
            purge_models_but(&conn, &v1).unwrap().is_empty(),
            "a fresh index has no record: nothing dropped"
        );
        add_file(&conn, "/notes.md", &[0, 1]);
        add_file(&conn, "/cat.png", &[2]);
        conn.execute("UPDATE chunks SET kind = 1 WHERE file_id = (SELECT id FROM files WHERE path = '/cat.png')", []).unwrap();
        copy_holding_no_chunks(&conn, "/cat.png", "/copy.png");
        assert!(
            purge_models_but(&conn, &v1).unwrap().is_empty(),
            "the same models again: nothing dropped"
        );
        assert_eq!(counts(&conn).unwrap(), (3, 3));

        let v2 = [
            tag(Kind::Text, "text-1"),
            tag(Kind::Document, "text-1"),
            tag(Kind::Image, "text-1+vision-2/140"),
        ];
        assert_eq!(
            purge_models_but(&conn, &v2).unwrap(),
            [(Kind::Image, 2)],
            "a new vision encoder: the images go, the copy included"
        );
        assert_eq!(counts(&conn).unwrap(), (1, 2), "text stays");
        assert!(is_indexed(&conn, "/notes.md").unwrap());
        assert!(purge_models_but(&conn, &v2).unwrap().is_empty());

        let v3 = [
            tag(Kind::Text, "text-2"),
            tag(Kind::Document, "text-2"),
            tag(Kind::Image, "text-1+vision-2/140"),
        ];
        let purged = purge_models_but(&conn, &v3).unwrap();
        assert_eq!(purged, [(Kind::Text, 1), (Kind::Document, 0)]);
        assert_eq!(
            counts(&conn).unwrap(),
            (0, 0),
            "a new text model: everything of that kind goes"
        );
        let recorded: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'model.text'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(recorded, "text-2");
        fs::remove_dir_all(&dir).unwrap();
    }
}
