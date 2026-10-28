//! vinoWhisper sessions (#143): a `.vwsession` bundle is a zip holding a
//! transcript, its summary once vinoWhisper has made one, and an attestation
//! of how each was made (karanshukla/vinoWhisper#123). Both texts are
//! indexed as chunks of the one file: the transcript's numbered by its line,
//! the summary's by its line from SUMMARY_LINES up, so a hit says which part
//! it came from.

use super::check_zip;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{Cursor, Read};

pub const EXTENSION: &str = "vwsession";
/// The first entry, stored, as ODF and EPUB do: what makes a zip a session.
const MIMETYPE: &[u8] = b"application/vnd.vinowhisper.session+zip";
/// The `format`s this build reads. A bundle of any other is refused rather
/// than guessed at.
const FORMATS: &[&str] = &["vinowhisper-session/1"];
const TRANSCRIPT: &str = "transcript.txt";
const SUMMARY: &str = "summary.md";
const ATTESTATION: &str = "attestation.json";
const MAX_ATTESTATION_BYTES: u64 = 64 << 10;
/// A transcript or summary past this isn't read: about 20 hours of speech.
/// Bundles are read in the runner, not the extract child, so what one can
/// unzip there is held well under what a document's parts may.
const MAX_PART_BYTES: u64 = crate::index::MAX_FILE_BYTES;
/// What `made_by` says of a summary a model wrote.
const AI_SUMMARY: &str = "ai-generated-summary";

/// Summary chunks are numbered from here, past any transcript's lines.
pub const SUMMARY_LINES: usize = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Part {
    Transcript,
    Summary,
}

impl Part {
    /// The part a chunk numbered `n` came from, and its line in that part.
    pub fn of(n: i64) -> (Part, i64) {
        if n >= SUMMARY_LINES as i64 {
            (Part::Summary, n - SUMMARY_LINES as i64)
        } else {
            (Part::Transcript, n)
        }
    }

    fn entry(self) -> &'static str {
        match self {
            Part::Transcript => TRANSCRIPT,
            Part::Summary => SUMMARY,
        }
    }
}

#[derive(serde::Deserialize)]
struct Attestation {
    format: String,
    session: Session,
    #[serde(default)]
    files: HashMap<String, Made>,
}

#[derive(serde::Deserialize)]
struct Session {
    /// RFC 3339, to the second.
    started: String,
    duration_s: u64,
}

#[derive(serde::Deserialize)]
struct Made {
    made_by: Option<String>,
}

/// What a result says of its session.
#[derive(Debug, PartialEq)]
pub struct About {
    pub started: String,
    pub duration_s: u64,
    pub has_summary: bool,
    /// The attestation marks the summary as written by a model.
    pub ai_summary: bool,
}

impl About {
    /// "2026-10-27 14:05, 42 min"
    pub fn when(&self) -> String {
        // RFC 3339's date and minute, without parsing the rest.
        let started = match self.started.get(..16) {
            Some(s) if s.as_bytes().get(10) == Some(&b'T') => s.replacen('T', " ", 1),
            _ => self.started.clone(),
        };
        let mins = self.duration_s.div_ceil(60);
        if mins < 60 {
            format!("{started}, {mins} min")
        } else {
            format!("{started}, {} h {:02} min", mins / 60, mins % 60)
        }
    }
}

type Zip<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// `bytes` as a session this build reads, and its attestation.
fn open(bytes: &[u8]) -> Result<(Zip<'_>, Attestation)> {
    check_zip(bytes)?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    {
        let first = zip.by_index(0).context("empty zip")?;
        if !first.name().is_ok_and(|n| n == "mimetype")
            || first.compression() != zip::CompressionMethod::Stored
        {
            bail!("not a vinoWhisper session: no stored mimetype first");
        }
        let mut mime = Vec::new();
        first
            .take(MIMETYPE.len() as u64 + 1)
            .read_to_end(&mut mime)?;
        if mime != MIMETYPE {
            bail!(
                "not a vinoWhisper session: mimetype {:?}",
                String::from_utf8_lossy(&mime)
            );
        }
    }
    let mut json = Vec::new();
    zip.by_name(ATTESTATION)?
        .take(MAX_ATTESTATION_BYTES + 1)
        .read_to_end(&mut json)?;
    if json.len() as u64 > MAX_ATTESTATION_BYTES {
        bail!("{ATTESTATION} is over {MAX_ATTESTATION_BYTES} bytes");
    }
    let att: Attestation = serde_json::from_slice(&json).context(ATTESTATION)?;
    if !FORMATS.contains(&att.format.as_str()) {
        bail!(Unknown(att.format));
    }
    Ok((zip, att))
}

/// A session in a format this build doesn't read: skipped, and counted.
#[derive(Debug)]
pub struct Unknown(pub String);

impl std::fmt::Display for Unknown {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "a vinoWhisper session in format {:?}, which this build doesn't read",
            self.0
        )
    }
}

