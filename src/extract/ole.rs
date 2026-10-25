//! Word 97-2003 `.doc` and PowerPoint 97-2003 `.ppt` (#97): both are compound
//! files (`cfb`) holding a binary stream we read the text out of ourselves.
//! Only what the text needs is parsed: Word's piece table, and PowerPoint's
//! text atoms per slide.

use crate::index::{MAX_DOCUMENT_BYTES, MAX_FILE_BYTES};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};

type Ole<'a> = cfb::CompoundFile<Cursor<&'a [u8]>>;

fn open(bytes: &[u8]) -> Result<Ole<'_>> {
    cfb::CompoundFile::open(Cursor::new(bytes)).context("not a compound file")
}

/// A stream's bytes. It can't be longer than the file it's in, but a
/// broken sector chain shouldn't be able to make it so.
fn stream(file: &mut Ole, name: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    file.open_stream(name)
        .with_context(|| format!("no {name} stream"))?
        .take(MAX_DOCUMENT_BYTES)
        .read_to_end(&mut out)?;
    Ok(out)
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn utf16(b: &[u8]) -> String {
    char::decode_utf16(b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn too_long(text: &str) -> Result<()> {
    if text.len() as u64 > MAX_FILE_BYTES {
        bail!("over {MAX_FILE_BYTES} bytes of text");
    }
    Ok(())
}

/// A `.doc`'s main text, a line per paragraph. Headers, footnotes and
/// comments are left out, as they are for a `.docx`.
pub fn doc(bytes: &[u8]) -> Result<String> {
    let mut file = open(bytes)?;
    let word = stream(&mut file, "WordDocument")?;
    // The FIB ([MS-DOC] 2.5.1): a fixed base, then three arrays that each
    // start with their length.
    match le16(&word, 0) {
        Some(0xA5EC) => {}
        Some(0xA5DC) => bail!("Word 95 or older"),
        _ => bail!("not a Word document"),
    }
    let short = || anyhow::anyhow!("FIB cut short");
    // 0xC1 since Word 97; Word 6 and 95 files have no piece table like this.
    if le16(&word, 2).ok_or_else(short)? < 0xC0 {
        bail!("Word 95 or older");
    }
    let flags = le16(&word, 0x0A).ok_or_else(short)?;
    if flags & 0x0100 != 0 {
        bail!("encrypted");
    }
    let rg_lw = 34 + 2 * usize::from(le16(&word, 32).ok_or_else(short)?);
    let lw_count = usize::from(le16(&word, rg_lw).ok_or_else(short)?);
    let ccp_text = le32(&word, rg_lw + 2 + 3 * 4).ok_or_else(short)?;
    // fcClx and lcbClx, the 34th pair of FibRgFcLcb97.
    let clx_at = rg_lw + 2 + 4 * lw_count + 2 + 33 * 8;
    let fc_clx = le32(&word, clx_at).ok_or_else(short)? as usize;
    let lcb_clx = le32(&word, clx_at + 4).ok_or_else(short)? as usize;
    let table = stream(
        &mut file,
        if flags & 0x0200 != 0 {
            "1Table"
        } else {
            "0Table"
        },
    )?;
    let clx = fc_clx
        .checked_add(lcb_clx)
        .and_then(|end| table.get(fc_clx..end))
        .context("piece table out of range")?;
    // Formatting (Prc) first, then the piece table (Pcdt).
    let mut at = 0;
    while clx.get(at) == Some(&1) {
        at += 3 + usize::from(le16(clx, at + 1).context("Clx cut short")?);
    }
    if clx.get(at) != Some(&2) {
        bail!("no piece table");
    }
    let len = le32(clx, at + 1).context("Clx cut short")? as usize;
    let plc = clx
        .get(at + 5..)
        .and_then(|p| p.get(..len))
        .context("piece table cut short")?;
    if len < 4 || !(len - 4).is_multiple_of(12) {
        bail!("piece table of {len} bytes");
    }
    let pieces = (len - 4) / 12;
    let mut raw = String::new();
    for i in 0..pieces {
        let (Some(start), Some(end), Some(fc)) = (
            le32(plc, 4 * i),
            le32(plc, 4 * (i + 1)),
            le32(plc, 4 * (pieces + 1) + 8 * i + 2),
        ) else {
            break;
        };
        // Only the main document's characters: footnotes, headers and the
        // rest follow it.
        let chars = end.min(ccp_text).saturating_sub(start) as usize;
        if chars == 0 {
            continue;
        }
        let compressed = fc & 0x4000_0000 != 0;
        let fc = (fc & 0x3FFF_FFFF) as usize;
        let (from, size) = if compressed {
            (fc / 2, chars)
        } else {
            (fc, chars * 2)
        };
        let Some(bytes) = word.get(from..).map(|b| &b[..size.min(b.len())]) else {
            continue;
        };
        if compressed {
            // [MS-DOC] 2.9.73's mapping is Windows-1252's.
            raw.push_str(
                &encoding_rs::WINDOWS_1252
                    .decode_without_bom_handling(bytes)
                    .0,
            );
        } else {
            raw.push_str(&utf16(bytes));
        }
        too_long(&raw)?;
    }
    Ok(word_text(&raw))
}

/// Word's special characters out of the text: a paragraph or line break as a
/// newline, a table cell's end as a tab. A field (a page number, a link, a
/// table of contents) shows its result; its code is left out.
fn word_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    // Per open field, whether it's still in its code.
    let mut fields: Vec<bool> = Vec::new();
    let mut in_code = 0usize;
    for c in raw.chars() {
        match c {
            '\u{13}' => {
                fields.push(true);
                in_code += 1;
            }
            '\u{14}' => {
                if let Some(code) = fields.last_mut().filter(|code| **code) {
                    *code = false;
                    in_code -= 1;
                }
            }
            '\u{15}' => {
                if fields.pop() == Some(true) {
                    in_code -= 1;
                }
            }
            _ if in_code > 0 => {}
            '\r' | '\u{b}' | '\u{c}' => out.push('\n'),
            '\u{7}' | '\t' => out.push('\t'),
            // A non-breaking hyphen.
            '\u{1e}' => out.push('-'),
            // Pictures, footnote marks, an optional hyphen.
            c if c < ' ' => {}
            c => out.push(c),
        }
    }
    out
}

/// PowerPoint record types ([MS-PPT] 2.13.24).
const DOCUMENT: u16 = 0x03E8;
const SLIDE: u16 = 0x03EE;
const SLIDE_PERSIST: u16 = 0x03F3;
const TEXT_CHARS: u16 = 0x0FA0;
const TEXT_BYTES: u16 = 0x0FA8;
const SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
const USER_EDIT: u16 = 0x0FF5;
const PERSIST_DIRECTORY: u16 = 0x1772;

struct Record<'a> {
    kind: u16,
    instance: u16,
    container: bool,
    body: &'a [u8],
}

/// The records in `data` one after another. One that runs past the end is
/// cut short; a header that does stops the walk.
fn records(data: &[u8]) -> impl Iterator<Item = Record<'_>> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let rec = record_at(data, at)?;
        at += 8 + rec.body.len();
        Some(rec)
    })
}

