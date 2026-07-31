//! Minimal ElevenLabs Scribe v2 Realtime WebSocket transport.
//!
//! This module deliberately owns only transport concerns. Callers provide PCM
//! frames through an unbounded channel and consume immutable transcript events.
//! It does not reconnect, log credentials, or interpret partial transcripts.

use std::{fmt, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::HeaderValue, HeaderName},
        Message,
    },
};
use url::Url;

/// The maximum number of keyterms accepted by the realtime endpoint.
pub const MAX_KEYTERMS: usize = 50;
/// The maximum time spent waiting for the server after a final commit.
pub const FINALIZE_TIMEOUT: Duration = Duration::from_secs(3);

/// Settings used to establish one realtime session.
pub struct RealtimeConfig {
    /// WebSocket base endpoint, supplied by the caller (for example, `wss://...`).
    pub endpoint: String,
    /// ElevenLabs API key. It is sent only as the `xi-api-key` request header.
    pub api_key: String,
    /// Optional vocabulary hints. At most the first 50 are sent.
    pub keyterms: Vec<String>,
}

/// A caller-visible event from a realtime session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeEvent {
    SessionStarted,
    /// Text from the current uncommitted transcript, including empty text to clear a UI tail.
    PartialTranscript(String),
    /// Text from a committed transcript. Empty or whitespace-only text is omitted.
    CommittedTranscript(String),
    /// A transport, protocol, or provider error. The task then completes.
    Error(RealtimeError),
    /// The socket closed cleanly, or finalization timed out after its final commit.
    Completed,
}

/// Commands that can be sent to a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealtimeControl {
    /// Stop accepting audio, send a final commit, then wait briefly for completion.
    Finalize,
}

/// Error values are deliberately redacted: they never contain the endpoint or API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeError {
    InvalidEndpoint,
    InvalidApiKey,
    ConnectionFailed,
    WebSocketFailed,
    InvalidMessage,
    Provider {
        code: Option<String>,
        message: String,
    },
    TaskFailed,
}

impl fmt::Display for RealtimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint => f.write_str("invalid realtime endpoint"),
            Self::InvalidApiKey => f.write_str("invalid realtime API key"),
            Self::ConnectionFailed => f.write_str("realtime connection failed"),
            Self::WebSocketFailed => f.write_str("realtime WebSocket operation failed"),
            Self::InvalidMessage => f.write_str("invalid realtime server message"),
            Self::Provider { code, message } => match code {
                Some(code) => write!(f, "realtime provider error ({code}): {message}"),
                None => write!(f, "realtime provider error: {message}"),
            },
            Self::TaskFailed => f.write_str("realtime session task failed"),
        }
    }
}

impl std::error::Error for RealtimeError {}

/// Handle for one background realtime connection.
///
/// Receive [`RealtimeEvent`] values with [`Self::recv`], send a control command
/// with [`Self::send_control`], or call [`Self::finalize`] to request finalization
/// and wait for the bounded server-completion phase.
pub struct RealtimeSession {
    events: mpsc::UnboundedReceiver<RealtimeEvent>,
    control: mpsc::UnboundedSender<RealtimeControl>,
    // Retained so callers that need a bounded join can use `finalize`.
    #[allow(dead_code)]
    task: JoinHandle<()>,
}

impl RealtimeSession {
    /// Start a background WebSocket session. This returns before network I/O begins.
    pub fn start(
        config: RealtimeConfig,
        audio: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Result<Self, RealtimeError> {
        let endpoint = build_realtime_url(&config.endpoint, &config.keyterms)?;
        if config.api_key.trim().is_empty() {
            return Err(RealtimeError::InvalidApiKey);
        }

        let (events_tx, events) = mpsc::unbounded_channel();
        let (control, control_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_session(
            endpoint,
            config.api_key,
            audio,
            control_rx,
            events_tx,
        ));

        Ok(Self {
            events,
            control,
            task,
        })
    }

    /// Receive the next event, or `None` once the session task has ended.
    pub async fn recv(&mut self) -> Option<RealtimeEvent> {
        self.events.recv().await
    }

    /// Request a session action without waiting for the task to finish.
    pub fn send_control(&self, control: RealtimeControl) -> Result<(), RealtimeError> {
        self.control
            .send(control)
            .map_err(|_| RealtimeError::TaskFailed)
    }

    /// Send the final commit and wait for the task's bounded completion phase.
    #[allow(dead_code)]
    pub async fn finalize(&mut self) -> Result<(), RealtimeError> {
        self.send_control(RealtimeControl::Finalize)?;
        match timeout(FINALIZE_TIMEOUT + Duration::from_secs(1), &mut self.task).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RealtimeError::TaskFailed),
            Err(_) => Err(RealtimeError::TaskFailed),
        }
    }
}

