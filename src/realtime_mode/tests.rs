use super::*;

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
fn placeholder_commit_backspaces_the_full_raw_segment() {
    assert_eq!(
        raw_committed_segment(false, "banana")
            .graphemes(true)
            .count(),
        6
    );
    assert_eq!(
        raw_committed_segment(true, "a\u{301} banana")
            .graphemes(true)
            .count(),
        9
    );
}

#[tokio::test]
async fn fallback_wait_requires_both_release_phases() {
    let (tx, mut rx) = mpsc::channel(2);
    tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
    tx.send(hotkey::HotkeyEvent::ReleaseCompleted)
        .await
        .unwrap();

    assert!(wait_for_release(Duration::from_secs(1), &mut rx).await);
}

#[tokio::test]
async fn fallback_wait_fails_closed_when_hotkey_channel_closes() {
    let (tx, mut rx) = mpsc::channel(1);
    drop(tx);

    assert!(!wait_for_release(Duration::from_secs(1), &mut rx).await);
}

#[tokio::test]
async fn fallback_wait_fails_closed_when_completion_channel_closes() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.send(hotkey::HotkeyEvent::ReleaseStarted).await.unwrap();
    drop(tx);

    assert!(!wait_for_release(Duration::from_secs(1), &mut rx).await);
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
