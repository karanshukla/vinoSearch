//! Text out of documents: PDF pages, the XML inside Office and
//! OpenDocument zips, spreadsheets, CSV with its header on every chunk, EPUB
//! chapters, subtitles, and Word 97, PowerPoint 97 and RTF files.
//! What comes out is chunked like any text file, numbered by page, slide or
//! row instead of line. The indexer runs it in a child process with limits,
//! since the files come from anywhere.

use crate::index::{CHUNK_CHARS, MAX_DOCUMENT_BYTES, MAX_FILE_BYTES, chunk};
use anyhow::{Context, Result, bail};
use quick_xml::events::Event;
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Duration;

mod html;
pub mod media;
mod ole;
mod pdf;
mod rtf;
pub mod session;

use html::entity;
pub use html::html;
use pdf::pdf;

/// Address space the child may map: a 32 MiB PDF's streams, decompressed, and lopdf's objects fit
/// many times over.
const CHILD_ADDRESS_SPACE: u64 = 1 << 30;
/// CPU seconds before SIGXCPU per run of parts. Parsing takes well under a second; a zip bomb or
/// quadratic parser doesn't.
const CHILD_CPU_SECONDS: u64 = 30;
/// Wall-clock limit for a child that hangs; idle priority stretches 30 CPU seconds on a busy
/// machine.
const CHILD_WALL: Duration = Duration::from_secs(120);

/// A long document is read a run of parts at a time: the first with everything else, the rest
/// after.
pub const PARTS_AT_ONCE: usize = 30;
pub const MAX_PARTS: usize = 5_000;
/// A part of a format without pages: about a printed page of text.
pub const PART_CHUNKS: usize = 3;

#[derive(Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Extract {
    pub parts: usize,
    pub chunks: Vec<(usize, String)>,
}

pub type Parser = fn(&Path, &[u8], usize, usize) -> Result<Extract>;

/// `parts`, run in a child (the runner executing itself as `vinosearch extract
/// <path>`, bytes on stdin) under memory, CPU and wall-clock limits, so a parser
/// that aborts, overflows its stack, eats memory or hangs costs that process, not
/// the runner. The limits apply afresh to each run of parts. Err for a parser
/// error, a child its limits stopped, or one that failed to start.
pub fn parts_in_child(path: &Path, bytes: &[u8], from: usize, count: usize) -> Result<Extract> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("extract")
        .arg("--from")
        .arg(from.to_string())
        .arg("--count")
        .arg(count.to_string())
        .arg("--")
        .arg(path);
    let out = crate::proc::run(cmd, bytes, CHILD_WALL)?;
    serde_json::from_slice(&out).context("reading the extractor's output")
}

/// `path`'s whole text, for reading aloud: every part in one run, still held to MAX_FILE_BYTES
/// (past that is hours of speech).
pub fn text(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!("over {MAX_DOCUMENT_BYTES} bytes");
    }
    let chunks = parts_in_child(path, &bytes, 0, usize::MAX)?.chunks;
    let size: usize = chunks.iter().map(|(_, c)| c.len()).sum();
    if size as u64 > MAX_FILE_BYTES {
        bail!("{size} bytes of text");
    }
    Ok(join(path, chunks))
}

/// Chunks back into their text: `chunk` cuts without overlap, so a part's chunks
/// concatenate; pages and slides are kept apart by a blank line. A table's header,
/// repeated on each chunk, is kept once per sheet.
fn join(path: &Path, chunks: Vec<(usize, String)>) -> String {
    if ext(path) == "pdf" {
        return pdf_pages(&chunks);
    }
    let header_lines = match ext(path).as_str() {
        "csv" | "tsv" => 1,
        e if SHEETS.contains(&e) => 2,
        _ => 0,
    };
    let by_part = paged(path);
    let mut out = String::new();
    let mut last_part = None;
    let mut last_header = None;
    for (n, text) in &chunks {
        let header_end = text
            .match_indices('\n')
            .nth(header_lines.max(1) - 1)
            .map_or(text.len(), |(i, _)| i + 1);
        let header = (header_lines > 0).then(|| &text[..header_end]);
        let new_part = if header_lines > 0 {
            header != last_header
        } else {
            by_part && last_part != Some(*n)
        };
        if new_part && !out.is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(match header {
            Some(_) if !new_part => &text[header_end..],
            _ => text,
        });
        last_part = Some(*n);
        last_header = header;
    }
    out
}

/// A PDF's pages, each on one line, a blank line apart: a text layer's line breaks
/// are layout, not meaning (some PDFs put every word on its own line).
fn pdf_pages(chunks: &[(usize, String)]) -> String {
    let mut pages: Vec<(usize, String)> = Vec::new();
    for (n, text) in chunks {
        match pages.last_mut() {
            Some((last, page)) if last == n => page.push_str(text),
            _ => pages.push((*n, text.clone())),
        }
    }
    pages
        .iter()
        .map(|(_, page)| page.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|page| !page.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The child's side: set the limits, read the document from stdin, write the chunks as JSON. `path`
/// is only for its extension.
pub fn child_main(path: &Path, from: usize, count: usize) -> Result<()> {
    limit(libc::RLIMIT_AS, CHILD_ADDRESS_SPACE)?;
    limit(libc::RLIMIT_CPU, CHILD_CPU_SECONDS)?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!("over {MAX_DOCUMENT_BYTES} bytes");
    }
    let out = parts(path, &bytes, from, count)?;
    serde_json::to_writer(std::io::stdout().lock(), &out)?;
    Ok(())
}

fn limit(resource: libc::__rlimit_resource_t, value: u64) -> Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: setrlimit reads one rlimit through a valid pointer.
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(std::io::Error::last_os_error()).context("setting a resource limit");
    }
    Ok(())
}

pub const EXTENSIONS: &[&str] = &[
    "pdf", "docx", "docm", "dotx", "dotm", "odt", "ott", "fodt", "pptx", "pptm", "potx", "ppsx",
    "ppsm", "odp", "otp", "fodp", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "xls", "ods", "ots",
    "csv", "tsv", "epub", "srt", "vtt", "doc", "dot", "ppt", "pot", "pps", "rtf",
];

/// Word's macro and template files keep the text where a .docx does.
const WORD: &[&str] = &["docx", "docm", "dotx", "dotm"];
const SLIDES: &[&str] = &["pptx", "pptm", "potx", "ppsx", "ppsm"];
/// calamine tells these apart by their content.
const SHEETS: &[&str] = &["xlsx", "xlsm", "xlsb", "xltx", "xltm", "xls", "ods", "ots"];

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub fn is_document(path: &Path) -> bool {
    EXTENSIONS.contains(&ext(path).as_str())
}

pub fn is_session(path: &Path) -> bool {
    ext(path) == session::EXTENSION
}

pub fn unit(path: &Path) -> &'static str {
    match ext(path).as_str() {
        "pdf" => "page",
        "odp" | "otp" | "fodp" | "ppt" | "pot" | "pps" => "slide",
        e if SLIDES.contains(&e) => "slide",
        "csv" | "tsv" => "row",
        e if SHEETS.contains(&e) => "row",
        "epub" => "chapter",
        "srt" | "vtt" => "line",
        _ => "paragraph",
    }
}

fn paged(path: &Path) -> bool {
    matches!(unit(path), "page" | "slide" | "chapter")
}

/// What a part of `path` is, for "120 of 640 pages": a page, slide, chapter or "part".
pub fn part_unit(path: &Path) -> &'static str {
    if is_document(path) && paged(path) {
        unit(path)
    } else {
        "part"
    }
}

