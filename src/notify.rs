//! Desktop notifications via notify-send.
//!
//! Shows transient desktop notifications for error states and status changes.
//! All notification failures are logged but never crash the daemon — if
//! notify-send is missing or the notification daemon is down, we simply
//! log and continue.

use anyhow::Result;
use log::{debug, warn};
use tokio::process::Command;

/// Show an error notification via `notify-send`.
///
/// Uses `--urgency=critical` and a 5-second expiry.
/// Failures are logged as warnings and suppressed (daemon MUST NOT crash on notification failure).
pub async fn error(title: &str, message: &str) -> Result<()> {
    debug!("error notification: {title} — {message}");

    match Command::new("notify-send")
        .arg(title)
        .arg(message)
        .arg("--urgency=critical")
        .arg("--expire-time=5000")
        .arg("--app-name=voice-daemon")
        .arg("--icon=dialog-error")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            // Don't wait — fire and forget. The notification daemon
            // will display it even after wtype exits.
            let _ = child.wait_with_output().await;
        }
        Err(e) => {
            warn!("failed to spawn notify-send: {e} — notification suppressed");
        }
    }

    Ok(())
}

/// Show an informational notification via `notify-send`.
///
/// Uses normal urgency and a 3-second expiry.
#[allow(dead_code)]
pub async fn info(title: &str, message: &str) -> Result<()> {
    debug!("info notification: {title} — {message}");

    match Command::new("notify-send")
        .arg(title)
        .arg(message)
        .arg("--expire-time=3000")
        .arg("--app-name=voice-daemon")
        .arg("--icon=dialog-information")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => {
            let _ = child.wait_with_output().await;
        }
        Err(e) => {
            warn!("failed to spawn notify-send: {e} — notification suppressed");
        }
    }

    Ok(())
}
