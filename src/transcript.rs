/// Cleans transcription artifacts without altering meaningful punctuation.
pub(crate) fn clean(text: &str) -> String {
    let mut text = text.trim();

    loop {
        let trimmed = text.trim_matches(|c| matches!(c, '"' | '“' | '”')).trim();
        if trimmed.len() == text.len() {
            break;
        }
        text = trimmed;
    }

    loop {
        let without_unicode_ellipsis = text.trim_end_matches('…');
        if without_unicode_ellipsis.len() != text.len() {
            text = without_unicode_ellipsis;
            continue;
        }

        let trailing_periods = text.bytes().rev().take_while(|&byte| byte == b'.').count();
        if trailing_periods >= 3 {
            text = text[..text.len() - trailing_periods].trim_end();
            continue;
        }

        let without_trailing_hyphens = text.trim_end_matches('-').trim_end();
        if without_trailing_hyphens.len() != text.len() {
            text = without_trailing_hyphens;
            continue;
        }

        break;
    }

    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn preserves_unchanged_prose_and_short_terminal_periods() {
        assert_eq!(clean("Hello, world!"), "Hello, world!");
        assert_eq!(clean("One period."), "One period.");
        assert_eq!(clean("Two periods.."), "Two periods..");
    }

    #[test]
    fn removes_boundary_whitespace_and_quote_wrappers() {
        assert_eq!(clean("  \"Hello\"  "), "Hello");
        assert_eq!(clean("“  Hello  ”"), "Hello");
        assert_eq!(clean("\"“Hello”\""), "Hello");
    }

    #[test]
    fn removes_terminal_ascii_and_unicode_ellipses() {
        assert_eq!(clean("unfinished..."), "unfinished");
        assert_eq!(clean("unfinished......"), "unfinished");
        assert_eq!(clean("unfinished……"), "unfinished");
        assert_eq!(clean("unfinished...…"), "unfinished");
    }

    #[test]
    fn removes_terminal_hyphens_and_their_preceding_space() {
        assert_eq!(clean("unfinished -"), "unfinished");
        assert_eq!(clean("unfinished---"), "unfinished");
        assert_eq!(clean("-"), "");
    }

    #[test]
    fn cleans_combined_wrappers_and_terminal_ellipsis() {
        assert_eq!(clean("“unfinished...”"), "unfinished");
    }

    #[test]
    fn handles_empty_and_punctuation_only_input() {
        assert_eq!(clean("   "), "");
        assert_eq!(clean("\"...\""), "");
        assert_eq!(clean("“…”"), "");
    }

    #[test]
    fn preserves_internal_quotes_single_quotes_and_internal_ellipses() {
        assert_eq!(clean("She said \"hello\"."), "She said \"hello\".");
        assert_eq!(clean("It's a speaker's note."), "It's a speaker's note.");
        assert_eq!(clean("Wait... what happened?"), "Wait... what happened?");
    }
}
