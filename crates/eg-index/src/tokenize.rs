//! Tokenising spreadsheet text.
//!
//! The words in a spreadsheet are rarely written as words. Headers are
//! `NetRevenue`, sheets are `Q3Sales` and `Sheet1`, names are `FY2024_Total`.
//! Tantivy's default tokenizer splits on non-alphanumerics only, so `Sheet1` is
//! one token and a search for `sheet` misses every sheet in the corpus — which
//! is not a subtle ranking problem, it is the index failing at the first thing
//! anyone types.
//!
//! So each run of alphanumerics is emitted whole *and* split at its internal
//! boundaries: case changes and letter/digit changes. `NetRevenue` indexes as
//! `netrevenue`, `net`, `revenue`, and matches all three. Emitting the whole
//! run as well as its parts is what keeps the other direction working: someone
//! who types `netrevenue` because that is what the header says still gets it.
//!
//! The reverse — typing `netrevenue` at a header that reads `Net Revenue` — is
//! not handled, and cannot be without deciding where a word ends in a string
//! that never said. That is what the vector index is for.

use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// A tokenizer that splits compound identifiers as well as punctuation.
#[derive(Clone, Default)]
pub struct SpreadsheetTokenizer;

/// Registered under this name, and named in the schema, so an index built by an
/// older tokenizer is a schema mismatch and gets rebuilt rather than searched
/// with tokens that no longer line up.
pub const TOKENIZER: &str = "spreadsheet";

/// Longer than this and it is not a word anyone will type. Matches tantivy's
/// own default limit.
const MAX_TOKEN_BYTES: usize = 40;

impl Tokenizer for SpreadsheetTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        VecTokenStream {
            tokens: tokens(text),
            next: 0,
            current: Token::default(),
        }
    }
}

/// Every token of a string, in order.
///
/// Built up front rather than streamed: one run of characters can yield several
/// tokens, and a buffer is a great deal easier to be sure of than a state
/// machine that has to remember how far into a split run it is.
fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut position = 0usize;
    for (start, run) in runs(text) {
        push(&mut out, run, start, &mut position);
        let parts = split_compound(run);
        if parts.len() > 1 {
            for (offset, part) in parts {
                push(&mut out, part, start + offset, &mut position);
            }
        }
    }
    out
}

fn push(out: &mut Vec<Token>, text: &str, offset: usize, position: &mut usize) {
    if text.len() > MAX_TOKEN_BYTES {
        return;
    }
    out.push(Token {
        offset_from: offset,
        offset_to: offset + text.len(),
        position: *position,
        text: text.to_lowercase(),
        position_length: 1,
    });
    *position += 1;
}

/// The runs of alphanumerics in a string, with their byte offsets.
fn runs(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, &text[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

/// Split one run at its internal boundaries: `NetRevenue`, `FY2024`, `HTTPCode`.
///
/// Returns the parts with their offsets into the run, or a single part when
/// there is nothing to split, which the caller reads as "already emitted".
fn split_compound(run: &str) -> Vec<(usize, &str)> {
    let chars: Vec<(usize, char)> = run.char_indices().collect();
    let mut parts = Vec::new();
    let mut start = 0usize;

    for w in 1..chars.len() {
        let (i, cur) = chars[w];
        let (_, prev) = chars[w - 1];
        let boundary = (prev.is_lowercase() && cur.is_uppercase())
            || (prev.is_numeric() != cur.is_numeric())
            // `HTTPCode`: the break is before the last capital of a run of
            // them, not after it, or the part would be `HTTPC`.
            || (prev.is_uppercase()
                && cur.is_uppercase()
                && chars.get(w + 1).is_some_and(|(_, n)| n.is_lowercase()));
        if boundary {
            parts.push((start, &run[start..i]));
            start = i;
        }
    }
    parts.push((start, &run[start..]));
    parts
}

/// A token stream over a buffer that was filled before the first `advance`.
pub struct VecTokenStream {
    tokens: Vec<Token>,
    next: usize,
    current: Token,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.next >= self.tokens.len() {
            return false;
        }
        self.current = self.tokens[self.next].clone();
        self.next += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        tokens(s).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn a_plain_phrase_tokenises_one_word_at_a_time() {
        assert_eq!(texts("Net Revenue"), vec!["net", "revenue"]);
    }

    #[test]
    fn a_compound_is_indexed_whole_and_in_parts() {
        assert_eq!(texts("NetRevenue"), vec!["netrevenue", "net", "revenue"]);
        assert_eq!(texts("Sheet1"), vec!["sheet1", "sheet", "1"]);
        assert_eq!(texts("FY2024"), vec!["fy2024", "fy", "2024"]);
    }

    #[test]
    fn a_run_of_capitals_breaks_before_the_word_it_starts() {
        assert_eq!(texts("HTTPCode"), vec!["httpcode", "http", "code"]);
    }

    #[test]
    fn punctuation_separates_without_producing_empty_tokens() {
        assert_eq!(
            texts("'Q3 Sales'!$B$2"),
            vec!["q3", "q", "3", "sales", "b", "2"]
        );
        assert!(texts("=*!()").is_empty());
    }

    #[test]
    fn offsets_point_back_into_the_text() {
        let text = "Net Revenue";
        for token in tokens(text) {
            assert_eq!(
                text[token.offset_from..token.offset_to].to_lowercase(),
                token.text
            );
        }
    }

    #[test]
    fn a_token_too_long_to_be_a_word_is_dropped() {
        let long = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert!(texts(&long).is_empty());
    }

    #[test]
    fn non_ascii_words_survive_whole() {
        assert_eq!(
            texts("Chiffre d'affaires"),
            vec!["chiffre", "d", "affaires"]
        );
        assert_eq!(texts("Umsätze"), vec!["umsätze"]);
    }
}