/// The chunks of `path`'s parts `from..from + count`, and how many parts it has.
/// Only a PDF's pages are read a run at a time; other formats are parsed whole (a
/// fraction of the time) and the run picked out.
pub fn parts(path: &Path, bytes: &[u8], from: usize, count: usize) -> Result<Extract> {
    if ext(path) == "pdf" {
        return pdf(bytes, from, count);
    }
    Ok(select(chunks(path, bytes)?, paged(path), from, count))
}

/// `paged` chunks are numbered by part from 1; others go PART_CHUNKS to a part.
pub fn select(chunks: Vec<(usize, String)>, paged: bool, from: usize, count: usize) -> Extract {
    let end = from.saturating_add(count);
    if paged {
        let parts = chunks.last().map_or(0, |(n, _)| *n);
        let chunks = chunks
            .into_iter()
            .filter(|(n, _)| (from + 1..=end).contains(n))
            .collect();
        return Extract { parts, chunks };
    }
    Extract {
        parts: chunks.len().div_ceil(PART_CHUNKS),
        chunks: chunks
            .into_iter()
            .skip(from.saturating_mul(PART_CHUNKS))
            .take(count.saturating_mul(PART_CHUNKS))
            .collect(),
    }
}

pub fn chunks(path: &Path, bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    if bytes.starts_with(b"PK") {
        check_zip(bytes)?;
    }
    match ext(path).as_str() {
        "pdf" => Ok(pdf(bytes, 0, usize::MAX)?.chunks),
        e if WORD.contains(&e) => Ok(chunk(&xml_text(
            &part(bytes, "word/document.xml")?,
            &Tags::DOCX,
        ))),
        "odt" | "ott" => Ok(chunk(&xml_text(&part(bytes, "content.xml")?, &Tags::ODF))),
        "fodt" => Ok(chunk(&xml_text(utf8(bytes)?, &Tags::ODF))),
        e if SLIDES.contains(&e) => pptx(bytes),
        "odp" | "otp" => Ok(numbered(
            xml_text(&part(bytes, "content.xml")?, &Tags::ODF).split('\u{c}'),
        )),
        "fodp" => Ok(numbered(xml_text(utf8(bytes)?, &Tags::ODF).split('\u{c}'))),
        "epub" => epub(bytes),
        session::EXTENSION => session::chunks(bytes),
        "doc" | "dot" => legacy_doc(bytes),
        "ppt" | "pot" | "pps" => legacy_ppt(bytes),
        "rtf" => Ok(chunk(&rtf::rtf(bytes)?)),
        "srt" | "vtt" => Ok(chunk(&subtitles(&String::from_utf8_lossy(bytes)))),
        "csv" | "tsv" => {
            let text = std::str::from_utf8(bytes).context("not UTF-8")?;
            let mut lines = text.lines();
            let header = lines.next().unwrap_or_default();
            Ok(table(header, lines.map(str::to_owned), 2))
        }
        _ => sheets(bytes),
    }
}

/// A Word 97 .doc, or an RTF or renamed .docx (pinned by
/// `tests::an_rtf_file_saved_as_doc_reads_as_rtf`,
/// `tests::a_docx_renamed_to_doc_reads_as_a_docx`).
fn legacy_doc(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    if bytes.starts_with(b"{\\rtf") {
        Ok(chunk(&rtf::rtf(bytes)?))
    } else if bytes.starts_with(b"PK") {
        chunks(Path::new("a.docx"), bytes)
    } else {
        Ok(chunk(&ole::doc(bytes)?))
    }
}

/// A PowerPoint 97 .ppt, or a renamed .pptx (pinned by
/// `tests::a_pptx_renamed_to_ppt_reads_as_a_pptx`).
fn legacy_ppt(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    if bytes.starts_with(b"PK") {
        chunks(Path::new("a.pptx"), bytes)
    } else {
        Ok(numbered(ole::ppt(bytes)?.iter()))
    }
}

fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("not UTF-8")
}

/// Each chapter in reading order. rbook reads zip entries without a limit; the size check in
/// `chunks` has already run.
fn epub(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let book = rbook::Epub::read(Cursor::new(bytes.to_vec()))?;
    let mut reader = book.reader();
    let mut chapters = Vec::new();
    while let Some(chapter) = reader.read_next() {
        chapters.push(xml_text(chapter?.content(), &Tags::XHTML));
    }
    Ok(numbered(chapters.iter()))
}

/// An SRT or WebVTT file's speech: cue numbers, ids, timings, VTT's header and NOTE/STYLE blocks
/// and inline tags left out.
fn subtitles(text: &str) -> String {
    let mut out = String::new();
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .peekable();
    let mut skipping = false;
    while let Some(line) = lines.next() {
        if line.is_empty() {
            skipping = false;
            continue;
        }
        let block = ["WEBVTT", "NOTE", "STYLE", "REGION"]
            .iter()
            .any(|word| line.split_whitespace().next() == Some(word));
        let id = lines.peek().is_some_and(|next| next.contains("-->"));
        skipping |= block;
        if skipping || id || line.contains("-->") {
            continue;
        }
        let mut depth = 0;
        for c in line.chars() {
            match c {
                '<' | '{' => depth += 1,
                '>' | '}' if depth > 0 => depth -= 1,
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out.push('\n');
    }
    out
}

const HTML: &[&str] = &["html", "htm"];

pub fn is_html(path: &Path) -> bool {
    HTML.contains(&ext(path).as_str())
}

fn numbered<S: AsRef<str>>(parts: impl Iterator<Item = S>) -> Vec<(usize, String)> {
    parts
        .enumerate()
        .flat_map(|(i, p)| chunk(p.as_ref()).into_iter().map(move |(_, c)| (i + 1, c)))
        .collect()
}

/// Each slide in order; MAX_PART_BYTES caps the slides together, not each.
fn pptx(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    let slide = |name: &str| {
        name.strip_prefix("ppt/slides/slide")?
            .strip_suffix(".xml")?
            .parse::<u32>()
            .ok()
    };
    let mut slides: Vec<(u32, String)> = zip
        .file_names()
        .filter_map(|n| n.ok())
        .filter_map(|n| Some((slide(&n)?, n.into_owned())))
        .collect();
    slides.sort();
    let mut total = 0;
    let mut texts = Vec::with_capacity(slides.len());
    for (_, n) in &slides {
        let xml = read(&mut zip, n)?;
        total += xml.len() as u64;
        if total > MAX_PART_BYTES {
            bail!("slides are over {MAX_PART_BYTES} bytes");
        }
        texts.push(xml_text(&xml, &Tags::PPTX));
    }
    Ok(numbered(texts.iter()))
}

fn sheets(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    use calamine::Reader;
    let mut book = calamine::open_workbook_auto_from_rs(Cursor::new(bytes))?;
    let mut out = Vec::new();
    for name in book.sheet_names() {
        let Ok(range) = book.worksheet_range(&name) else {
            continue;
        };
        let mut rows = range.rows().map(|r| {
            r.iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("\t")
        });
        let header = format!("{name}\n{}", rows.next().unwrap_or_default());
        out.extend(table(&header, rows, 2));
    }
    Ok(out)
}

/// Rows packed into chunks that each start with `header`, so the model sees what the columns are;
/// numbered by first row.
fn table(header: &str, rows: impl Iterator<Item = String>, first: usize) -> Vec<(usize, String)> {
    let header = cut(header, CHUNK_CHARS / 2);
    let budget = CHUNK_CHARS - header.len() - 1;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = first;
    for (i, row) in rows.enumerate() {
        let row = cut(&row, budget - 1);
        if !cur.is_empty() && cur.len() + row.len() + 1 > budget {
            out.push((start, format!("{header}\n{cur}")));
            cur.clear();
        }
        if cur.is_empty() {
            start = first + i;
        }
        cur.push_str(row);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push((start, format!("{header}\n{cur}")));
    }
    out
}

fn cut(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Zips that say they unpack past this, or hold more entries, aren't opened:
/// their text would be over MAX_TEXT_BYTES anyway, and calamine has no limit. A zip
/// can lie about its sizes, so this stops accidents and naive zip bombs, not a crafted one.
const MAX_UNZIPPED_BYTES: u64 = 256 << 20;
const MAX_ZIP_ENTRIES: usize = 10_000;

fn check_zip(bytes: &[u8]) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))?;
    if zip.len() > MAX_ZIP_ENTRIES {
        bail!("{} zip entries", zip.len());
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        total = total.saturating_add(zip.by_index_raw(i)?.size());
    }
    if total > MAX_UNZIPPED_BYTES {
        bail!("unzips to {total} bytes");
    }
    Ok(())
}

