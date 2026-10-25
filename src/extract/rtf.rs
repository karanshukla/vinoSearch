//! RTF's text (#97): control words and the groups that hold no text (fonts,
//! styles, pictures, field codes, ...) stripped, a line per paragraph.

use crate::index::MAX_TEXT_BYTES;
use anyhow::{Result, bail};
use encoding_rs::{Encoding, WINDOWS_1252};
use std::collections::HashMap;

/// Destinations whose content isn't the document's text. Any group starting
/// with `\*` is skipped too. Headers and footnotes are left out, as they are
/// for a `.docx`.
const SKIP: &[&str] = &[
    "annotation",
    "atnauthor",
    "atnid",
    "bkmkend",
    "bkmkstart",
    "colortbl",
    "datastore",
    "docvar",
    "falt",
    "fldinst",
    "filetbl",
    "fonttbl",
    "footer",
    "footerf",
    "footerl",
    "footerr",
    "footnote",
    "header",
    "headerf",
    "headerl",
    "headerr",
    "info",
    "latentstyles",
    "listoverridetable",
    "listtable",
    "listtext",
    "nonshppict",
    "objclass",
    "objdata",
    "pict",
    "pntext",
    "pntxta",
    "pntxtb",
    "private",
    "revtbl",
    "rsidtbl",
    "sn",
    "sp",
    "stylesheet",
    "sv",
    "tc",
    "template",
    "themedata",
    "xe",
    "xmlnstbl",
];

/// Groups nested deeper than this aren't a document.
const MAX_DEPTH: usize = 1024;

#[derive(Clone, Copy)]
struct Group {
    skip: bool,
    /// `\ucN`: how many fallback characters follow a `\u`.
    uc: u32,
    /// `\fN`, the font, whose charset can set the codepage.
    font: Option<i64>,
}

/// The codepage a font table's `\fcharsetN` stands for. 0 is the
/// document's own, `\ansicpg`.
fn charset_codepage(charset: i64) -> Option<u16> {
    Some(match charset {
        77 => 10000,
        128 => 932,
        129 => 949,
        130 => 1361,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        254 => 437,
        _ => return None,
    })
}

struct Out {
    text: String,
    /// Bytes from `\'hh` or 8-bit text, decoded together so a double-byte
    /// codepage's pairs stay pairs.
    bytes: Vec<u8>,
    encoding: &'static Encoding,
    /// A `\u` high surrogate waiting for its low half.
    high: Option<u16>,
}

impl Out {
    fn flush(&mut self) {
        if !self.bytes.is_empty() {
            let (s, _) = self.encoding.decode_without_bom_handling(&self.bytes);
            self.text.push_str(&s);
            self.bytes.clear();
        }
    }

    fn push(&mut self, s: &str) {
        self.flush();
        self.high = None;
        self.text.push_str(s);
    }

    fn unit(&mut self, u: u16) {
        self.flush();
        match (self.high.take(), u) {
            (_, 0xD800..=0xDBFF) => self.high = Some(u),
            (Some(high), 0xDC00..=0xDFFF) => self.text.extend(
                char::decode_utf16([high, u]).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
            ),
            _ => self
                .text
                .push(char::from_u32(u.into()).unwrap_or(char::REPLACEMENT_CHARACTER)),
        }
    }
}

