//! An HTML page as text that keeps the source's line numbers, and the
//! entities EPUB chapters share with it.

/// Tags whose content is not text.
pub(super) const HTML_SKIP: &[&str] = &["script", "style", "template", "svg"];
/// Tags inside a word: the rest stand for a space, so `<td>a</td><td>b</td>`
/// stays two words.
pub(super) const HTML_INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "code", "data", "del", "dfn", "em", "font", "i", "ins",
    "kbd", "mark", "q", "s", "samp", "small", "span", "strong", "sub", "sup", "time", "u", "var",
    "wbr",
];

/// An HTML page's text: tags, comments, scripts and styles left out and
/// entities decoded. Every newline of the source is kept, so a chunk's line
/// is the line it starts on in the file.
pub fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() / 2);
    let lower = text.to_ascii_lowercase();
    let mut rest = text;
    while let Some(i) = rest.find(['<', '&']) {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if rest.starts_with('&') {
            match decode_ref(rest) {
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
            let body = closing_tag(&lower, rest, &name);
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

/// Stands in for skipped source: its newlines, so the lines after keep their
/// numbers.
fn newlines(out: &mut String, skipped: &str) {
    out.extend(std::iter::repeat_n('\n', skipped.matches('\n').count()));
}

/// The longest entity name or numeric reference looked for after a `&`.
const LONGEST_REF: usize = 32;

/// The `&...;` reference `rest` starts with, decoded, and its length. Looks
/// no further than LONGEST_REF, so a page of bare `&`s isn't a scan to the
/// end each (pinned by
/// `extract::tests::a_megabyte_of_bare_ampersands_or_scripts_reads_in_linear_time`).
fn decode_ref(rest: &str) -> Option<(String, usize)> {
    rest[1..]
        .bytes()
        .take(LONGEST_REF + 1)
        .position(|b| b == b';')
        .and_then(|end| Some((char_ref(&rest[1..=end])?, end + 2)))
}

/// Where `</name` is in `rest`, a tail of the page `lower` is the ASCII
/// lowercase of: that keeps byte offsets, so it indexes like the page.
fn closing_tag(lower: &str, rest: &str, name: &str) -> usize {
    lower[lower.len() - rest.len()..]
        .find(&format!("</{name}"))
        .unwrap_or(rest.len())
}

/// Where the tag `s` starts with ends, past its `>`. A `>` inside a quoted
/// attribute value doesn't end it; an apostrophe in an unquoted one doesn't
/// start a quote.
pub(super) fn tag_end(s: &str) -> usize {
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
pub(super) fn char_ref(name: &str) -> Option<String> {
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
pub(super) fn entity(name: &str) -> Option<&'static str> {
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
