//! Native clipboard paste shortcut selection.

use tokio::process::Command;

/// The keyboard shortcut used to paste the current Wayland clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteShortcut {
    CtrlV,
    CtrlShiftV,
}

/// How a native clipboard paste should be delivered and visually delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PastePlan {
    pub shortcut: PasteShortcut,
    pub is_image: bool,
}

/// Chooses a paste plan without ever reading clipboard contents.
///
/// Image clipboard data always uses the standard paste shortcut and space
/// delimiters. For non-image data, Kitty's terminal paste shortcut is used only
/// when the active window class is exactly `kitty`, ignoring ASCII case. A
/// missing active window class deliberately falls back to text with Ctrl+V.
pub fn select_paste_plan(clipboard_types: &str, active_class: Option<&str>) -> PastePlan {
    if clipboard_types
        .lines()
        .any(|mime_type| mime_type.trim().starts_with("image/"))
    {
        return PastePlan {
            shortcut: PasteShortcut::CtrlV,
            is_image: true,
        };
    }

    let is_kitty = active_class.is_some_and(|class| class.eq_ignore_ascii_case("kitty"));

    PastePlan {
        shortcut: if is_kitty {
            PasteShortcut::CtrlShiftV
        } else {
            PasteShortcut::CtrlV
        },
        is_image: false,
    }
}

/// Detects the native-paste plan for the current clipboard and active window.
/// Probe failures intentionally use the text Ctrl+V fallback.
pub async fn paste_plan() -> PastePlan {
    let clipboard_types = match Command::new("wl-paste").arg("--list-types").output().await {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout).ok(),
        _ => None,
    };

    let Some(clipboard_types) = clipboard_types else {
        return PastePlan {
            shortcut: PasteShortcut::CtrlV,
            is_image: false,
        };
    };
    if clipboard_types
        .lines()
        .any(|mime_type| mime_type.trim().starts_with("image/"))
    {
        return PastePlan {
            shortcut: PasteShortcut::CtrlV,
            is_image: true,
        };
    }

    let active_class = crate::window::active_class().await;
    select_paste_plan(&clipboard_types, active_class.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_mime_has_priority_over_kitty() {
        assert_eq!(
            select_paste_plan("text/plain\nimage/png\n", Some("kitty")),
            PastePlan {
                shortcut: PasteShortcut::CtrlV,
                is_image: true,
            }
        );
    }

    #[test]
    fn only_exact_kitty_class_uses_terminal_paste() {
        assert_eq!(
            select_paste_plan("text/plain", Some("KiTtY")),
            PastePlan {
                shortcut: PasteShortcut::CtrlShiftV,
                is_image: false,
            }
        );
        assert_eq!(
            select_paste_plan("text/plain", Some("kitty-terminal")),
            PastePlan {
                shortcut: PasteShortcut::CtrlV,
                is_image: false,
            }
        );
    }

    #[test]
    fn missing_or_non_kitty_active_class_falls_back() {
        for active_class in [None, Some("firefox")] {
            assert_eq!(
                select_paste_plan("text/plain", active_class),
                PastePlan {
                    shortcut: PasteShortcut::CtrlV,
                    is_image: false,
                }
            );
        }
    }
}
