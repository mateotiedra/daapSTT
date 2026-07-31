//! Hotkey detection via evdev.
//!
//! Monitors all keyboard devices in `/dev/input/event*` for F24 press
//! and release events. Sends events through a tokio channel to the
//! orchestrator.
//!
//! # Architecture
//!
//! - Device manager runs in a tokio task, periodically scanning for
//!   new keyboard devices and managing per-device reader threads.
//! - Each keyboard device gets a `spawn_blocking` thread that reads
//!   events via evdev's blocking API.
//! - Raw key events are sent through an mpsc channel to a state machine.
//! - The state machine emits `HotkeyEvent::Press` and `HotkeyEvent::Release`
//!   for F24 down and up, respectively.

use anyhow::Result;
use evdev::{Device, InputEventKind, Key};
use log::{debug, info, warn};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// A hotkey event: either a press (start recording) or release (stop recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RawEvent {
    F24Down,
    F24Up,
}

const COOLDOWN_DURATION: Duration = Duration::from_millis(200);

/// State for the modifier-free F24 hotkey.
#[derive(Default)]
struct HotkeyState {
    f24_pressed: bool,
    recording: bool,
    cooldown_until: Option<Instant>,
}

impl HotkeyState {
    /// Processes an event, ignoring repeats and duplicate transitions.
    fn process(&mut self, event: RawEvent, now: Instant) -> Option<HotkeyEvent> {
        match event {
            RawEvent::F24Down => {
                if self.f24_pressed {
                    return None;
                }
                self.f24_pressed = true;

                if self.recording || self.cooldown_until.is_some_and(|until| now < until) {
                    None
                } else {
                    self.recording = true;
                    self.cooldown_until = None;
                    Some(HotkeyEvent::Press)
                }
            }
            RawEvent::F24Up => {
                if !self.f24_pressed {
                    return None;
                }
                self.f24_pressed = false;

                if self.recording {
                    self.recording = false;
                    self.cooldown_until = Some(now + COOLDOWN_DURATION);
                    Some(HotkeyEvent::Release)
                } else {
                    None
                }
            }
        }
    }
}

/// Start monitoring keyboard devices for F24 events.
///
/// Sends `HotkeyEvent::Press` on F24 down and `HotkeyEvent::Release` on F24 up.
///
/// The `shutdown` Notify is triggered by the caller to signal graceful
/// shutdown — the state machine and manager use it to exit cleanly.
pub async fn run(tx: mpsc::Sender<HotkeyEvent>, shutdown: Arc<Notify>) -> Result<()> {
    let (raw_tx, mut raw_rx) = mpsc::channel::<RawEvent>(128);

    // Spawn the device manager that keeps track of keyboard devices
    let shutdown_clone = shutdown.clone();
    let manager_handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Track (path → JoinHandle) for active device readers
        let mut active_readers: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();

        loop {
            // Re-enumerate keyboard devices
            let devices = match find_keyboards() {
                Ok(d) => d,
                Err(e) => {
                    warn!("failed to enumerate keyboards: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Clean up finished reader threads
            active_readers.retain(|_, h| !h.is_finished());

            // Collect current device paths
            let current_paths: HashSet<PathBuf> = devices.iter().map(|d| d.path.clone()).collect();

            // Start readers for NEW devices (not already monitored)
            for device_info in devices {
                if active_readers.contains_key(&device_info.path) {
                    continue; // already monitoring this device
                }

                let raw_tx = raw_tx.clone();
                let path = device_info.path.clone();
                let path_for_closure = path.clone();

                let handle = tokio::task::spawn_blocking(move || {
                    read_keyboard_events(&path_for_closure, raw_tx);
                });
                active_readers.insert(path, handle);
            }

            // Remove stale entries for disconnected devices
            let disconnected: Vec<PathBuf> = active_readers
                .keys()
                .filter(|p| !current_paths.contains(*p))
                .cloned()
                .collect();
            for path in disconnected {
                info!("device disconnected: {path:?}");
                active_readers.remove(&path);
            }

            if current_paths.is_empty() {
                warn!("no keyboard devices found, retrying in 5s");
            } else {
                debug!("monitoring {} keyboard device(s)", active_readers.len());
            }

            // Wait before re-scanning for new devices
            tokio::select! {
                _ = shutdown_clone.notified() => {
                    info!("hotkey manager shutting down");
                    drop(active_readers); // drop all handles
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    // Time to re-scan
                }
            }
        }
    });

    // State machine: processes raw key events and emits HotkeyEvents.
    // Also listens for the external shutdown signal to exit cleanly.
    let state_shutdown = shutdown.clone();
    let state_handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Suppress mirrored F24 sequences from duplicate virtual keyboard devices.
        let mut state = HotkeyState::default();

        loop {
            let event = tokio::select! {
                event = raw_rx.recv() => event,
                _ = state_shutdown.notified() => {
                    info!("state machine received shutdown signal");
                    break;
                }
            };

            let Some(event) = event else {
                // raw_rx channel closed — device threads exited
                info!("raw event channel closed");
                break;
            };

            if let Some(hotkey_event) = state.process(event, Instant::now()) {
                if tx.send(hotkey_event).await.is_err() {
                    break;
                }
            }
        }

        info!("hotkey state machine exiting");
        Ok(())
    });

    // Wait for the state machine (runs forever until channel closes)
    state_handle.await??;

    // Signal shutdown
    shutdown.notify_one();
    let _ = manager_handle.await;

    Ok(())
}

