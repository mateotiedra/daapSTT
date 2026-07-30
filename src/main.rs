//! Voice Input Daemon — global push-to-talk dictation via ElevenLabs Scribe v2.
//!
//! # Architecture
//!
//! ```text
//! evdev keyboard ──► hotkey.rs ──► (Press/Release events via channel)
//!                                      │
//!                              main.rs (orchestrator)
//!                                      │
//!                    ┌─────────────────┼─────────────────┐
//!                    ▼                 ▼                  ▼
//!              audio.rs         transcribe.rs        deliver.rs
//!           (pw-record)    (ElevenLabs API)        (wtype)
//!                    │                 │                  │
//!                    └─────────────────┴──────────────────┘
//!                                      │
//!                                notify.rs
//!                            (notify-send)
//! ```
//!
//! # Flow
//!
//! 1. User presses Alt+Space → `hotkey.rs` emits `Press`
//! 2. Orchestrator types `§` marker via `deliver.rs`
//! 3. Orchestrator starts `pw-record` via `audio.rs`
//! 4. User releases Alt+Space → `hotkey.rs` emits `Release`
//! 5. Orchestrator stops recording, gets WAV buffer
//! 6. Orchestrator sends WAV to ElevenLabs via `transcribe.rs`
//! 7. On success: backspace `§`, type transcript via `deliver.rs`
//! 8. On failure/silence: backspace `§`, notify via `notify.rs`

mod audio;
mod config;
mod deliver;
mod hotkey;
mod keyterms;
mod media;
mod notify;
mod transcribe;

use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

/// Wrapper to track recording session state.
struct RecordingState {
    /// Whether the § marker has been typed (and needs cleanup).
    marker_active: bool,
    /// Current marker character for cleanup purposes.
    marker_char: String,
    /// Tracks whether media was playing when recording started,
    /// so we can resume it when recording ends.
    media_state: media::MediaState,
}

impl RecordingState {
    fn new(marker_char: String) -> Self {
        Self {
            marker_active: false,
            marker_char,
            media_state: media::MediaState::new(),
        }
    }

