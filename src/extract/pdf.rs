//! A PDF's text layer, page by page, without its running headers, footers and
//! page numbers.

use anyhow::{Result, bail};
use unicode_normalization::UnicodeNormalization;

use super::Extract;
use crate::index::chunk;

/// The text layer of pages `from..from + count`, NFKC so ligatures like `ﬁ`
/// read as `fi`, without its headers, footers and page numbers. A scan has
/// none and comes out empty. The whole file is loaded each run, but only
/// these pages' content is decoded, which is most of the time a page takes.
pub(super) fn pdf(bytes: &[u8], from: usize, count: usize) -> Result<Extract> {
    let mut doc = lopdf::Document::load_mem(bytes)?;
    if doc.is_encrypted() {
        bail!("encrypted");
    }
    prefer_to_unicode(&mut doc);
    let pages = doc.get_pages();
    let mut texts: Vec<(usize, String)> = pages
        .keys()
        .enumerate()
        .skip(from)
        .take(count)
        .map(|(i, p)| {
            let text = doc
                .extract_text(&[*p])
                .unwrap_or_default()
                .nfkc()
                .collect::<String>();
            (i + 1, if readable(&text) { text } else { String::new() })
        })
        .collect();
    drop_furniture(&mut texts);
    let chunks = texts
        .into_iter()
        .flat_map(|(n, text)| chunk(&text).into_iter().map(move |(_, c)| (n, c)))
        .collect();
    Ok(Extract {
        parts: pages.len(),
        chunks,
    })
}

/// Lines at each end of a page that may be its header or footer.
pub(super) const FURNITURE_LINES: usize = 3;

/// Takes the running headers, footers and page numbers out of `pages` (numbered
/// from 1) while their text is still in lines. Once `pdf_pages` puts a page
/// on one line they can't be told from its first and last sentences, and reading
/// aloud says "Annual Report 2025 3 Introduction The company grew...".
///
/// A page's first and last FURNITURE_LINES non-empty lines are its slots. A slot
/// goes when it is only a page number ("3", "Page 3 of 40", "3/40", "iv"), or when,
/// with its digits blanked, it fills a slot on 3 or more pages and on more than
/// half of them, or of the odd or the even ones, for left and right headers. So a
/// PDF of one or two pages loses only its page numbers. Indexing reads a run of
/// PARTS_AT_ONCE pages at a time, and counts within the run.
pub(super) fn drop_furniture(pages: &mut [(usize, String)]) {
    let slots: Vec<Vec<(usize, String)>> = pages.iter().map(|(_, text)| slots_of(text)).collect();
    let repeats = Repeats::count(pages, &slots);
    for ((_, text), slots) in pages.iter_mut().zip(&slots) {
        let gone: Vec<usize> = slots
            .iter()
            .filter(|(_, s)| page_number(s) || repeats.repeated(s))
            .map(|(i, _)| *i)
            .collect();
        if !gone.is_empty() {
            *text = text
                .lines()
                .enumerate()
                .filter(|(i, _)| !gone.contains(i))
                .map(|(_, l)| l)
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
}

/// A page's slots: its first and last FURNITURE_LINES non-empty lines,
/// `blanked`, by line index.
fn slots_of(text: &str) -> Vec<(usize, String)> {
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let head = FURNITURE_LINES.min(lines.len());
    let tail = lines.len().saturating_sub(FURNITURE_LINES).max(head);
    lines[..head]
        .iter()
        .chain(&lines[tail..])
        .map(|&(i, l)| (i, blanked(l)))
        .collect()
}

/// Pages with text, and how many of them each slot's text is on, each
/// counted as [even, odd].
struct Repeats<'a> {
    with_text: [usize; 2],
    on: std::collections::HashMap<&'a str, [usize; 2]>,
}

impl<'a> Repeats<'a> {
    fn count(pages: &[(usize, String)], slots: &'a [Vec<(usize, String)>]) -> Self {
        let mut repeats = Repeats {
            with_text: [0; 2],
            on: Default::default(),
        };
        for ((n, _), slots) in pages.iter().zip(slots) {
            if slots.is_empty() {
                continue;
            }
            repeats.with_text[n % 2] += 1;
            let mut seen: Vec<&str> = slots.iter().map(|(_, s)| s.as_str()).collect();
            seen.sort_unstable();
            seen.dedup();
            for s in seen {
                repeats.on.entry(s).or_default()[n % 2] += 1;
            }
        }
        repeats
    }

    /// On 3 pages or more, and on more than half of them, or of the odd or
    /// the even ones.
    fn repeated(&self, s: &str) -> bool {
        let all = self.with_text[0] + self.with_text[1];
        self.on.get(s).is_some_and(|c| {
            c[0] + c[1] >= 3
                && ((c[0] + c[1]) * 2 > all || (0..2).any(|p| c[p] * 2 > self.with_text[p]))
        })
    }
}

/// A line trimmed, its whitespace collapsed and each run of digits made `#`, so
/// "Page 3 of 40" and "Page 4 of 40" are the same slot.
pub(super) fn blanked(line: &str) -> String {
    let mut out = String::new();
    for word in line.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        let mut digits = false;
        for c in word.chars() {
            if !c.is_ascii_digit() {
                out.push(c);
            } else if !digits {
                out.push('#');
            }
            digits = c.is_ascii_digit();
        }
    }
    out
}

