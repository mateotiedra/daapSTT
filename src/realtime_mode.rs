//! Realtime recording orchestration and live-text delivery.

use std::time::Duration;

use log::{info, warn};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;

use crate::live_text::LiveText;
use crate::{
    audio, config, deliver, hotkey, keyterms, notify, placeholder, realtime, transcribe_batch,
    RecordingState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RealtimeNextStep {
    Done,
    FallbackBatch,
    NotifyFailure,
}

fn realtime_next_step(
    committed_any: bool,
    failed: bool,
    tail_safe: bool,
    usable_audio: bool,
) -> RealtimeNextStep {
    if !failed {
        RealtimeNextStep::Done
    } else if !tail_safe || committed_any {
        RealtimeNextStep::NotifyFailure
    } else if usable_audio {
        RealtimeNextStep::FallbackBatch
    } else {
        RealtimeNextStep::Done
    }
}

pub(crate) async fn handle_realtime_press(
    config: &config::Config,
    state: &mut RecordingState,
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) {
    info!("realtime recording started");
    if !crate::begin_recording(config, state).await {
        return;
    }
    let max_dur = Duration::from_secs(config.max_recording_secs);
    let (pcm_tx, pcm_rx) = mpsc::unbounded_channel();
    let recording_handle =
        match audio::start_recording(max_dur, config.record_target.as_deref(), Some(pcm_tx)) {
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

    let realtime_config = match keyterms::load(&config.keyterms_path) {
        Ok(keyterms) => realtime::RealtimeConfig {
            endpoint: config.elevenlabs_realtime_url.clone(),
            api_key: config.elevenlabs_api_key.clone(),
            keyterms,
        },
        Err(e) => {
            warn!("failed to load realtime keyterms: {e}");
            if wait_for_release(max_dur, hotkey_rx).await {
                finish_realtime_without_session(config, state, recording_handle).await;
            } else {
                let _ = recording_handle.stop().await;
                state.restore_recording_audio().await;
            }
            return;
        }
    };
    let mut session = match realtime::RealtimeSession::start(realtime_config, pcm_rx) {
        Ok(session) => session,
        Err(e) => {
            warn!("failed to start realtime session: {e}");
            if wait_for_release(max_dur, hotkey_rx).await {
                finish_realtime_without_session(config, state, recording_handle).await;
            } else {
                let _ = recording_handle.stop().await;
                state.restore_recording_audio().await;
            }
            return;
        }
    };

    let mut live_text = LiveText::new();
    let mut failed = None;
    let mut tail_safe = true;
    let mut session_open = true;
    let mut release_started = false;
    let mut keyboard_safe = true;
    let timer = tokio::time::sleep(max_dur);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            biased;
            hotkey = hotkey_rx.recv() => match hotkey {
                Some(hotkey::HotkeyEvent::ReleaseStarted) => {
                    release_started = true;
                    break;
                }
                Some(hotkey::HotkeyEvent::ReleaseCompleted) => {
                    warn!("ignoring stray hotkey release completion while recording")
                }
                Some(hotkey::HotkeyEvent::Press) => warn!("ignoring repeated hotkey press while recording"),
                None => {
                    warn!("hotkey channel closed; modifier state is unknown");
                    keyboard_safe = false;
                    break;
                }
            },
            event = session.recv(), if session_open => match event {
                Some(event) => process_realtime_event(event, state, &mut live_text, &mut failed, &mut tail_safe).await,
                None => { session_open = false; if failed.is_none() { failed = Some(realtime::RealtimeError::TaskFailed); } }
            },
            _ = &mut timer => { info!("max recording duration ({}s) reached — auto-stopping", config.max_recording_secs); break; }
        }
    }

    let audio_result = recording_handle.stop().await;
    state.restore_recording_audio().await;
    if !keyboard_safe {
        state.restore_recording_audio().await;
        return;
    }
    if release_started && !crate::wait_for_release_completion(hotkey_rx).await {
        // The hotkey's modifier state is unknown, so never touch live text or
        // the marker through wtype.
        state.restore_recording_audio().await;
        return;
    }
    let audio_data = match audio_result {
        Ok(audio) => audio,
        Err(e) => {
            warn!("audio capture error: {e}");
            cleanup_live_tail(&mut live_text, tail_safe).await;
            state.cleanup_marker().await;
            state.restore_recording_audio().await;
            let _ = notify::error("Voice daemon", "Audio capture failed").await;
            return;
        }
    };
    if session
        .send_control(realtime::RealtimeControl::Finalize)
        .is_err()
        && failed.is_none()
    {
        failed = Some(realtime::RealtimeError::TaskFailed);
    }
    while session_open {
        match session.recv().await {
            // Partials are only a live preview. A late cumulative partial during
            // finalization would be inserted and immediately removed by cleanup.
            Some(event) if !process_during_finalization(&event) => {}
            Some(event) => {
                process_realtime_event(event, state, &mut live_text, &mut failed, &mut tail_safe)
                    .await
            }
            None => session_open = false,
        }
    }
    finish_realtime_result(config, state, audio_data, live_text, failed, tail_safe).await;
}

