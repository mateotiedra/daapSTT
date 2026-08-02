//! Voice Input Daemon — global push-to-talk dictation via ElevenLabs Scribe v2.

mod audio;
mod clipboard;
mod config;
mod deliver;
mod hotkey;
mod keyterms;
mod live_text;
mod media;
mod mic;
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
    output_mute_state: media::OutputMuteState,
    mic_mute_state: mic::MicMuteState,
}

impl RecordingState {
    fn new(marker_char: String) -> Self {
        Self {
            marker_active: false,
            marker_char,
            output_mute_state: media::OutputMuteState::new(),
            mic_mute_state: mic::MicMuteState::new(),
        }
    }

    pub(crate) async fn cleanup_marker(&mut self) {
        if self.marker_active {
            let _ = deliver::backspace_marker().await;
            self.marker_active = false;
        }
    }

    pub(crate) async fn mute_other_mic_apps(&mut self) {
        self.mic_mute_state.mute_other_apps().await;
    }

    /// Restore output sinks and other microphone capture streams for every
    /// recording exit. Both underlying snapshots are consumed, so this is safe
    /// to call from overlapping defensive cleanup paths.
    pub(crate) async fn restore_recording_audio(&mut self) {
        media::restore_muted_outputs(&mut self.output_mute_state).await;
        self.mic_mute_state.restore().await;
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
                Some(hotkey::HotkeyEvent::ReleaseStarted) => {
                    info!("ignoring hotkey release start without press")
                }
                Some(hotkey::HotkeyEvent::ReleaseCompleted) => {
                    info!("ignoring hotkey release completion without press")
                }
                None => { info!("hotkey channel closed"); break; }
            },
            _ = shutdown_notify.notified() => {
                info!("shutting down gracefully...");
                state.cleanup_marker().await;
                state.restore_recording_audio().await;
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
    if config.mute_audio_outputs {
        media::mute_unmuted_outputs(&mut state.output_mute_state).await;
    }
    if let Err(e) = deliver::type_marker(&state.marker_char).await {
        warn!("failed to type marker: {e}");
        state.restore_recording_audio().await;
        return false;
    }
    state.marker_active = true;
    if config.mute_other_mic_apps {
        // Snapshot before spawning our own pw-record stream so only other apps
        // are muted and the physical microphone remains available to daapSTT.
        state.mute_other_mic_apps().await;
    }
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
                state.restore_recording_audio().await;
                let _ = notify::error(
                    "Voice daemon",
                    "Failed to start recording — microphone not available?",
                )
                .await;
                return;
            }
        };

    let release_started = match tokio::time::timeout(max_dur, wait_for_release_started(hotkey_rx))
        .await
    {
        Ok(ReleaseStart::Started) => true,
        Ok(ReleaseStart::ChannelClosed) => {
            warn!("hotkey channel closed; skipping keyboard cleanup because modifier state is unknown");
            let _ = recording_handle.stop().await;
            state.restore_recording_audio().await;
            return;
        }
        Err(_) => {
            info!(
                "max recording duration ({}s) reached — auto-stopping",
                config.max_recording_secs
            );
            false
        }
    };

    info!("recording stopped — transcribing...");
    let audio_result = recording_handle.stop().await;
    state.restore_recording_audio().await;
    if release_started && !wait_for_release_completion(hotkey_rx).await {
        // A restored Alt may still be held. Do not send any wtype-backed cleanup.
        state.restore_recording_audio().await;
        return;
    }
    let audio_data = match audio_result {
        Ok(audio) => audio,
        Err(e) => {
            warn!("audio capture error: {e}");
            state.cleanup_marker().await;
            state.restore_recording_audio().await;
            let _ = notify::error("Voice daemon", "Audio capture failed").await;
            return;
        }
    };
    transcribe_batch(config, state, audio_data).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseStart {
    Started,
    ChannelClosed,
}

