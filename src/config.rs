//! Configuration — reads settings from environment variables.
//!
//! The primary source is `GROQ_API_KEY` from the environment.
//! When running as a systemd service, this is injected via
//! `EnvironmentFile=-%h/.config/voice-daemon/env`.

use std::env;

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
}

impl Config {
    /// Load configuration from the environment.
    ///
    /// # Errors
    ///
    /// Returns an error if `GROQ_API_KEY` is not set.
    pub fn from_env() -> anyhow::Result<Self> {
        let groq_api_key = env::var("GROQ_API_KEY")
            .map_err(|_| anyhow::anyhow!("GROQ_API_KEY environment variable is not set. Get a key at https://console.groq.com/keys and add it to ~/.config/voice-daemon/env"))?;

        Ok(Config {
            groq_api_key,
            groq_api_url: env::var("GROQ_API_URL")
                .unwrap_or_else(|_| "https://api.groq.com/openai/v1/audio/transcriptions".to_string()),
            model: env::var("GROQ_MODEL")
                .unwrap_or_else(|_| "whisper-large-v3-turbo".to_string()),
            response_format: env::var("GROQ_RESPONSE_FORMAT")
                .unwrap_or_else(|_| "text".to_string()),
            language: env::var("GROQ_LANGUAGE")
                .unwrap_or_else(|_| "en".to_string()),
            marker_char: env::var("VOICE_MARKER_CHAR")
                .unwrap_or_else(|_| "§".to_string()),
            max_recording_secs: env::var("VOICE_MAX_RECORDING_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(60),
        })
    }
}