fn process_during_finalization(event: &realtime::RealtimeEvent) -> bool {
    !matches!(event, realtime::RealtimeEvent::PartialTranscript(_))
}

/// Returns false only when a release began but its completion cannot be
/// observed. In that case callers must avoid every keyboard operation.
async fn wait_for_release(
    max_dur: Duration,
    hotkey_rx: &mut mpsc::Receiver<hotkey::HotkeyEvent>,
) -> bool {
    match tokio::time::timeout(max_dur, crate::wait_for_release_started(hotkey_rx)).await {
        Ok(crate::ReleaseStart::Started) => crate::wait_for_release_completion(hotkey_rx).await,
        Ok(crate::ReleaseStart::ChannelClosed) => false,
        Err(_) => {
            info!("max recording duration reached — auto-stopping");
            true
        }
    }
}

async fn finish_realtime_without_session(
    config: &config::Config,
    state: &mut RecordingState,
    recording_handle: audio::RecordingHandle,
) {
    let audio_result = recording_handle.stop().await;
    state.restore_recording_audio().await;
    match audio_result {
        Ok(audio_data) => {
            finish_realtime_result(
                config,
                state,
                audio_data,
                LiveText::new(),
                Some(realtime::RealtimeError::TaskFailed),
                true,
            )
            .await
        }
        Err(e) => {
            warn!("audio capture error: {e}");
            state.cleanup_marker().await;
            state.restore_recording_audio().await;
            let _ = notify::error("Voice daemon", "Audio capture failed").await;
        }
    }
}

async fn process_realtime_event(
    event: realtime::RealtimeEvent,
    state: &mut RecordingState,
    live_text: &mut LiveText,
    failed: &mut Option<realtime::RealtimeError>,
    tail_safe: &mut bool,
) {
    match event {
        realtime::RealtimeEvent::SessionStarted | realtime::RealtimeEvent::Completed => {}
        realtime::RealtimeEvent::Error(error) => {
            warn!("realtime transcription failed");
            *failed = Some(error);
        }
        realtime::RealtimeEvent::PartialTranscript(text) => {
            if failed.is_none() {
                let text = crate::transcript::clean(&text);
                apply_live_text(state, live_text, &text, false, failed, tail_safe).await;
            }
        }
        realtime::RealtimeEvent::CommittedTranscript(text) => {
            if failed.is_none() {
                let text = crate::transcript::clean(&text);
                apply_live_text(state, live_text, &text, true, failed, tail_safe).await;
            }
        }
    }
}

async fn apply_live_text(
    state: &mut RecordingState,
    live_text: &mut LiveText,
    text: &str,
    committed: bool,
    failed: &mut Option<realtime::RealtimeError>,
    tail_safe: &mut bool,
) {
    if !text.is_empty() && !live_text.committed_any() && live_text.tail().is_empty() {
        state.cleanup_marker().await;
    }
    let committed_segment =
        committed.then(|| raw_committed_segment(live_text.committed_any(), text));
    let transition = if committed {
        live_text.commit(text)
    } else {
        live_text.partial(text)
    };
    if let Err(e) =
        deliver::apply_realtime_edit(transition.edit.backspaces, &transition.edit.insert).await
    {
        warn!("failed to deliver realtime transcript: {e}");
        *failed = Some(realtime::RealtimeError::TaskFailed);
        *tail_safe = false;
        return;
    }
    *live_text = transition.next;

    let Some(segment) = committed_segment else {
        return;
    };
    let chunks = placeholder::parse_banana_chunks(&segment);
    if !chunks
        .iter()
        .any(|chunk| matches!(chunk, placeholder::TranscriptChunk::ClipboardPlaceholder))
    {
        return;
    }

    // The raw committed segment is still at the cursor. Remove only that
    // segment, then redeliver its literals and native clipboard pastes. The
    // LiveText state remains raw so pasted content is never revisited later.
    if let Err(e) = deliver::apply_realtime_edit(segment.graphemes(true).count(), "").await {
        warn!("failed to prepare realtime clipboard paste: {e}");
        *failed = Some(realtime::RealtimeError::TaskFailed);
        *tail_safe = false;
        return;
    }
    if let Err(e) = deliver::deliver_chunks(&chunks).await {
        warn!("failed to deliver realtime clipboard paste: {e}");
        *failed = Some(realtime::RealtimeError::TaskFailed);
        *tail_safe = false;
    }
}

