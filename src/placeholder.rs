//! Parsing of the `banana` dictation placeholder.

/// An ordered portion of a transcript to deliver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TranscriptChunk<'a> {
    Literal(&'a str),
    ClipboardPlaceholder,
}

/// Splits standalone ASCII `banana` tokens into literal and clipboard chunks.
///
/// Matching is case-insensitive. Unicode alphanumeric characters and
/// underscores are word characters, so a token embedded in an identifier is
/// left literal.
pub fn parse_banana_chunks(text: &str) -> Vec<TranscriptChunk<'_>> {
    let mut chunks = Vec::new();
    let mut cursor = 0;

    while let Some(found) = find_banana(&text[cursor..]) {
        let start = cursor + found;
        let end = start + "banana".len();
        if start > cursor {
            chunks.push(TranscriptChunk::Literal(&text[cursor..start]));
        }
        chunks.push(TranscriptChunk::ClipboardPlaceholder);
        cursor = end;
    }
    if cursor < text.len() {
        chunks.push(TranscriptChunk::Literal(&text[cursor..]));
    }
    chunks
}

fn find_banana(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.len() < 6 {
        return None;
    }
    for start in 0..=bytes.len() - 6 {
        if !text.is_char_boundary(start) || !text.is_char_boundary(start + 6) {
            continue;
        }
        if bytes[start..start + 6].eq_ignore_ascii_case(b"banana")
            && !previous_is_word_char(text, start)
            && !next_is_word_char(text, start + 6)
        {
            return Some(start);
        }
    }
    None
}

fn previous_is_word_char(text: &str, index: usize) -> bool {
    text[..index].chars().next_back().is_some_and(is_word_char)
}

fn next_is_word_char(text: &str, index: usize) -> bool {
    text[index..].chars().next().is_some_and(is_word_char)
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_case_insensitive_tokens_in_order() {
        assert_eq!(
            parse_banana_chunks("BANANA, banana! (BaNaNa)"),
            vec![
                TranscriptChunk::ClipboardPlaceholder,
                TranscriptChunk::Literal(", "),
                TranscriptChunk::ClipboardPlaceholder,
                TranscriptChunk::Literal("! ("),
                TranscriptChunk::ClipboardPlaceholder,
                TranscriptChunk::Literal(")"),
            ]
        );
    }

    #[test]
    fn preserves_multiple_literals_and_multiline_order() {
        assert_eq!(
            parse_banana_chunks("before banana\nafter banana end"),
            vec![
                TranscriptChunk::Literal("before "),
                TranscriptChunk::ClipboardPlaceholder,
                TranscriptChunk::Literal("\nafter "),
                TranscriptChunk::ClipboardPlaceholder,
                TranscriptChunk::Literal(" end"),
            ]
        );
    }

    #[test]
    fn leaves_nonmatching_identifiers_literal() {
        assert_eq!(
            parse_banana_chunks("bananas banana_split ébanana bananaé"),
            vec![TranscriptChunk::Literal(
                "bananas banana_split ébanana bananaé"
            )]
        );
    }

    #[test]
    fn empty_input_has_no_chunks() {
        assert!(parse_banana_chunks("").is_empty());
    }
}