fn part(bytes: &[u8], name: &str) -> Result<String> {
    read(&mut zip::ZipArchive::new(Cursor::new(bytes))?, name)
}

/// Parts past this are skipped: a zip bomb's XML can unpack to gigabytes.
const MAX_PART_BYTES: u64 = 64 << 20;

fn read(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<String> {
    let mut s = String::new();
    zip.by_name(name)?
        .take(MAX_PART_BYTES + 1)
        .read_to_string(&mut s)?;
    if s.len() as u64 > MAX_PART_BYTES {
        bail!("{name} is over {MAX_PART_BYTES} bytes");
    }
    Ok(s)
}

struct Tags {
    text: &'static str,
    paragraph: &'static [&'static str],
    tab: &'static [&'static str],
    line_break: &'static [&'static str],
    page: &'static str,
    skip: &'static [&'static str],
}

impl Tags {
    const DOCX: Tags = Tags {
        text: "w:t",
        paragraph: &["w:p"],
        tab: &["w:tab"],
        line_break: &["w:br", "w:cr"],
        page: "",
        skip: &[],
    };
    const PPTX: Tags = Tags {
        text: "a:t",
        paragraph: &["a:p"],
        tab: &[],
        line_break: &["a:br"],
        page: "",
        skip: &[],
    };
    const ODF: Tags = Tags {
        text: "",
        paragraph: &["text:p", "text:h"],
        tab: &["text:tab"],
        line_break: &["text:line-break"],
        page: "draw:page",
        skip: &[],
    };
    const XHTML: Tags = Tags {
        text: "",
        paragraph: &[
            "p",
            "div",
            "li",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "tr",
            "dt",
            "dd",
            "blockquote",
            "pre",
            "figcaption",
        ],
        tab: &["td", "th"],
        line_break: &["br"],
        page: "",
        skip: &["head", "script", "style"],
    };
}

impl Tags {
    /// The whitespace an opening or empty element stands for. ODF's run of spaces
    /// `<text:s text:c="3"/>` is one: enough to keep words apart (pinned by
    /// `tests::an_odf_run_of_spaces_reads_as_one_space`).
    fn mark(&self, out: &mut String, name: &str) {
        if self.tab.contains(&name) {
            out.push('\t');
        } else if self.line_break.contains(&name) {
            out.push('\n');
        } else if name == "text:s" {
            out.push(' ');
        }
    }
}

