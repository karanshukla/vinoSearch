//! Exact words: the FTS5 lookup a query's vector score is nudged by.

use rusqlite::{Connection, params};

/// The chunks holding every word of a query, best (bm25) first,
/// looked up once per query for every kind.
pub struct Words {
    pub(super) ids: Vec<i64>,
    /// The query is one identifier-like word (`INV-20431`, `E0502`, `TODO`)
    /// or wholly quoted: exact matches lead instead of nudging.
    pub(super) exact: bool,
}

/// Added to the cosine of a chunk holding every word of the query. About the
/// gap between a good result and the next few, so exact words reorder near
/// ties and lift a chunk just under its kind's `min_score` over it, without
/// carrying an unrelated chunk past a related one.
pub(super) const WORDS_BOOST: f32 = 0.05;
/// A word in more chunks than this is no exact string to find, and ranking
/// it costs more than the vectors: bm25 over 2,856 chunks took 14 ms, and
/// "the" (81k of 113k chunks) 250 ms (2026-10-25). Such words are left out of the lookup,
/// and counting up to this stops early.
pub(super) const WORDS_MAX_CHUNKS: usize = 2000;
/// Chunks taken from the word index, across every kind.
const WORDS_TAKE: usize = 200;

impl Words {
    /// None for a query with no words, or only words in too many chunks.
    /// A failed lookup is None too: exact words add to a search, they never
    /// stop one.
    pub fn find(conn: &Connection, query: &str) -> Option<Self> {
        let Expression { terms, exact } = expression(query)?;
        match ranked(conn, &terms) {
            Ok(ids) => ids.map(|ids| Self { ids, exact }),
            Err(e) => {
                eprintln!("words {terms:?}: {e}");
                None
            }
        }
    }
}

/// A query as the word index reads it.
pub(super) struct Expression {
    /// Its terms as FTS5 phrases.
    pub(super) terms: Vec<String>,
    /// Whether exact matches should lead.
    pub(super) exact: bool,
}

/// `query`'s terms as FTS5 phrases: every word, or each quoted part. Each is
/// quoted, so no word is read as an operator (AND, NEAR), a column filter
/// (`:`), a prefix (`*`) or a syntax error, and the tokenizer splits it on
/// punctuation as it split the chunks; a term with no letters or digits, an
/// empty phrase, is left out (pinned by
/// `index::tests::no_query_can_break_the_word_lookup`,
/// `index::tests::operators_are_words_like_any_other`).
pub(super) fn expression(query: &str) -> Option<Expression> {
    let query = query.trim();
    let words = terms(query);
    let phrases: Vec<String> = words
        .iter()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if phrases.is_empty() {
        return None;
    }
    let quoted =
        query.len() > 1 && query.starts_with('"') && query[1..].find('"') == Some(query.len() - 2);
    Some(Expression {
        terms: phrases,
        exact: quoted || (words.len() == 1 && identifier(query)),
    })
}

/// `query`'s words, a quoted part (to the closing quote or the end) as one.
fn terms(query: &str) -> Vec<&str> {
    let mut terms = Vec::new();
    let mut rest = query;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
        } else if c == '"' {
            let body = &rest[1..];
            let end = body.find('"').unwrap_or(body.len());
            terms.push(&body[..end]);
            rest = body.get(end + 1..).unwrap_or("");
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"')
                .unwrap_or(rest.len());
            terms.push(&rest[..end]);
            rest = &rest[end..];
        }
    }
    terms
}

/// A word an embedding has no sense of: one with a digit, inner punctuation
/// (`foo_bar`, `v2.1`, `INV-20431`) or all capitals (`TODO`, `ENOENT`).
fn identifier(word: &str) -> bool {
    let inner = word.trim_matches(|c: char| !c.is_alphanumeric());
    let letters = inner.chars().filter(|c| c.is_alphabetic()).count();
    inner.chars().any(|c| c.is_ascii_digit())
        || inner.contains(|c: char| !c.is_alphanumeric() && c != '\'')
        || (letters >= 2 && !inner.chars().any(char::is_lowercase))
}

/// Chunks holding every term in no more than WORDS_MAX_CHUNKS chunks, best
/// first, at most WORDS_TAKE; None if every term is in more. Without the
/// common ones a long query costs little more than a short one: "how is the
/// NPU driver version checked" took 30 ms with them (2026-10-25).
pub(super) fn ranked(conn: &Connection, terms: &[String]) -> rusqlite::Result<Option<Vec<i64>>> {
    let mut count = conn.prepare(
        "SELECT count(*) FROM (SELECT 1 FROM chunk_words WHERE chunk_words MATCH ?1 LIMIT ?2)",
    )?;
    let mut rare = Vec::new();
    for t in terms {
        let n: i64 = count.query_row(params![t, WORDS_MAX_CHUNKS as i64 + 1], |r| r.get(0))?;
        if n as usize <= WORDS_MAX_CHUNKS {
            rare.push(t.as_str());
        }
    }
    if rare.is_empty() {
        return Ok(None);
    }
    conn.prepare("SELECT rowid FROM chunk_words WHERE chunk_words MATCH ?1 ORDER BY rank LIMIT ?2")?
        .query_map(params![rare.join(" "), WORDS_TAKE as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map(Some)
}
