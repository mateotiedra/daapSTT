//! Media player control via playerctl.
//!
//! Pauses all MPRIS-compatible media players (Spotify, browsers, VLC, etc.)
//! when recording starts, and resumes only the players that were actually
//! playing at the time of the pause.
//!
//! Uses `playerctl` under the hood — the standard MPRIS controller for Linux.
//! If playerctl is not installed, all operations are silent no-ops.

use log::{debug, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Tracks media state at pause time.
/// - `was_playing`: whether any player was playing (controls whether we pause)
/// - `playing_players`: names of specific players that were playing (controls resume)
pub struct MediaState {
    was_playing: Arc<AtomicBool>,
    playing_players: Arc<Mutex<Vec<String>>>,
}

impl MediaState {
    pub fn new() -> Self {
        Self {
            was_playing: Arc::new(AtomicBool::new(false)),
            playing_players: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// Check which MPRIS players are playing, pause them all,
/// and record both the playing status and specific player names.
pub async fn pause_all(state: &MediaState) {
    // Each recording gets a fresh snapshot. Otherwise a prior recording's
    // players could be resumed after a later recording that found none playing.
    state.was_playing.store(false, Ordering::Relaxed);
    state.playing_players.lock().await.clear();

    // 1. Get the full status output from all players.
    //    We use an explicit format string to get both player name and status,
    //    since the default output only includes statuses without names.
    let output = match Command::new("playerctl")
        .args([
            "--all-players",
            "status",
            "--format",
            "{{playerName}}\t{{status}}\n",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(out) => out,
        Err(e) => {
            debug!("playerctl not found or failed: {e}");
            return;
        }
    };

    if !output.status.success() {
        debug!(
            "playerctl status exited with {} — treating as not playing",
            output.status
        );
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // 2. Extract the names of players that are currently playing.
    let playing_names = parse_playing_names(&stdout);
    if playing_names.is_empty() {
        debug!("no media players are playing — nothing to pause");
        return;
    }

    // 3. Record this recording's players before pausing them.
    debug!("pausing: {:?}", playing_names);

    state.was_playing.store(true, Ordering::Relaxed);
    *state.playing_players.lock().await = playing_names;

    // 4. Fire-and-forget pause all players
    match Command::new("playerctl")
        .args(["--all-players", "pause"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
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

/// Resume media playback — only for players that were playing at pause time.
pub async fn resume(state: &MediaState) {
    // Consume the snapshot before issuing play commands so a second resume call
    // cannot replay a prior recording's players.
    let was_playing = state.was_playing.swap(false, Ordering::Relaxed);
    let players = std::mem::take(&mut *state.playing_players.lock().await);

    if !was_playing || players.is_empty() {
        return;
    }

    debug!("resuming: {:?}", players);

    for player_name in &players {
        match Command::new("playerctl")
            .args(["--player", player_name.as_str(), "play"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let name = player_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = child.wait().await {
                        debug!("playerctl play for '{name}' exited with error: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("failed to spawn playerctl play for '{player_name}': {e}");
            }
        }
    }
}

/// Parse the output of `playerctl --all-players status --format '...'`.
///
/// Format is tab-separated: `playerName\tstatus`, one per line.
/// Example:
/// ```text
/// firefox	Paused
/// spotify	Playing
/// ```
fn parse_playing_names(stdout: &str) -> Vec<String> {
    let mut playing = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Split on tab: "spotify\tPlaying" → ("spotify", "Playing")
        if let Some(tab) = line.find('\t') {
            let player = &line[..tab];
            let status = &line[tab + 1..];
            if status.eq_ignore_ascii_case("Playing") {
                playing.push(player.to_string());
            }
        }
    }

    playing
}

#[cfg(test)]
mod tests {
    use super::parse_playing_names;

    #[test]
    fn records_only_players_that_are_playing() {
        let output = "firefox\tPaused\nspotify\tPlaying\nvlc\tStopped\nmpv\tplaying\n";

        assert_eq!(parse_playing_names(output), ["spotify", "mpv"]);
    }
}
