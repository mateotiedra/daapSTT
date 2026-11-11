//! Hotkey detection via evdev.
//!
//! Monitors F24 and mouse Forward-capable devices in `/dev/input/event*`
//! without grabbing them. F24 starts immediately; Forward starts after a
//! continuous one-second hold. The keyd virtual keyboard's LeftAlt transitions
//! determine when it is safe to inject text after release.
//!
//! Each device has a nonblocking evdev reader feeding a shared state machine.
//! Only the initiating input owns a recording; overlapping holds are ignored
//! until released and pressed again.

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
    MouseDown(u64),
    MouseUp(u64),
    MouseDisconnected(u64),
}

const COOLDOWN_DURATION: Duration = Duration::from_millis(200);
/// keyd emits a restored Alt synchronously with F24-up, if it restores one.
const ALT_RESTORE_SETTLE_DURATION: Duration = Duration::from_millis(20);
const MOUSE_HOLD_DURATION: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
enum ReleasePhase {
    Idle,
    Settling { deadline: Instant },
    WaitingForAltUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordingSource {
    Keyboard,
    Mouse(u64),
}

/// State shared by the immediate F24 and delayed Forward hotkeys.
struct HotkeyState {
    f24_pressed: bool,
    recording: Option<RecordingSource>,
    // None means held but suppressed until release, including the active mouse.
    mouse_holds: HashMap<u64, Option<Instant>>,
    keyd_alt_down: bool,
    cooldown_until: Option<Instant>,
    release_phase: ReleasePhase,
}

impl Default for HotkeyState {
    fn default() -> Self {
        Self {
            f24_pressed: false,
            recording: None,
            mouse_holds: HashMap::new(),
            keyd_alt_down: false,
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
                self.suppress_mouse_holds();
                if self.recording.is_some() || self.cooldown_until.is_some_and(|until| now < until) {
                    None
                } else {
                    self.recording = Some(RecordingSource::Keyboard);
                    self.cooldown_until = None;
                    Some(HotkeyEvent::Press)
                }
            }
            RawEvent::F24Up => {
                if !self.f24_pressed {
                    return None;
                }
                self.f24_pressed = false;
                if self.recording == Some(RecordingSource::Keyboard) {
                    Some(self.release(now))
                } else {
                    None
                }
            }
            RawEvent::KeydLeftAltDown => {
                self.keyd_alt_down = true;
                if matches!(self.release_phase, ReleasePhase::Settling { .. }) {
                    self.release_phase = ReleasePhase::WaitingForAltUp;
                }
                None
            }
            RawEvent::KeydLeftAltUp => {
                self.keyd_alt_down = false;
                if matches!(self.release_phase, ReleasePhase::WaitingForAltUp) {
                    self.release_phase = ReleasePhase::Idle;
                    Some(HotkeyEvent::ReleaseCompleted)
                } else {
                    None
                }
            }
            RawEvent::MouseDown(id) => {
                let can_arm = self.recording.is_none()
                    && !self.f24_pressed
                    && matches!(self.release_phase, ReleasePhase::Idle);
                self.mouse_holds.entry(id).or_insert_with(|| {
                    can_arm.then_some(now + MOUSE_HOLD_DURATION)
                });
                None
            }
            RawEvent::MouseUp(id) | RawEvent::MouseDisconnected(id) => {
                self.mouse_holds.remove(&id);
                if self.recording == Some(RecordingSource::Mouse(id)) {
                    Some(self.release(now))
                } else {
                    None
                }
            }
        }
    }

    fn suppress_mouse_holds(&mut self) {
        for deadline in self.mouse_holds.values_mut() {
            *deadline = None;
        }
    }

    fn release(&mut self, now: Instant) -> HotkeyEvent {
        self.recording = None;
        self.cooldown_until = Some(now + COOLDOWN_DURATION);
        self.release_phase = if self.keyd_alt_down {
            ReleasePhase::WaitingForAltUp
        } else {
            ReleasePhase::Settling {
                deadline: now + ALT_RESTORE_SETTLE_DURATION,
            }
        };
        HotkeyEvent::ReleaseStarted
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.mouse_holds
            .values()
            .filter_map(|deadline| *deadline)
            .chain(self.settle_deadline())
            .min()
    }