/// Waits for the first phase of a physical release, ignoring events that do
/// not belong to the active recording.
pub(crate) async fn wait_for_release_started(
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) -> ReleaseStart {
    loop {
        match hotkey_rx.recv().await {
            Some(hotkey::HotkeyEvent::ReleaseStarted) => return ReleaseStart::Started,
            Some(hotkey::HotkeyEvent::Press) => {
                warn!("ignoring repeated hotkey press while recording")
            }
            Some(hotkey::HotkeyEvent::ReleaseCompleted) => {
                warn!("ignoring stray hotkey release completion while recording")
            }
            None => return ReleaseStart::ChannelClosed,
        }
    }
}

/// Waits until keyd's possible restored Alt has been released. A closed
/// channel is unsafe: callers must not perform keyboard cleanup or delivery.
pub(crate) async fn wait_for_release_completion(
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) -> bool {
    loop {
        match hotkey_rx.recv().await {
            Some(hotkey::HotkeyEvent::ReleaseCompleted) => return true,
            Some(hotkey::HotkeyEvent::Press) => {
                warn!("ignoring repeated hotkey press while waiting for release completion")
            }
            Some(hotkey::HotkeyEvent::ReleaseStarted) => {
                warn!("ignoring repeated hotkey release start while waiting for completion")
            }
            None => {
                warn!("hotkey channel closed before release completion; keyboard state is unsafe");
                return false;
            }
        }
    }
}

pub(crate) async fn transcribe_batch(
    config: &config::Config,
    state: &mut RecordingState,
    audio_data: audio::AudioRecording,
) {
    if audio_data.data.len() < 800 {
        info!(
            "recording too short ({:.2}s) — likely silence, discarding",
            audio_data.duration_secs
        );
        state.cleanup_marker().await;
        state.restore_recording_audio().await;
        return;
    }
    if audio::is_silence(audio_data.peak_amplitude) {
        warn!(
            "recording silent (peak amp {}) — skipping transcription",
            audio_data.peak_amplitude
        );
        state.cleanup_marker().await;
        state.restore_recording_audio().await;
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
            state.restore_recording_audio().await;
        }
        Ok(text) => {
            let chunks = placeholder::parse_banana_chunks(&text);
            info!("transcription completed");
            state.cleanup_marker().await;
            let result = if chunks
                .iter()
                .any(|chunk| matches!(chunk, placeholder::TranscriptChunk::ClipboardPlaceholder))
            {
                deliver::deliver_chunks(&chunks).await
            } else {
                deliver::type_text(&text).await
            };
            if let Err(e) = result {
                warn!("failed to deliver transcription: {e}");
            }
            state.restore_recording_audio().await;
        }
        Err(e) => {
            let err_msg = e.to_string();
            warn!("transcription failed: {err_msg}");
            state.cleanup_marker().await;
            let _ = notify::error("Voice daemon", batch_error_notification(&err_msg)).await;
            state.restore_recording_audio().await;
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

    #[tokio::test]
    async fn release_start_wait_routes_stray_events_before_start() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(hotkey::HotkeyEvent::Press).await.unwrap();
        tx.send(hotkey::HotkeyEvent::ReleaseCompleted)
            .await
            .unwrap();
        tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();

        assert_eq!(
            wait_for_release_started(&mut rx).await,
            ReleaseStart::Started
        );
    }

    #[tokio::test]
    async fn release_start_wait_reports_channel_close() {
        let (tx, mut rx) = mpsc::channel(1);
        drop(tx);

        assert_eq!(
            wait_for_release_started(&mut rx).await,
            ReleaseStart::ChannelClosed
        );
    }

    #[tokio::test]
    async fn release_completion_wait_ignores_repeated_events() {
        let (tx, mut rx) = mpsc::channel(3);
        tx.send(hotkey::HotkeyEvent::Press).await.unwrap();
        tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
        tx.send(hotkey::HotkeyEvent::ReleaseCompleted)
            .await
            .unwrap();

        assert!(wait_for_release_completion(&mut rx).await);
    }

    #[tokio::test]
    async fn release_completion_wait_fails_closed_on_channel_close() {
        let (tx, mut rx) = mpsc::channel(1);
        drop(tx);

        assert!(!wait_for_release_completion(&mut rx).await);
    }
}
