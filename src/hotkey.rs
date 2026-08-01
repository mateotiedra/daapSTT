//! Hotkey detection via evdev.
//!
//! Monitors all F24-capable devices in `/dev/input/event*`. The keyd virtual
//! keyboard's LeftAlt transitions are also used to determine when it is safe
//! to inject text after an F24 release.
//!
//! # Architecture
//!
//! - Device manager runs in a tokio task, periodically scanning for
//!   new keyboard devices and managing per-device reader threads.
//! - Each keyboard device gets a `spawn_blocking` thread that reads
//!   events via evdev's blocking API.
//! - Raw key events are sent through an mpsc channel to a state machine.
//! - The state machine emits `Press` for F24 down, `ReleaseStarted` for F24
//!   up, and `ReleaseCompleted` once any restored keyd Alt has cleared.

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

/// Semantic hotkey events. `ReleaseStarted` stops recording immediately;
/// `ReleaseCompleted` means keyd is no longer holding a restored LeftAlt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Press,
    ReleaseStarted,
    ReleaseCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RawEvent {
    F24Down,
    F24Up,
    KeydLeftAltDown,
    KeydLeftAltUp,
}

const COOLDOWN_DURATION: Duration = Duration::from_millis(200);
/// keyd emits a restored Alt synchronously with F24-up, if it restores one.
const ALT_RESTORE_SETTLE_DURATION: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy)]
enum ReleasePhase {
    Idle,
    Settling { deadline: Instant },
    WaitingForAltUp,
}

/// State for the modifier-free F24 hotkey.
struct HotkeyState {
    f24_pressed: bool,
    recording: bool,
    cooldown_until: Option<Instant>,
    release_phase: ReleasePhase,
}

impl Default for HotkeyState {
    fn default() -> Self {
        Self {
            f24_pressed: false,
            recording: false,
            cooldown_until: None,
            release_phase: ReleasePhase::Idle,
        }
    }
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
                    self.release_phase = ReleasePhase::Settling {
                        deadline: now + ALT_RESTORE_SETTLE_DURATION,
                    };
                    Some(HotkeyEvent::ReleaseStarted)
                } else {
                    None
                }
            }
            RawEvent::KeydLeftAltDown => {
                if matches!(self.release_phase, ReleasePhase::Settling { .. }) {
                    self.release_phase = ReleasePhase::WaitingForAltUp;
                }
                None
            }
            RawEvent::KeydLeftAltUp => {
                if matches!(self.release_phase, ReleasePhase::WaitingForAltUp) {
                    self.release_phase = ReleasePhase::Idle;
                    Some(HotkeyEvent::ReleaseCompleted)
                } else {
                    None
                }
            }
        }
    }

    fn settle_deadline(&self) -> Option<Instant> {
        match self.release_phase {
            ReleasePhase::Settling { deadline } => Some(deadline),
            ReleasePhase::Idle | ReleasePhase::WaitingForAltUp => None,
        }
    }

    /// Completes a release only when no restored Alt arrived during settling.
    fn settle(&mut self, now: Instant) -> Option<HotkeyEvent> {
        let ReleasePhase::Settling { deadline } = self.release_phase else {
            return None;
        };
        if now < deadline {
            return None;
        }
        self.release_phase = ReleasePhase::Idle;
        Some(HotkeyEvent::ReleaseCompleted)
    }
}

/// Start monitoring keyboard devices for F24 events.
///
/// Sends `Press` on F24 down, `ReleaseStarted` on F24 up, then
/// `ReleaseCompleted` once a possible keyd-restored Alt has cleared.
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
                let is_keyd_virtual_keyboard = device_info.is_keyd_virtual_keyboard;

                let handle = tokio::task::spawn_blocking(move || {
                    read_keyboard_events(&path_for_closure, is_keyd_virtual_keyboard, raw_tx);
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
            let hotkey_event = if let Some(deadline) = state.settle_deadline() {
                tokio::select! {
                    biased;
                    event = raw_rx.recv() => match event {
                        Some(event) => state.process(event, Instant::now()),
                        None => {
                            info!("raw event channel closed");
                            break;
                        }
                    },
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        state.settle(Instant::now())
                    }
                    _ = state_shutdown.notified() => {
                        info!("state machine received shutdown signal");
                        break;
                    }
                }
            } else {
                let event = tokio::select! {
                    event = raw_rx.recv() => event,
                    _ = state_shutdown.notified() => {
                        info!("state machine received shutdown signal");
                        break;
                    }
                };
                let Some(event) = event else {
                    info!("raw event channel closed");
                    break;
                };
                state.process(event, Instant::now())
            };

            if let Some(hotkey_event) = hotkey_event {
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
    // Only this authoritative keyd output device may affect Alt safety.
    is_keyd_virtual_keyboard: bool,
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
                let input_id = device.input_id();
                let is_keyd_virtual_keyboard = device.name() == Some("keyd virtual keyboard");
                debug!(
                    "found F24 device {path:?}: name={:?}, vendor={:04x}, product={:04x}",
                    device.name(),
                    input_id.vendor(),
                    input_id.product(),
                );
                keyboards.push(KeyboardInfo {
                    path,
                    is_keyd_virtual_keyboard,
                });
            }
        }
    }

    Ok(keyboards)
}

/// Converts non-repeat F24 transitions from all readers and LeftAlt transitions
/// only from the authoritative keyd virtual keyboard.
fn raw_event(key: Key, value: i32, is_keyd_virtual_keyboard: bool) -> Option<RawEvent> {
    match (key, value, is_keyd_virtual_keyboard) {
        (Key::KEY_F24, 1, _) => Some(RawEvent::F24Down),
        (Key::KEY_F24, 0, _) => Some(RawEvent::F24Up),
        (Key::KEY_LEFTALT, 1, true) => Some(RawEvent::KeydLeftAltDown),
        (Key::KEY_LEFTALT, 0, true) => Some(RawEvent::KeydLeftAltUp),
        _ => None,
    }
}

/// Read keyboard events from a single device in a blocking loop.
///
/// Runs in `spawn_blocking`. Sends F24 events and, for keyd's authoritative
/// virtual keyboard only, LeftAlt transitions.
fn read_keyboard_events(
    path: &std::path::Path,
    is_keyd_virtual_keyboard: bool,
    tx: mpsc::Sender<RawEvent>,
) {
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

                    let send = raw_event(key, ev.value(), is_keyd_virtual_keyboard);

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
mod tests;
