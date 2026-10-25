//! Text out of documents (#55): PDF pages, the XML inside Office and
//! OpenDocument zips, spreadsheets, CSV with its header on every chunk, EPUB
//! chapters, subtitles, and Word 97, PowerPoint 97 and RTF files (#97).
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

mod ole;
mod rtf;

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

/// What a chunk's number counts in `path`: "page", "slide", "row" or
/// "paragraph".
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
        // Older Words saved RTF under .doc, and a renamed .docx or .pptx is
        // still one.
        "doc" | "dot" if bytes.starts_with(b"{\\rtf") => Ok(chunk(&rtf::rtf(bytes)?)),
        "doc" | "dot" if bytes.starts_with(b"PK") => chunks(Path::new("a.docx"), bytes),
        "doc" | "dot" => Ok(chunk(&ole::doc(bytes)?)),
        "ppt" | "pot" | "pps" if bytes.starts_with(b"PK") => chunks(Path::new("a.pptx"), bytes),
        "ppt" | "pot" | "pps" => Ok(numbered(ole::ppt(bytes)?.iter())),
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

/// Read as text, but through `html` (#97).
const HTML: &[&str] = &["html", "htm"];

pub fn is_html(path: &Path) -> bool {
    HTML.contains(&ext(path).as_str())
}

/// Tags whose content is not text.
const HTML_SKIP: &[&str] = &["script", "style", "template", "svg"];
/// Tags inside a word: the rest stand for a space, so `<td>a</td><td>b</td>`
/// stays two words.
const HTML_INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "del", "dfn", "em", "font", "i", "ins",
    "kbd", "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var",
    "wbr",
];

/// An HTML page's text: tags, comments, scripts and styles left out and
/// entities decoded. Every newline of the source is kept, so a chunk's line
/// is the line it starts on in the file.
pub fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() / 2);
    // Kept in place of skipped source: its newlines.
    let newlines = |out: &mut String, skipped: &str| {
        out.extend(std::iter::repeat_n('\n', skipped.matches('\n').count()));
    };
    // Lowercased once for finding closing tags: ASCII lowercasing keeps
    // byte offsets, so it indexes like `text`.
    let lower = text.to_ascii_lowercase();
    let mut rest = text;
    while let Some(i) = rest.find(['<', '&']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('&') {
            // Bounded, so a page of bare `&`s isn't a scan to the end each.
            let decoded = rest[1..]
                .bytes()
                .take(33)
                .position(|b| b == b';')
                .and_then(|end| Some((char_ref(&rest[1..=end])?, end + 2)));
            match decoded {
                Some((s, len)) => {
                    out.push_str(&s);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
            continue;
        }
        if let Some(comment) = rest.strip_prefix("<!--") {
            let end = comment.find("-->").map_or(comment.len(), |e| e + 3);
            newlines(&mut out, &comment[..end]);
            rest = &comment[end..];
            continue;
        }
        let tag = &rest[1..];
        let closing = tag.starts_with('/');
        let tag = tag.strip_prefix('/').unwrap_or(tag);
        if !(closing
            || tag.starts_with(['!', '?'])
            || tag.starts_with(|c: char| c.is_ascii_alphabetic()))
        {
            out.push('<');
            rest = &rest[1..];
            continue;
        }
        let name = tag[..tag
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(tag.len())]
            .to_ascii_lowercase();
        let end = tag_end(rest);
        newlines(&mut out, &rest[..end]);
        let self_closing = rest[..end].ends_with("/>");
        rest = &rest[end..];
        if !closing && !self_closing && HTML_SKIP.contains(&name.as_str()) {
            let close = format!("</{name}");
            let body = lower[text.len() - rest.len()..]
                .find(&close)
                .unwrap_or(rest.len());
            newlines(&mut out, &rest[..body]);
            rest = &rest[body..];
            let end = tag_end(rest);
            newlines(&mut out, &rest[..end]);
            rest = &rest[end..];
        }
        if !HTML_INLINE.contains(&name.as_str()) {
            out.push(' ');
        }
    }
    out.push_str(rest);
    out.split('\n')
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where the tag `s` starts with ends, past its `>`. A `>` inside a quoted
/// attribute value doesn't end it; an apostrophe in an unquoted one doesn't
/// start a quote.
fn tag_end(s: &str) -> usize {
    let mut quote = None;
    let mut last = ' ';
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') if last == '=' => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '>') => return i + 1,
            _ => {}
        }
        if !c.is_whitespace() {
            last = c;
        }
    }
    s.len()
}