fn xml_text(xml: &str, tags: &Tags) -> String {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = tags.text.is_empty();
    let mut skipped = 0u32;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let name = name.into_inner();
                if name == tags.text {
                    in_text = true;
                }
                if tags.skip.contains(&name) {
                    skipped += 1;
                }
                tags.mark(&mut out, name);
            }
            Ok(Event::Empty(e)) => tags.mark(&mut out, e.name().into_inner()),
            Ok(Event::End(e)) => {
                let name = e.name();
                let name = name.into_inner();
                if !tags.text.is_empty() && name == tags.text {
                    in_text = false;
                }
                if tags.skip.contains(&name) {
                    skipped = skipped.saturating_sub(1);
                }
                if tags.paragraph.contains(&name) {
                    out.push('\n');
                }
                if !tags.page.is_empty() && name == tags.page {
                    out.push('\u{c}');
                }
            }
            Ok(Event::Text(t)) if in_text && skipped == 0 => out.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) if in_text && skipped == 0 => match r.resolve_char_ref() {
                Ok(Some(c)) => out.push(c),
                _ => out.push_str(entity(&r).unwrap_or("")),
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::pdf::*;
    use super::*;
    use std::io::Write;

    fn zipped(files: &[(&str, &str)]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn docx_keeps_paragraphs_and_entities() {
        let xml = r#"<w:document><w:body>
            <w:p><w:r><w:t>Fish &amp; chips</w:t></w:r><w:r><w:tab/><w:t>£4</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second</w:t></w:r></w:p>
            <w:sectPr>ignored</w:sectPr></w:body></w:document>"#;
        let doc = zipped(&[("word/document.xml", xml)]);
        let c = chunks(Path::new("a.docx"), &doc).unwrap();
        assert_eq!(c, vec![(1, "Fish & chips\t£4\nSecond\n".to_owned())]);
    }

    #[test]
    fn slides_are_numbered_in_slide_order() {
        let slide = |t: &str| format!("<p:sld><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>");
        let doc = zipped(&[
            ("ppt/slides/slide10.xml", &slide("ten")),
            ("ppt/slides/slide2.xml", &slide("two")),
            ("ppt/slides/_rels/slide2.xml.rels", "<r/>"),
        ]);
        let c = chunks(Path::new("a.pptx"), &doc).unwrap();
        assert_eq!(c, vec![(1, "two\n".to_owned()), (2, "ten\n".to_owned())]);
    }

    #[test]
    fn odp_splits_on_pages() {
        let xml = "<office:document-content><draw:page><text:p>one</text:p></draw:page>\
                   <draw:page><text:p>two<text:s/>words</text:p></draw:page></office:document-content>";
        let doc = zipped(&[("content.xml", xml)]);
        let c = chunks(Path::new("a.odp"), &doc).unwrap();
        assert_eq!(
            c,
            vec![(1, "one\n".to_owned()), (2, "two words\n".to_owned())]
        );
    }

    #[test]
    fn every_csv_chunk_starts_with_the_header() {
        let rows: Vec<String> = (0..200).map(|i| format!("{i},payee {i},12.34")).collect();
        let csv = format!("n,payee,amount\n{}", rows.join("\n"));
        let c = chunks(Path::new("a.csv"), csv.as_bytes()).unwrap();
        assert!(c.len() > 1);
        assert_eq!(c[0].0, 2);
        for (row, text) in &c {
            assert!(text.starts_with("n,payee,amount\n"));
            assert!(text.len() <= CHUNK_CHARS);
            let first = text.lines().nth(1).unwrap();
            assert_eq!(
                csv.lines().nth(row - 1),
                Some(first),
                "numbered by the file line its first row is on"
            );
        }
    }

    #[test]
    fn joined_chunks_give_back_the_text() {
        let long = "word ".repeat(CHUNK_CHARS);
        let xml = format!(
            "<w:document><w:body><w:p><w:r><w:t>{long}</w:t></w:r></w:p>\
             <w:p><w:r><w:t>after</w:t></w:r></w:p></w:body></w:document>"
        );
        let docx = Path::new("a.docx");
        let c = chunks(docx, &zipped(&[("word/document.xml", &xml)])).unwrap();
        assert!(c.len() > 2);
        assert_eq!(join(docx, c), format!("{long}\nafter\n"));

        let rows: Vec<String> = (0..200).map(|i| format!("{i},payee {i},12.34")).collect();
        let csv = format!("n,payee,amount\n{}\n", rows.join("\n"));
        let path = Path::new("a.csv");
        let c = chunks(path, csv.as_bytes()).unwrap();
        assert!(c.len() > 1);
        assert_eq!(join(path, c), csv);
    }

    fn type3_pdf(codes: &[u8], unicode: &str) -> Vec<u8> {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let mut cmap = String::from(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
             1 begincodespacerange\n<00> <FF>\nendcodespacerange\n",
        );
        cmap.push_str(&format!("{} beginbfchar\n", codes.len()));
        for (code, ch) in codes.iter().zip(unicode.chars()) {
            cmap.push_str(&format!("<{code:02X}> <{:04X}>\n", ch as u32));
        }
        cmap.push_str(
            "endbfchar\nendcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
        );
        let to_unicode = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
        let names: Vec<Object> = codes
            .iter()
            .map(|c| Object::Name(format!("g{c}").into_bytes()))
            .collect();
        let mut differences = vec![Object::Integer(i64::from(codes[0]))];
        differences.extend(names);
        let font = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type3",
            "FontBBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
            "FontMatrix" => vec![0.001.into(), 0.into(), 0.into(), 0.001.into(), 0.into(), 0.into()],
            "CharProcs" => dictionary! {},
            "Encoding" => dictionary! { "Type" => "Encoding", "Differences" => differences },
            "FirstChar" => i64::from(codes[0]),
            "LastChar" => i64::from(codes[codes.len() - 1]),
            "Widths" => codes.iter().map(|_| Object::Integer(500)).collect::<Vec<_>>(),
            "ToUnicode" => to_unicode,
        });
        let mut content = b"BT /F1 12 Tf 72 720 Td (".to_vec();
        for c in codes {
            content.extend(format!("\\{c:03o}").bytes());
        }
        content.extend(b") Tj ET");
        let content = doc.add_object(Stream::new(dictionary! {}, content));
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn text_pdf(pages: &[String]) -> Vec<u8> {
        use lopdf::{Document, Object, Stream, dictionary};
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let font = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let kids: Vec<Object> = pages
            .iter()
            .map(|text| {
                let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
                let content = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
                doc.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "Contents" => content,
                    "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
                    "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
                })
                .into()
            })
            .collect();
        let count = kids.len() as i64;
        doc.objects.insert(
            pages_id,
            dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }.into(),
        );
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    fn seven_page_pdf() -> Vec<u8> {
        let pages: Vec<String> = ["one", "two", "three", "four", "five", "six", "seven"]
            .iter()
            .map(|n| format!("This is page {n}"))
            .collect();
        text_pdf(&pages)
    }

    #[test]
    fn a_pdf_is_read_a_run_of_pages_at_a_time() {
        let run = parts(Path::new("a.pdf"), &seven_page_pdf(), 2, 3).unwrap();
        assert_eq!(run.parts, 7);
        let numbers: Vec<usize> = run.chunks.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, [3, 4, 5]);
        assert!(run.chunks[0].1.contains("This is page three"), "{run:?}");
    }

    #[test]
    fn a_run_past_the_last_page_is_empty_but_still_counts_the_pages() {
        let past = parts(Path::new("a.pdf"), &seven_page_pdf(), 7, 30).unwrap();
        assert_eq!((past.parts, past.chunks.len()), (7, 0));
    }

    #[test]
    fn a_whole_pdf_reads_every_page() {
        assert_eq!(
            chunks(Path::new("a.pdf"), &seven_page_pdf()).unwrap().len(),
            7
        );
    }

    fn word(n: usize) -> &'static str {
        [
            "", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ][n]
    }

    fn without_furniture(pages: &[&str]) -> Vec<String> {
        let mut pages: Vec<(usize, String)> = pages
            .iter()
            .enumerate()
            .map(|(i, p)| (i + 1, (*p).to_owned()))
            .collect();
        drop_furniture(&mut pages);
        pages.into_iter().map(|(_, p)| p).collect()
    }

    #[test]
    fn a_running_header_goes_with_its_page_number() {
        let pages: Vec<String> = (1..=5)
            .map(|n| format!("Annual Report 2025  {n}\nBody of page {}.\n", word(n)))
            .collect();
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let want: Vec<String> = (1..=5)
            .map(|n| format!("Body of page {}.", word(n)))
            .collect();
        assert_eq!(without_furniture(&pages), want);
    }

    #[test]
    fn left_and_right_headers_go_though_each_is_on_half_the_pages() {
        let pages: Vec<String> = (1..=6)
            .map(|n| {
                if n % 2 == 1 {
                    format!("Field Notes on Soil\nPage {} text.\n{n}", word(n))
                } else {
                    format!("{n}\nChapter 2: Clay\nPage {} text.", word(n))
                }
            })
            .collect();
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let want: Vec<String> = (1..=6).map(|n| format!("Page {} text.", word(n))).collect();
        assert_eq!(without_furniture(&pages), want);
    }

    #[test]
    fn a_line_in_a_slot_on_under_half_the_pages_stays() {
        let opens = [1, 2, 5, 6];
        let pages: Vec<String> = (1..=10)
            .map(|n| {
                let first = if opens.contains(&n) {
                    "Water the beds at dawn.".to_owned()
                } else {
                    format!("Page {} begins here.", word(n))
                };
                format!("{first}\nThe rest of page {}.\nExample Press", word(n))
            })
            .collect();
        let pages: Vec<&str> = pages.iter().map(String::as_str).collect();
        let out = without_furniture(&pages);
        assert_eq!(
            out[0], "Water the beds at dawn.\nThe rest of page one.",
            "the opening line is on 4 of 10 pages, two odd and two even: it's the text"
        );
        assert_eq!(out[2], "Page three begins here.\nThe rest of page three.");
        assert!(
            out.iter().all(|p| !p.contains("Example Press")),
            "the footer is on every page: {out:?}"
        );
    }

    #[test]
    fn two_pages_lose_only_their_page_numbers() {
        let out = without_furniture(&[
            "Annual Report\nFirst page.\nPage 1 of 2",
            "Annual Report\nSecond page.\n2",
        ]);
        assert_eq!(
            out,
            ["Annual Report\nFirst page.", "Annual Report\nSecond page."]
        );
    }

    #[test]
    fn a_number_in_the_body_is_not_a_page_number() {
        let page = "alpha\nbravo\ncharlie\n42\ndelta\necho\nfoxtrot";
        assert_eq!(without_furniture(&[page]), [page]);
    }

    #[test]
    fn page_numbers_are_told_from_text() {
        for line in [
            "3",
            " 12 ",
            "Page 3",
            "page 3 of 40",
            "PAGE 3 OF 40",
            "3/40",
            "3 / 40",
            "iv",
            "XII",
            "xlii",
        ] {
            assert!(page_number(&blanked(line)), "{line}");
        }
        for line in [
            "3 apples",
            "Page",
            "Page three",
            "page3",
            "Chapter 3",
            "3 of",
            "iiii",
            "Mix",
            "dim",
            "Vic",
            "",
        ] {
            assert!(!page_number(&blanked(line)), "{line}");
        }
        assert_eq!(blanked("  Page 3   of 40 "), "Page # of #");
        assert_eq!(blanked("v2.10"), "v#.#");
    }

    const FURNITURE: &str = "tests/fixtures/furniture.pdf";
    const FURNITURE_PDF: &[u8] = include_bytes!("../tests/fixtures/furniture.pdf");

    #[test]
    fn a_pdf_reads_without_its_headers_footers_and_page_numbers() {
        let path = Path::new(FURNITURE);
        assert_eq!(
            join(path, chunks(path, FURNITURE_PDF).unwrap()),
            "Clay holds water well but drains slowly after rain. \
             Adding compost each autumn loosens it over a few years.\n\n\
             Sandy beds dry out within a day of watering. \
             A mulch of leaves keeps the top layer damp in summer.\n\n\
             Earthworms were counted in a square of turned soil. \
             The composted bed had twice as many as the bare one.\n\n\
             Test the pH before liming, since most vegetables like it near neutral. \
             These notes will be updated after the next season."
        );
    }

    #[test]
    fn a_run_of_two_pages_keeps_the_header_but_not_the_page_number() {
        let run = parts(Path::new(FURNITURE), FURNITURE_PDF, 2, 2).unwrap();
        assert!(
            run.chunks[0].1.starts_with("Field Notes on Garden Soil\n"),
            "{run:?}"
        );
        assert!(
            run.chunks.iter().all(|(_, c)| !c.contains("Page ")),
            "{run:?}"
        );
    }

    #[test]
    fn paged_parts_are_picked_by_their_number() {
        let c = |n: usize, t: &str| (n, t.to_owned());
        let all = vec![c(1, "a"), c(1, "b"), c(2, "c"), c(4, "d")];
        let run = select(all.clone(), true, 1, 2);
        assert_eq!(run.parts, 4);
        assert_eq!(run.chunks, [c(2, "c")]);
        assert_eq!(
            select(all.clone(), true, 0, 1).chunks,
            [c(1, "a"), c(1, "b")]
        );
        assert_eq!(select(all.clone(), true, 3, 30).chunks, [c(4, "d")]);
        assert_eq!(select(all.clone(), true, 0, usize::MAX).chunks, all);
        assert_eq!(select(Vec::new(), true, 0, 30), Extract::default());
    }

    #[test]
    fn parts_without_pages_are_runs_of_chunks() {
        let all: Vec<(usize, String)> = (1..=10).map(|i| (i * 4, format!("c{i}"))).collect();
        let run = select(all.clone(), false, 1, 2);
        assert_eq!(run.parts, 10usize.div_ceil(PART_CHUNKS));
        assert_eq!(run.chunks, all[PART_CHUNKS..3 * PART_CHUNKS]);
        assert_eq!(
            select(all.clone(), false, 3, 30).chunks,
            all[3 * PART_CHUNKS..]
        );
        assert_eq!(select(all.clone(), false, 0, usize::MAX).chunks, all);
        assert!(select(all, false, 30, 30).chunks.is_empty());
    }

    #[test]
    fn a_part_is_a_page_slide_or_chapter_where_there_are_those() {
        for (name, unit) in [
            ("a.pdf", "page"),
            ("a.pptx", "slide"),
            ("a.odp", "slide"),
            ("a.epub", "chapter"),
            ("a.docx", "part"),
            ("a.csv", "part"),
            ("a.srt", "part"),
            ("a.txt", "part"),
            ("a.md", "part"),
        ] {
            assert_eq!(part_unit(Path::new(name)), unit, "{name}");
        }
    }

    #[test]
    fn a_type3_font_is_read_through_its_to_unicode_map() {
        let codes: Vec<u8> = (3..=11).collect();
        let pdf = type3_pdf(&codes, "OpenResto");
        let c = chunks(Path::new("a.pdf"), &pdf).unwrap();
        assert_eq!(join(Path::new("a.pdf"), c), "OpenResto");
    }

    #[test]
    fn glyph_codes_are_not_text() {
        assert!(!readable("--\"+!_,+)6!*&+ 1&3&161/&)U,4+\"/_"));
        assert!(readable("OpenResto: simple, secure, self hosted."));
        assert!(readable("fn main() { println!(\"{}\", x[0]); }"));
        assert!(readable("Total: $1,234.56 (2026-10-23)"));
        assert!(readable(
            "1. Introduction : : : : : : : : : : : : : : : : : : 1"
        ));
        assert!(readable(
            "Code Pages ..................................... 4"
        ));
        assert!(readable(""));
    }

    #[test]
    fn pages_and_sheets_are_joined_with_a_blank_line() {
        let page = |n: usize, t: &str| (n, t.to_owned());
        assert_eq!(
            join(
                Path::new("a.pdf"),
                vec![page(1, "one "), page(1, "more\n"), page(3, "three\n")]
            ),
            "one more\n\nthree"
        );
        assert_eq!(
            join(
                Path::new("a.pdf"),
                vec![
                    page(1, "Commission\n \n-\n \nLicense\n \n"),
                    page(1, "Agreement\n")
                ]
            ),
            "Commission - License Agreement"
        );
        assert_eq!(
            join(
                Path::new("a.ods"),
                vec![
                    page(2, "S1\nh\nr1\n"),
                    page(3, "S1\nh\nr2\n"),
                    page(2, "S2\nh\nq\n"),
                ]
            ),
            "S1\nh\nr1\nr2\n\nS2\nh\nq\n"
        );
        let slide = |t: &str| format!("<p:sld><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>");
        let pptx = Path::new("a.pptx");
        let doc = zipped(&[
            ("ppt/slides/slide1.xml", &slide("one")),
            ("ppt/slides/slide2.xml", &slide("two")),
        ]);
        assert_eq!(join(pptx, chunks(pptx, &doc).unwrap()), "one\n\ntwo\n");
    }

    #[test]
    fn zips_that_unpack_too_far_are_not_opened() {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        w.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..(MAX_UNZIPPED_BYTES >> 20) + 1 {
            w.write_all(&zeros).unwrap();
        }
        let bomb = w.finish().unwrap().into_inner();
        assert!(bomb.len() < 1 << 20);
        let e = chunks(Path::new("a.xlsx"), &bomb).unwrap_err();
        assert!(e.to_string().starts_with("unzips to"), "{e}");

        let names: Vec<(String, &str)> = (0..=MAX_ZIP_ENTRIES)
            .map(|i| (format!("f{i}"), ""))
            .collect();
        let many = zipped(
            &names
                .iter()
                .map(|(n, b)| (n.as_str(), *b))
                .collect::<Vec<_>>(),
        );
        assert!(chunks(Path::new("a.docx"), &many).is_err());
    }

    fn epub_of(chapters: &[(&str, &str)]) -> Vec<u8> {
        let container = r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#;
        let mut manifest = String::new();
        let mut spine = String::new();
        for (i, (name, _)) in chapters.iter().enumerate() {
            manifest.push_str(&format!(
                r#"<item id="c{i}" href="{name}" media-type="application/xhtml+xml"/>"#
            ));
            spine.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
        }
        let opf = format!(
            r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="id">urn:uuid:1</dc:identifier><dc:title>T</dc:title><dc:language>en</dc:language>
  </metadata>
  <manifest>{manifest}</manifest>
  <spine>{spine}</spine>
</package>"#
        );
        let mut files = vec![
            ("mimetype", "application/epub+zip".to_owned()),
            ("META-INF/container.xml", container.to_owned()),
            ("OEBPS/content.opf", opf),
        ];
        for (name, body) in chapters {
            files.push((
                Box::leak(format!("OEBPS/{name}").into_boxed_str()),
                format!(
                    "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\">\
                     <head><title>skip me</title><style>p {{ color: red }}</style></head>\
                     <body>{body}</body></html>"
                ),
            ));
        }
        let files: Vec<(&str, &str)> = files.iter().map(|(n, b)| (*n, b.as_str())).collect();
        zipped(&files)
    }

    #[test]
    fn epub_chapters_are_numbered_in_reading_order() {
        let book = epub_of(&[
            ("b.xhtml", "<h1>Two</h1><p>Fish&#160;&amp; chips</p>"),
            ("a.xhtml", "<p>Second<br/>line</p><script>var x;</script>"),
        ]);
        let path = Path::new("a.epub");
        let c = chunks(path, &book).unwrap();
        assert_eq!(
            c,
            vec![
                (1, "Two\nFish\u{a0}& chips\n".to_owned()),
                (2, "Second\nline\n".to_owned())
            ],
            "listed out of name order: the spine decides"
        );
        assert_eq!(unit(path), "chapter");
        assert_eq!(join(path, c), "Two\nFish\u{a0}& chips\n\nSecond\nline\n");
    }

    #[test]
    fn office_variants_read_like_their_base_format() {
        let word = zipped(&[(
            "word/document.xml",
            "<w:document><w:body><w:p><w:r><w:t>macro doc</w:t></w:r></w:p></w:body></w:document>",
        )]);
        for e in ["docm", "dotx", "dotm"] {
            let c = chunks(Path::new(&format!("a.{e}")), &word).unwrap();
            assert_eq!(c, vec![(1, "macro doc\n".to_owned())], "{e}");
        }
        let slides = zipped(&[(
            "ppt/slides/slide1.xml",
            "<p:sld><a:p><a:r><a:t>show</a:t></a:r></a:p></p:sld>",
        )]);
        for e in ["pptm", "potx", "ppsx", "ppsm"] {
            let path = format!("a.{e}");
            assert_eq!(
                chunks(Path::new(&path), &slides).unwrap(),
                vec![(1, "show\n".to_owned())]
            );
            assert_eq!(unit(Path::new(&path)), "slide");
        }
        let odf = "<office:document-content><office:body><office:text>\
                   <text:p>flat</text:p></office:text></office:body></office:document-content>";
        assert_eq!(
            chunks(Path::new("a.ott"), &zipped(&[("content.xml", odf)])).unwrap(),
            vec![(1, "flat\n".to_owned())]
        );
        assert_eq!(
            chunks(Path::new("a.fodt"), odf.as_bytes()).unwrap(),
            vec![(1, "flat\n".to_owned())]
        );
        let pages = "<office:document><draw:page><text:p>one</text:p></draw:page>\
                     <draw:page><text:p>two</text:p></draw:page></office:document>";
        assert_eq!(
            chunks(Path::new("a.fodp"), pages.as_bytes()).unwrap(),
            vec![(1, "one\n".to_owned()), (2, "two\n".to_owned())]
        );
        assert_eq!(
            chunks(Path::new("a.otp"), &zipped(&[("content.xml", pages)])).unwrap(),
            vec![(1, "one\n".to_owned()), (2, "two\n".to_owned())]
        );
    }

    #[test]
    fn an_ods_template_is_a_spreadsheet() {
        let content = r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet>
<table:table table:name="Budget"><table:table-row><table:table-cell office:value-type="string"><text:p>payee</text:p></table:table-cell></table:table-row>
<table:table-row><table:table-cell office:value-type="string"><text:p>rent</text:p></table:table-cell></table:table-row></table:table>
</office:spreadsheet></office:body></office:document-content>"#;
        let ots = zipped(&[
            (
                "mimetype",
                "application/vnd.oasis.opendocument.spreadsheet-template",
            ),
            ("META-INF/manifest.xml", "<manifest:manifest/>"),
            ("content.xml", content),
        ]);
        let c = chunks(Path::new("a.ots"), &ots).unwrap();
        assert_eq!(c.len(), 1);
        assert!(c[0].1.contains("payee") && c[0].1.contains("rent"), "{c:?}");
        assert_eq!(unit(Path::new("a.ots")), "row");
    }

    #[test]
    fn subtitles_keep_only_what_is_said() {
        let srt = "\u{feff}1\n00:00:01,000 --> 00:00:02,500\n<i>Where were you</i>\nlast night?\n\n\
                   2\n00:00:03,000 --> 00:00:04,000\n{\\an8}At the restaurant.\n";
        assert_eq!(
            subtitles(srt),
            "Where were you\nlast night?\nAt the restaurant.\n"
        );
        let vtt = "WEBVTT - a title\n\nNOTE written by hand\nstill the note\n\n\
                   STYLE\n::cue { color: red }\n\nintro\n00:01.000 --> 00:02.000 align:start\n\
                   <v Roger>Hello there.\n\n00:03.000 --> 00:04.000\nGoodbye.\n";
        assert_eq!(subtitles(vtt), "Hello there.\nGoodbye.\n");
        assert_eq!(unit(Path::new("a.vtt")), "line");
    }

    #[test]
    fn html_keeps_the_text_on_its_own_lines() {
        let page = "<!DOCTYPE html>\n<html><head><title>Fish &amp; chips</title>\n\
                    <style>\nbody { color: red }\n</style>\n<script>if (a < b) {}</script>\n\
                    </head>\n<body><!-- a\ncomment -->\n<p class=\"a>b\">Cod <b>and</b> \
                    <i>ch</i>ips&nbsp;&#163;4 &#x2014; &bogus; 1 &lt; 2</p>\n\
                    <table><tr><td>one</td><td>two</td></tr></table><br/>a < b\n</body></html>\n";
        let text = html(page);
        assert_eq!(text.lines().count(), page.lines().count());
        assert_eq!(
            text.lines().filter(|l| !l.is_empty()).collect::<Vec<_>>(),
            [
                "Fish & chips",
                "Cod and chips £4 — &bogus; 1 < 2",
                "one two a < b"
            ]
        );
        assert_eq!(
            text.lines().position(|l| l.starts_with("Cod")),
            page.lines().position(|l| l.contains("Cod"))
        );
    }

    #[test]
    fn html_survives_broken_markup() {
        for page in [
            "<",
            "&",
            "<p",
            "<script>never closed",
            "<!-- never closed",
            "</",
            "<a href='x>",
        ] {
            let _ = html(page);
        }
        assert_eq!(html("<a title=don't>it</a>s"), "its");
        assert!(is_html(Path::new("A.HTM")) && !is_html(Path::new("a.md")));
    }

    #[test]
    fn a_megabyte_of_bare_ampersands_or_scripts_reads_in_linear_time() {
        for unit in ["&", "<script>a</script>"] {
            let page = unit.repeat((1 << 20) / unit.len());
            let t0 = std::time::Instant::now();
            html(&page);
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{unit}: {:?}, where linear is about 1 s on CI in a debug build",
                t0.elapsed()
            );
        }
    }

    fn ole_of(streams: &[(&str, &[u8])]) -> Vec<u8> {
        let mut file =
            cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new()))
                .unwrap();
        for (name, body) in streams {
            file.create_stream(name).unwrap().write_all(body).unwrap();
        }
        file.flush().unwrap();
        file.into_inner().into_inner()
    }

    fn doc_of(pieces: &[(&str, bool)], main: u32) -> Vec<u8> {
        const F_WHICH_TBL_STM_IS_1TABLE: u16 = 0x0200;
        let mut word = vec![0u8; 1024];
        word[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        word[2..4].copy_from_slice(&0xC1u16.to_le_bytes());
        word[0x0A..0x0C].copy_from_slice(&F_WHICH_TBL_STM_IS_1TABLE.to_le_bytes());
        word[32..34].copy_from_slice(&14u16.to_le_bytes());
        let lw = 34 + 28;
        word[lw..lw + 2].copy_from_slice(&22u16.to_le_bytes());
        word[lw + 2 + 12..lw + 2 + 16].copy_from_slice(&main.to_le_bytes());
        let fc_lcb = lw + 2 + 88;
        word[fc_lcb..fc_lcb + 2].copy_from_slice(&93u16.to_le_bytes());
        let mut cps = vec![0u32];
        let mut pcds = Vec::new();
        for (text, compressed) in pieces {
            let fc = word.len() as u32;
            let chars: Vec<u16> = text.encode_utf16().collect();
            if *compressed {
                let (bytes, _, bad) = encoding_rs::WINDOWS_1252.encode(text);
                assert!(!bad);
                word.extend_from_slice(&bytes);
                pcds.push((fc * 2) | 0x4000_0000);
            } else {
                word.extend(chars.iter().flat_map(|c| c.to_le_bytes()));
                pcds.push(fc);
            }
            cps.push(cps.last().unwrap() + chars.len() as u32);
        }
        let mut plc: Vec<u8> = cps.iter().flat_map(|c| c.to_le_bytes()).collect();
        for fc in pcds {
            plc.extend([0, 0]);
            plc.extend(fc.to_le_bytes());
            plc.extend([0, 0]);
        }
        const PRC_TO_STEP_OVER: [u8; 5] = [1, 2, 0, 0xAA, 0xBB];
        const PCDT: u8 = 2;
        let mut table = vec![0u8; 16];
        let fc_clx = table.len() as u32;
        table.extend(PRC_TO_STEP_OVER);
        table.push(PCDT);
        table.extend((plc.len() as u32).to_le_bytes());
        table.extend(&plc);
        let clx = fc_lcb + 2 + 33 * 8;
        word[clx..clx + 4].copy_from_slice(&fc_clx.to_le_bytes());
        word[clx + 4..clx + 8].copy_from_slice(&((table.len() as u32) - fc_clx).to_le_bytes());
        ole_of(&[("WordDocument", &word), ("1Table", &table)])
    }

    #[test]
    fn doc_reads_the_piece_table() {
        let first = "Fish & chips €4\r\
                     See \u{13} HYPERLINK \"http://x\" \u{14}the site\u{15} now\r";
        let second = "Ελληνικά\u{b}line\u{1}\r\
                      a\u{7}b\u{7}\u{7}\r\
                      \u{13}PAGE\u{15}non\u{1e}breaking\r";
        let main = (first.chars().count() + second.chars().count()) as u32;
        let doc = doc_of(
            &[(first, true), (second, false), ("a footnote\r", true)],
            main,
        );
        let path = Path::new("a.doc");
        assert_eq!(unit(path), "paragraph");
        assert_eq!(
            join(path, chunks(path, &doc).unwrap()),
            "Fish & chips €4\nSee the site now\nΕλληνικά\nline\na\tb\t\t\nnon-breaking\n"
        );
        let c = chunks(Path::new("a.dot"), &doc).unwrap();
        assert_eq!(c[0].0, 1);
    }

    fn ppt_record(kind: u16, instance: u16, body: &[u8]) -> Vec<u8> {
        let mut r = (instance << 4).to_le_bytes().to_vec();
        r.extend(kind.to_le_bytes());
        r.extend((body.len() as u32).to_le_bytes());
        r.extend(body);
        r
    }

    fn ppt_container(kind: u16, instance: u16, children: &[Vec<u8>]) -> Vec<u8> {
        let mut r = ppt_record(kind, instance, &children.concat());
        r[0] |= 0xF;
        r
    }

    fn chars_atom(text: &str) -> Vec<u8> {
        let body: Vec<u8> = text.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        ppt_record(0x0FA0, 0, &body)
    }

    fn slide_container(texts: &[&str]) -> Vec<u8> {
        let boxes: Vec<Vec<u8>> = texts
            .iter()
            .map(|t| ppt_container(0xF00D, 0, &[chars_atom(t)]))
            .collect();
        ppt_container(
            0x03EE,
            0,
            &[ppt_container(
                0x040C,
                0,
                &[ppt_container(0xF002, 0, &boxes)],
            )],
        )
    }

    /// Slides' placeholder texts in the slide list, and each slide's own in its container;
    /// an old slide version sits unreferenced as a fast save leaves it, so only the list can order
    /// them.
    fn ppt_of(slides: &[(&[&str], &[&str])]) -> Vec<u8> {
        let mut stream = slide_container(&["stale"]);
        let mut offsets = Vec::new();
        for (_, own) in slides {
            offsets.push(stream.len() as u32);
            stream.extend(slide_container(own));
        }
        let mut list = Vec::new();
        for (i, (listed, _)) in slides.iter().enumerate().rev() {
            let mut persist = (i as u32 + 2).to_le_bytes().to_vec();
            persist.extend([0; 16]);
            list.push(ppt_record(0x03F3, 0, &persist));
            for t in *listed {
                list.push(ppt_record(0x0F9F, 0, &[0; 4]));
                if t.is_ascii() {
                    list.push(ppt_record(0x0FA8, 0, t.as_bytes()));
                } else {
                    list.push(chars_atom(t));
                }
            }
        }
        let document_at = stream.len() as u32;
        stream.extend(ppt_container(
            0x03E8,
            0,
            &[
                ppt_record(0x03E9, 0, &[0; 40]),
                ppt_container(0x0FF0, 2, &[chars_atom("notes list")]),
                ppt_container(0x0FF0, 0, &list),
            ],
        ));
        let dir_at = stream.len() as u32;
        let mut dir = (1u32 | ((slides.len() as u32 + 1) << 20))
            .to_le_bytes()
            .to_vec();
        dir.extend(document_at.to_le_bytes());
        for o in offsets {
            dir.extend(o.to_le_bytes());
        }
        stream.extend(ppt_record(0x1772, 0, &dir));
        let edit_at = stream.len() as u32;
        let mut edit = vec![0u8; 28];
        edit[12..16].copy_from_slice(&dir_at.to_le_bytes());
        edit[16..20].copy_from_slice(&1u32.to_le_bytes());
        stream.extend(ppt_record(0x0FF5, 0, &edit));
        let mut user = ppt_record(0x0FF6, 0, &[0; 20]);
        user[8..12].copy_from_slice(&0x14u32.to_le_bytes());
        user[12..16].copy_from_slice(&0xE391_C05Fu32.to_le_bytes());
        user[16..20].copy_from_slice(&edit_at.to_le_bytes());
        ole_of(&[("PowerPoint Document", &stream), ("Current User", &user)])
    }

    #[test]
    fn ppt_slides_follow_the_slide_list() {
        let ppt = ppt_of(&[
            (&["Second title"], &["Second title", "a box"]),
            (&["First\rtitle", "Café"], &["Ünïcode box"]),
        ]);
        let path = Path::new("a.ppt");
        let c = chunks(path, &ppt).unwrap();
        assert_eq!(
            c,
            vec![
                (1, "First\ntitle\nCafé\nÜnïcode box\n".to_owned()),
                (2, "Second title\na box\n".to_owned()),
            ]
        );
        assert_eq!(unit(path), "slide");
        assert_eq!(
            join(path, c),
            "First\ntitle\nCafé\nÜnïcode box\n\nSecond title\na box\n"
        );
        for e in ["pot", "pps"] {
            assert_eq!(unit(Path::new(&format!("a.{e}"))), "slide");
        }
    }

    #[test]
    fn ppt_without_an_edit_history_reads_every_slide() {
        let ppt = ppt_of(&[(&[], &["box"])]);
        let mut file = cfb::CompoundFile::open(Cursor::new(ppt)).unwrap();
        let mut stream = Vec::new();
        file.open_stream("PowerPoint Document")
            .unwrap()
            .read_to_end(&mut stream)
            .unwrap();
        let ppt = ole_of(&[("PowerPoint Document", &stream)]);
        let c = chunks(Path::new("a.ppt"), &ppt).unwrap();
        assert_eq!(c, vec![(1, "stale\n".to_owned()), (2, "box\n".to_owned())]);
    }

    #[test]
    fn encrypted_ppt_is_an_error() {
        let mut ppt = ppt_of(&[(&["x"], &[])]);
        let mut file = cfb::CompoundFile::open(Cursor::new(&mut ppt)).unwrap();
        let mut user = Vec::new();
        file.open_stream("Current User")
            .unwrap()
            .read_to_end(&mut user)
            .unwrap();
        user[12..16].copy_from_slice(&0xF3D1_C4DFu32.to_le_bytes());
        file.create_stream("Current User")
            .unwrap()
            .write_all(&user)
            .unwrap();
        file.flush().unwrap();
        drop(file);
        let e = chunks(Path::new("a.ppt"), &ppt).unwrap_err();
        assert_eq!(e.to_string(), "encrypted");
    }

    const RTF: &str = r#"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fswiss Helvetica;}}
{\colortbl;\red255\green0\blue0;}{\*\generator Riched20 10.0;}{\info{\title Secret}}
\pard\plain Fish \'26 chips \'80\~4\par
{\header page header\par}
Caf\'e9 \u233?s, \u-10179?\u-8704? and {\uc2\u8364 EU} done\par
{\field{\*\fldinst HYPERLINK "http://x"}{\fldrslt the site}}\line next\tab col\par
{\pict\pngblip 89504e470d0a}{\*\shppict{\pict 0102}}a\bin2 {}\b bold\b0\par
\ldblquote q\rdblquote  \{braces\}\par
}"#;

    #[test]
    fn rtf_keeps_the_text_and_drops_the_rest() {
        let path = Path::new("a.rtf");
        assert_eq!(unit(path), "paragraph");
        assert_eq!(
            join(path, chunks(path, RTF.as_bytes()).unwrap()),
            "Fish & chips €\u{a0}4\nCafé és, 😀 and € done\nthe site\nnext\tcol\nabold\n“q” {braces}\n"
        );
        let sjis = b"{\\rtf1\\ansi\\ansicpg932 \\'82\\'a0\\'82\\'a2\\par}";
        assert_eq!(rtf::rtf(sjis).unwrap(), "\u{3042}\u{3044}\n");
        assert!(
            rtf::rtf(
                &b"{\\rtf1"
                    .iter()
                    .chain(&[b'{'; 5000])
                    .copied()
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    #[test]
    fn a_fonts_charset_sets_the_codepage_until_its_group_ends() {
        let fonts = br"{\rtf1\ansi{\fonttbl{\f0 Arial;}{\f1\fcharset204 Arial Cyr;}}
{\f1 \'cf\'f0\'e8\'e2\'e5\'f2} \'e9\par}";
        assert_eq!(rtf::rtf(fonts).unwrap(), "Привет é\n");
    }

    #[test]
    fn an_rtf_file_saved_as_doc_reads_as_rtf() {
        assert_eq!(
            chunks(Path::new("a.doc"), RTF.as_bytes()).unwrap(),
            chunks(Path::new("a.rtf"), RTF.as_bytes()).unwrap()
        );
    }

    #[test]
    fn a_docx_renamed_to_doc_reads_as_a_docx() {
        let word = zipped(&[(
            "word/document.xml",
            "<w:document><w:body><w:p><w:r><w:t>renamed</w:t></w:r></w:p></w:body></w:document>",
        )]);
        assert_eq!(
            chunks(Path::new("a.doc"), &word).unwrap(),
            vec![(1, "renamed\n".to_owned())]
        );
    }

    #[test]
    fn a_pptx_renamed_to_ppt_reads_as_a_pptx() {
        let slides = zipped(&[(
            "ppt/slides/slide1.xml",
            "<p:sld><a:p><a:r><a:t>renamed</a:t></a:r></a:p></p:sld>",
        )]);
        assert_eq!(
            chunks(Path::new("a.ppt"), &slides).unwrap(),
            vec![(1, "renamed\n".to_owned())]
        );
    }

    #[test]
    fn an_odf_run_of_spaces_reads_as_one_space() {
        let odf = "<office:document-content><office:body><office:text>\
                   <text:p>a<text:s text:c=\"3\"/>b</text:p>\
                   </office:text></office:body></office:document-content>";
        assert_eq!(
            chunks(Path::new("a.fodt"), odf.as_bytes()).unwrap(),
            vec![(1, "a b\n".to_owned())]
        );
    }

    #[test]
    fn broken_doc_ppt_and_rtf_do_not_panic() {
        let doc = doc_of(&[("Some text\r", true), ("more\r", false)], 15);
        let ppt = ppt_of(&[(&["title"], &["box"]), (&["two"], &[])]);
        for (name, file) in [
            ("a.doc", doc),
            ("a.ppt", ppt),
            ("a.rtf", RTF.as_bytes().to_vec()),
        ] {
            let path = Path::new(name);
            assert!(!chunks(path, &file).unwrap().is_empty(), "{name}");
            for at in (0..file.len()).step_by(file.len() / 300 + 1) {
                let _ = chunks(path, &file[..at]);
                for byte in [0x00, 0xFF, 0x7F, b'{', b'\\'] {
                    let mut broken = file.clone();
                    broken[at] = byte;
                    let _ = chunks(path, &broken);
                }
            }
        }
    }

    #[test]
    fn every_document_extension_has_a_unit_and_a_parser() {
        for e in EXTENSIONS {
            let path = format!("a.{e}");
            assert!(is_document(Path::new(&path)));
        }
    }

    #[test]
    fn garbage_under_any_document_extension_is_an_error_or_nothing_not_a_panic() {
        for e in EXTENSIONS {
            let _ = chunks(Path::new(&format!("a.{e}")), b"not a document");
        }
    }

    #[test]
    fn broken_files_are_errors_not_panics() {
        for name in [
            "a.pdf", "a.docx", "a.pptx", "a.odt", "a.xlsx", "a.ods", "a.doc", "a.ppt", "a.rtf",
        ] {
            assert!(chunks(Path::new(name), b"%PDF-1.7 not really").is_err());
        }
    }
}
