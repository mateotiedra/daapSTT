//! Configuration — reads settings from environment variables.
//!
//! The primary source is `GROQ_API_KEY` from the environment.
//! When running as a systemd service, this is injected via
//! `EnvironmentFile=-%h/.config/voice-daemon/env`.
//!
//! For convenience, when running outside systemd (e.g. `cargo run`),
//! the env file is also loaded automatically if it exists.

use std::env;
use std::path::Path;

/// Application configuration.
pub struct Config {
    /// Groq API key (required).
    pub groq_api_key: String,
    /// Groq API base URL.
    pub groq_api_url: String,
    /// Whisper model to use.
    pub model: String,
    /// Response format (text or json).
    pub response_format: String,
    /// Transcription language.
    pub language: String,
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
    /// Returns an error if `GROQ_API_KEY` is not set.
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

        let groq_api_key = env::var("GROQ_API_KEY").map_err(|_| {
            anyhow::anyhow!(
                "GROQ_API_KEY is not set.\n\
                \n\
                To fix this, either:\n\
                1. Set it inline: GROQ_API_KEY=gsk_... cargo run\n\
                2. Export it: export GROQ_API_KEY=gsk_...\n\
                3. Write it to ~/.config/voice-daemon/env (used by systemd and cargo run)\n\
                \n\
                Get a free key at https://console.groq.com/keys"
            )
        })?;

        Ok(Config {
            groq_api_key,
            groq_api_url: env::var("GROQ_API_URL").unwrap_or_else(|_| {
                "https://api.groq.com/openai/v1/audio/transcriptions".to_string()
            }),
            model: env::var("GROQ_MODEL").unwrap_or_else(|_| "whisper-large-v3-turbo".to_string()),
            response_format: env::var("GROQ_RESPONSE_FORMAT")
                .unwrap_or_else(|_| "text".to_string()),
            language: env::var("GROQ_LANGUAGE").unwrap_or_else(|_| "en".to_string()),
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
