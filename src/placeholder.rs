//! Expansion of the `banana` dictation placeholder.

/// Replaces standalone ASCII `banana` tokens, case-insensitively.
///
/// Unicode alphanumeric characters and underscores are considered word
/// characters, so a placeholder embedded in an identifier is left unchanged.
pub fn replace_banana(text: &str, replacement: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(found) = find_banana(&text[cursor..]) {
        let start = cursor + found;
        let end = start + "banana".len();
        result.push_str(&text[cursor..start]);
        result.push_str(replacement);
        cursor = end;
    }
    result.push_str(&text[cursor..]);
    result
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
    fn replaces_case_insensitive_punctuation_and_multiple_tokens() {
        assert_eq!(
            replace_banana("BANANA, banana! (BaNaNa)", "clip"),
            "clip, clip! (clip)"
        );
    }

    #[test]
    fn does_not_replace_embedded_or_unicode_word_tokens() {
        assert_eq!(
            replace_banana("bananas banana_split ébanana bananaé", "clip"),
            "bananas banana_split ébanana bananaé"
        );
    }

    #[test]
    fn supports_empty_and_multiline_replacements() {
        assert_eq!(replace_banana("banana banana", ""), " ");
        assert_eq!(
            replace_banana("a banana\nb", "one\ntwo\n"),
            "a one\ntwo\n\nb"
        );
    }
}
