use super::*;

#[test]
fn xwayland_discord_and_unknown_windows_use_safe_deferred_delivery() {
    let window = |class: &str, xwayland| crate::window::ActiveWindow {
        class: class.to_owned(),
        xwayland,
    };

    assert_eq!(
        delivery_for_active_window(Some(&window("discord", false))),
        RealtimeDelivery::Deferred
    );
    assert_eq!(
        delivery_for_active_window(Some(&window("DiScOrD", true))),
        RealtimeDelivery::Deferred
    );
    assert_eq!(
        delivery_for_active_window(Some(&window("Google-chrome", true))),
        RealtimeDelivery::Deferred
    );
    assert_eq!(
        delivery_for_active_window(Some(&window("chromium", true))),
        RealtimeDelivery::Deferred
    );
    assert_eq!(
        delivery_for_active_window(Some(&window("Google-chrome", false))),
        RealtimeDelivery::Live
    );
    assert_eq!(
        delivery_for_active_window(Some(&window("firefox", false))),
        RealtimeDelivery::Live
    );
    assert_eq!(delivery_for_active_window(None), RealtimeDelivery::Deferred);
}

#[test]
fn fallback_happens_after_a_provider_failure_before_any_commit() {
    assert_eq!(
        realtime_next_step(false, true, true, true),
        RealtimeNextStep::FallbackBatch
    );
}

#[test]
fn unsafe_delivery_failure_notifies_without_batch_fallback() {
    assert_eq!(
        realtime_next_step(false, true, false, true),
        RealtimeNextStep::NotifyFailure
    );
}

#[test]
fn failure_after_a_commit_notifies_without_batch_fallback() {
    assert_eq!(
        realtime_next_step(true, true, true, true),
        RealtimeNextStep::NotifyFailure
    );
}

#[test]
fn success_or_unusable_audio_needs_no_fallback() {
    assert_eq!(
        realtime_next_step(false, false, true, true),
        RealtimeNextStep::Done
    );
    assert_eq!(
        realtime_next_step(false, true, true, false),
        RealtimeNextStep::Done
    );
}

#[test]
fn placeholder_commit_preserves_the_prefix_before_banana() {
    let segment = raw_committed_segment(false, "prefix banana suffix");
    let chunks = placeholder::parse_banana_chunks(&segment);

    assert_eq!(clipboard_replacement_plan(&segment, &chunks), (13, 1, true));

    let segment = raw_committed_segment(true, "a\u{301} banana");
    let chunks = placeholder::parse_banana_chunks(&segment);
    assert_eq!(clipboard_replacement_plan(&segment, &chunks), (6, 1, true));
}

#[tokio::test]
async fn fallback_wait_requires_both_release_phases() {
    let (tx, mut rx) = mpsc::channel(2);
    tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
    tx.send(hotkey::HotkeyEvent::ReleaseCompleted)
        .await
        .unwrap();

    assert!(wait_for_release(Duration::from_secs(1), &mut rx, true).await);
}

#[tokio::test]
async fn fallback_wait_fails_closed_when_hotkey_channel_closes() {
    let (tx, mut rx) = mpsc::channel(1);
    drop(tx);

    assert!(!wait_for_release(Duration::from_secs(1), &mut rx, true).await);
}

#[tokio::test]
async fn fallback_wait_fails_closed_when_completion_channel_closes() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
    drop(tx);

    assert!(!wait_for_release(Duration::from_secs(1), &mut rx, true).await);
}

#[tokio::test]
async fn deferred_timeout_never_claims_keyboard_safety() {
    let (_tx, mut rx) = mpsc::channel(1);

    assert!(!wait_for_release(Duration::ZERO, &mut rx, false).await);
}

#[test]
fn finalization_ignores_only_provisional_partials() {
    assert!(!process_during_finalization(
        &realtime::RealtimeEvent::PartialTranscript("late preview".into())
    ));
    assert!(process_during_finalization(
        &realtime::RealtimeEvent::CommittedTranscript("final output".into())
    ));
    assert!(process_during_finalization(
        &realtime::RealtimeEvent::Error(realtime::RealtimeError::TaskFailed)
    ));
}
