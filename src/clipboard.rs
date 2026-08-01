//! Wayland clipboard snapshotting for placeholder expansion.

use log::warn;
use tokio::process::Command;

/// Captures the text clipboard once. Any failure is intentionally represented as
/// an empty value, and clipboard contents are never logged.
pub async fn capture() -> String {
    match Command::new("wl-paste")
        .args(["--type", "text", "--no-newline"])
        .output()
        .await
    {
        Ok(output) if output.status.success() => normalize_output(output.stdout),
        _ => {
            warn!("failed to read clipboard");
            String::new()
        }
    }
}

fn normalize_output(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            warn!("failed to read clipboard");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_preserves_valid_multiline_text() {
        assert_eq!(normalize_output(b"one\ntwo\n".to_vec()), "one\ntwo\n");
    }

    #[test]
    fn normalization_rejects_non_utf8_output() {
        assert_eq!(normalize_output(vec![0xff]), "");
    }
}