fn raw_committed_segment(already_committed: bool, text: &str) -> String {
    if already_committed {
        format!(" {text}")
    } else {
        text.to_owned()
    }
}

async fn cleanup_live_tail(live_text: &mut LiveText, tail_safe: bool) -> bool {
    if !tail_safe {
        return false;
    }
    let transition = live_text.cleanup();
    if transition.edit.backspaces == 0 && transition.edit.insert.is_empty() {
        return true;
    }
    if let Err(e) =
        deliver::apply_realtime_edit(transition.edit.backspaces, &transition.edit.insert).await
    {
        warn!("failed to clear realtime transcript tail: {e}");
        return false;
    }
    *live_text = transition.next;
    true
}

async fn finish_realtime_result(
    config: &config::Config,
    state: &mut RecordingState,
    audio_data: audio::AudioRecording,
    mut live_text: LiveText,
    failed: Option<realtime::RealtimeError>,
    tail_safe: bool,
) {
    let usable_audio =
        audio_data.data.len() >= 800 && !audio::is_silence(audio_data.peak_amplitude);
    if !usable_audio {
        cleanup_live_tail(&mut live_text, tail_safe).await;
        state.cleanup_marker().await;
        if audio::is_silence(audio_data.peak_amplitude) && audio_data.data.len() >= 800 {
            let _ = notify::error(
                "Voice daemon",
                "Microphone appears silent — check your input volume/source in PipeWire",
            )
            .await;
        }
        state.restore_recording_audio().await;
        return;
    }
    match realtime_next_step(
        live_text.committed_any(),
        failed.is_some(),
        tail_safe,
        usable_audio,
    ) {
        RealtimeNextStep::FallbackBatch => {
            if cleanup_live_tail(&mut live_text, tail_safe).await {
                transcribe_batch(config, state, audio_data).await;
            } else {
                state.cleanup_marker().await;
                let _ = notify::error(
                    "Voice daemon",
                    realtime_error_notification(failed.as_ref().expect("failure required")),
                )
                .await;
                state.restore_recording_audio().await;
            }
        }
        RealtimeNextStep::NotifyFailure => {
            cleanup_live_tail(&mut live_text, tail_safe).await;
            state.cleanup_marker().await;
            let _ = notify::error(
                "Voice daemon",
                realtime_error_notification(failed.as_ref().expect("failure required")),
            )
            .await;
            state.restore_recording_audio().await;
        }
        RealtimeNextStep::Done => {
            cleanup_live_tail(&mut live_text, tail_safe).await;
            state.cleanup_marker().await;
            state.restore_recording_audio().await;
        }
    }
}

fn realtime_error_notification(error: &realtime::RealtimeError) -> &'static str {
    match error {
        realtime::RealtimeError::InvalidApiKey => {
            "Authentication failed — check ELEVENLABS_API_KEY"
        }
        realtime::RealtimeError::ConnectionFailed | realtime::RealtimeError::WebSocketFailed => {
            "Realtime transcription failed — check network connection"
        }
        realtime::RealtimeError::Provider { code, .. }
            if matches!(code.as_deref(), Some("auth_error")) =>
        {
            "Authentication failed — check ELEVENLABS_API_KEY"
        }
        realtime::RealtimeError::Provider { code, .. }
            if matches!(
                code.as_deref(),
                Some(
                    "rate_limited"
                        | "quota_exceeded"
                        | "commit_throttled"
                        | "queue_overflow"
                        | "resource_exhausted"
                )
            ) =>
        {
            "Realtime service is rate limited — try again shortly"
        }
        _ => "Realtime transcription failed",
    }
}

#[cfg(test)]
mod tests;