impl std::error::Error for Unknown {}

fn read(zip: &mut Zip, name: &str) -> Result<String> {
    let mut s = String::new();
    zip.by_name(name)?
        .take(MAX_PART_BYTES + 1)
        .read_to_string(&mut s)?;
    if s.len() as u64 > MAX_PART_BYTES {
        bail!("{name} is over {MAX_PART_BYTES} bytes");
    }
    Ok(s)
}

fn has(zip: &Zip, name: &str) -> bool {
    zip.index_for_name(name).is_some()
}

/// `path`'s bytes, held to MAX_DOCUMENT_BYTES as indexing holds it.
pub fn load(path: &std::path::Path) -> Result<Vec<u8>> {
    use crate::index::MAX_DOCUMENT_BYTES;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!("over {MAX_DOCUMENT_BYTES} bytes");
    }
    Ok(bytes)
}

/// The transcript's chunks by line, then the summary's from SUMMARY_LINES.
pub fn chunks(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let (mut zip, _) = open(bytes)?;
    let mut out = crate::index::chunk(&read(&mut zip, TRANSCRIPT)?);
    if has(&zip, SUMMARY) {
        let summary = crate::index::chunk(&read(&mut zip, SUMMARY)?);
        out.extend(summary.into_iter().map(|(n, c)| (SUMMARY_LINES + n, c)));
    }
    Ok(out)
}

pub fn about(bytes: &[u8]) -> Result<About> {
    let (zip, att) = open(bytes)?;
    Ok(About {
        has_summary: has(&zip, SUMMARY),
        ai_summary: att.files.get(SUMMARY).and_then(|m| m.made_by.as_deref()) == Some(AI_SUMMARY),
        started: att.session.started,
        duration_s: att.session.duration_s,
    })
}

/// One part's text; Err for a summary not made yet.
pub fn text(bytes: &[u8], part: Part) -> Result<String> {
    let (mut zip, _) = open(bytes)?;
    read(&mut zip, part.entry())
}

/// The `[MM:SS]` of the paragraph holding line `line` (from 1) of a
/// transcript: the last stamp starting a line at or before it, as "MM:SS".
pub fn stamp_at(transcript: &str, line: usize) -> Option<String> {
    transcript
        .lines()
        .take(line)
        .filter_map(stamp)
        .last()
        .map(str::to_owned)
}