/// Whether a `blanked` line is only a page number: `(page )#( of #)`, `#/#`, or a
/// roman numeral, any case.
pub(super) fn page_number(blanked: &str) -> bool {
    let line = blanked.to_lowercase();
    let rest = line.strip_prefix("page ").unwrap_or(&line);
    match rest.strip_prefix('#') {
        Some(rest) => {
            let rest = rest.trim_start();
            rest.is_empty()
                || rest
                    .strip_prefix("of")
                    .or_else(|| rest.strip_prefix('/'))
                    .is_some_and(|total| total.trim_start() == "#")
        }
        None => roman(blanked),
    }
}

const NUMERALS: [(u32, &str); 13] = [
    (1000, "m"),
    (900, "cm"),
    (500, "d"),
    (400, "cd"),
    (100, "c"),
    (90, "xc"),
    (50, "l"),
    (40, "xl"),
    (10, "x"),
    (9, "ix"),
    (5, "v"),
    (4, "iv"),
    (1, "i"),
];

/// Whether `s` is a roman numeral written the usual way, all in one case:
/// "iv" and "XII", not "iiii", "Mix" or "dim" (pinned by
/// `extract::tests::page_numbers_are_told_from_text`).
pub(super) fn roman(s: &str) -> bool {
    if s.is_empty() || s.len() > 15 || (s != s.to_lowercase() && s != s.to_uppercase()) {
        return false;
    }
    let s = s.to_lowercase();
    let (value, rest) = read_roman_greedily(&s);
    rest.is_empty() && write_roman(value) == s
}

/// The value of `s`'s leading numerals, taking the largest first, and what
/// follows them.
fn read_roman_greedily(s: &str) -> (u32, &str) {
    let mut value = 0;
    let mut rest = s;
    for (n, numeral) in NUMERALS {
        while let Some(r) = rest.strip_prefix(numeral) {
            rest = r;
            value += n;
        }
    }
    (value, rest)
}

/// `value` in lowercase roman numerals, written the usual way.
fn write_roman(mut value: u32) -> String {
    let mut usual = String::new();
    for (n, numeral) in NUMERALS {
        while value >= n {
            usual.push_str(numeral);
            value -= n;
        }
    }
    usual
}

/// lopdf decodes by a font's /Encoding whenever it has one and reads /ToUnicode only
/// without, the reverse of the spec's order. A Type 3 font always has an /Encoding, and
/// Chromium's Print to PDF draws web fonts as Type 3 with made-up glyph names, so its
/// pages came out as glyph codes. With /Encoding gone, lopdf takes /ToUnicode.
pub(super) fn prefer_to_unicode(doc: &mut lopdf::Document) {
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
pub(super) fn readable(text: &str) -> bool {
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
