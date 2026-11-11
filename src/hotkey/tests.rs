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

#[test]
fn mouse_starts_at_half_second_and_releases_in_two_phases() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.advance(now + MOUSE_HOLD_DURATION - Duration::from_nanos(1)), None);
    let started = now + MOUSE_HOLD_DURATION;
    assert_eq!(state.advance(started), Some(HotkeyEvent::Press));
    assert_eq!(state.advance(started + Duration::from_secs(3)), None);
    let released = started + Duration::from_secs(4);
    assert_eq!(
        state.process(RawEvent::MouseUp(1), released),
        Some(HotkeyEvent::ReleaseStarted)
    );
    assert_eq!(state.advance(released), None);
    assert_eq!(
        state.advance(released + ALT_RESTORE_SETTLE_DURATION),
        Some(HotkeyEvent::ReleaseCompleted)
    );
    assert_eq!(state.advance(released + Duration::from_secs(1)), None);
}

#[test]
fn short_mouse_clicks_do_not_accumulate_and_repeats_do_not_restart_hold() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), now + Duration::from_millis(400)), None);
    assert_eq!(state.advance(now + MOUSE_HOLD_DURATION), None);
    let second = now + Duration::from_secs(2);
    assert_eq!(state.process(RawEvent::MouseDown(1), second), None);
    assert_eq!(state.process(RawEvent::MouseDown(1), second + Duration::from_millis(400)), None);
    assert_eq!(state.advance(second + Duration::from_millis(499)), None);
    assert_eq!(state.advance(second + MOUSE_HOLD_DURATION), Some(HotkeyEvent::Press));
}

#[test]
fn forward_codes_coalesce_and_key_repeats_do_not_rearm() {
    let mut buttons = ForwardButtons::default();
    assert_eq!(buttons.process(Key::BTN_EXTRA, 2, 7), None);
    assert_eq!(buttons.process(Key::BTN_EXTRA, 1, 7), Some(RawEvent::MouseDown(7)));
    assert_eq!(buttons.process(Key::BTN_EXTRA, 1, 7), None);
    assert_eq!(buttons.process(Key::BTN_FORWARD, 1, 7), None);
    assert_eq!(buttons.process(Key::BTN_EXTRA, 0, 7), None);
    assert_eq!(buttons.process(Key::BTN_FORWARD, 2, 7), None);
    assert_eq!(buttons.process(Key::BTN_FORWARD, 0, 7), Some(RawEvent::MouseUp(7)));
    assert_eq!(buttons.process(Key::BTN_FORWARD, 0, 7), None);
    assert_eq!(buttons.process(Key::BTN_FORWARD, 1, 7), Some(RawEvent::MouseDown(7)));
    assert_eq!(buttons.process(Key::BTN_FORWARD, 0, 7), Some(RawEvent::MouseUp(7)));
    assert_eq!(buttons.process(Key::BTN_LEFT, 1, 7), None);
}

#[test]
fn mouse_rearms_only_after_release_and_a_fresh_full_hold() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    for offset in [0, 3] {
        let down = now + Duration::from_secs(offset);
        assert_eq!(state.process(RawEvent::MouseDown(1), down), None);
        assert_eq!(state.advance(down + MOUSE_HOLD_DURATION), Some(HotkeyEvent::Press));
        let up = down + MOUSE_HOLD_DURATION + Duration::from_millis(1);
        assert_eq!(state.process(RawEvent::MouseUp(1), up), Some(HotkeyEvent::ReleaseStarted));
        assert_eq!(state.process(RawEvent::MouseUp(1), up), None);
        assert_eq!(state.advance(up + ALT_RESTORE_SETTLE_DURATION), Some(HotkeyEvent::ReleaseCompleted));
    }
}

#[test]
fn keyboard_cancels_pending_mouse_until_mouse_is_released() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    cycle_to_release(&mut state, now + Duration::from_millis(500));
    assert_eq!(state.advance(now + Duration::from_secs(1)), Some(HotkeyEvent::ReleaseCompleted));
    assert_eq!(state.advance(now + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseDown(1), now + Duration::from_secs(2)), None);
    assert_eq!(state.advance(now + Duration::from_secs(3)), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), now + Duration::from_secs(3)), None);
    assert_eq!(state.process(RawEvent::MouseDown(1), now + Duration::from_secs(4)), None);
    assert_eq!(state.advance(now + Duration::from_secs(5)), Some(HotkeyEvent::Press));
}

