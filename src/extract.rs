//! Text out of documents (#55): PDF pages, the XML inside Office and
//! OpenDocument zips, spreadsheets, and CSV with its header on every chunk.
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
    let header_lines = match ext(path).as_str() {
        "csv" | "tsv" => 1,
        "xlsx" | "xls" | "ods" => 2,
        _ => 0,
    };
    let by_part = matches!(unit(path), "page" | "slide");
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
    "pdf", "docx", "odt", "pptx", "odp", "xlsx", "xls", "ods", "csv", "tsv",
];

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
        "pptx" | "odp" => "slide",
        "xlsx" | "xls" | "ods" | "csv" | "tsv" => "row",
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
        "docx" => Ok(chunk(&xml_text(
            &part(bytes, "word/document.xml")?,
            &Tags::DOCX,
        ))),
        "odt" => Ok(chunk(&xml_text(&part(bytes, "content.xml")?, &Tags::ODF))),
        "pptx" => pptx(bytes),
        "odp" => Ok(numbered(
            xml_text(&part(bytes, "content.xml")?, &Tags::ODF).split('\u{c}'),
        )),
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
    let doc = lopdf::Document::load_mem(bytes)?;
    if doc.is_encrypted() {
        bail!("encrypted");
    }
    let pages = doc.get_pages().into_keys().map(|p| {
        doc.extract_text(&[p])
            .unwrap_or_default()
            .nfkc()
            .collect::<String>()
    });
    Ok(numbered(pages))
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
}

impl Tags {
    const DOCX: Tags = Tags {
        text: "w:t",
        paragraph: &["w:p"],
        tab: &["w:tab"],
        line_break: &["w:br", "w:cr"],
        page: "",
    };
    const PPTX: Tags = Tags {
        text: "a:t",
        paragraph: &["a:p"],
        tab: &[],
        line_break: &["a:br"],
        page: "",
    };
    const ODF: Tags = Tags {
        text: "",
        paragraph: &["text:p", "text:h"],
        tab: &["text:tab"],
        line_break: &["text:line-break"],
        page: "draw:page",
    };
}

/// The text in `xml`, a newline at each paragraph's end.
fn xml_text(xml: &str, tags: &Tags) -> String {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    let mut in_text = tags.text.is_empty();
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
                mark(&mut out, name);
            }
            Ok(Event::Empty(e)) => mark(&mut out, e.name().into_inner()),
            Ok(Event::End(e)) => {
                let name = e.name();
                let name = name.into_inner();
                if !tags.text.is_empty() && name == tags.text {
                    in_text = false;
                }
                if tags.paragraph.contains(&name) {
                    out.push('\n');
                }
                if !tags.page.is_empty() && name == tags.page {
                    out.push('\u{c}');
                }
            }
            Ok(Event::Text(t)) if in_text => out.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(r)) if in_text => match r.resolve_char_ref() {
                Ok(Some(c)) => out.push(c),
                _ => out.push_str(match &*r {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
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

    #[test]
    fn pages_and_sheets_are_joined_with_a_blank_line() {
        let page = |n: usize, t: &str| (n, t.to_owned());
        assert_eq!(
            join(
                Path::new("a.pdf"),
                vec![page(1, "one "), page(1, "more\n"), page(3, "three\n")]
            ),
            "one more\n\nthree\n"
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

    #[test]
    fn broken_files_are_errors_not_panics() {
        for name in ["a.pdf", "a.docx", "a.pptx", "a.odt", "a.xlsx", "a.ods"] {
            assert!(chunks(Path::new(name), b"%PDF-1.7 not really").is_err());
        }
    }
}