/// `name` from `&name;` as text: a numeric reference or one of `entity`'s.
fn char_ref(name: &str) -> Option<String> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code).map(String::from);
    }
    entity(name).map(str::to_owned)
}

/// XML's entities and the common ones of HTML, which EPUB 2 chapters also
/// use through their DTD.
fn entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "lsquo" => "‘",
        "rsquo" => "’",
        "ldquo" => "“",
        "rdquo" => "”",
        "laquo" => "«",
        "raquo" => "»",
        "middot" => "·",
        "bull" => "•",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "euro" => "€",
        "pound" => "£",
        "times" => "×",
        _ => return None,
    })
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
        // Each of these once scanned to the end of the page per occurrence:
        // minutes at this size. Linear is about 1 s on CI in a debug build.
        for unit in ["&", "<script>a</script>"] {
            let page = unit.repeat((1 << 20) / unit.len());
            let t0 = std::time::Instant::now();
            html(&page);
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{unit}: {:?}",
                t0.elapsed()
            );
        }
        assert!(is_html(Path::new("A.HTM")) && !is_html(Path::new("a.md")));
    }

    /// A compound file holding `streams`.
    fn ole_of(streams: &[(&str, &[u8])]) -> Vec<u8> {
        // Version 3, 512-byte sectors, as Office writes them.
        let mut file =
            cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new()))
                .unwrap();
        for (name, body) in streams {
            file.create_stream(name).unwrap().write_all(body).unwrap();
        }
        file.flush().unwrap();
        file.into_inner().into_inner()
    }

    /// A Word 97 file whose text is `pieces`, each stored as 8-bit
    /// Windows-1252 (true) or UTF-16, the main document being the first
    /// `main` characters.
    fn doc_of(pieces: &[(&str, bool)], main: u32) -> Vec<u8> {
        let mut word = vec![0u8; 1024];
        word[0..2].copy_from_slice(&0xA5ECu16.to_le_bytes());
        word[2..4].copy_from_slice(&0xC1u16.to_le_bytes());
        // fWhichTblStm: the table is 1Table.
        word[0x0A..0x0C].copy_from_slice(&0x0200u16.to_le_bytes());
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
        // A Prc to step over, then the Pcdt.
        let mut table = vec![0u8; 16];
        let fc_clx = table.len() as u32;
        table.extend([1, 2, 0, 0xAA, 0xBB, 2]);
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

    /// A slide whose drawing holds `texts` a couple of containers down, as a
    /// text box's would be.
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

    /// `slides`' placeholder texts in the slide list, in order, and each
    /// slide's own texts in its container. An old version of a slide sits
    /// in the stream unreferenced, as a fast save leaves it.
    fn ppt_of(slides: &[(&[&str], &[&str])]) -> Vec<u8> {
        let mut stream = slide_container(&["stale"]);
        let mut offsets = Vec::new();
        for (_, own) in slides {
            offsets.push(stream.len() as u32);
            stream.extend(slide_container(own));
        }
        let mut list = Vec::new();
        // The slide list in reverse of the stream's order: it decides.
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
        // The codepage from the font's charset, back to the document's after
        // its group.
        let fonts = br"{\rtf1\ansi{\fonttbl{\f0 Arial;}{\f1\fcharset204 Arial Cyr;}}
{\f1 \'cf\'f0\'e8\'e2\'e5\'f2} \'e9\par}";
        assert_eq!(rtf::rtf(fonts).unwrap(), "Привет é\n");
        // An RTF file saved as .doc, as older Words did.
        assert_eq!(
            chunks(Path::new("a.doc"), RTF.as_bytes()).unwrap(),
            chunks(path, RTF.as_bytes()).unwrap()
        );
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

    /// Cut short and with bytes overwritten, at many places: errors or less
    /// text, never a panic or a hang.
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
            // Garbage in: an error or nothing, never a panic.
            let _ = chunks(Path::new(&path), b"not a document");
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
