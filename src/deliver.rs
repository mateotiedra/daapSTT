//! Text delivery via wtype keystroke simulation.
//!
//! Types the § marker character on recording start, backspaces it on stop,
//! and types the transcribed text into the active Wayland window.
//!
//! # Known Limitations
//!
//! - If the cursor moves between typing the marker and the transcript
//!   (e.g., user clicks elsewhere), the backspace will delete the wrong
//!   character. This is a v1 limitation — a future version could track
//!   the window under the cursor at press time.
//! - Text is typed at full speed with no inter-character delay. Some
//!   applications may drop keystrokes if they arrive too fast; this can
//!   be mitigated with the `-d` delay flag if needed.

use anyhow::{Context, Result};
use log::{debug, warn};
use tokio::process::Command;

/// Type the marker character into the active window.
///
/// Uses `wtype` text mode — types the raw string without key simulation,
/// which means any UTF-8 character works regardless of keyboard layout.
pub async fn type_marker(marker: &str) -> Result<()> {
    debug!("typing marker: {marker}");

    let mut output = Command::new("wtype")
        .arg("-") // read text from stdin to avoid shell escaping issues
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn wtype for marker")?;

    // Write the marker text to wtype's stdin
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = output.stdin.take() {
        stdin
            .write_all(marker.as_bytes())
            .await
            .context("failed to write marker to wtype stdin")?;
        // stdin is dropped here, which closes the pipe
    }

    let status = output
        .wait_with_output()
        .await
        .context("failed to wait for wtype marker")?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        warn!("wtype marker failed: {stderr}");
        return Err(anyhow::anyhow!("wtype marker failed: {stderr}"));
    }

    Ok(())
}

/// Backspace to remove the marker character.
///
/// Simulates a single Backspace key press via `wtype -k backspace`.
/// Uses named key resolution from libxkbcommon, which is
/// layout-independent.
pub async fn backspace_marker() -> Result<()> {
    debug!("backspacing marker");

    let output = Command::new("wtype")
        .arg("-k")
        .arg("backspace")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn wtype for backspace")?;

    let status = output
        .wait_with_output()
        .await
        .context("failed to wait for wtype backspace")?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        warn!("wtype backspace failed: {stderr}");
        return Err(anyhow::anyhow!("wtype backspace failed: {stderr}"));
    }

    Ok(())
}

/// Apply one realtime-tail replacement edit.
///
/// Backspaces and replacement text are sent through one `wtype` invocation so
/// their order is preserved. `backspaces` must count user-visible graphemes,
/// not bytes or Unicode scalar values.
pub async fn apply_realtime_edit(backspaces: usize, text: &str) -> Result<()> {
    if backspaces == 0 && text.is_empty() {
        return Ok(());
    }

    debug!(
        "applying realtime edit: {backspaces} backspaces, {} bytes inserted",
        text.len()
    );

    let mut command = Command::new("wtype");
    for _ in 0..backspaces {
        command.arg("-k").arg("backspace");
    }
    if text.is_empty() {
        command.stdin(std::process::Stdio::null());
    } else {
        command.arg("-").stdin(std::process::Stdio::piped());
    }

    let mut output = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn wtype for realtime edit")?;

    if !text.is_empty() {
        use tokio::io::AsyncWriteExt;
        if let Some(mut stdin) = output.stdin.take() {
            stdin
                .write_all(text.as_bytes())
                .await
                .context("failed to write realtime edit text to wtype stdin")?;
        }
    }

    let status = output
        .wait_with_output()
        .await
        .context("failed to wait for wtype realtime edit")?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        warn!("wtype realtime edit failed: {stderr}");
        return Err(anyhow::anyhow!("wtype realtime edit failed: {stderr}"));
    }

    Ok(())
}

/// Type the transcribed text into the active window.
///
/// Pipes the text through wtype's stdin to avoid shell escaping issues
/// with special characters. Text is typed as-is — no transformation
/// is applied.
pub async fn type_text(text: &str) -> Result<()> {
    debug!("typing transcript: {text}");

    let mut output = Command::new("wtype")
        .arg("-") // read text from stdin
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn wtype for text")?;

    // Write the text to wtype's stdin
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = output.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .context("failed to write text to wtype stdin")?;
        // stdin is dropped here, which closes the pipe
    }

    let status = output
        .wait_with_output()
        .await
        .context("failed to wait for wtype text")?;

    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        warn!("wtype text failed: {stderr}");
        return Err(anyhow::anyhow!("wtype text failed: {stderr}"));
    }

    Ok(())
}