fn record_at(data: &[u8], at: usize) -> Option<Record<'_>> {
    let ver_instance = le16(data, at)?;
    let kind = le16(data, at + 2)?;
    let len = le32(data, at + 4)? as usize;
    let body = data.get(at + 8..)?;
    Some(Record {
        kind,
        instance: ver_instance >> 4,
        container: ver_instance & 0xF == 0xF,
        body: &body[..len.min(body.len())],
    })
}

/// Text atoms nested this deep aren't looked for: a real slide's are a few
/// levels down, and a crafted file could nest a record per 8 bytes.
const MAX_DEPTH: u32 = 32;

/// The text atoms anywhere in `data`, in order.
fn texts(data: &[u8], depth: u32, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    for rec in records(data) {
        if rec.container {
            texts(rec.body, depth + 1, out);
        } else {
            atom_text(&rec, out);
        }
    }
}

/// `rec`'s text onto `out`, if it's a text atom.
fn atom_text(rec: &Record, out: &mut Vec<String>) {
    match rec.kind {
        TEXT_CHARS => out.push(utf16(rec.body)),
        // The low bytes of UTF-16, so Latin-1.
        TEXT_BYTES => out.push(rec.body.iter().map(|&b| char::from(b)).collect()),
        _ => {}
    }
}

