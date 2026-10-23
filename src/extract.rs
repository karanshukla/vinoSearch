//! Text out of documents (#55): PDF pages, the XML inside Office and
//! OpenDocument zips, spreadsheets, CSV with its header on every chunk, EPUB
//! chapters and subtitles (#97).
//! What comes out is chunked like any text file, numbered by page, slide or
//! row instead of line. The indexer runs it in a child process with limits
//! (#84), since the files come from anywhere.

use crate::index::{CHUNK_CHARS, MAX_DOCUMENT_BYTES, MAX_FILE_BYTES, chunk};
use anyhow::{Context, Result, bail};
use quick_xml::events::Event;
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Duration;
use unicode_normalization::UnicodeNormalization;

/// Address space the extracting child may map (#84). The runner's own share
/// is small: this subcommand loads no runtime or model. A 32 MiB PDF's
/// streams, decompressed, and lopdf's objects for them fit many times over.
const CHILD_ADDRESS_SPACE: u64 = 1 << 30;
/// CPU seconds before the child gets SIGXCPU. Parsing is well under a second
/// a document; a zip bomb or a quadratic parser isn't.
const CHILD_CPU_SECONDS: u64 = 30;
/// Wall-clock limit, for a child that hangs rather than spins. Indexing
/// runs at idle priority, so a busy machine can stretch 30 CPU seconds out.
const CHILD_WALL: Duration = Duration::from_secs(120);

/// A document's bytes to its numbered chunks: `chunks`, or `chunks_in_child`.
pub type Parser = fn(&Path, &[u8]) -> Result<Vec<(usize, String)>>;

/// `chunks`, run in a child process (the runner executing itself as
/// `vinosearch extract <path>`, with `bytes` on its stdin) under memory, CPU
/// and wall-clock limits, so a parser that aborts, overflows its stack, eats
/// memory or hangs costs that process, not the runner (#84). Err for a
/// parser error, a child its limits stopped, or one that failed to start.
pub fn chunks_in_child(path: &Path, bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("extract").arg(path);
    let out = crate::proc::run(cmd, bytes, CHILD_WALL)?;
    serde_json::from_slice(&out).context("reading the extractor's output")
}

/// `path`'s whole text, for reading aloud: parsed in the child as indexing
/// does, under the same limits, then joined back up.
pub fn text(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!("over {MAX_DOCUMENT_BYTES} bytes");
    }
    let chunks = chunks_in_child(path, &bytes)?;
    let size: usize = chunks.iter().map(|(_, c)| c.len()).sum();
    if size as u64 > MAX_FILE_BYTES {
        bail!("{size} bytes of text");
    }
    Ok(join(path, chunks))
}