/// Information about a discovered keyboard device.
#[derive(Debug, Clone)]
struct KeyboardInfo {
    path: PathBuf,
}

/// Find all keyboard devices in /dev/input/.
///
/// Detects keyboards by checking if the device supports KEY_F24.
fn find_keyboards() -> Result<Vec<KeyboardInfo>> {
    let mut keyboards = Vec::new();

    for (path, device) in evdev::enumerate() {
        // Check if this device supports keyboard keys
        if let Some(keys) = device.supported_keys() {
            if keys.contains(Key::KEY_F24) {
                keyboards.push(KeyboardInfo { path });
            }
        }
    }

    Ok(keyboards)
}

/// Converts the relevant non-repeat evdev key transitions to raw events.
fn raw_event(key: Key, value: i32) -> Option<RawEvent> {
    match (key, value) {
        (Key::KEY_F24, 1) => Some(RawEvent::F24Down),
        (Key::KEY_F24, 0) => Some(RawEvent::F24Up),
        _ => None,
    }
}

/// Read keyboard events from a single device in a blocking loop.
///
/// Runs in `spawn_blocking`. Sends `RawEvent` messages for relevant
/// F24 presses/releases.
fn read_keyboard_events(path: &std::path::Path, tx: mpsc::Sender<RawEvent>) {
    // Try to open the device — if it fails, just return
    let mut device = loop {
        match Device::open(path) {
            Ok(d) => break d,
            Err(e) => {
                warn!("failed to open keyboard device {path:?}: {e} — retrying");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };

    // Set the device fd to non-blocking mode so fetch_events() returns
    // immediately. This allows us to check is_closed() between polls.
    let fd = device.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    info!(
        "monitoring keyboard device: {path:?} — {name}",
        name = device.name().unwrap_or("unknown")
    );

    loop {
        // Check if the channel has been closed (state machine exited)
        if tx.is_closed() {
            info!("channel closed, exiting device reader for {path:?}");
            return;
        }

        // Block waiting for the next event batch
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    // Only care about key events
                    let InputEventKind::Key(key) = ev.kind() else {
                        continue;
                    };

                    let send = raw_event(key, ev.value());

                    if let Some(raw_event) = send {
                        if tx.blocking_send(raw_event).is_err() {
                            info!("channel closed, exiting device reader for {path:?}");
                            return;
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No events available yet — normal for non-blocking mode.
                // The loop will sleep 5ms and try again.
            }
            Err(e) => {
                // Device probably disconnected or real I/O error
                warn!("error reading from {path:?}: {e} — device disconnected?");
                return;
            }
        }

        // Small sleep to avoid busy-waiting when no events are available
        // fetch_events() returns immediately, so we yield the CPU
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time() -> Instant {
        Instant::now()
    }

    #[test]
    fn normal_hold_and_release() {
        let now = time();
        let mut state = HotkeyState::default();

        assert_eq!(
            state.process(RawEvent::F24Down, now),
            Some(HotkeyEvent::Press)
        );
        assert_eq!(
            state.process(RawEvent::F24Up, now),
            Some(HotkeyEvent::Release)
        );
    }

    #[test]
    fn repeats_and_duplicate_transitions_are_ignored() {
        let now = time();
        let mut state = HotkeyState::default();

        assert_eq!(raw_event(Key::KEY_F24, 2), None);
        assert_eq!(
            state.process(RawEvent::F24Down, now),
            Some(HotkeyEvent::Press)
        );
        assert_eq!(state.process(RawEvent::F24Down, now), None);
        assert_eq!(
            state.process(RawEvent::F24Up, now),
            Some(HotkeyEvent::Release)
        );
        assert_eq!(state.process(RawEvent::F24Up, now), None);
    }

    #[test]
    fn cooldown_suppresses_mirrored_sequence_then_retriggers() {
        let now = time();
        let mut state = HotkeyState::default();

        assert_eq!(
            state.process(RawEvent::F24Down, now),
            Some(HotkeyEvent::Press)
        );
        assert_eq!(
            state.process(RawEvent::F24Up, now),
            Some(HotkeyEvent::Release)
        );
        assert_eq!(
            state.process(RawEvent::F24Down, now + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            state.process(RawEvent::F24Up, now + Duration::from_millis(101)),
            None
        );
        assert_eq!(
            state.process(RawEvent::F24Down, now + COOLDOWN_DURATION),
            Some(HotkeyEvent::Press)
        );
    }

    #[test]
    fn release_recovers_after_suppressed_press() {
        let now = time();
        let mut state = HotkeyState::default();

        assert_eq!(
            state.process(RawEvent::F24Down, now),
            Some(HotkeyEvent::Press)
        );
        assert_eq!(
            state.process(RawEvent::F24Up, now),
            Some(HotkeyEvent::Release)
        );
        assert_eq!(
            state.process(RawEvent::F24Down, now + Duration::from_millis(1)),
            None
        );
        assert_eq!(
            state.process(RawEvent::F24Up, now + Duration::from_millis(2)),
            None
        );
        assert_eq!(
            state.process(RawEvent::F24Down, now + COOLDOWN_DURATION),
            Some(HotkeyEvent::Press)
        );
    }
}
