//! Persistent operating mode for the voice daemon.

use anyhow::{Context, Result};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

const CONFIG_DIRECTORY: &str = "voice-daemon";
const MODE_FILE: &str = "mode";
const SERVICE_NAME: &str = "voice-daemon.service";
static TEMP_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// The daemon's transcription operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Batch,
    Realtime,
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Batch => formatter.write_str("Batch"),
            Self::Realtime => formatter.write_str("Realtime"),
        }
    }
}

impl FromStr for Mode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim() {
            "Batch" => Ok(Self::Batch),
            "Realtime" => Ok(Self::Realtime),
            _ => anyhow::bail!("invalid mode; expected `Batch` or `Realtime`"),
        }
    }
}

/// Returns the XDG configuration path used to persist the daemon mode.
pub fn path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("could not determine XDG config directory")?;
    Ok(config_dir.join(CONFIG_DIRECTORY).join(MODE_FILE))
}

/// Loads the persisted mode. A missing mode file means batch mode.
pub fn load() -> Result<Mode> {
    load_from(&path()?)
}

/// Persists `mode` and restarts the user service.
///
/// The mode is intentionally saved before restarting. If restarting fails, this
/// returns an error that makes clear the next service start will use the saved mode.
pub fn set_and_restart(mode: Mode) -> Result<()> {
    set_and_restart_with(mode, &Systemctl)
}

/// Persists `mode` and invokes an injectable service restarter.
pub fn set_and_restart_with<R: Restarter>(mode: Mode, restarter: &R) -> Result<()> {
    set_and_restart_at(&path()?, mode, restarter)
}

fn set_and_restart_at<R: Restarter>(mode_path: &Path, mode: Mode, restarter: &R) -> Result<()> {
    store_at(mode_path, mode)?;
    restarter.restart().with_context(|| {
        format!(
            "mode saved as {mode}, but failed to restart {SERVICE_NAME}; the new mode will apply on its next start"
        )
    })
}

/// Minimal command boundary for testing service restart behavior.
pub trait Restarter {
    fn restart(&self) -> Result<()>;
}

/// Restarts the user-level daemon via systemd.
pub struct Systemctl;

impl Restarter for Systemctl {
    fn restart(&self) -> Result<()> {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "restart", SERVICE_NAME])
            .status()
            .context("failed to run systemctl")?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("systemctl exited with {status}")
        }
    }
}

fn load_from(mode_path: &Path) -> Result<Mode> {
    match fs::read_to_string(mode_path) {
        Ok(contents) => contents.parse().with_context(|| {
            format!(
                "malformed mode file {} (expected `Batch` or `Realtime`)",
                mode_path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Mode::Batch),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read mode file {}", mode_path.display()))
        }
    }
}

fn store_at(mode_path: &Path, mode: Mode) -> Result<()> {
    let parent = mode_path
        .parent()
        .context("mode path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create mode directory {}", parent.display()))?;

    let temp_path = temporary_path(parent);
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| {
                format!(
                    "failed to create temporary mode file {}",
                    temp_path.display()
                )
            })?;
        file.write_all(format!("{mode}\n").as_bytes())
            .context("failed to write mode file")?;
        file.sync_all().context("failed to sync mode file")?;
        fs::rename(&temp_path, mode_path).with_context(|| {
            format!(
                "failed to atomically replace mode file {}",
                mode_path.display()
            )
        })?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn temporary_path(parent: &Path) -> PathBuf {
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{MODE_FILE}.{}.{}.tmp",
        std::process::id(),
        sequence
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Succeeds;
    impl Restarter for Succeeds {
        fn restart(&self) -> Result<()> {
            Ok(())
        }
    }

    struct Fails;
    impl Restarter for Fails {
        fn restart(&self) -> Result<()> {
            anyhow::bail!("service unavailable")
        }
    }

    fn test_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "daapstt-mode-{name}-{}-{unique}",
                std::process::id()
            ))
            .join(MODE_FILE)
    }

    #[test]
    fn missing_file_defaults_to_batch() {
        let mode_path = test_path("missing");
        assert_eq!(load_from(&mode_path).unwrap(), Mode::Batch);
    }

    #[test]
    fn loads_valid_persisted_modes() {
        let mode_path = test_path("valid");
        fs::create_dir_all(mode_path.parent().unwrap()).unwrap();
        fs::write(&mode_path, "Realtime\n").unwrap();
        assert_eq!(load_from(&mode_path).unwrap(), Mode::Realtime);
        fs::remove_dir_all(mode_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_data_has_a_clear_error() {
        let mode_path = test_path("malformed");
        fs::create_dir_all(mode_path.parent().unwrap()).unwrap();
        fs::write(&mode_path, "fast\n").unwrap();
        let error = load_from(&mode_path).unwrap_err().to_string();
        assert!(error.contains("malformed mode file"));
        assert!(error.contains("Batch"));
        fs::remove_dir_all(mode_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn atomic_update_replaces_the_saved_mode() {
        let mode_path = test_path("atomic");
        store_at(&mode_path, Mode::Batch).unwrap();
        store_at(&mode_path, Mode::Realtime).unwrap();
        assert_eq!(fs::read_to_string(&mode_path).unwrap(), "Realtime\n");
        let temporary_files = fs::read_dir(mode_path.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(temporary_files, 0);
        fs::remove_dir_all(mode_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn restart_success_and_failure_are_observable_after_persisting() {
        let success_path = test_path("restart-success");
        set_and_restart_at(&success_path, Mode::Realtime, &Succeeds).unwrap();
        assert_eq!(load_from(&success_path).unwrap(), Mode::Realtime);

        let failure_path = test_path("restart-failure");
        let error = set_and_restart_at(&failure_path, Mode::Realtime, &Fails)
            .unwrap_err()
            .to_string();
        assert!(error.contains("mode saved as Realtime"));
        assert_eq!(load_from(&failure_path).unwrap(), Mode::Realtime);

        fs::remove_dir_all(success_path.parent().unwrap()).unwrap();
        fs::remove_dir_all(failure_path.parent().unwrap()).unwrap();
    }
}
