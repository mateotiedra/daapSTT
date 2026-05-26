//! Transcription via Groq Whisper API.
//!
//! Sends WAV audio to `api.groq.com/openai/v1/audio/transcriptions`
//! and returns the transcribed text.
//!
//! # API
//!
//! Uses Groq's OpenAI-compatible audio transcription endpoint.
//! Free tier: 2,000 requests/day, 7,200 audio seconds/hour.
//! No credit card required.
//!
//! # Retry Policy
//!
//! Retries on transient failures (network timeout, 5xx) up to 2 times
//! with exponential backoff: 1s, then 2s.

use crate::config::Config;
use anyhow::{Context, Result};
use log::{debug, info, warn};
use reqwest::multipart::{Form, Part};
use std::time::Duration;

/// Maximum number of retry attempts for transient failures.
const MAX_RETRIES: u32 = 2;

/// Groq Transcription API response types.
#[derive(Debug)]
enum TranscriptionResponse {
    /// Successful transcription — plain text.
    Success(String),
    /// The audio contained no speech.
    NoSpeech,
}

/// Send audio data to the Groq Whisper API for transcription.
///
/// Takes a WAV buffer and returns the transcribed text.
/// Retries automatically on transient failures (5xx, network errors).
///
/// # Errors
///
/// Returns an error on:
/// - Network failures after retries exhausted
/// - API authentication errors (401)
/// - Rate limiting (429)
/// - Invalid audio or request (4xx, other than 429)
pub async fn transcribe(config: &Config, audio: &[u8]) -> Result<String> {
    // If the API URL is just a base URL (without the transcription endpoint path),
    // append the path automatically.
    let api_url = if config
        .groq_api_url
        .contains("/openai/v1/audio/transcriptions")
    {
        config.groq_api_url.clone()
    } else {
        format!(
            "{}/openai/v1/audio/transcriptions",
            config.groq_api_url.trim_end_matches('/')
        )
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    // Retry loop — build a fresh form on each attempt
    let mut last_error = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = Duration::from_secs(1 << (attempt - 1)); // 1s, 2s
            debug!("retry attempt {attempt} after {backoff:?}");
            tokio::time::sleep(backoff).await;
        }

        match try_transcribe(&client, &api_url, &config.groq_api_key, audio, config).await {
            Ok(TranscriptionResponse::Success(text)) => {
                let trimmed = text.trim();
                info!("transcription successful: {trimmed}");
                return Ok(trimmed.to_string());
            }
            Ok(TranscriptionResponse::NoSpeech) => {
                info!("transcription returned no speech");
                return Ok(String::new());
            }
            Err(e) => {
                let err_msg = e.to_string();
                // Only retry on transient errors
                if is_transient(&err_msg) && attempt < MAX_RETRIES {
                    warn!(
                        "transient error (attempt {}/{}): {err_msg}",
                        attempt + 1,
                        MAX_RETRIES + 1
                    );
                    last_error = Some(e);
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("transcription failed after retries")))
}

/// Attempt a single transcription request.
async fn try_transcribe(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    audio: &[u8],
    config: &Config,
) -> Result<TranscriptionResponse> {
    // Build multipart form for this attempt
    let audio_part = Part::bytes(audio.to_vec())
        .file_name("recording.wav")
        .mime_str("audio/wav")
        .context("failed to set MIME type for audio part")?;

    let form = Form::new()
        .part("file", audio_part)
        .text("model", config.model.clone())
        .text("language", config.language.clone())
        .text("response_format", config.response_format.clone());

    debug!("POST {url}");

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {api_key}"))
        .multipart(form)
        .send()
        .await
        .context("failed to send transcription request — network error?")?;

    let status = response.status();
    debug!("response status: {status}");

    match status.as_u16() {
        200 => {
            let text = response
                .text()
                .await
                .context("failed to read transcription response body")?;

            debug!("transcription response body: {text}");

            if text.trim().is_empty() {
                Ok(TranscriptionResponse::NoSpeech)
            } else {
                Ok(TranscriptionResponse::Success(text))
            }
        }
        401 => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow::anyhow!(
                "authentication failed (401) — check GROQ_API_KEY. Response: {body}"
            ))
        }
        429 => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow::anyhow!(
                "rate limited (429) — too many requests. Response: {body}"
            ))
        }
        code if code >= 500 => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow::anyhow!("Groq API server error ({code}): {body}"))
        }
        code => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow::anyhow!("Groq API error ({code}): {body}"))
        }
    }
}

/// Check if an error message indicates a transient failure worth retrying.
fn is_transient(error_msg: &str) -> bool {
    let msg = error_msg.to_lowercase();
    msg.contains("network error")
        || msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("server error (5")
        || msg.contains("503")
        || msg.contains("502")
        || msg.contains("504")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_transient() {
        assert!(is_transient(
            "failed to send transcription request — network error?"
        ));
        assert!(is_transient(
            "Groq API server error (503): service unavailable"
        ));
        assert!(is_transient("connection timed out"));
        assert!(!is_transient("authentication failed (401)"));
        assert!(!is_transient("rate limited (429)"));
        assert!(!is_transient("Groq API error (400): bad request"));
    }
}
