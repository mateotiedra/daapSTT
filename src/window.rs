//! Active-window detection through Hyprland.

use log::debug;
use tokio::process::Command;

/// Extracts the active window class from `hyprctl -j activewindow` output.
fn class_from_hyprctl(output: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(output)
        .ok()?
        .get("class")?
        .as_str()
        .map(str::to_owned)
}

/// Selects the Hyprland instance serving this process's Wayland display.
///
/// `hyprctl activewindow` normally relies on `HYPRLAND_INSTANCE_SIGNATURE`,
/// but user services can start before Hyprland imports that variable into the
/// systemd manager. `hyprctl instances` does not have that requirement.
fn instance_from_hyprctl(output: &str, wayland_display: Option<&str>) -> Option<String> {
    let parsed = serde_json::from_str::<serde_json::Value>(output).ok()?;
    let instances = parsed
        .as_array()?
        .iter()
        .filter_map(|instance| {
            Some((
                instance.get("instance")?.as_str()?,
                instance.get("wl_socket")?.as_str()?,
            ))
        })
        .collect::<Vec<_>>();

    if let Some(wayland_display) = wayland_display.filter(|display| !display.is_empty()) {
        let mut matching = instances
            .iter()
            .filter(|(_, socket)| *socket == wayland_display);
        let selected = matching.next();
        if selected.is_some() && matching.next().is_none() {
            return selected.map(|(instance, _)| (*instance).to_owned());
        }
    }

    (instances.len() == 1).then(|| instances[0].0.to_owned())
}

async fn hyprctl_json(args: &[&str]) -> Option<String> {
    let output = Command::new("hyprctl").args(args).output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Returns the active Hyprland window class when it can be determined.
pub async fn active_class() -> Option<String> {
    if let Some(class) = hyprctl_json(&["-j", "activewindow"])
        .await
        .as_deref()
        .and_then(class_from_hyprctl)
    {
        return Some(class);
    }

    // A systemd user service may not inherit HYPRLAND_INSTANCE_SIGNATURE even
    // though its WAYLAND_DISPLAY is valid. Resolve the instance explicitly so
    // Discord detection keeps working after login and compositor restarts.
    debug!("resolving Hyprland instance without an inherited signature");
    let instances = hyprctl_json(&["-j", "instances"]).await?;
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let instance = instance_from_hyprctl(&instances, wayland_display.as_deref())?;
    let active_window = hyprctl_json(&["-j", "-i", &instance, "activewindow"]).await?;
    class_from_hyprctl(&active_window)
}

/// Whether a window class identifies the native Discord client.
pub fn is_discord_class(class: Option<&str>) -> bool {
    class.is_some_and(|class| class.eq_ignore_ascii_case("discord"))
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
    fn selects_instance_for_the_service_wayland_display() {
        let instances = r#"[
            {"instance":"first_signature","wl_socket":"wayland-0"},
            {"instance":"discord_signature","wl_socket":"wayland-1"}
        ]"#;

        assert_eq!(
            instance_from_hyprctl(instances, Some("wayland-1")),
            Some("discord_signature".to_owned())
        );
    }

    #[test]
    fn selects_the_only_instance_without_wayland_environment() {
        let instances = r#"[
            {"instance":"only_signature","wl_socket":"wayland-7"}
        ]"#;

        assert_eq!(
            instance_from_hyprctl(instances, None),
            Some("only_signature".to_owned())
        );
    }

    #[test]
    fn refuses_an_ambiguous_instance_fallback() {
        let instances = r#"[
            {"instance":"first_signature","wl_socket":"wayland-0"},
            {"instance":"second_signature","wl_socket":"wayland-2"}
        ]"#;

        assert_eq!(instance_from_hyprctl(instances, Some("wayland-1")), None);
        assert_eq!(instance_from_hyprctl("not json", Some("wayland-1")), None);
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
