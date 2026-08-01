//! Text and native clipboard delivery via wtype.

use anyhow::{Context, Result};
use log::{debug, warn};
use tokio::process::Command;

use crate::clipboard::{self, PasteShortcut};
use crate::placeholder::TranscriptChunk;

/// Type the marker character into the active window.
pub async fn type_marker(marker: &str) -> Result<()> {
    debug!("typing marker");
    type_with_wtype(marker, "marker").await
}

/// Backspace to remove the marker character.
pub async fn backspace_marker() -> Result<()> {
    debug!("backspacing marker");
    run_wtype(&backspace_args(1), "marker backspace").await
}

/// Apply one realtime-tail replacement edit.
///
/// Backspaces and replacement text are sent through one `wtype` invocation so
/// their order is preserved. `backspaces` must count visible graphemes.
pub async fn apply_realtime_edit(backspaces: usize, text: &str) -> Result<()> {
    if backspaces == 0 && text.is_empty() {
        return Ok(());
    }

    debug!("applying realtime edit: {backspaces} backspaces");
    let mut command = Command::new("wtype");
    for arg in backspace_args(backspaces) {
        command.arg(arg);
    }
    if text.is_empty() {
        command.stdin(std::process::Stdio::null());
        run_command(command, "realtime edit").await
    } else {
        command.arg("-").stdin(std::process::Stdio::piped());
        run_command_with_text(command, text, "realtime edit").await
    }
}

/// Type plain transcript text into the active window.
pub async fn type_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    debug!("typing transcript");
    type_with_wtype(text, "text").await
}

/// Delivers transcript chunks in order, pasting the native clipboard at each
/// placeholder without reading or logging its payload.
pub async fn deliver_chunks(chunks: &[TranscriptChunk<'_>]) -> Result<()> {
    for chunk in chunks {
        match chunk {
            TranscriptChunk::Literal(text) => type_text(text).await?,
            TranscriptChunk::ClipboardPlaceholder => paste_clipboard().await?,
        }
    }
    Ok(())
}

async fn paste_clipboard() -> Result<()> {
    let shortcut = clipboard::paste_shortcut().await;
    run_wtype(paste_args(shortcut), "native paste").await
}

fn backspace_args(backspaces: usize) -> Vec<&'static str> {
    std::iter::repeat_n(["-k", "backspace"], backspaces)
        .flatten()
        .collect()
}

fn paste_args(shortcut: PasteShortcut) -> &'static [&'static str] {
    match shortcut {
        PasteShortcut::CtrlV => &["-M", "ctrl", "-k", "v", "-m", "ctrl"],
        PasteShortcut::CtrlShiftV => &[
            "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl",
        ],
    }
}

async fn type_with_wtype(text: &str, operation: &str) -> Result<()> {
    let mut command = Command::new("wtype");
    command.arg("-").stdin(std::process::Stdio::piped());
    run_command_with_text(command, text, operation).await
}

async fn run_wtype(args: &[&str], operation: &str) -> Result<()> {
    let mut command = Command::new("wtype");
    command.args(args).stdin(std::process::Stdio::null());
    run_command(command, operation).await
}

async fn run_command_with_text(mut command: Command, text: &str, operation: &str) -> Result<()> {
    let mut child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start wtype for {operation}"))?;
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .with_context(|| format!("failed to write wtype {operation}"))?;
    }
    wait_for_wtype(child, operation).await
}

async fn run_command(mut command: Command, operation: &str) -> Result<()> {
    let child = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start wtype for {operation}"))?;
    wait_for_wtype(child, operation).await
}

async fn wait_for_wtype(mut child: tokio::process::Child, operation: &str) -> Result<()> {
    let status = child
        .wait()
        .await
        .with_context(|| format!("failed waiting for wtype {operation}"))?;
    if !status.success() {
        warn!("wtype {operation} failed");
        anyhow::bail!("wtype {operation} failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_paste_command_sequences_are_exact() {
        assert_eq!(
            paste_args(PasteShortcut::CtrlV),
            ["-M", "ctrl", "-k", "v", "-m", "ctrl"]
        );
        assert_eq!(
            paste_args(PasteShortcut::CtrlShiftV),
            ["-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"]
        );
    }

    #[test]
    fn backspace_planner_repeats_one_key_per_grapheme() {
        assert_eq!(backspace_args(2), ["-k", "backspace", "-k", "backspace"]);
    }
}
