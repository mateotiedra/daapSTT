//! Pure state transitions for the mutable tail of realtime transcription.
//!
//! A caller applies a returned [`LiveTextEdit`] first, then replaces its state
//! with [`LiveTextTransition::next`] only if delivery succeeded.

use unicode_segmentation::UnicodeSegmentation;

/// The keystrokes needed to replace the visible mutable tail.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveTextEdit {
    /// Number of complete graphemes to delete with Backspace.
    pub backspaces: usize,
    /// Text to insert after the backspaces.
    pub insert: String,
}

/// An immutable state update paired with the edit that makes it visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveTextTransition {
    pub next: LiveText,
    pub edit: LiveTextEdit,
}

/// Tracks whether any text has been committed and the one editable visible tail.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveText {
    committed_any: bool,
    committed_text: String,
    tail: String,
}

impl LiveText {
    /// Creates state with no committed text or visible mutable tail.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether at least one non-empty segment has been committed.
    pub fn committed_any(&self) -> bool {
        self.committed_any
    }

    /// The full immutable text already delivered to the user.
    pub fn committed_text(&self) -> &str {
        &self.committed_text
    }

    /// The text currently considered mutable and visible to the user.
    pub fn tail(&self) -> &str {
        &self.tail
    }

    /// Replaces the mutable tail with a realtime partial.
    ///
    /// Once text was committed, a non-empty subsequent segment gets one leading
    /// space. An empty partial always removes the tail without adding a space.
    pub fn partial(&self, partial: &str) -> LiveTextTransition {
        self.replace_tail(segment_text(self.committed_any, partial))
    }

    /// Reconciles the mutable tail with a finalized segment and commits it.
    ///
    /// Empty final segments only clear the mutable tail; they do not establish a
    /// committed segment or a separator for a later segment.
    pub fn commit(&self, finalized: &str) -> LiveTextTransition {
        if finalized.is_empty() {
            return self.cleanup();
        }

        let target = segment_text(self.committed_any, finalized);
        let edit = tail_edit(&self.tail, &target);
        LiveTextTransition {
            next: Self {
                committed_any: true,
                committed_text: format!("{}{target}", self.committed_text),
                tail: String::new(),
            },
            edit,
        }
    }

    /// Rewrites committed text using a grapheme-safe common-prefix edit.
    ///
    /// Callers must first remove the mutable tail, then apply this edit only if
    /// the visible text is known to be safe to modify.
    pub fn rewrite_committed(&self, text: &str) -> LiveTextTransition {
        debug_assert!(self.tail.is_empty());
        LiveTextTransition {
            next: Self {
                committed_any: self.committed_any,
                committed_text: text.to_owned(),
                tail: String::new(),
            },
            edit: tail_edit(&self.committed_text, text),
        }
    }

    /// Removes only the mutable visible tail, retaining already committed text.
    pub fn cleanup(&self) -> LiveTextTransition {
        self.replace_tail(String::new())
    }

    fn replace_tail(&self, target: String) -> LiveTextTransition {
        let edit = tail_edit(&self.tail, &target);
        LiveTextTransition {
            next: Self {
                committed_any: self.committed_any,
                committed_text: self.committed_text.clone(),
                tail: target,
            },
            edit,
        }
    }
}

fn segment_text(committed_any: bool, text: &str) -> String {
    if text.is_empty() || !committed_any {
        text.to_owned()
    } else {
        format!(" {text}")
    }
}

fn tail_edit(old: &str, new: &str) -> LiveTextEdit {
    let old_graphemes: Vec<&str> = old.graphemes(true).collect();
    let new_graphemes: Vec<&str> = new.graphemes(true).collect();
    let common = old_graphemes
        .iter()
        .zip(&new_graphemes)
        .take_while(|(old, new)| old == new)
        .count();

    LiveTextEdit {
        backspaces: old_graphemes.len() - common,
        insert: new_graphemes[common..].concat(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_partial_inserts_only_new_suffix() {
        let state = LiveText::new();
        let first = state.partial("hello");
        assert_eq!(
            first.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: "hello".into()
            }
        );
        // Advance state only after this edit was delivered successfully.
        let state = first.next;

        let second = state.partial("hello world");
        assert_eq!(
            second.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: " world".into()
            }
        );
        assert_eq!(second.next.tail(), "hello world");
    }

    #[test]
    fn revised_partial_replaces_only_changed_suffix() {
        let state = LiveText::new().partial("hello there").next;
        let transition = state.partial("hello world");
        assert_eq!(transition.edit.backspaces, 5);
        assert_eq!(transition.edit.insert, "world");
    }

    #[test]
    fn edits_are_grapheme_safe_for_combining_and_emoji() {
        let combining = LiveText::new().partial("cafe\u{301}").next;
        let transition = combining.partial("café");
        assert_eq!(transition.edit.backspaces, 1);
        assert_eq!(transition.edit.insert, "é");

        let emoji = LiveText::new().partial("go 👨‍👩‍👧‍👦").next;
        let transition = emoji.partial("go 👍");
        assert_eq!(transition.edit.backspaces, 1);
        assert_eq!(transition.edit.insert, "👍");
    }

    #[test]
    fn empty_partial_clears_the_tail() {
        let state = LiveText::new().partial("draft").next;
        let transition = state.partial("");
        assert_eq!(
            transition.edit,
            LiveTextEdit {
                backspaces: 5,
                insert: String::new()
            }
        );
        assert_eq!(transition.next.tail(), "");
        assert!(!transition.next.committed_any());
    }

    #[test]
    fn commit_reconciles_then_makes_text_immutable() {
        let state = LiveText::new().partial("hel").next;
        let transition = state.commit("hello");
        assert_eq!(
            transition.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: "lo".into()
            }
        );
        assert!(transition.next.committed_any());
        assert_eq!(transition.next.tail(), "");
    }

    #[test]
    fn later_segments_have_exactly_one_leading_separator() {
        let state = LiveText::new().commit("first").next;
        let partial = state.partial("second");
        assert_eq!(
            partial.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: " second".into()
            }
        );
        let state = partial.next;

        let commit = state.commit("second segment");
        assert_eq!(
            commit.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: " segment".into()
            }
        );
        let state = commit.next;

        let next = state.commit("third");
        assert_eq!(
            next.edit,
            LiveTextEdit {
                backspaces: 0,
                insert: " third".into()
            }
        );
    }

    #[test]
    fn committed_rewrite_replaces_only_changed_grapheme_suffix() {
        let state = LiveText::new().commit("hello 👨‍👩‍👧‍👦 banana").next;
        let transition = state.rewrite_committed("hello 👨‍👩‍👧‍👦 clipboard");
        assert_eq!(transition.edit.backspaces, 6);
        assert_eq!(transition.edit.insert, "clipboard");
        assert_eq!(transition.next.committed_text(), "hello 👨‍👩‍👧‍👦 clipboard");
    }

    #[test]
    fn cleanup_removes_only_mutable_tail() {
        let state = LiveText::new().commit("kept").next.partial("discard").next;
        let transition = state.cleanup();
        assert_eq!(
            transition.edit,
            LiveTextEdit {
                backspaces: 8,
                insert: String::new()
            }
        );
        assert!(transition.next.committed_any());
        assert_eq!(transition.next.tail(), "");
    }
}