/// A `.ppt`'s text, slide by slide in show order. Notes and masters are left
/// out, as they are for a `.pptx`.
pub fn ppt(bytes: &[u8]) -> Result<Vec<String>> {
    let mut file = open(bytes)?;
    let doc = stream(&mut file, "PowerPoint Document")?;
    let user = stream(&mut file, "Current User").unwrap_or_default();
    // The current user's header token says so, or the last edit has an
    // encryptSessionPersistIdRef, the field only an encrypted file has.
    let last_edit = le32(&user, 16).and_then(|at| record_at(&doc, at as usize));
    if le32(&user, 12) == Some(0xF3D1_C4DF)
        || last_edit.is_some_and(|r| r.kind == USER_EDIT && r.body.len() >= 32)
    {
        bail!("encrypted");
    }
    // Without a usable edit history: every slide in the stream, in the order
    // they're stored. After a fast save that can include old versions.
    let slides = persisted_slides(&doc, &user).unwrap_or_else(|_| {
        records(&doc)
            .filter(|r| r.kind == SLIDE)
            .map(|r| {
                let mut t = Vec::new();
                texts(r.body, 0, &mut t);
                t
            })
            .collect()
    });
    let mut total = 0;
    let slides = slides
        .into_iter()
        .map(|texts| {
            let mut slide = String::new();
            // A slide number's or date's placeholder is a lone `*`.
            for t in texts.into_iter().filter(|t| t != "*") {
                for c in t.chars() {
                    slide.push(match c {
                        '\r' | '\u{b}' => '\n',
                        c => c,
                    });
                }
                slide.push('\n');
            }
            slide
        })
        .inspect(|s| total += s.len())
        .collect();
    if total as u64 > MAX_FILE_BYTES {
        bail!("over {MAX_FILE_BYTES} bytes of text");
    }
    Ok(slides)
}

/// Each slide's text blocks, found the way PowerPoint does: the current
/// edit's persist directory gives where each object is, the document's
/// slide list which slides are shown and in what order ([MS-PPT] 2.1.2).
/// Placeholder text sits in the slide list, a text box's in the slide.
fn persisted_slides(doc: &[u8], user: &[u8]) -> Result<Vec<Vec<String>>> {
    // CurrentUserAtom: the offset of the last edit.
    let mut edit = le32(user, 16).context("no current user")? as usize;
    let mut persist: HashMap<u32, usize> = HashMap::new();
    let mut doc_ref = None;
    let mut seen = HashSet::new();
    // Newest edit first; an object's latest offset is the one that counts.
    while seen.insert(edit) {
        let rec = record_at(doc, edit).filter(|r| r.kind == USER_EDIT);
        let body = rec.context("no edit record")?.body;
        doc_ref.get_or_insert(le32(body, 16).context("edit cut short")?);
        let dir_at = le32(body, 12).context("edit cut short")? as usize;
        let dir = record_at(doc, dir_at)
            .filter(|r| r.kind == PERSIST_DIRECTORY)
            .context("no persist directory")?
            .body;
        let mut at = 0;
        while let Some(entry) = le32(dir, at) {
            let (first, count) = (entry & 0xF_FFFF, entry >> 20);
            at += 4;
            for id in first..first + count {
                let Some(offset) = le32(dir, at) else { break };
                persist.entry(id).or_insert(offset as usize);
                at += 4;
            }
        }
        match le32(body, 8) {
            Some(0) | None => break,
            Some(last) => edit = last as usize,
        }
    }
    let at = doc_ref
        .and_then(|r| persist.get(&r))
        .context("no document")?;
    let document = record_at(doc, *at)
        .filter(|r| r.kind == DOCUMENT)
        .context("no document")?;
    let list = records(document.body)
        .find(|r| r.kind == SLIDE_LIST_WITH_TEXT && r.instance == 0)
        .context("no slide list")?;
    let mut slides: Vec<(u32, Vec<String>)> = Vec::new();
    for rec in records(list.body) {
        if rec.kind == SLIDE_PERSIST {
            let id = le32(rec.body, 0).context("slide cut short")?;
            slides.push((id, Vec::new()));
        } else if let Some((_, t)) = slides.last_mut() {
            atom_text(&rec, t);
        }
    }
    // A slide listed twice is read once: a crafted list could name one big
    // slide for every few bytes.
    let mut read = HashSet::new();
    Ok(slides
        .into_iter()
        .map(|(id, mut listed)| {
            let mut own = Vec::new();
            if let Some(slide) = persist
                .get(&id)
                .filter(|_| read.insert(id))
                .and_then(|&at| record_at(doc, at))
                .filter(|r| r.kind == SLIDE)
            {
                texts(slide.body, 0, &mut own);
            }
            // Newer PowerPoints keep placeholder text in both places.
            let mut have: HashSet<String> = listed.iter().cloned().collect();
            for t in own {
                if have.insert(t.clone()) {
                    listed.push(t);
                }
            }
            listed
        })
        .collect())
}