/// Build the endpoint URL without putting credentials in it.
pub fn build_realtime_url(endpoint: &str, keyterms: &[String]) -> Result<Url, RealtimeError> {
    let mut url = Url::parse(endpoint).map_err(|_| RealtimeError::InvalidEndpoint)?;
    if !matches!(url.scheme(), "ws" | "wss") || url.host_str().is_none() {
        return Err(RealtimeError::InvalidEndpoint);
    }

    let mut query = url.query_pairs_mut();
    query.append_pair("model_id", "scribe_v2_realtime");
    query.append_pair("audio_format", "pcm_16000");
    query.append_pair("commit_strategy", "vad");
    for keyterm in keyterms.iter().take(MAX_KEYTERMS) {
        query.append_pair("keyterms", keyterm);
    }
    drop(query);
    Ok(url)
}

#[derive(Serialize)]
struct AudioChunk {
    message_type: &'static str,
    audio_base_64: String,
    #[serde(skip_serializing_if = "is_false")]
    commit: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// Serialize a raw PCM frame using the realtime protocol's exact field names.
pub fn serialize_audio_chunk(pcm: &[u8], commit: bool) -> Result<String, RealtimeError> {
    serde_json::to_string(&AudioChunk {
        message_type: "input_audio_chunk",
        audio_base_64: STANDARD.encode(pcm),
        commit,
    })
    .map_err(|_| RealtimeError::InvalidMessage)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IncomingMessage {
    SessionStarted,
    PartialTranscript(String),
    CommittedTranscript(String),
    ProviderError {
        code: Option<String>,
        message: String,
    },
    Other,
}

#[derive(Deserialize)]
struct WireMessage {
    message_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

const PROVIDER_ERROR_TYPES: &[&str] = &[
    "auth_error",
    "quota_exceeded",
    "transcriber_error",
    "input_error",
    "error",
    "commit_throttled",
    "unaccepted_terms",
    "rate_limited",
    "queue_overflow",
    "resource_exhausted",
    "session_time_limit_exceeded",
    "chunk_size_exceeded",
    "insufficient_audio_activity",
];

fn parse_incoming_message(payload: &str) -> Result<IncomingMessage, RealtimeError> {
    let message: WireMessage =
        serde_json::from_str(payload).map_err(|_| RealtimeError::InvalidMessage)?;
    Ok(match message.message_type.as_str() {
        "session_started" => IncomingMessage::SessionStarted,
        "partial_transcript" => {
            IncomingMessage::PartialTranscript(message.text.unwrap_or_default())
        }
        "committed_transcript" => {
            IncomingMessage::CommittedTranscript(message.text.unwrap_or_default())
        }
        message_type if PROVIDER_ERROR_TYPES.contains(&message_type) => {
            let nested = message
                .error
                .as_ref()
                .and_then(serde_json::Value::as_object);
            let nested_string = |field: &str| {
                nested
                    .and_then(|error| error.get(field))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            };
            let nested_error_text = message.error.as_ref().and_then(serde_json::Value::as_str);
            IncomingMessage::ProviderError {
                code: nested_string("code")
                    .or(message.code)
                    .or_else(|| Some(message_type.to_string())),
                message: nested_string("message")
                    .or(message.message)
                    .or(nested_error_text.map(str::to_owned))
                    .unwrap_or_else(|| "realtime service error".to_string()),
            }
        }
        _ => IncomingMessage::Other,
    })
}

async fn run_session(
    endpoint: Url,
    api_key: String,
    mut audio: mpsc::UnboundedReceiver<Vec<u8>>,
    mut controls: mpsc::UnboundedReceiver<RealtimeControl>,
    events: mpsc::UnboundedSender<RealtimeEvent>,
) {
    let mut request = match endpoint.as_str().into_client_request() {
        Ok(request) => request,
        Err(_) => return emit_error(&events, RealtimeError::InvalidEndpoint),
    };
    let api_key = match HeaderValue::from_str(&api_key) {
        Ok(api_key) => api_key,
        Err(_) => return emit_error(&events, RealtimeError::InvalidApiKey),
    };
    request
        .headers_mut()
        .insert(HeaderName::from_static("xi-api-key"), api_key);

    let (mut socket, _) = match connect_async(request).await {
        Ok(connection) => connection,
        Err(_) => return emit_error(&events, RealtimeError::ConnectionFailed),
    };

    let mut audio_closed = false;
    loop {
        tokio::select! {
            control = controls.recv() => {
                match control {
                    Some(RealtimeControl::Finalize) | None => {
                        finalize_socket(&mut socket, &events).await;
                        return;
                    }
                }
            }
            frame = audio.recv(), if !audio_closed => {
                match frame {
                    Some(pcm) => {
                        let payload = match serialize_audio_chunk(&pcm, false) {
                            Ok(payload) => payload,
                            Err(error) => return emit_error(&events, error),
                        };
                        match timeout(FINALIZE_TIMEOUT, socket.send(Message::Text(payload.into()))).await {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) | Err(_) => return emit_error(&events, RealtimeError::WebSocketFailed),
                        }
                    }
                    None => {
                        // The recorder has stopped. Wait for the caller's explicit
                        // Finalize control so it can finish draining queued events.
                        audio_closed = true;
                    }
                }
            }
            incoming = socket.next() => {
                if handle_socket_message(incoming, &events) == SocketOutcome::Ended {
                    return;
                }
            }
        }
    }
}

async fn finalize_socket<S>(socket: &mut S, events: &mpsc::UnboundedSender<RealtimeEvent>)
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let payload = match serialize_audio_chunk(&[], true) {
        Ok(payload) => payload,
        Err(error) => return emit_error(events, error),
    };
    match timeout(FINALIZE_TIMEOUT, socket.send(Message::Text(payload.into()))).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return emit_error(events, RealtimeError::WebSocketFailed),
        Err(_) => return emit_completed(events),
    }

