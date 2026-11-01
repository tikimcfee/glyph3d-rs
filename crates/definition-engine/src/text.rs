//! Spans of a passage: the word a tap selects, and the context it is asked in.
//!
//! Every position is a byte offset into UTF-8 text, and every [`Span`] the
//! engine keeps has both ends on char boundaries ([`Span::checked`]). A tap
//! arrives as one byte (a glyph's first byte, from a pick), so
//! [`word_at`] takes a byte and returns the whole word around it.

use serde::{Deserialize, Serialize};

/// The longest context, in bytes on either side of a selection, that
/// [`context_of`] reaches for the sentence around it. A sentence longer than
/// this is cut at the nearest char boundary inside the limit.
pub const CONTEXT_REACH_BYTES: usize = 320;

/// A byte range of a passage, both ends on char boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// A span of `text`, refused unless `start <= end <= text.len()` and both
    /// ends are char boundaries.
    pub fn checked(text: &str, start: usize, end: usize) -> Result<Span, String> {
        if start > end || end > text.len() {
            return Err(format!("span {start}..{end} is outside a text of {} bytes", text.len()));
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(format!("span {start}..{end} splits a character"));
        }
        Ok(Span { start, end })
    }

    /// The span's text. Panics if the span does not fit `text`: every span
    /// the engine holds was checked against the text it is read from.
    pub fn of<'a>(&self, text: &'a str) -> &'a str {
        text.get(self.start..self.end)
            .unwrap_or_else(|| panic!("span {}..{} does not fit a text of {} bytes", self.start, self.end, text.len()))
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A char a word is made of.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A char that joins two word chars into one word ("don't", "well-known")
/// but never starts or ends one.
fn is_joiner(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '-')
}

