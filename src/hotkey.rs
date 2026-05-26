//! Hotkey detection via evdev.
//!
//! Monitors all keyboard devices in `/dev/input/event*` for
//! Alt+Space press and release events. Sends events through
//! a tokio channel to the orchestrator.
//!
//! # Architecture
//!
//! - Device manager runs in a tokio task, periodically scanning for
//!   new keyboard devices and managing per-device reader threads.
//! - Each keyboard device gets a `spawn_blocking` thread that reads
//!   events via evdev's blocking API.
//! - Raw key events are sent through an mpsc channel to a state machine
//!   that tracks Alt+Space combo state across all devices.
//! - The state machine emits `HotkeyEvent::Press` when Alt is down and
//!   Space is pressed, and `HotkeyEvent::Release` when either key is
//!   released while recording.

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
    AltDown,
    AltUp,
    SpaceDown,
    SpaceUp,
}

/// Start monitoring keyboard devices for Alt+Space events.
///
/// Sends `HotkeyEvent::Press` when Alt+Space is pressed (both keys down)
/// and `HotkeyEvent::Release` when either key is released while recording.
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
        // Cooldown period after Release to suppress duplicate events from
        // virtual keyboard devices (e.g., keyd) that mirror physical key presses.
        let cooldown_duration = Duration::from_millis(200);
        let mut cooldown_until: Option<Instant> = None;
        let mut alt_pressed = false;
        let mut space_pressed = false;
        let mut recording = false;

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

            let now = Instant::now();
            let in_cooldown = cooldown_until.map_or(false, |t| now < t);

            // Update state based on raw key events
            match event {
                RawEvent::AltDown => {
                    debug!("Alt pressed");
                    if space_pressed && !recording && !in_cooldown {
                        debug!("Space+Alt pressed → Press");
                        if tx.send(HotkeyEvent::Press).await.is_err() {
                            break;
                        }
                        recording = true;
                        cooldown_until = None; // clear cooldown on new recording
                    }
                    alt_pressed = true;
                }
                RawEvent::AltUp => {
                    debug!("Alt released");
                    if recording {
                        debug!("Alt released while recording → Release");
                        if tx.send(HotkeyEvent::Release).await.is_err() {
                            break; // channel closed
                        }
                        recording = false;
                        cooldown_until = Some(now + cooldown_duration);
                    }
                    alt_pressed = false;
                }
                RawEvent::SpaceDown => {
                    debug!("Space pressed");
                    if alt_pressed && !recording && !in_cooldown {
                        debug!("Alt+Space pressed → Press");
                        if tx.send(HotkeyEvent::Press).await.is_err() {
                            break;
                        }
                        recording = true;
                        cooldown_until = None; // clear cooldown on new recording
                    }
                    space_pressed = true;
                }
                RawEvent::SpaceUp => {
                    debug!("Space released");
                    if recording {
                        debug!("Space released while recording → Release");
                        if tx.send(HotkeyEvent::Release).await.is_err() {
                            break;
                        }
                        recording = false;
                        cooldown_until = Some(now + cooldown_duration);
                    }
                    space_pressed = false;
                }
            }

            // Sanity check: if recording but neither alt nor space is pressed,
            // we missed a release event — emit Release to recover
            if recording && !alt_pressed && !space_pressed {
                warn!("state inconsistency: recording but no keys held — emitting Release");
                if tx.send(HotkeyEvent::Release).await.is_err() {
                    break;
                }
                recording = false;
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
/// Detects keyboards by checking if the device supports KEY_SPACE.
fn find_keyboards() -> Result<Vec<KeyboardInfo>> {
    let mut keyboards = Vec::new();

    for (path, device) in evdev::enumerate() {
        // Check if this device supports keyboard keys
        if let Some(keys) = device.supported_keys() {
            if keys.contains(Key::KEY_SPACE) {
                keyboards.push(KeyboardInfo {
                    path: std::path::PathBuf::from(path),
                });
            }
        }
    }

    Ok(keyboards)
}

/// Read keyboard events from a single device in a blocking loop.
///
/// Runs in `spawn_blocking`. Sends `RawEvent` messages for relevant
/// key presses/releases (Alt and Space).
fn read_keyboard_events(
    path: &std::path::Path,
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

    info!("monitoring keyboard device: {path:?} — {name}", name = device.name().unwrap_or("unknown"));

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

                    let send = match key {
                        Key::KEY_LEFTALT | Key::KEY_RIGHTALT => match ev.value() {
                            1 => Some(RawEvent::AltDown),
                            0 => Some(RawEvent::AltUp),
                            _ => None, // repeat — ignore
                        },
                        Key::KEY_SPACE => match ev.value() {
                            1 => Some(RawEvent::SpaceDown),
                            0 => Some(RawEvent::SpaceUp),
                            _ => None,
                        },
                        _ => None,
                    };

                    if let Some(raw_event) = send {
                        debug!("device {path:?}: {raw_event:?}");
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
