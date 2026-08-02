//! Per-application microphone privacy via PipeWire's PulseAudio compatibility layer.
//!
//! Active microphone capture streams are muted while daapSTT records, without
//! muting the physical source that daapSTT itself needs. Only streams that were
//! unmuted by this module are restored afterward.

use log::{debug, info, warn};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Default)]
pub struct MicMuteState {
    muted_streams: Vec<CaptureStream>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CaptureStream {
    index: u32,
    label: String,
}

#[derive(Debug, Deserialize)]
struct Source {
    index: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SourceOutput {
    index: u32,
    source: u32,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    properties: HashMap<String, String>,
}

impl MicMuteState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mute every currently active, unmuted microphone capture stream.
    ///
    /// This runs before daapSTT starts `pw-record`, so daapSTT's own capture
    /// stream is not part of the snapshot. Missing `pactl` support is a no-op.
    pub async fn mute_other_apps(&mut self) {
        // Recover from an interrupted prior lifecycle before taking a new snapshot.
        self.restore().await;

        let (sources_result, outputs_result) =
            tokio::join!(pactl_list("sources"), pactl_list("source-outputs"));

        let outputs_json = match outputs_result {
            Ok(json) => json,
            Err(error) => {
                debug!("cannot inspect microphone capture streams: {error}");
                return;
            }
        };
        let sources_json = match sources_result {
            Ok(json) => Some(json),
            Err(error) => {
                debug!("cannot classify PulseAudio sources: {error}; using stream metadata");
                None
            }
        };

        let streams = match microphone_capture_streams(sources_json.as_deref(), &outputs_json) {
            Ok(streams) => streams,
            Err(error) => {
                warn!("failed to parse pactl microphone state: {error}");
                return;
            }
        };

        for stream in streams {
            if set_source_output_mute(stream.index, true).await {
                self.muted_streams.push(stream);
            }
        }

        if !self.muted_streams.is_empty() {
            info!(
                "temporarily muted microphone for: {}",
                stream_labels(&self.muted_streams)
            );
        }
    }

    /// Restore only capture streams successfully muted by [`Self::mute_other_apps`].
    pub async fn restore(&mut self) {
        let streams = std::mem::take(&mut self.muted_streams);
        if streams.is_empty() {
            return;
        }

        let mut restored = Vec::new();
        for stream in streams {
            if set_source_output_mute(stream.index, false).await {
                restored.push(stream);
            }
        }

        if !restored.is_empty() {
            info!("restored microphone for: {}", stream_labels(&restored));
        }
    }
}

async fn pactl_list(kind: &str) -> Result<Vec<u8>, String> {
    let output = Command::new("pactl")
        .args(["--format=json", "list", kind])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("failed to run pactl: {error}"))?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "pactl list {kind} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn set_source_output_mute(index: u32, muted: bool) -> bool {
    let index = index.to_string();
    let mute = if muted { "1" } else { "0" };
    let output = Command::new("pactl")
        .args(["set-source-output-mute", &index, mute])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            warn!(
                "failed to {} microphone capture stream {index}: {}",
                if muted { "mute" } else { "restore" },
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(error) => {
            warn!(
                "failed to run pactl while trying to {} microphone capture stream {index}: {error}",
                if muted { "mute" } else { "restore" }
            );
            false
        }
    }
}

fn microphone_capture_streams(
    sources_json: Option<&[u8]>,
    outputs_json: &[u8],
) -> serde_json::Result<Vec<CaptureStream>> {
    let monitor_sources = match sources_json {
        Some(json) => serde_json::from_slice::<Vec<Source>>(json)?
            .into_iter()
            .filter(is_monitor_source)
            .map(|source| source.index)
            .collect(),
        None => HashSet::new(),
    };
    let outputs = serde_json::from_slice::<Vec<SourceOutput>>(outputs_json)?;

    Ok(outputs
        .into_iter()
        .filter(|output| !output.mute)
        .filter(|output| !monitor_sources.contains(&output.source))
        .filter(|output| {
            output
                .properties
                .get("stream.capture.sink")
                .is_none_or(|value| !value.eq_ignore_ascii_case("true"))
        })
        .map(|output| CaptureStream {
            index: output.index,
            label: capture_stream_label(&output.properties),
        })
        .collect())
}

fn is_monitor_source(source: &Source) -> bool {
    source.name.ends_with(".monitor")
        || source
            .properties
            .get("device.class")
            .is_some_and(|class| class.eq_ignore_ascii_case("monitor"))
}

fn capture_stream_label(properties: &HashMap<String, String>) -> String {
    [
        "application.process.binary",
        "application.name",
        "media.name",
        "node.name",
    ]
    .into_iter()
    .find_map(|key| properties.get(key))
    .cloned()
    .unwrap_or_else(|| "unknown app".to_owned())
}

fn stream_labels(streams: &[CaptureStream]) -> String {
    streams
        .iter()
        .map(|stream| stream.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCES: &[u8] = br#"[
        {
            "index": 40,
            "name": "alsa_output.speaker.monitor",
            "properties": {"device.class": "monitor"}
        },
        {
            "index": 41,
            "name": "alsa_input.microphone",
            "properties": {"device.class": "sound"}
        }
    ]"#;

    const OUTPUTS: &[u8] = br#"[
        {
            "index": 100,
            "source": 41,
            "mute": false,
            "properties": {
                "application.name": "WEBRTC VoiceEngine",
                "application.process.binary": "Discord"
            }
        },
        {
            "index": 101,
            "source": 41,
            "mute": true,
            "properties": {"application.name": "Already muted"}
        },
        {
            "index": 102,
            "source": 40,
            "mute": false,
            "properties": {"application.name": "Screen audio"}
        },
        {
            "index": 103,
            "source": 999,
            "mute": false,
            "properties": {
                "application.name": "Cava",
                "stream.capture.sink": "true"
            }
        }
    ]"#;

    #[test]
    fn selects_only_unmuted_microphone_capture_streams() {
        assert_eq!(
            microphone_capture_streams(Some(SOURCES), OUTPUTS).unwrap(),
            [CaptureStream {
                index: 100,
                label: "Discord".to_owned(),
            }]
        );
    }

    #[test]
    fn stream_metadata_still_excludes_sink_capture_without_source_list() {
        let streams = microphone_capture_streams(None, OUTPUTS).unwrap();

        assert_eq!(
            streams,
            [
                CaptureStream {
                    index: 100,
                    label: "Discord".to_owned(),
                },
                CaptureStream {
                    index: 102,
                    label: "Screen audio".to_owned(),
                },
            ]
        );
    }
}