    let deadline = Instant::now() + FINALIZE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return emit_completed(events);
        }
        match timeout(remaining, socket.next()).await {
            Ok(message) => match handle_socket_message(message, events) {
                // A VAD commit may already be queued when the final manual commit is
                // sent. Keep draining for the bounded completion window so that a
                // later final segment is not dropped.
                SocketOutcome::Committed | SocketOutcome::Continue => {}
                SocketOutcome::Ended => return,
            },
            Err(_) => return emit_completed(events),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketOutcome {
    Continue,
    Committed,
    Ended,
}

fn handle_socket_message(
    incoming: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    events: &mpsc::UnboundedSender<RealtimeEvent>,
) -> SocketOutcome {
    let message = match incoming {
        Some(Ok(Message::Text(payload))) => payload,
        Some(Ok(Message::Close(_))) | None => {
            emit_completed(events);
            return SocketOutcome::Ended;
        }
        Some(Ok(_)) => return SocketOutcome::Continue,
        Some(Err(_)) => {
            emit_error(events, RealtimeError::WebSocketFailed);
            return SocketOutcome::Ended;
        }
    };

    match parse_incoming_message(&message) {
        Ok(IncomingMessage::SessionStarted) => {
            let _ = events.send(RealtimeEvent::SessionStarted);
            SocketOutcome::Continue
        }
        Ok(IncomingMessage::PartialTranscript(text)) => {
            let _ = events.send(RealtimeEvent::PartialTranscript(text));
            SocketOutcome::Continue
        }
        Ok(IncomingMessage::Other) => SocketOutcome::Continue,
        Ok(IncomingMessage::CommittedTranscript(text)) => {
            if text.trim().is_empty() {
                SocketOutcome::Continue
            } else {
                let _ = events.send(RealtimeEvent::CommittedTranscript(text));
                SocketOutcome::Committed
            }
        }
        Ok(IncomingMessage::ProviderError { code, message }) => {
            emit_error(events, RealtimeError::Provider { code, message });
            SocketOutcome::Ended
        }
        Err(error) => {
            emit_error(events, error);
            SocketOutcome::Ended
        }
    }
}

fn emit_error(events: &mpsc::UnboundedSender<RealtimeEvent>, error: RealtimeError) {
    let _ = events.send(RealtimeEvent::Error(error));
    emit_completed(events);
}

fn emit_completed(events: &mpsc::UnboundedSender<RealtimeEvent>) {
    let _ = events.send(RealtimeEvent::Completed);
}

#[cfg(test)]
mod tests;
