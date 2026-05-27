//! Media player control via playerctl.
//!
//! Pauses all MPRIS-compatible media players (Spotify, browsers, VLC, etc.)
//! when recording starts, and resumes them when recording ends.
//!
//! Uses `playerctl` under the hood — the standard MPRIS controller for Linux.
//! If playerctl is not installed, all operations are silent no-ops.

use log::{debug, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;

/// Tracks whether media was playing when we paused it,
/// so we only resume if we actually paused something.
pub struct MediaState {
    was_playing: Arc<AtomicBool>,
}

impl MediaState {
    pub fn new() -> Self {
        Self {
            was_playing: Arc::new(AtomicBool::new(false)),
        }
    }

    #[allow(dead_code)]
    pub fn was_playing(&self) -> bool {
        self.was_playing.load(Ordering::Relaxed)
    }
}

/// Check if any MPRIS player is currently playing, pause them if so,
/// and record the state for later resume.
pub async fn pause_all(state: &MediaState) {
    // 1. Check if anything is playing
    let playing = match check_if_playing().await {
        Ok(p) => p,
        Err(()) => return, // playerctl not available
    };

    if !playing {
        debug!("no media players are playing — nothing to pause");
        return;
    }

    debug!("media is playing — pausing all players");
    state.was_playing.store(true, Ordering::Relaxed);

    // 2. Fire-and-forget pause
    match Command::new("playerctl")
        .args(["--all-players", "pause"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Don't wait — let it run in background
            tokio::spawn(async move {
                if let Err(e) = child.wait().await {
                    debug!("playerctl pause exited with error: {e}");
                }
            });
        }
        Err(e) => {
            warn!("failed to spawn playerctl pause: {e}");
        }
    }
}

/// Resume media playback — only if we paused something earlier.
pub async fn resume(state: &MediaState) {
    if !state.was_playing.load(Ordering::Relaxed) {
        return; // nothing was playing when we paused
    }

    debug!("resuming media playback");

    match Command::new("playerctl")
        .args(["--all-players", "play"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            tokio::spawn(async move {
                if let Err(e) = child.wait().await {
                    debug!("playerctl play exited with error: {e}");
                }
            });
        }
        Err(e) => {
            warn!("failed to spawn playerctl play: {e}");
        }
    }
}

/// Run `playerctl --all-players status` and check if any player reports "Playing".
///
/// Returns `Err(())` if playerctl is not installed or fails to execute.
async fn check_if_playing() -> Result<bool, ()> {
    let output = match Command::new("playerctl")
        .args(["--all-players", "status"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            debug!("playerctl not found or failed: {e}");
            return Err(());
        }
    };

    if !output.status.success() {
        debug!(
            "playerctl status exited with {} — treating as not playing",
            output.status
        );
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.contains("Playing"))
}
