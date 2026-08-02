//! Output-sink muting via `pactl`.
//!
//! A recording snapshots the output sinks that were unmuted, mutes only those
//! sinks, and restores precisely that snapshot afterwards. Sinks that were
//! already muted are deliberately left untouched.

use log::{debug, warn};
use serde::Deserialize;
use tokio::process::Command;

/// Output sinks muted for the active recording and awaiting restoration.
pub struct OutputMuteState {
    muted_sinks: Vec<MutedSink>,
}

struct MutedSink {
    index: u32,
    label: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Sink {
    index: u32,
    name: String,
    #[serde(default)]
    description: Option<String>,
    mute: bool,
}

impl Sink {
    fn label(&self) -> String {
        self.description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
            .unwrap_or(&self.name)
            .to_owned()
    }
}

impl OutputMuteState {
    pub fn new() -> Self {
        Self {
            muted_sinks: Vec::new(),
        }
    }
}

/// Mute every output sink that is currently unmuted.
///
/// Any stale recording snapshot is restored before a new one is taken. Missing
/// or failing `pactl`, and malformed sink JSON, degrade safely to a no-op.
pub async fn mute_unmuted_outputs(state: &mut OutputMuteState) {
    restore_muted_outputs(state).await;

    let output = match Command::new("pactl")
        .args(["--format=json", "list", "sinks"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            debug!("pactl is unavailable while listing output sinks: {error}");
            return;
        }
    };

    if !output.status.success() {
        debug!("pactl list sinks exited with {}", output.status);
        return;
    }

    let sinks = match unmuted_sinks(&output.stdout) {
        Ok(sinks) => sinks,
        Err(error) => {
            warn!("could not parse pactl output-sink JSON: {error}");
            return;
        }
    };

    for sink in sinks {
        let label = sink.label();
        if set_sink_mute(sink.index, true).await {
            debug!("muted output sink {} ({label})", sink.index);
            state.muted_sinks.push(MutedSink {
                index: sink.index,
                label,
            });
        }
    }
}

/// Restore only output sinks that this recording successfully muted.
///
/// The snapshot is consumed before issuing commands, making repeated cleanup
/// idempotent even if a restore command itself fails.
pub async fn restore_muted_outputs(state: &mut OutputMuteState) {
    let muted_sinks = std::mem::take(&mut state.muted_sinks);
    for sink in muted_sinks {
        if set_sink_mute(sink.index, false).await {
            debug!("restored output sink {} ({})", sink.index, sink.label);
        }
    }
}

async fn set_sink_mute(index: u32, mute: bool) -> bool {
    let value = if mute { "1" } else { "0" };
    match Command::new("pactl")
        .args(["set-sink-mute", &index.to_string(), value])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            warn!(
                "pactl set-sink-mute {index} {value} exited with {}",
                output.status
            );
            false
        }
        Err(error) => {
            warn!("failed to run pactl set-sink-mute {index} {value}: {error}");
            false
        }
    }
}

fn unmuted_sinks(bytes: &[u8]) -> Result<Vec<Sink>, serde_json::Error> {
    let sinks: Vec<Sink> = serde_json::from_slice(bytes)?;
    Ok(sinks.into_iter().filter(|sink| !sink.mute).collect())
}

#[cfg(test)]
mod tests {
    use super::unmuted_sinks;

    #[test]
    fn selects_only_unmuted_sinks_and_uses_descriptive_labels() {
        let sinks = unmuted_sinks(
            br#"[
                {"index": 42, "name": "alsa_output.one", "description": "Desk speakers", "mute": false},
                {"index": 7, "name": "alsa_output.two", "description": null, "mute": true},
                {"index": 9, "name": "bluez_output.headset", "description": "", "mute": false}
            ]"#,
        )
        .unwrap();

        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].index, 42);
        assert_eq!(sinks[0].label(), "Desk speakers");
        assert_eq!(sinks[1].index, 9);
        assert_eq!(sinks[1].label(), "bluez_output.headset");
    }

    #[test]
    fn rejects_invalid_sink_json() {
        assert!(unmuted_sinks(br#"[{"index":"not-a-number"}]"#).is_err());
    }
}