    async fn cleanup_marker(&mut self) {
        if self.marker_active {
            let _ = deliver::backspace_marker().await;
            self.marker_active = false;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(command) = std::env::args().nth(1) {
        return run_command(command, std::env::args().skip(2));
    }

    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("voice-daemon starting");

    // Load configuration
    let config = config::Config::from_env()?;
    info!("configuration loaded");

    let mut state = RecordingState::new(config.marker_char.clone());

    // Shared shutdown signal — used by signal handler, main loop, and hotkey module
    let shutdown_notify = Arc::new(Notify::new());

    // Create hotkey event channel
    let (hotkey_tx, mut hotkey_rx) = mpsc::channel::<hotkey::HotkeyEvent>(32);

    // Spawn hotkey detection task
    let hotkey_shutdown = shutdown_notify.clone();
    let hotkey_handle = tokio::spawn(async move {
        if let Err(e) = hotkey::run(hotkey_tx, hotkey_shutdown).await {
            error!("hotkey detection failed: {e}");
        }
    });

    // Set up signal handling for graceful shutdown
    let shutdown_signal = shutdown_notify.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm =
                signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
            let mut sigint =
                signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");

            tokio::select! {
                _ = sigterm.recv() => info!("received SIGTERM"),
                _ = sigint.recv() => info!("received SIGINT"),
                _ = tokio::signal::ctrl_c() => info!("received Ctrl+C"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            info!("received Ctrl+C");
        }
        shutdown_signal.notify_waiters();
    });

    info!("listening for Alt+Space hotkey...");

    // Main event loop — wait for hotkey events or shutdown signal
    loop {
        tokio::select! {
            event = hotkey_rx.recv() => {
                match event {
                    Some(hotkey::HotkeyEvent::Press) => {
                        handle_press(&config, &mut state, &mut hotkey_rx).await;
                    }
                    Some(hotkey::HotkeyEvent::Release) => {
                        // Release without a preceding Press — ignore
                        info!("ignoring hotkey release without press");
                    }
                    None => {
                        info!("hotkey channel closed");
                        break;
                    }
                }
            }
            _ = shutdown_notify.notified() => {
                info!("shutting down gracefully...");
                // Clean up any active marker
                state.cleanup_marker().await;
                break;
            }
        }
    }

    // Signal the hotkey module to shut down via the shared Notify.
    // The signal handler already called notify_one() to wake us —
    // we call it again to wake the hotkey state machine.
    shutdown_notify.notify_one();
    drop(hotkey_rx);
    let _ = hotkey_handle.await;
    info!("voice-daemon shut down");
    Ok(())
}

fn run_command(command: String, args: impl Iterator<Item = String>) -> Result<()> {
    if command != "keyterms" {
        anyhow::bail!("unknown command: {command}\n\n{}", usage());
    }

    let args = args.collect::<Vec<_>>();
    match args.as_slice() {
        [] => keyterms::interactive(),
        [subcommand] if subcommand == "list" => {
            for term in keyterms::load()? {
                println!("{term}");
            }
            Ok(())
        }
        [subcommand, term] if subcommand == "add" => keyterms::add(term),
        [subcommand, term] if subcommand == "remove" => keyterms::remove(term),
        _ => anyhow::bail!("invalid keyterms command\n\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "Usage:\n  daapstt                 Start the voice daemon\n  daapstt keyterms        Manage keyterms interactively\n  daapstt keyterms list\n  daapstt keyterms add <term>\n  daapstt keyterms remove <term>"
}

/// Handle a press event: record, transcribe, deliver.
async fn handle_press(
    config: &config::Config,
    state: &mut RecordingState,
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) {
    info!("recording started");

    // Pause media players before recording starts
    if config.pause_media {
        media::pause_all(&state.media_state).await;
    }

    // Type marker character
    if let Err(e) = deliver::type_marker(&state.marker_char).await {
        warn!("failed to type marker: {e}");
        media::resume(&state.media_state).await;
        return;
    }
    state.marker_active = true;

    // Start audio capture
    let max_dur = Duration::from_secs(config.max_recording_secs);
    let recording_handle = match audio::start_recording(max_dur, config.record_target.as_deref()) {
        Ok(handle) => handle,
        Err(e) => {
            warn!("failed to start recording: {e}");
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
            let _ = notify::error(
                "Voice daemon",
                "Failed to start recording — microphone not available?",
            )
            .await;
            return;
        }
    };

    // Wait for the next event (should be Release) with timeout
    let release_received = tokio::time::timeout(
        Duration::from_secs(config.max_recording_secs),
        hotkey_rx.recv(),
    )
    .await;

    match release_received {
        Ok(Some(hotkey::HotkeyEvent::Release)) => {
            // Normal release — proceed to transcribe
        }
        Ok(Some(_other)) => {
            warn!("unexpected hotkey event while recording");
        }
        Ok(None) => {
            info!("hotkey channel closed, stopping");
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
            return;
        }
        Err(_elapsed) => {
            info!(
                "max recording duration ({}s) reached — auto-stopping",
                config.max_recording_secs
            );
        }
    }

    info!("recording stopped — transcribing...");

    // Stop recording and collect audio
    let audio_data = match recording_handle.stop().await {
        Ok(audio) => audio,
        Err(e) => {
            warn!("audio capture error: {e}");
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
            let _ = notify::error("Voice daemon", "Audio capture failed").await;
            return;
        }
    };

    // Check for silence / very short recording
    // WAV is 44-byte header + PCM data; < 800 bytes total ≈ < 0.02s of audio
    if audio_data.data.len() < 800 {
        info!(
            "recording too short ({:.2}s) — likely silence, discarding",
            audio_data.duration_secs
        );
        state.cleanup_marker().await;
        media::resume(&state.media_state).await;
        return;
    }

    // Check for silent audio (peak amplitude below threshold)
    if audio::is_silence(audio_data.peak_amplitude) {
        warn!(
            "recording silent (peak amp {}) — skipping transcription",
            audio_data.peak_amplitude
        );
        state.cleanup_marker().await;
        media::resume(&state.media_state).await;
        let _ = notify::error(
            "Voice daemon",
            "Microphone appears silent — check your input volume/source in PipeWire",
        )
        .await;
        return;
    }

    // Transcribe
    match transcribe::transcribe(config, &audio_data.data).await {
        Ok(text) if text.trim().is_empty() => {
            info!("empty transcription — silence");
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
        }
        Ok(text) => {
            info!("transcription: {text}");
            state.cleanup_marker().await;
            let _ = deliver::type_text(&text).await;
            media::resume(&state.media_state).await;
        }
        Err(e) => {
            let err_msg = e.to_string();
            warn!("transcription failed: {err_msg}");

            // Determine notification message based on error type
            let notify_msg = if err_msg.contains("rate limited") || err_msg.contains("429") {
                "Rate limited — try again shortly"
            } else if err_msg.contains("authentication") || err_msg.contains("401") {
                "Authentication failed — check ELEVENLABS_API_KEY"
            } else if err_msg.contains("network error") || err_msg.contains("timeout") {
                "Transcription failed — check network connection"
            } else {
                "Transcription failed"
            };

            state.cleanup_marker().await;
            let _ = notify::error("Voice daemon", notify_msg).await;
            media::resume(&state.media_state).await;
        }
    }
}
