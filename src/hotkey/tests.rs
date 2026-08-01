use super::*;

fn cycle_to_release(state: &mut HotkeyState, now: Instant) {
    assert_eq!(
        state.process(RawEvent::F24Down, now),
        Some(HotkeyEvent::Press)
    );
    assert_eq!(
        state.process(RawEvent::F24Up, now + Duration::from_millis(1)),
        Some(HotkeyEvent::ReleaseStarted)
    );
}

#[test]
fn direct_f24_cycle_completes_after_settling() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    cycle_to_release(&mut state, now);
    assert_eq!(state.settle(now + ALT_RESTORE_SETTLE_DURATION), None);
    assert_eq!(
        state.settle(now + Duration::from_millis(1) + ALT_RESTORE_SETTLE_DURATION),
        Some(HotkeyEvent::ReleaseCompleted)
    );
}

#[test]
fn space_before_alt_waits_for_restored_alt_to_be_released() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    cycle_to_release(&mut state, now);
    assert_eq!(
        state.process(RawEvent::KeydLeftAltDown, now + Duration::from_millis(2)),
        None
    );
    assert_eq!(
        state.settle(now + Duration::from_secs(1)),
        None,
        "an observed restored Alt must not use a fixed delay"
    );
    assert_eq!(
        state.process(RawEvent::KeydLeftAltUp, now + Duration::from_secs(1)),
        Some(HotkeyEvent::ReleaseCompleted)
    );
}

#[test]
fn alt_before_space_completes_after_settling() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    cycle_to_release(&mut state, now);
    // keyd emits no restored Alt-down when Alt was released first.
    assert_eq!(
        state.settle(now + Duration::from_millis(1) + ALT_RESTORE_SETTLE_DURATION),
        Some(HotkeyEvent::ReleaseCompleted)
    );
}

#[test]
fn key_repeats_are_ignored() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(raw_event(Key::KEY_F24, 2, false), None);
    assert_eq!(raw_event(Key::KEY_LEFTALT, 2, true), None);
    assert_eq!(raw_event(Key::KEY_LEFTALT, 1, false), None);
    assert_eq!(
        state.process(RawEvent::F24Down, now),
        Some(HotkeyEvent::Press)
    );
    assert_eq!(state.process(RawEvent::F24Down, now), None);
    assert_eq!(
        state.process(RawEvent::F24Up, now),
        Some(HotkeyEvent::ReleaseStarted)
    );
    assert_eq!(state.process(RawEvent::F24Up, now), None);
}

#[test]
fn mirrored_transitions_are_suppressed() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    cycle_to_release(&mut state, now);
    assert_eq!(state.process(RawEvent::KeydLeftAltUp, now), None);
    assert_eq!(state.process(RawEvent::KeydLeftAltDown, now), None);
    assert_eq!(state.process(RawEvent::KeydLeftAltDown, now), None);
    assert_eq!(
        state.process(RawEvent::KeydLeftAltUp, now),
        Some(HotkeyEvent::ReleaseCompleted)
    );
}

#[test]
fn cooldown_suppresses_mirrored_f24_sequence_then_retriggers() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    cycle_to_release(&mut state, now);
    assert_eq!(
        state.process(RawEvent::F24Down, now + Duration::from_millis(100)),
        None
    );
    assert_eq!(
        state.process(RawEvent::F24Up, now + Duration::from_millis(101)),
        None
    );
    assert_eq!(
        state.process(
            RawEvent::F24Down,
            now + Duration::from_millis(1) + COOLDOWN_DURATION,
        ),
        Some(HotkeyEvent::Press)
    );
}
