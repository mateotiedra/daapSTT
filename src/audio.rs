//! Audio capture via pw-record subprocess.
//!
//! Spawns `pw-record` in raw mode (16kHz, mono, s16le) on recording start,
//! accumulates raw PCM bytes in memory, and constructs a WAV header on stop.
//!
//! # WAV Construction
//!
//! Groq's Whisper API accepts WAV format. Rather than relying on pw-record's
//! container format (which uses a PipeWire-specific header rather than standard
//! WAV), we capture raw PCM and prepend a 44-byte WAV header ourselves.

use anyhow::{Context, Result};
use log::{debug, info};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::Duration;

/// Captured audio as WAV bytes (16kHz, mono, 16-bit PCM).
#[derive(Debug)]
pub struct AudioRecording {
    /// Complete WAV file bytes (44-byte header + PCM data).
    pub data: Vec<u8>,
    /// Duration of the recording in seconds (approximate).
    pub duration_secs: f64,
}

/// Handle to an active recording — holds the subprocess.
pub struct RecordingHandle {
    child: Child,
}

impl RecordingHandle {
    /// Stop recording and collect the audio data.
    ///
    /// Kills the pw-record subprocess, reads any remaining stdout,
    /// and constructs a complete WAV buffer.
    pub async fn stop(mut self) -> Result<AudioRecording> {
        // Kill the pw-record process
        debug!("killing pw-record process (pid: {:?})", self.child.id());
        self.child.kill().await.context("failed to kill pw-record")?;

        // Read remaining stdout
        let mut raw_pcm = Vec::new();
        if let Some(mut stdout) = self.child.stdout.take() {
            stdout
                .read_to_end(&mut raw_pcm)
                .await
                .context("failed to read pw-record stdout")?;
        }

        // Wait for the process to exit
        let status = self.child.wait().await.context("failed to wait for pw-record")?;
        debug!("pw-record exited with status: {status}");

        let duration_secs = pcm_duration_secs(raw_pcm.len());

        // Build WAV from raw PCM
        let wav_data = build_wav(&raw_pcm);
        info!(
            "recording complete: {} raw PCM bytes → {} WAV bytes, {:.2}s",
            raw_pcm.len(),
            wav_data.len(),
            duration_secs
        );

        Ok(AudioRecording {
            data: wav_data,
            duration_secs,
        })
    }
}

/// Start recording audio from the default microphone.
///
/// Spawns `pw-record` in raw mode and returns a handle that can be
/// used to stop recording and collect the audio data.
///
/// Recording is automatically stopped after `max_duration` via a
/// timeout that runs in the caller's context.
pub fn start_recording(_max_duration: Duration) -> Result<RecordingHandle> {
    info!("starting pw-record (raw PCM, 16kHz, mono, s16le)");

    let child = Command::new("pw-record")
        .arg("-a") // raw mode — no container header
        .arg("--rate")
        .arg("16000")
        .arg("--channels")
        .arg("1")
        .arg("--format")
        .arg("s16")
        .arg("-") // output to stdout
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn pw-record — is PipeWire running?")?;

    debug!("pw-record spawned with pid {:?}", child.id());

    Ok(RecordingHandle {
        child,
    })
}

/// Build a standard WAV file from raw 16-bit signed little-endian PCM data.
///
/// Produces a 44-byte RIFF WAV header followed by the raw PCM payload.
/// Format: 16kHz, mono, 16-bit PCM.
fn build_wav(pcm: &[u8]) -> Vec<u8> {
    let data_size = pcm.len() as u32;
    let file_size = 36 + data_size; // total file size minus 8 (RIFF header)

    let mut wav = Vec::with_capacity(44 + pcm.len());

    // RIFF chunk descriptor
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt sub-chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // sub-chunk size (16 for PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // audio format (1 = PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // number of channels (1 = mono)
    wav.extend_from_slice(&16000u32.to_le_bytes()); // sample rate
    wav.extend_from_slice(&32000u32.to_le_bytes()); // byte rate (sample_rate * channels * bits_per_sample / 8)
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align (channels * bits_per_sample / 8)
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data sub-chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend_from_slice(pcm);

    wav
}

/// Calculate duration in seconds from raw PCM byte count.
///
/// 16kHz, mono, 16-bit → 32,000 bytes per second.
fn pcm_duration_secs(byte_count: usize) -> f64 {
    byte_count as f64 / 32000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_wav_header() {
        let pcm = vec![0u8; 32000]; // 1 second of silence
        let wav = build_wav(&pcm);

        // Total file should be 44 (header) + 32000 (data) bytes
        assert_eq!(wav.len(), 44 + 32000);

        // Check RIFF header
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");

        // Check fmt chunk
        assert_eq!(&wav[12..16], b"fmt ");

        // Check data chunk
        assert_eq!(&wav[36..40], b"data");

        // Data size should be 32000
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_size, 32000);
    }

    #[test]
    fn test_duration_calculation() {
        assert!((pcm_duration_secs(32000) - 1.0).abs() < 0.001);
        assert!((pcm_duration_secs(64000) - 2.0).abs() < 0.001);
        assert!((pcm_duration_secs(16000) - 0.5).abs() < 0.001);
        assert_eq!(pcm_duration_secs(0), 0.0);
    }
}
