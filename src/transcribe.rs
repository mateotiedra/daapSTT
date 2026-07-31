//! Transcription via the ElevenLabs Scribe batch API.
//!
//! WAV audio is submitted to `POST /v1/speech-to-text` using Scribe v2.
//! Keyterms are loaded for every transcription and sent as repeated multipart
//! `keyterms` fields, as required for the API's array parameter.
//!
//! # Retry policy
//!
//! Retries only timeout/network transport failures and 5xx responses, up to
//! two times with backoff of one then two seconds.

use crate::config::Config;
use anyhow::{Context, Result};
use log::{debug, info, warn};
use reqwest::multipart::{Form, Part};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::Duration;

/// Maximum number of retries after the initial request.
const MAX_RETRIES: u32 = 2;
const MAX_KEYTERMS: usize = 1000;
const MAX_KEYTERM_CHARS: usize = 50;
const MAX_KEYTERM_WORDS: usize = 5;

#[derive(Deserialize)]
struct ScribeResponse {
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusClass {
    Authentication,
    RateLimited,
    Request,
    Server,
}

/// Send WAV audio to ElevenLabs Scribe and return the transcribed text.
///
/// # Errors
///
/// Returns an error for invalid credentials, rate limits, invalid requests,
/// provider errors, invalid responses, or transport failures after retries.
pub async fn transcribe(config: &Config, audio: &[u8]) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create HTTP client")?;

    // Keyterms are intentionally loaded for every request so edits take effect
    // without restarting the daemon.
    let keyterms = valid_keyterms(
        crate::keyterms::load(&config.keyterms_path).context("failed to load keyterms")?,
    );

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = Duration::from_secs(1 << (attempt - 1));
            debug!("retrying transcription after {backoff:?}");
            tokio::time::sleep(backoff).await;
        }

        match try_transcribe(
            &client,
            &config.elevenlabs_api_url,
            &config.elevenlabs_api_key,
            audio,
            &keyterms,
        )
        .await
        {
            Ok(text) => {
                info!("transcription successful");
                return Ok(crate::transcript::clean(&text));
            }
            Err(AttemptError::Transport(error)) if is_transient_transport(&error) => {
                if attempt < MAX_RETRIES {
                    warn!(
                        "transcription transport failure (attempt {}/{})",
                        attempt + 1,
                        MAX_RETRIES + 1
                    );
                    continue;
                }
                return Err(transport_error(&error));
            }
            Err(AttemptError::Transport(error)) => return Err(transport_error(&error)),
            Err(AttemptError::Status(status)) if is_transient_status(status) => {
                if attempt < MAX_RETRIES {
                    warn!(
                        "transcription server failure (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES + 1,
                        status.as_u16()
                    );
                    continue;
                }
                return Err(status_error(status));
            }
            Err(AttemptError::Status(status)) => return Err(status_error(status)),
            Err(AttemptError::InvalidResponse) => {
                return Err(anyhow::anyhow!(
                    "invalid transcription response from ElevenLabs"
                ));
            }
        }
    }

    unreachable!("the retry loop always returns")
}

enum AttemptError {
    Transport(reqwest::Error),
    Status(StatusCode),
    InvalidResponse,
}

/// Attempt one Scribe request without retrying.
async fn try_transcribe(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    audio: &[u8],
    keyterms: &[String],
) -> std::result::Result<String, AttemptError> {
    let audio_part = Part::bytes(audio.to_vec())
        .file_name("recording.wav")
        .mime_str("audio/wav")
        .map_err(AttemptError::Transport)?;

    // The API schema declares keyterms as a multipart array. Reqwest encodes
    // this as one `keyterms` part per item, matching the official SDK's list
    // submission rather than serializing the list as a JSON string.
    let mut form = Form::new()
        .part("file", audio_part)
        .text("model_id", "scribe_v2");
    for keyterm in keyterms {
        form = form.text("keyterms", keyterm.clone());
    }

    debug!("POST ElevenLabs speech-to-text endpoint");
    let response = client
        .post(url)
        .header("xi-api-key", api_key)
        .multipart(form)
        .send()
        .await
        .map_err(AttemptError::Transport)?;

    let status = response.status();
    debug!(
        "ElevenLabs speech-to-text response status: {}",
        status.as_u16()
    );
    if !status.is_success() {
        // Do not read or log provider error bodies: they may contain request data.
        return Err(AttemptError::Status(status));
    }

    let body = response.bytes().await.map_err(AttemptError::Transport)?;
    parse_transcription(&body).map_err(|_| AttemptError::InvalidResponse)
}