/// `bytes`' text, a line per paragraph.
pub fn rtf(bytes: &[u8]) -> Result<String> {
    if !bytes.starts_with(b"{\\rtf") {
        bail!("not RTF");
    }
    let mut out = Out {
        text: String::new(),
        bytes: Vec::new(),
        encoding: WINDOWS_1252,
        high: None,
    };
    let mut stack: Vec<Group> = Vec::new();
    let mut group = Group {
        skip: false,
        uc: 1,
        font: None,
    };
    // The document's codepage, and the fonts' that have one of their own.
    let mut ansi = WINDOWS_1252;
    let mut fonts: HashMap<i64, &'static Encoding> = HashMap::new();
    let encoding = |fonts: &HashMap<_, _>, ansi, font: Option<i64>| {
        font.and_then(|f| fonts.get(&f).copied()).unwrap_or(ansi)
    };
    // Fallback characters after a `\u` still to skip.
    let mut fallback = 0u32;
    let mut group_start = false;
    let mut i = 0;
    while i < bytes.len() {
        if out.text.len() as u64 > MAX_TEXT_BYTES {
            bail!("over {MAX_TEXT_BYTES} bytes of text");
        }
        let starting = std::mem::take(&mut group_start);
        match bytes[i] {
            b'{' => {
                if stack.len() == MAX_DEPTH {
                    bail!("groups nested over {MAX_DEPTH} deep");
                }
                stack.push(group);
                group_start = true;
                fallback = 0;
                i += 1;
            }
            b'}' => {
                group = stack.pop().unwrap_or(group);
                out.flush();
                out.encoding = encoding(&fonts, ansi, group.font);
                fallback = 0;
                i += 1;
            }
            b'\r' | b'\n' => i += 1,
            b'\\' => {
                let Some(&next) = bytes.get(i + 1) else { break };
                if !next.is_ascii_alphabetic() {
                    i += 2;
                    let hex = (next == b'\'')
                        .then(|| bytes.get(i..i + 2))
                        .flatten()
                        .and_then(|h| std::str::from_utf8(h).ok())
                        .and_then(|h| u8::from_str_radix(h, 16).ok());
                    if next == b'\'' {
                        i = (i + 2).min(bytes.len());
                    }
                    if next == b'*' {
                        group.skip |= starting;
                        continue;
                    }
                    if fallback > 0 {
                        fallback -= 1;
                        continue;
                    }
                    if group.skip {
                        continue;
                    }
                    match next {
                        b'\'' => {
                            if let Some(b) = hex {
                                out.bytes.push(b);
                            }
                        }
                        b'\\' | b'{' | b'}' => out.push(&char::from(next).to_string()),
                        b'~' => out.push("\u{a0}"),
                        b'_' => out.push("-"),
                        b'\r' | b'\n' => out.push("\n"),
                        _ => {}
                    }
                    continue;
                }
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && end - start < 32 && bytes[end].is_ascii_alphabetic() {
                    end += 1;
                }
                let word = std::str::from_utf8(&bytes[start..end]).unwrap_or_default();
                let num_start = end;
                if bytes.get(end) == Some(&b'-') {
                    end += 1;
                }
                while end < bytes.len() && end - num_start < 11 && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let param = std::str::from_utf8(&bytes[num_start..end])
                    .ok()
                    .and_then(|n| n.parse::<i64>().ok());
                if bytes.get(end) == Some(&b' ') {
                    end += 1;
                }
                i = end;
                if word == "bin" {
                    // Raw bytes, `param` of them.
                    let n = usize::try_from(param.unwrap_or(0)).unwrap_or(0);
                    i = i.saturating_add(n).min(bytes.len());
                    continue;
                }
                if fallback > 0 {
                    fallback -= 1;
                    continue;
                }
                if SKIP.contains(&word) {
                    group.skip = true;
                }
                match word {
                    "ansicpg" => {
                        ansi = param
                            .and_then(|p| u16::try_from(p).ok())
                            .and_then(codepage::to_encoding_no_replacement)
                            .unwrap_or(WINDOWS_1252);
                        out.flush();
                        out.encoding = encoding(&fonts, ansi, group.font);
                    }
                    "f" => {
                        group.font = param;
                        out.flush();
                        out.encoding = encoding(&fonts, ansi, group.font);
                    }
                    // In the font table, for the font just named.
                    "fcharset" | "cpg" => {
                        let page = match word {
                            "cpg" => param.and_then(|p| u16::try_from(p).ok()),
                            _ => param.and_then(charset_codepage),
                        };
                        if let (Some(font), Some(e)) = (
                            group.font,
                            page.and_then(codepage::to_encoding_no_replacement),
                        ) {
                            fonts.insert(font, e);
                        }
                    }
                    "uc" => group.uc = param.and_then(|p| u32::try_from(p).ok()).unwrap_or(1),
                    _ if group.skip => {}
                    "u" => {
                        // A signed 16-bit value: Word writes U+8000 and up as negative.
                        out.unit(param.unwrap_or(0) as u16);
                        fallback = group.uc;
                    }
                    "par" | "line" | "row" | "sect" | "page" => out.push("\n"),
                    "tab" | "cell" => out.push("\t"),
                    "emdash" => out.push("—"),
                    "endash" => out.push("–"),
                    "lquote" => out.push("‘"),
                    "rquote" => out.push("’"),
                    "ldblquote" => out.push("“"),
                    "rdblquote" => out.push("”"),
                    "bullet" => out.push("•"),
                    "emspace" | "enspace" | "qmspace" => out.push(" "),
                    _ => {}
                }
            }
            b => {
                i += 1;
                if fallback > 0 {
                    fallback -= 1;
                } else if !group.skip {
                    out.bytes.push(b);
                }
            }
        }
    }
    out.flush();
    if out.text.len() as u64 > MAX_TEXT_BYTES {
        bail!("over {MAX_TEXT_BYTES} bytes of text");
    }
    Ok(out.text)
}