/// Chunks back into the text they came from. `chunk` cuts without overlap,
/// so a part's chunks concatenate back into it; pages and slides are kept
/// apart by a blank line. A table's chunks each repeat its header (and a
/// sheet's name), which is kept once per sheet.
fn join(path: &Path, chunks: Vec<(usize, String)>) -> String {
    if ext(path) == "pdf" {
        return pdf_pages(&chunks);
    }
    let header_lines = match ext(path).as_str() {
        "csv" | "tsv" => 1,
        e if SHEETS.contains(&e) => 2,
        _ => 0,
    };
    let by_part = matches!(unit(path), "page" | "slide" | "chapter");
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

/// A PDF's pages, each on one line, a blank line apart. A text layer's line
/// breaks are layout, not meaning: some PDFs put every word on its own line,
/// which read aloud as one sentence per word.
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

/// The child's side: set the limits, read the document from stdin, write
/// its chunks to stdout as JSON. `path` is only for its extension.
pub fn child_main(path: &Path) -> Result<()> {
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
    let chunks = chunks(path, &bytes)?;
    serde_json::to_writer(std::io::stdout().lock(), &chunks)?;
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

/// Taken as documents while they are on.
pub const EXTENSIONS: &[&str] = &[
    "pdf", "docx", "docm", "dotx", "dotm", "odt", "ott", "fodt", "pptx", "pptm", "potx", "ppsx",
    "ppsm", "odp", "otp", "fodp", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "xls", "ods", "ots",
    "csv", "tsv", "epub", "srt", "vtt",
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

/// What a chunk's number counts in `path`: "page", "slide", "row" or
/// "paragraph".
pub fn unit(path: &Path) -> &'static str {
    match ext(path).as_str() {
        "pdf" => "page",
        "odp" | "otp" | "fodp" => "slide",
        e if SLIDES.contains(&e) => "slide",
        "csv" | "tsv" => "row",
        e if SHEETS.contains(&e) => "row",
        "epub" => "chapter",
        "srt" | "vtt" => "line",
        _ => "paragraph",
    }
}

/// `path`'s text as numbered chunks, from `bytes`, its content.
pub fn chunks(path: &Path, bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    if bytes.starts_with(b"PK") {
        check_zip(bytes)?;
    }
    match ext(path).as_str() {
        "pdf" => pdf(bytes),
        e if WORD.contains(&e) => Ok(chunk(&xml_text(
            &part(bytes, "word/document.xml")?,
            &Tags::DOCX,
        ))),
        "odt" | "ott" => Ok(chunk(&xml_text(&part(bytes, "content.xml")?, &Tags::ODF))),
        // Flat OpenDocument: the same XML, unzipped.
        "fodt" => Ok(chunk(&xml_text(utf8(bytes)?, &Tags::ODF))),
        e if SLIDES.contains(&e) => pptx(bytes),
        "odp" | "otp" => Ok(numbered(
            xml_text(&part(bytes, "content.xml")?, &Tags::ODF).split('\u{c}'),
        )),
        "fodp" => Ok(numbered(xml_text(utf8(bytes)?, &Tags::ODF).split('\u{c}'))),
        "epub" => epub(bytes),
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

/// Each page's text layer, NFKC so ligatures like `ﬁ` read as `fi`. A scan
/// has none and comes out empty.
fn pdf(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let mut doc = lopdf::Document::load_mem(bytes)?;
    if doc.is_encrypted() {
        bail!("encrypted");
    }
    prefer_to_unicode(&mut doc);
    let pages = doc.get_pages().into_keys().map(|p| {
        let text = doc
            .extract_text(&[p])
            .unwrap_or_default()
            .nfkc()
            .collect::<String>();
        if readable(&text) { text } else { String::new() }
    });
    Ok(numbered(pages))
}

/// lopdf decodes by a font's /Encoding whenever it has one and reads /ToUnicode only
/// without, the reverse of the spec's order. A Type 3 font always has an /Encoding, and
/// Chromium's Print to PDF draws web fonts as Type 3 with made-up glyph names, so its
/// pages came out as glyph codes (#92). With /Encoding gone, lopdf takes /ToUnicode.
fn prefer_to_unicode(doc: &mut lopdf::Document) {
    for object in doc.objects.values_mut() {
        if let Ok(font) = object.as_dict_mut()
            && font.has_type(b"Font")
            && font.has(b"ToUnicode")
        {
            font.remove(b"Encoding");
        }
    }
}

/// Whether a page's text is mostly letters and digits. A font lopdf still can't decode
/// gives glyph codes, mostly punctuation, and they'd be embedded, shown as the snippet and
/// read aloud; better the page has no chunks. A run of one mark counts once, so a table of
/// contents' dot leaders don't count against it.
fn readable(text: &str) -> bool {
    let (mut words, mut all, mut last) = (0, 0, None);
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        if c.is_alphanumeric() {
            words += 1;
        } else if last == Some(c) {
            continue;
        }
        all += 1;
        last = Some(c);
    }
    words * 2 >= all
}

fn utf8(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("not UTF-8")
}

/// Each chapter in reading order. The size check in `chunks` has already run on the
/// zip; rbook reads its entries without a limit of its own.
fn epub(bytes: &[u8]) -> Result<Vec<(usize, String)>> {
    let book = rbook::Epub::read(Cursor::new(bytes.to_vec()))?;
    let mut reader = book.reader();
    let mut chapters = Vec::new();
    while let Some(chapter) = reader.read_next() {
        chapters.push(xml_text(chapter?.content(), &Tags::XHTML));
    }
    Ok(numbered(chapters.iter()))
}

/// What's said in an SRT or WebVTT file: cue numbers and ids, timings, VTT's header
/// and NOTE/STYLE blocks, and inline tags left out.
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

/// One chunk or more per part, numbered from 1 by the part it came from.
fn numbered<S: AsRef<str>>(parts: impl Iterator<Item = S>) -> Vec<(usize, String)> {
    parts
        .enumerate()
        .flat_map(|(i, p)| chunk(p.as_ref()).into_iter().map(move |(_, c)| (i + 1, c)))
        .collect()
}

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
    // MAX_PART_BYTES across all slides, not each.
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

/// Each sheet as tab-separated rows, its name and header row on every chunk.
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

/// Rows packed into chunks that each start with `header`, so the model sees
/// what the columns are; numbered by their first row, `first` being the row
/// after the header.
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

/// `s` up to `max` bytes, on a char boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Zips that say they unpack past this, or hold more entries, aren't opened.
/// Their text would be over MAX_FILE_BYTES anyway, and calamine unpacks
/// without a limit of its own. A zip can lie about its sizes, so this stops
/// accidents and naive zip bombs, not a crafted one.
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

/// Element names that matter in a format's XML.
struct Tags {
    /// Where text sits; empty for any text at all.
    text: &'static str,
    /// Ends a paragraph: a newline.
    paragraph: &'static [&'static str],
    tab: &'static [&'static str],
    line_break: &'static [&'static str],
    /// Ends a page or slide: a form feed.
    page: &'static str,
    /// Nothing inside these is text.
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
    /// An EPUB chapter.
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

/// The text in `xml`, a newline at each paragraph's end.
fn xml_text(xml: &str, tags: &Tags) -> String {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = tags.text.is_empty();
    let mut skipped = 0u32;
    // ODF's run of spaces, <text:s text:c="3"/>, counts as one: enough to
    // keep words apart.
    let mark = |out: &mut String, name: &str| {
        if tags.tab.contains(&name) {
            out.push('\t');
        } else if tags.line_break.contains(&name) {
            out.push('\n');
        } else if name == "text:s" {
            out.push(' ');
        }
    };
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
                mark(&mut out, name);
            }
            Ok(Event::Empty(e)) => mark(&mut out, e.name().into_inner()),
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
                _ => out.push_str(match &*r {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    // HTML's, which EPUB 2 chapters use through their DTD.
                    "nbsp" => " ",
                    "mdash" => "—",
                    "ndash" => "–",
                    "hellip" => "…",
                    "lsquo" => "‘",
                    "rsquo" => "’",
                    "ldquo" => "“",
                    "rdquo" => "”",
                    _ => "",
                }),
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
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
            // Numbered by the file line its first row is on.
            let first = text.lines().nth(1).unwrap();
            assert_eq!(csv.lines().nth(row - 1), Some(first));
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

    /// One page drawn in a Type 3 font the way Chromium writes one: made-up glyph names
    /// in /Differences, the text only in /ToUnicode.
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
        // Listed out of name order: the spine decides.
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
            ]
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
    fn every_document_extension_has_a_unit_and_a_parser() {
        for e in EXTENSIONS {
            let path = format!("a.{e}");
            assert!(is_document(Path::new(&path)));
            // Garbage in: an error or nothing, never a panic.
            let _ = chunks(Path::new(&path), b"not a document");
        }
    }

    #[test]
    fn broken_files_are_errors_not_panics() {
        for name in ["a.pdf", "a.docx", "a.pptx", "a.odt", "a.xlsx", "a.ods"] {
            assert!(chunks(Path::new(name), b"%PDF-1.7 not really").is_err());
        }
    }
}