fn parse_transcription(body: &[u8]) -> Result<String, serde_json::Error> {
    serde_json::from_slice::<ScribeResponse>(body).map(|response| response.text)
}

fn classify_status(status: StatusCode) -> StatusClass {
    match status.as_u16() {
        401 => StatusClass::Authentication,
        429 => StatusClass::RateLimited,
        500..=599 => StatusClass::Server,
        _ => StatusClass::Request,
    }
}

fn is_transient_status(status: StatusCode) -> bool {
    classify_status(status) == StatusClass::Server
}

fn is_transient_transport(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

fn transport_error(error: &reqwest::Error) -> anyhow::Error {
    if error.is_timeout() {
        anyhow::anyhow!("transcription request timed out")
    } else {
        anyhow::anyhow!("transcription request failed due to a network error")
    }
}

fn status_error(status: StatusCode) -> anyhow::Error {
    match classify_status(status) {
        StatusClass::Authentication => anyhow::anyhow!(
            "authentication failed (401) — check ELEVENLABS_API_KEY in ~/.config/voice-daemon/env"
        ),
        StatusClass::RateLimited => anyhow::anyhow!("rate limited (429) — too many requests"),
        StatusClass::Server => anyhow::anyhow!("ElevenLabs server error ({})", status.as_u16()),
        StatusClass::Request => anyhow::anyhow!("ElevenLabs request error ({})", status.as_u16()),
    }
}

/// Keep only keyterms accepted by ElevenLabs and cap the list at its limit.
/// Invalid entries are ignored rather than making a recording fail.
fn valid_keyterms(keyterms: Vec<String>) -> Vec<String> {
    keyterms
        .into_iter()
        .filter(|term| {
            !term.is_empty()
                && term.chars().count() <= MAX_KEYTERM_CHARS
                && term.split_whitespace().count() <= MAX_KEYTERM_WORDS
                && !term
                    .chars()
                    .any(|c| matches!(c, '<' | '>' | '{' | '}' | '[' | ']' | '\\'))
        })
        .take(MAX_KEYTERMS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_from_scribe_response() {
        assert_eq!(
            parse_transcription(br#"{"text":"hello world"}"#).unwrap(),
            "hello world"
        );
        assert!(parse_transcription(br#"{"unexpected":"value"}"#).is_err());
        assert!(parse_transcription(b"not json").is_err());
    }

    #[test]
    fn classifies_provider_statuses() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            StatusClass::Authentication
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            StatusClass::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST),
            StatusClass::Request
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            StatusClass::Server
        );
    }

    #[test]
    fn retries_only_server_statuses() {
        assert!(is_transient_status(StatusCode::BAD_GATEWAY));
        assert!(!is_transient_status(StatusCode::UNAUTHORIZED));
        assert!(!is_transient_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_transient_status(StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[test]
    fn filters_and_limits_keyterms() {
        let too_long = "a".repeat(MAX_KEYTERM_CHARS + 1);
        let terms = valid_keyterms(vec![
            "valid term".to_string(),
            String::new(),
            too_long,
            "one two three four five six".to_string(),
            "invalid[term".to_string(),
        ]);
        assert_eq!(terms, vec!["valid term"]);

        let capped = valid_keyterms((0..MAX_KEYTERMS + 1).map(|n| format!("term{n}")).collect());
        assert_eq!(capped.len(), MAX_KEYTERMS);
    }
}
