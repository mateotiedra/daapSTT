//! Configuration — reads settings from environment variables.
//!
//! `ELEVENLABS_API_KEY` is loaded from the environment. When running as a
//! systemd service, it is injected via
//! `EnvironmentFile=-%h/.config/voice-daemon/env`. For convenience, that file
//! is also loaded for `cargo run` when it exists.

use std::env;
use std::path::Path;

/// Application configuration.
pub struct Config {
    /// ElevenLabs API key (required).
    pub elevenlabs_api_key: String,
    /// ElevenLabs Speech-to-Text endpoint.
    pub elevenlabs_api_url: String,
    /// ElevenLabs Scribe realtime WebSocket endpoint.
    pub elevenlabs_realtime_url: String,
    /// Marker character typed on recording start.
    pub marker_char: String,
    /// Max recording duration in seconds.
    pub max_recording_secs: u64,
    /// Optional PipeWire target node ID for recording.
    /// When set, forces pw-record to use this specific input device.
    pub record_target: Option<String>,
    /// Whether to pause media players (via playerctl) when recording starts.
    /// When enabled, media is paused on hotkey press and resumed on release.
    pub pause_media: bool,
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// Automatically sources `~/.config/voice-daemon/env` when running
    /// outside of systemd, so `cargo run` works without manual `export`.
    ///
    /// # Errors
    ///
    /// Returns an error if `ELEVENLABS_API_KEY` is not set.
    pub fn from_env() -> anyhow::Result<Self> {
        // When not launched by systemd, try to load the env file automatically
        // so `cargo run` works without manual `export`.
        if env::var("INVOCATION_ID").is_err() {
            let env_path = dirs::config_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("voice-daemon")
                .join("env");
            if env_path.exists() {
                load_env_file(&env_path);
            }
        }

        let elevenlabs_api_key = env::var("ELEVENLABS_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ELEVENLABS_API_KEY is not set.\n\
                    \n\
                    Add ELEVENLABS_API_KEY=... to ~/.config/voice-daemon/env.\n\
                    That file is used by systemd and cargo run."
                )
            })?;

        Ok(Config {
            elevenlabs_api_key,
            elevenlabs_api_url: env::var("ELEVENLABS_API_URL")
                .unwrap_or_else(|_| "https://api.elevenlabs.io/v1/speech-to-text".to_string()),
            elevenlabs_realtime_url: env::var("ELEVENLABS_REALTIME_URL").unwrap_or_else(|_| {
                "wss://api.elevenlabs.io/v1/speech-to-text/realtime".to_string()
            }),
            marker_char: env::var("VOICE_MARKER_CHAR").unwrap_or_else(|_| "§".to_string()),
            max_recording_secs: env::var("VOICE_MAX_RECORDING_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
            record_target: env::var("VOICE_RECORD_TARGET").ok(),
            pause_media: env::var("VOICE_PAUSE_MEDIA")
                .ok()
                .map(|v| v.to_lowercase() != "false" && v != "0")
                .unwrap_or(true),
        })
    }
}

/// Parse a simple KEY=VALUE env file and set variables in the current process.
///
/// Ignores empty lines, lines starting with `#`, and lines without `=`.
/// Does NOT handle variable substitution or quoted values — matching systemd's
/// `EnvironmentFile=` semantics closely enough for our use case.
fn load_env_file(path: &Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if !key.is_empty() {
            env::set_var(key, value);
        }
    }
}