/// `[MM:SS] ` starting `line`, minutes 2 digits or more: its "MM:SS".
fn stamp(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let end = rest.find("] ")?;
    let (m, s) = rest[..end].split_once(':')?;
    let digits = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    (m.len() >= 2 && s.len() == 2 && digits(m) && digits(s)).then(|| &rest[..end])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    pub const TRANSCRIPT_TEXT: &str = "vinoWhisper transcript, 2026-10-27 14:05, system audio, NPU\n\n\
        [00:00] Welcome to the call about the quarterly budget.\n\n\
        [01:12] The second item is the office move.\n\
        It happens in March.\n\n\
        [61:05] Past the hour now.\n";

    pub fn attestation(format: &str) -> String {
        format!(
            r#"{{"format": "{format}",
              "generator": {{"name": "vinoWhisper", "version": "1.1.0"}},
              "session": {{"started": "2026-10-27T14:05:12-04:00", "duration_s": 2530,
                           "source": "system audio", "language": "en", "task": "transcribe"}},
              "files": {{"transcript.txt": {{"sha256": "00", "made_by": "speech-to-text", "model": null}},
                         "summary.md": {{"sha256": "00", "made_by": "ai-generated-summary"}}}}}}"#
        )
    }

    /// A bundle as vinoWhisper writes one: `mimetype` first and stored, the
    /// rest deflated in any order.
    pub fn bundle(summary: Option<&str>, format: &str) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("mimetype", stored).unwrap();
        w.write_all(MIMETYPE).unwrap();
        let deflated = SimpleFileOptions::default();
        w.start_file(ATTESTATION, deflated).unwrap();
        w.write_all(attestation(format).as_bytes()).unwrap();
        if let Some(s) = summary {
            w.start_file(SUMMARY, deflated).unwrap();
            w.write_all(s.as_bytes()).unwrap();
        }
        w.start_file(TRANSCRIPT, deflated).unwrap();
        w.write_all(TRANSCRIPT_TEXT.as_bytes()).unwrap();
        w.finish().unwrap().into_inner()
    }

    pub const SUMMARY_TEXT: &str =
        "# Budget call\n\nThe budget and the office move.\n\n- The move is in March [01:12]\n";

    #[test]
    fn transcript_by_line_then_summary_past_it() {
        let c = chunks(&bundle(Some(SUMMARY_TEXT), "vinowhisper-session/1")).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].0, 1);
        assert!(c[0].1.contains("[01:12] The second item"));
        assert_eq!(c[1].0, SUMMARY_LINES + 1);
        assert_eq!(Part::of(c[1].0 as i64), (Part::Summary, 1));
        assert_eq!(Part::of(5), (Part::Transcript, 5));
        assert!(c[1].1.starts_with("# Budget call"));
    }

    #[test]
    fn a_session_without_a_summary_is_its_transcript() {
        let b = bundle(None, "vinowhisper-session/1");
        let c = chunks(&b).unwrap();
        assert!(c.iter().all(|(n, _)| *n < SUMMARY_LINES));
        let about = about(&b).unwrap();
        assert!(!about.has_summary);
        assert!(text(&b, Part::Summary).is_err());
        assert_eq!(text(&b, Part::Transcript).unwrap(), TRANSCRIPT_TEXT);
    }

    #[test]
    fn an_unknown_format_is_refused_not_guessed() {
        let e = chunks(&bundle(Some(SUMMARY_TEXT), "vinowhisper-session/2")).unwrap_err();
        assert!(e.downcast_ref::<Unknown>().is_some(), "{e:#}");
    }

    #[test]
    fn a_zip_without_the_mimetype_first_is_no_session() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        w.start_file(ATTESTATION, opts).unwrap();
        w.write_all(attestation("vinowhisper-session/1").as_bytes())
            .unwrap();
        w.start_file("mimetype", opts).unwrap();
        w.write_all(MIMETYPE).unwrap();
        w.start_file(TRANSCRIPT, opts).unwrap();
        w.write_all(b"[00:00] hi\n").unwrap();
        let b = w.finish().unwrap().into_inner();
        assert!(chunks(&b).is_err());
    }

    #[test]
    fn a_part_too_big_to_be_a_transcript_is_refused() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("mimetype", stored).unwrap();
        w.write_all(MIMETYPE).unwrap();
        let deflated = SimpleFileOptions::default();
        w.start_file(ATTESTATION, deflated).unwrap();
        w.write_all(attestation("vinowhisper-session/1").as_bytes())
            .unwrap();
        w.start_file(TRANSCRIPT, deflated).unwrap();
        // Deflates to a few KiB.
        w.write_all(&vec![b'a'; MAX_PART_BYTES as usize + 1])
            .unwrap();
        let b = w.finish().unwrap().into_inner();
        assert!(b.len() < 64 << 10);
        assert!(chunks(&b).is_err());
    }

    #[test]
    fn about_reads_the_attestation() {
        let a = about(&bundle(Some(SUMMARY_TEXT), "vinowhisper-session/1")).unwrap();
        assert!(a.has_summary && a.ai_summary);
        assert_eq!(a.when(), "2026-10-27 14:05, 43 min");
        let long = About {
            duration_s: 3_700,
            ..a
        };
        assert_eq!(long.when(), "2026-10-27 14:05, 1 h 02 min");
    }

    /// Bundles vinoWhisper's own writer made (vinoWhisper#123, b244bb8).
    #[test]
    fn vinowhispers_own_bundles_read() {
        let with = include_bytes!("../../tests/fixtures/session_v1.vwsession");
        let c = chunks(with).unwrap();
        let (transcript, summary): (Vec<_>, Vec<_>) =
            c.iter().partition(|(n, _)| *n < SUMMARY_LINES);
        assert!(
            transcript[0]
                .1
                .contains("Café, naïve, “quoted” — and an ellipsis…")
        );
        assert!(summary[0].1.starts_with("# Local captioning on the NPU"));
        let a = about(with).unwrap();
        assert!(a.has_summary && a.ai_summary);
        assert_eq!(a.when(), "2026-10-27 14:05, 1 h 02 min");
        let text = text(with, Part::Transcript).unwrap();
        assert_eq!(stamp_at(&text, 99).as_deref(), Some("61:05"));

        let without = include_bytes!("../../tests/fixtures/session_v1_nosummary.vwsession");
        assert!(
            chunks(without)
                .unwrap()
                .iter()
                .all(|(n, _)| *n < SUMMARY_LINES)
        );
        assert!(!about(without).unwrap().has_summary);
    }

    #[test]
    fn a_line_is_under_the_last_stamp_before_it() {
        // Line 1 is the header, with no stamp.
        assert_eq!(stamp_at(TRANSCRIPT_TEXT, 1), None);
        assert_eq!(stamp_at(TRANSCRIPT_TEXT, 3).as_deref(), Some("00:00"));
        // "It happens in March." is in the [01:12] paragraph.
        assert_eq!(stamp_at(TRANSCRIPT_TEXT, 6).as_deref(), Some("01:12"));
        assert_eq!(stamp_at(TRANSCRIPT_TEXT, 99).as_deref(), Some("61:05"));
        assert_eq!(stamp("[1:01:05] no"), None);
        assert_eq!(stamp("[00:00]no space"), None);
    }
}
