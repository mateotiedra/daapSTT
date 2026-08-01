//! Native clipboard paste shortcut selection.

use tokio::process::Command;

/// The keyboard shortcut used to paste the current Wayland clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteShortcut {
    CtrlV,
    CtrlShiftV,
}

/// Chooses a paste shortcut without ever reading clipboard contents.
///
/// Image clipboard data always uses the standard paste shortcut. For non-image
/// clipboard data, Kitty's terminal paste shortcut is used only when the active
/// window class is exactly `kitty`, ignoring ASCII case. All missing or invalid
/// probe data deliberately falls back to the standard shortcut.
pub fn select_paste_shortcut(clipboard_types: &str, active_window: Option<&str>) -> PasteShortcut {
    if clipboard_types
        .lines()
        .any(|mime_type| mime_type.trim().starts_with("image/"))
    {
        return PasteShortcut::CtrlV;
    }

    let is_kitty = active_window
        .and_then(|window| serde_json::from_str::<serde_json::Value>(window).ok())
        .and_then(|window| window.get("class")?.as_str().map(str::to_owned))
        .is_some_and(|class| class.eq_ignore_ascii_case("kitty"));

    if is_kitty {
        PasteShortcut::CtrlShiftV
    } else {
        PasteShortcut::CtrlV
    }
}

/// Detects the native-paste shortcut for the current clipboard and active
/// window. Probe failures intentionally use Ctrl+V.
pub async fn paste_shortcut() -> PasteShortcut {
    let clipboard_types = match Command::new("wl-paste").arg("--list-types").output().await {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout).ok(),
        _ => None,
    };

    let Some(clipboard_types) = clipboard_types else {
        return PasteShortcut::CtrlV;
    };
    if clipboard_types
        .lines()
        .any(|mime_type| mime_type.trim().starts_with("image/"))
    {
        return PasteShortcut::CtrlV;
    }

    let active_window = match Command::new("hyprctl")
        .args(["-j", "activewindow"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout).ok(),
        _ => None,
    };
    select_paste_shortcut(&clipboard_types, active_window.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_mime_has_priority_over_kitty() {
        assert_eq!(
            select_paste_shortcut("text/plain\nimage/png\n", Some(r#"{"class":"kitty"}"#)),
            PasteShortcut::CtrlV
        );
    }

    #[test]
    fn only_exact_kitty_class_uses_terminal_paste() {
        assert_eq!(
            select_paste_shortcut("text/plain", Some(r#"{"class":"KiTtY"}"#)),
            PasteShortcut::CtrlShiftV
        );
        assert_eq!(
            select_paste_shortcut("text/plain", Some(r#"{"class":"kitty-terminal"}"#)),
            PasteShortcut::CtrlV
        );
    }

    #[test]
    fn invalid_or_missing_probe_data_falls_back() {
        assert_eq!(
            select_paste_shortcut("text/plain", None),
            PasteShortcut::CtrlV
        );
        assert_eq!(
            select_paste_shortcut("text/plain", Some("not json")),
            PasteShortcut::CtrlV
        );
    }
}