    fn advance(&mut self, now: Instant) -> Option<HotkeyEvent> {
        if let Some(event) = self.settle(now) {
            return Some(event);
        }
        let ready = self.mouse_holds
            .iter()
            .filter_map(|(&id, &deadline)| {
                deadline.filter(|deadline| now >= *deadline).map(|deadline| (deadline, id))
            })
            .min()
            .map(|(_, id)| id);
        if let Some(id) = ready {
            self.suppress_mouse_holds();
            self.recording = Some(RecordingSource::Mouse(id));
            return Some(HotkeyEvent::Press);
        }
        None
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

/// Monitor F24 and mouse Forward devices without grabbing input.
///
/// Emits the same Press/ReleaseStarted/ReleaseCompleted protocol for both.
///
/// The `shutdown` Notify is triggered by the caller to signal graceful
/// shutdown — the state machine and manager use it to exit cleanly.
pub async fn run(tx: mpsc::Sender<HotkeyEvent>, shutdown: Arc<Notify>) -> Result<()> {
    let (raw_tx, mut raw_rx) = mpsc::channel::<RawEvent>(128);

    // Spawn the manager that keeps track of hotkey input devices.
    let shutdown_clone = shutdown.clone();
    let manager_handle: JoinHandle<Result<()>> = tokio::spawn(async move {
        // Track (path → JoinHandle) for active device readers
        let mut active_readers: HashMap<PathBuf, JoinHandle<()>> = HashMap::new();
        let mut next_device_id = 0_u64;

        loop {
            // Re-enumerate hotkey input devices.
            let devices = match find_input_devices() {
                Ok(d) => d,
                Err(e) => {
                    warn!("failed to enumerate input devices: {e}");
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
                let device_id = next_device_id;
                next_device_id += 1;

                let handle = tokio::task::spawn_blocking(move || {
                    read_input_events(&path_for_closure, device_id, is_keyd_virtual_keyboard, raw_tx);
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
                warn!("no hotkey input devices found, retrying after rescan");
            } else {
                debug!("monitoring {} hotkey input device(s)", active_readers.len());
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
            let hotkey_event = if let Some(deadline) = state.next_deadline() {
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
                        state.advance(Instant::now())
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

/// Information about a discovered hotkey input device.
#[derive(Debug, Clone)]
struct InputDeviceInfo {
    path: PathBuf,
    // Only this authoritative keyd output device may affect Alt safety.
    is_keyd_virtual_keyboard: bool,
}

/// Find F24 and Forward-button devices, including keyd's virtual pointer
/// because keyd may exclusively grab the physical mouse.
fn find_input_devices() -> Result<Vec<InputDeviceInfo>> {
    let mut devices = Vec::new();

    for (path, device) in evdev::enumerate() {
        if let Some(keys) = device.supported_keys() {
            if keys.contains(Key::KEY_F24)
                || keys.contains(Key::BTN_EXTRA)
                || keys.contains(Key::BTN_FORWARD)
            {
                let is_keyd_virtual_keyboard = device.name() == Some("keyd virtual keyboard");
                debug!("found hotkey input device {path:?}: name={:?}", device.name());
                devices.push(InputDeviceInfo {
                    path,
                    is_keyd_virtual_keyboard,
                });
            }
        }
    }

    Ok(devices)
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

/// Coalesce the two Forward codes into one hold per device.
/// A duplicate/repeat cannot restart the timer or release another held code.
#[derive(Default)]
struct ForwardButtons(u8);

impl ForwardButtons {
    fn process(&mut self, key: Key, value: i32, device_id: u64) -> Option<RawEvent> {
        let bit = match key {
            Key::BTN_EXTRA => 1,
            Key::BTN_FORWARD => 2,
            _ => return None,
        };
        let was_held = self.0 != 0;
        match value {
            1 => self.0 |= bit,
            0 => self.0 &= !bit,
            _ => return None,
        }
        match (was_held, self.0 != 0) {
            (false, true) => Some(RawEvent::MouseDown(device_id)),
            (true, false) => Some(RawEvent::MouseUp(device_id)),
            _ => None,
        }
    }
}

/// Read without grabbing, so normal mouse clicks remain available to apps.
fn read_input_events(
    path: &std::path::Path,
    device_id: u64,
    is_keyd_virtual_keyboard: bool,
    tx: mpsc::Sender<RawEvent>,
) {
    let mut device = match Device::open(path) {
        Ok(device) => device,
        Err(error) => {
            warn!("failed to open input device {path:?}: {error}");
            return;
        }
    };
    let mut forward_buttons = ForwardButtons::default();

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
        "monitoring input device: {path:?} — {name}",
        name = device.name().unwrap_or("unknown")
    );

    loop {
        // Check if the channel has been closed (state machine exited)
        if tx.is_closed() {
            info!("channel closed, exiting device reader for {path:?}");
            return;
        }

        // Poll the next event batch.
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    // Only care about key events
                    let InputEventKind::Key(key) = ev.kind() else {
                        continue;
                    };

                    let send = raw_event(key, ev.value(), is_keyd_virtual_keyboard)
                        .or_else(|| forward_buttons.process(key, ev.value(), device_id));

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
                let _ = tx.blocking_send(RawEvent::MouseDisconnected(device_id));
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