#[test]
fn mouse_pressed_during_keyboard_recording_cannot_stop_or_restart_it() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::F24Down, now), Some(HotkeyEvent::Press));
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.advance(now + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), now + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseDown(1), now + Duration::from_secs(2)), None);
    let up = now + Duration::from_secs(3);
    assert_eq!(state.process(RawEvent::F24Up, up), Some(HotkeyEvent::ReleaseStarted));
    assert_eq!(state.advance(up + ALT_RESTORE_SETTLE_DURATION), Some(HotkeyEvent::ReleaseCompleted));
    assert_eq!(state.advance(up + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), up + Duration::from_secs(2)), None);
}

#[test]
fn overlapping_keyboard_does_not_stop_mouse_and_restored_alt_remains_safe() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    let active = now + MOUSE_HOLD_DURATION;
    assert_eq!(state.advance(active), Some(HotkeyEvent::Press));
    assert_eq!(state.process(RawEvent::F24Down, active), None);
    assert_eq!(state.process(RawEvent::F24Up, active), None);
    assert_eq!(state.process(RawEvent::KeydLeftAltDown, active), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), active), Some(HotkeyEvent::ReleaseStarted));
    assert_eq!(state.advance(active + Duration::from_secs(2)), None);
    assert_eq!(
        state.process(RawEvent::KeydLeftAltUp, active + Duration::from_secs(2)),
        Some(HotkeyEvent::ReleaseCompleted)
    );
}

#[test]
fn mouse_release_does_not_transfer_recording_to_overlapping_held_keyboard() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    let active = now + MOUSE_HOLD_DURATION;
    assert_eq!(state.advance(active), Some(HotkeyEvent::Press));
    assert_eq!(state.process(RawEvent::F24Down, active), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), active), Some(HotkeyEvent::ReleaseStarted));
    assert_eq!(state.advance(active + ALT_RESTORE_SETTLE_DURATION), Some(HotkeyEvent::ReleaseCompleted));
    assert_eq!(state.process(RawEvent::F24Up, active + Duration::from_secs(1)), None);
    assert_eq!(
        state.process(RawEvent::F24Down, active + Duration::from_secs(2)),
        Some(HotkeyEvent::Press)
    );
}

#[test]
fn independent_mice_cannot_release_each_other_or_queue_a_second_recording() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.process(RawEvent::MouseDown(2), now + Duration::from_millis(500)), None);
    let active = now + MOUSE_HOLD_DURATION;
    assert_eq!(state.advance(active), Some(HotkeyEvent::Press));
    assert_eq!(state.process(RawEvent::MouseDisconnected(2), active), None);
    assert_eq!(state.process(RawEvent::MouseDown(3), active), None);
    assert_eq!(state.process(RawEvent::MouseDisconnected(1), active), Some(HotkeyEvent::ReleaseStarted));
    assert_eq!(state.advance(active + ALT_RESTORE_SETTLE_DURATION), Some(HotkeyEvent::ReleaseCompleted));
    assert_eq!(state.advance(active + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseUp(3), active + Duration::from_secs(2)), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), active + Duration::from_secs(2)), None);
}

#[test]
fn unplugging_pending_mouse_cancels_its_timer_and_replacement_starts_fresh() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.process(RawEvent::MouseDisconnected(1), now + Duration::from_millis(400)), None);
    assert_eq!(state.advance(now + MOUSE_HOLD_DURATION), None);
    let replaced = now + Duration::from_secs(2);
    assert_eq!(state.process(RawEvent::MouseDown(2), replaced), None);
    assert_eq!(state.advance(replaced + Duration::from_millis(499)), None);
    assert_eq!(state.advance(replaced + MOUSE_HOLD_DURATION), Some(HotkeyEvent::Press));
}

#[test]
fn mirrored_mouse_holds_emit_one_cycle_and_do_not_restart_after_release() {
    let now = Instant::now();
    let mut state = HotkeyState::default();
    assert_eq!(state.process(RawEvent::MouseDown(1), now), None);
    assert_eq!(state.process(RawEvent::MouseDown(2), now + Duration::from_millis(1)), None);
    let active = now + MOUSE_HOLD_DURATION + Duration::from_millis(2);
    assert_eq!(state.advance(active), Some(HotkeyEvent::Press));
    assert_eq!(state.advance(active), None);
    assert_eq!(state.process(RawEvent::MouseUp(2), active), None);
    assert_eq!(state.process(RawEvent::MouseUp(1), active), Some(HotkeyEvent::ReleaseStarted));
    assert_eq!(state.advance(active + ALT_RESTORE_SETTLE_DURATION), Some(HotkeyEvent::ReleaseCompleted));
    assert_eq!(state.advance(active + Duration::from_secs(2)), None);
}
