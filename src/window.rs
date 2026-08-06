//! Active-window detection through Hyprland.

use tokio::process::Command;

/// Extracts the active window class from `hyprctl -j activewindow` output.
fn class_from_hyprctl(output: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()?
        .get("class")?
        .as_str()
        .map(str::to_owned)
}

/// Returns the active Hyprland window class when it can be determined.
pub async fn active_class() -> Option<String> {
    let output = Command::new("hyprctl")
        .args(["-j", "activewindow"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    class_from_hyprctl(&String::from_utf8(output.stdout).ok()?)
}

/// Whether a window class identifies the native Discord client.
pub fn is_discord_class(class: Option<&str>) -> bool {
    class.is_some_and(|class| class.eq_ignore_ascii_case("discord"))
}

/// Whether the active window is the native Discord client.
pub async fn active_is_discord() -> bool {
    let class = active_class().await;
    is_discord_class(class.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_valid_active_window_classes() {
        assert_eq!(
            class_from_hyprctl(r#"{"class":"discord","title":"channel"}"#),
            Some("discord".to_owned())
        );
        assert_eq!(class_from_hyprctl("not json"), None);
        assert_eq!(class_from_hyprctl(r#"{"title":"missing class"}"#), None);
    }

    #[test]
    fn only_exact_discord_class_is_deferred() {
        assert!(is_discord_class(Some("discord")));
        assert!(is_discord_class(Some("DiScOrD")));
        assert!(!is_discord_class(Some("discord-canary")));
        assert!(!is_discord_class(Some("vesktop")));
        assert!(!is_discord_class(None));
    }
}
