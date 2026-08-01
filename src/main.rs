//! Voice Input Daemon — global push-to-talk dictation via ElevenLabs Scribe v2.

mod audio;
mod clipboard;
mod config;
mod deliver;
mod hotkey;
mod keyterms;
mod live_text;
mod media;
mod mode;
mod notify;
mod placeholder;
mod realtime;
mod realtime_mode;
mod transcribe;
mod transcript;

use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

pub(crate) struct RecordingState {
    marker_active: bool,
    marker_char: String,
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

    pub(crate) async fn cleanup_marker(&mut self) {
        if self.marker_active {
            let _ = deliver::backspace_marker().await;
            self.marker_active = false;
        }
    }

    pub(crate) async fn resume_media(&self) {
        media::resume(&self.media_state).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RealtimeCommand {
    On,
    Off,
    Status,
}

fn parse_realtime_command(args: &[String]) -> Result<RealtimeCommand> {
    match args {
        [action] if action == "on" => Ok(RealtimeCommand::On),
        [action] if action == "off" => Ok(RealtimeCommand::Off),
        [action] if action == "status" => Ok(RealtimeCommand::Status),
        _ => anyhow::bail!("invalid realtime command\n\n{}", usage()),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(command) = std::env::args().nth(1) {
        return run_command(command, std::env::args().skip(2));
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!("voice-daemon starting");

    let config = config::Config::from_env()?;
    // The mode is deliberately loaded once: mode changes restart the daemon.
    let operating_mode = mode::load()?;
    info!("configuration loaded; mode: {operating_mode}");

    let mut state = RecordingState::new(config.marker_char.clone());
    let shutdown_notify = Arc::new(Notify::new());
    let (hotkey_tx, mut hotkey_rx) = mpsc::channel::<hotkey::HotkeyEvent>(32);

    let hotkey_shutdown = shutdown_notify.clone();
    let hotkey_handle = tokio::spawn(async move {
        if let Err(e) = hotkey::run(hotkey_tx, hotkey_shutdown).await {
            error!("hotkey detection failed: {e}");
        }
    });

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
    loop {
        tokio::select! {
            event = hotkey_rx.recv() => match event {
                Some(hotkey::HotkeyEvent::Press) => match operating_mode {
                    mode::Mode::Batch => handle_batch_press(&config, &mut state, &mut hotkey_rx).await,
                    mode::Mode::Realtime => realtime_mode::handle_realtime_press(&config, &mut state, &mut hotkey_rx).await,
                },
                Some(hotkey::HotkeyEvent::Release) => info!("ignoring hotkey release without press"),
                None => { info!("hotkey channel closed"); break; }
            },
            _ = shutdown_notify.notified() => {
                info!("shutting down gracefully...");
                state.cleanup_marker().await;
                break;
            }
        }
    }

    shutdown_notify.notify_one();
    drop(hotkey_rx);
    let _ = hotkey_handle.await;
    info!("voice-daemon shut down");
    Ok(())
}

fn run_command(command: String, args: impl Iterator<Item = String>) -> Result<()> {
    let args = args.collect::<Vec<_>>();
    match command.as_str() {
        "keyterms" => {
            let path = config::keyterms_path();
            match args.as_slice() {
                [] => keyterms::interactive(&path),
                [subcommand] if subcommand == "list" => {
                    for term in keyterms::load(&path)? {
                        println!("{term}");
                    }
                    Ok(())
                }
                [subcommand, term] if subcommand == "add" => keyterms::add(&path, term),
                [subcommand, term] if subcommand == "remove" => keyterms::remove(&path, term),
                _ => anyhow::bail!("invalid keyterms command\n\n{}", usage()),
            }
        }
        "realtime" => match parse_realtime_command(&args)? {
            RealtimeCommand::On => mode::set_and_restart(mode::Mode::Realtime),
            RealtimeCommand::Off => mode::set_and_restart(mode::Mode::Batch),
            RealtimeCommand::Status => {
                println!("{}", mode::load()?);
                Ok(())
            }
        },
        _ => anyhow::bail!("unknown command: {command}\n\n{}", usage()),
    }
}

fn usage() -> &'static str {
    "Usage:\n  daapstt                 Start the voice daemon\n  daapstt keyterms        Manage keyterms interactively\n  daapstt keyterms list\n  daapstt keyterms add <term>\n  daapstt keyterms remove <term>\n  daapstt realtime on|off|status"
}

pub(crate) async fn begin_recording(config: &config::Config, state: &mut RecordingState) -> bool {
    if config.pause_media {
        media::pause_all(&state.media_state).await;
    }
    if let Err(e) = deliver::type_marker(&state.marker_char).await {
        warn!("failed to type marker: {e}");
        media::resume(&state.media_state).await;
        return false;
    }
    state.marker_active = true;
    true
}

/// The original batch path. Keep its capture and transcription behavior as the default.
async fn handle_batch_press(
    config: &config::Config,
    state: &mut RecordingState,
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) {
    info!("recording started");
    if !begin_recording(config, state).await {
        return;
    }
    let max_dur = Duration::from_secs(config.max_recording_secs);
    let recording_handle =
        match audio::start_recording(max_dur, config.record_target.as_deref(), None) {
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

    match tokio::time::timeout(max_dur, hotkey_rx.recv()).await {
        Ok(Some(hotkey::HotkeyEvent::Release)) => {}
        Ok(Some(_)) => warn!("unexpected hotkey event while recording"),
        Ok(None) => {
            info!("hotkey channel closed, stopping");
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
            return;
        }
        Err(_) => info!(
            "max recording duration ({}s) reached — auto-stopping",
            config.max_recording_secs
        ),
    }

    let clipboard = clipboard::capture().await;
    info!("recording stopped — transcribing...");
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
    transcribe_batch(config, state, audio_data, &clipboard).await;
}

pub(crate) async fn transcribe_batch(
    config: &config::Config,
    state: &mut RecordingState,
    audio_data: audio::AudioRecording,
    clipboard: &str,
) {
    if audio_data.data.len() < 800 {
        info!(
            "recording too short ({:.2}s) — likely silence, discarding",
            audio_data.duration_secs
        );
        state.cleanup_marker().await;
        media::resume(&state.media_state).await;
        return;
    }
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
    match transcribe::transcribe(config, &audio_data.data).await {
        Ok(text) if text.trim().is_empty() => {
            state.cleanup_marker().await;
            media::resume(&state.media_state).await;
        }
        Ok(text) => {
            let text = placeholder::replace_banana(&text, clipboard);
            info!("transcription completed");
            state.cleanup_marker().await;
            let _ = deliver::type_text(&text).await;
            media::resume(&state.media_state).await;
        }
        Err(e) => {
            let err_msg = e.to_string();
            warn!("transcription failed: {err_msg}");
            state.cleanup_marker().await;
            let _ = notify::error("Voice daemon", batch_error_notification(&err_msg)).await;
            media::resume(&state.media_state).await;
        }
    }
}

fn batch_error_notification(error: &str) -> &'static str {
    if error.contains("rate limited") || error.contains("429") {
        "Rate limited — try again shortly"
    } else if error.contains("authentication") || error.contains("401") {
        "Authentication failed — check ELEVENLABS_API_KEY"
    } else if error.contains("network error") || error.contains("timeout") {
        "Transcription failed — check network connection"
    } else {
        "Transcription failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_realtime_cli_actions() {
        assert_eq!(
            parse_realtime_command(&["on".into()]).unwrap(),
            RealtimeCommand::On
        );
        assert_eq!(
            parse_realtime_command(&["off".into()]).unwrap(),
            RealtimeCommand::Off
        );
        assert_eq!(
            parse_realtime_command(&["status".into()]).unwrap(),
            RealtimeCommand::Status
        );
        assert!(parse_realtime_command(&[]).is_err());
        assert!(parse_realtime_command(&["on".into(), "now".into()]).is_err());
    }
}