/// The word containing byte `at`, or None when `at` is not inside a word
/// (whitespace, punctuation, past the end). A byte inside a multi-byte char
/// counts as that char.
pub fn word_at(text: &str, at: usize) -> Option<Span> {
    if at >= text.len() {
        return None;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let index = match chars.binary_search_by(|(offset, _)| offset.cmp(&at)) {
        Ok(index) => index,
        Err(after) => after - 1,
    };
    let word_char_at = |i: usize| chars.get(i).is_some_and(|&(_, c)| is_word_char(c));
    let in_word = |i: usize| {
        word_char_at(i) || (i > 0 && chars.get(i).is_some_and(|&(_, c)| is_joiner(c)) && word_char_at(i - 1) && word_char_at(i + 1))
    };
    if !in_word(index) {
        return None;
    }
    let mut first = index;
    while first > 0 && in_word(first - 1) {
        first -= 1;
    }
    let mut last = index;
    while in_word(last + 1) {
        last += 1;
    }
    let end = chars.get(last + 1).map_or(text.len(), |&(offset, _)| offset);
    Some(Span { start: chars[first].0, end })
}

/// The `nth` (1-based) whole-word occurrence of `word` in `text`: a match
/// that is exactly the word [`word_at`] finds there, so "art" does not match
/// inside "start".
pub fn find_word(text: &str, word: &str, nth: usize) -> Option<Span> {
    if word.is_empty() || nth == 0 {
        return None;
    }
    text.match_indices(word)
        .filter(|&(start, _)| word_at(text, start) == Some(Span { start, end: start + word.len() }))
        .nth(nth - 1)
        .map(|(start, _)| Span { start, end: start + word.len() })
}

/// The sentence around `span`: back to the previous sentence end (`.`, `!`
/// or `?` followed by whitespace), blank line or list item, forward through
/// the next, each side at most [`CONTEXT_REACH_BYTES`], whitespace trimmed.
/// A list item or heading is a sentence here: notes are written in them and
/// rarely end one with a period, so without that the context of a word in a
/// note was five bullets wide (measured on `definition-engine-root`). This is
/// what an expansion is asked in, so a word with several senses can be
/// answered for the one it has here.
pub fn context_of(text: &str, span: Span) -> Span {
    let mut start = span.start.saturating_sub(CONTEXT_REACH_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (span.end + CONTEXT_REACH_BYTES).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[start..span.start];
    if let Some(cut) = last_sentence_break(before) {
        start += cut;
    }
    let after = &text[span.end..end];
    if let Some(cut) = first_sentence_end(after) {
        end = span.end + cut;
    }
    let trimmed_start = start + (text[start..end].len() - text[start..end].trim_start().len());
    let trimmed_end = trimmed_start + text[trimmed_start..end].trim_end().len();
    Span { start: trimmed_start, end: trimmed_end.max(trimmed_start) }
}

/// The line starting at `line` opens a block of its own: a list item
/// (`-`, `*` or `+` and a space, or digits and `.` or `)` and a space) or a
/// heading (`#`), after any indentation.
fn opens_block(line: &[u8]) -> bool {
    let rest = &line[line.iter().take_while(|b| matches!(b, b' ' | b'\t')).count()..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    match rest {
        [b'-' | b'*' | b'+', b' ', ..] | [b'#', ..] => true,
        _ => digits > 0 && matches!(rest.get(digits..digits + 2), Some([b'.' | b')', b' '])),
    }
}

/// The byte just past the last sentence break in `before` (a terminator and
/// the whitespace after it, a blank line, or the newline before a list item
/// or heading), if any.
fn last_sentence_break(before: &str) -> Option<usize> {
    let bytes = before.as_bytes();
    (1..bytes.len()).rev().find_map(|i| {
        let terminator = matches!(bytes[i - 1], b'.' | b'!' | b'?') && bytes[i].is_ascii_whitespace();
        let blank_line = bytes[i - 1] == b'\n' && bytes[i] == b'\n';
        let block = bytes[i - 1] == b'\n' && opens_block(&bytes[i..]);
        if terminator {
            // A numbered item's own marker ("1. ") is not a sentence end:
            // the item starts at its line.
            let line_start = bytes[..i].iter().rposition(|&b| b == b'\n').map_or(0, |n| n + 1);
            let marker = bytes[line_start..i - 1].iter().all(|b| b.is_ascii_digit() || matches!(b, b' ' | b'\t'));
            Some(if marker && opens_block(&bytes[line_start..]) { line_start } else { i + 1 })
        } else if blank_line {
            Some(i + 1)
        } else if block {
            Some(i)
        } else {
            None
        }
    })
}

/// The byte just past the first sentence end in `after` (its terminator, or
/// the newline that starts a blank line, a list item or a heading), if any.
fn first_sentence_end(after: &str) -> Option<usize> {
    let bytes = after.as_bytes();
    (0..bytes.len()).find_map(|i| {
        let next_is_space = bytes.get(i + 1).is_none_or(|b| b.is_ascii_whitespace());
        if matches!(bytes[i], b'.' | b'!' | b'?') && next_is_space {
            Some(i + 1)
        } else if bytes[i] == b'\n' && (bytes.get(i + 1) == Some(&b'\n') || opens_block(&bytes[i + 1..])) {
            Some(i)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_anywhere_in_a_word_selects_all_of_it() {
        let text = "the well-known café don't";
        for at in 4..14 {
            assert_eq!(word_at(text, at).map(|s| s.of(text)), Some("well-known"), "at {at}");
        }
        let cafe = text.find("café").unwrap();
        // Byte inside the two-byte é counts as the é.
        assert_eq!(word_at(text, cafe + 4).map(|s| s.of(text)), Some("café"));
        assert_eq!(word_at(text, text.len() - 2).map(|s| s.of(text)), Some("don't"));
    }

    #[test]
    fn a_tap_between_words_selects_nothing() {
        let text = "a - b. 'c'";
        assert_eq!(word_at(text, 1), None, "space");
        assert_eq!(word_at(text, 2), None, "a lone hyphen joins nothing");
        assert_eq!(word_at(text, 5), None, "period");
        assert_eq!(word_at(text, 7), None, "a leading quote is not part of the word");
        assert_eq!(word_at(text, 8).map(|s| s.of(text)), Some("c"));
        assert_eq!(word_at(text, text.len()), None, "past the end");
    }

    #[test]
    fn find_word_matches_whole_words_only() {
        let text = "start art, art.";
        let second = find_word(text, "art", 2).unwrap();
        assert_eq!(second.start, 11);
        assert_eq!(find_word(text, "art", 1).unwrap().start, 6);
        assert_eq!(find_word(text, "art", 3), None);
        assert_eq!(find_word(text, "", 1), None);
    }

    #[test]
    fn context_is_the_sentence_the_word_stands_in() {
        let text = "First one. The bank of the river flooded! Then it dried.\n\nNext.";
        let bank = find_word(text, "bank", 1).unwrap();
        assert_eq!(context_of(text, bank).of(text), "The bank of the river flooded!");
        let next = find_word(text, "Next", 1).unwrap();
        assert_eq!(context_of(text, next).of(text), "Next.");
    }

    #[test]
    fn a_list_item_is_its_own_context() {
        let text = "- record agent transcript:\n- create map:\n    - program invocations, including parameters\n    1. numbered item\n# heading word";
        let span = find_word(text, "invocations", 1).unwrap();
        assert_eq!(context_of(text, span).of(text), "- program invocations, including parameters");
        let span = find_word(text, "numbered", 1).unwrap();
        assert_eq!(context_of(text, span).of(text), "1. numbered item");
        let span = find_word(text, "heading", 1).unwrap();
        assert_eq!(context_of(text, span).of(text), "# heading word");
        // A hyphen inside a line is not a list marker.
        let text = "a well - made line\ncontinues here.";
        let span = find_word(text, "continues", 1).unwrap();
        assert_eq!(context_of(text, span).of(text), "a well - made line\ncontinues here.");
    }

    #[test]
    fn context_of_a_long_sentence_is_cut_on_char_boundaries() {
        // Two-byte chars with no sentence break: the reach lands mid-char.
        let text = format!("{}word{}", "é ".repeat(200), " ü".repeat(200));
        let word = find_word(&text, "word", 1).unwrap();
        let context = context_of(&text, word);
        assert!(context.of(&text).contains("word"));
        assert!(context.end - context.start <= 2 * CONTEXT_REACH_BYTES + 4);
    }

    #[test]
    fn spans_refuse_split_characters_and_overruns() {
        let text = "é";
        assert!(Span::checked(text, 0, 1).is_err());
        assert!(Span::checked(text, 0, 3).is_err());
        assert!(Span::checked(text, 1, 0).is_err());
        assert!(Span::checked(text, 0, 2).is_ok());
    }
}
