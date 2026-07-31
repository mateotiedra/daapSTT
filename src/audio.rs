//! Audio capture via pw-record subprocess.
//!
//! Spawns `pw-record` in raw mode (16kHz, mono, s16le) on recording start,
//! accumulates raw PCM bytes in memory, and constructs a WAV header on stop.
//!
//! # WAV Construction
//!
//! ElevenLabs Scribe accepts WAV format. Rather than relying on pw-record's
//! container format (which uses a PipeWire-specific header rather than standard
//! WAV), we capture raw PCM and prepend a 44-byte WAV header ourselves.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc::UnboundedSender, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

/// Threshold below which a recording is considered silent.
///
/// 16-bit PCM ranges ±32767. A peak below 50 indicates the microphone
/// is either muted, capturing the wrong source, or not producing signal.
const SILENCE_THRESHOLD: i16 = 50;

/// Captured audio as WAV bytes (16kHz, mono, 16-bit PCM).
#[derive(Debug)]
pub struct AudioRecording {
    /// Complete WAV file bytes (44-byte header + PCM data).
    pub data: Vec<u8>,
    /// Duration of the recording in seconds (approximate).
    pub duration_secs: f64,
    /// Maximum absolute sample amplitude (0..32767). Used to detect silence.
    pub peak_amplitude: i16,
}

/// Handle to an active recording — holds the subprocess and a background
/// task that continuously drains pw-record's stdout to prevent pipe buffer
/// overflow (which would truncate recordings longer than ~2 seconds).
pub struct RecordingHandle {
    child: Child,
    /// Shared buffer collecting raw PCM from stdout. Locked by both the
    /// drain task (writer) and stop() (reader, after drain completes).
    raw_pcm: Arc<Mutex<Vec<u8>>>,
    /// Background task that reads pw-record's stdout into `raw_pcm`.
    drain_handle: JoinHandle<()>,
}

impl RecordingHandle {
    /// Stop recording and collect the audio data.
    ///
    /// Sends SIGTERM to pw-record for graceful shutdown, waits for the
    /// process to exit, then awaits the background drain task to finish
    /// collecting any remaining stdout data. Escalates to SIGKILL if the
    /// process doesn't exit within 500ms.
    pub async fn stop(mut self) -> Result<AudioRecording> {
        let pid = self.child.id().expect("pw-record has no pid");
        debug!("stopping pw-record (pid: {pid})");

        // Send SIGTERM for graceful shutdown — lets pw-record flush buffers
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }

        // Wait for the process to fully exit
        let wait_result = timeout(Duration::from_millis(500), self.child.wait()).await;
        match wait_result {
            Ok(Ok(status)) => {
                debug!("pw-record exited with status: {status}");
            }
            Ok(Err(e)) => {
                return Err(anyhow::anyhow!("failed to wait for pw-record: {e}"));
            }
            Err(_elapsed) => {
                warn!("pw-record did not exit after SIGTERM — sending SIGKILL");
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                // Wait again after SIGKILL
                self.child
                    .wait()
                    .await
                    .context("failed to wait for pw-record after SIGKILL")?;
            }
        }

        // Await the background drain task — it will finish when pw-record
        // closes its stdout pipe after exiting.
        let drain_result = timeout(Duration::from_secs(2), self.drain_handle).await;
        match drain_result {
            Ok(Ok(())) => {
                debug!("audio drain task completed");
            }
            Ok(Err(join_err)) => {
                warn!("audio drain task panicked: {join_err}");
            }
            Err(_elapsed) => {
                warn!("audio drain task timed out — using partial data");
            }
        }

        // Take the collected raw PCM from the shared buffer
        let raw_pcm = self.raw_pcm.lock().await.clone();

        let duration_secs = pcm_duration_secs(raw_pcm.len());

        // Build WAV from raw PCM
        let wav_data = build_wav(&raw_pcm);
        let peak = compute_peak_amplitude(&raw_pcm);
        let silent = is_silence(peak);

        // Save debug WAV so the user can inspect captured audio
        save_debug_wav(&wav_data);

        if silent {
            warn!(
                "recording appears silent — peak amplitude {} (threshold: {}). \
                Check your microphone input source and volume in PipeWire.",
                peak, SILENCE_THRESHOLD
            );
        } else {
            info!(
                "recording complete: {} raw PCM bytes → {} WAV bytes, {:.2}s, peak amp {}",
                raw_pcm.len(),
                wav_data.len(),
                duration_secs,
                peak
            );
        }

        Ok(AudioRecording {
            data: wav_data,
            duration_secs,
            peak_amplitude: peak,
        })
    }
}

/// Start recording audio from the default microphone.
///
/// Spawns `pw-record` in raw mode and returns a handle that can be
/// used to stop recording and collect the audio data.
///
/// A background tokio task continuously drains pw-record's stdout into
/// an in-memory buffer to prevent pipe buffer overflow. Without this,
/// the 64KB Linux pipe buffer fills up in ~2 seconds at 16kHz/16-bit/mono,
/// causing pw-record to block and truncate the recording.
///
/// When `record_target` is `Some`, forces pw-record to use that specific
/// PipeWire node ID (obtainable via `pw-record --list-targets`).
///
/// Recording is automatically stopped after `max_duration` via a
/// timeout that runs in the caller's context.
///
/// When `pcm_sender` is `Some`, each raw PCM chunk is also sent to it without
/// blocking the drain task. The recording always retains its own complete PCM
/// buffer; if the receiver is dropped, fan-out is disabled and capture continues.
pub fn start_recording(
    _max_duration: Duration,
    record_target: Option<&str>,
    pcm_sender: Option<UnboundedSender<Vec<u8>>>,
) -> Result<RecordingHandle> {
    if let Some(target) = record_target {
        info!("starting pw-record (raw PCM, 16kHz, mono, s16le, target: {target})");
    } else {
        info!("starting pw-record (raw PCM, 16kHz, mono, s16le)");
    }

    let mut cmd = Command::new("pw-record");
    cmd.arg("-a") // raw mode — no container header
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
        .kill_on_drop(true);

    if let Some(target) = record_target {
        cmd.arg("--target").arg(target);
    }

    let mut child = cmd
        .spawn()
        .context("failed to spawn pw-record — is PipeWire running?")?;

    debug!("pw-record spawned with pid {:?}", child.id());

    // Take stdout and spawn a background task to continuously drain it.
    // This prevents the pipe buffer from filling up (64KB default on Linux),
    // which would block pw-record and truncate recordings longer than ~2 seconds.
    let mut stdout = child.stdout.take().expect("pw-record stdout not piped");
    let raw_pcm = Arc::new(Mutex::new(Vec::new()));
    let raw_pcm_clone = raw_pcm.clone();

    let drain_handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let mut pcm_sender = pcm_sender;
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break, // pipe closed — pw-record exited
                Ok(n) => {
                    fan_out_pcm_chunk(&raw_pcm_clone, &mut pcm_sender, &buf[..n]).await;
                }
                Err(e) => {
                    warn!("error reading pw-record stdout: {e}");
                    break;
                }
            }
        }
    });

    Ok(RecordingHandle {
        child,
        raw_pcm,
        drain_handle,
    })
}

/// Append a captured chunk locally, then optionally fan it out to a consumer.
///
/// An unbounded sender keeps this operation non-blocking. A dropped receiver
/// only disables future fan-out; it never affects local recording capture.
async fn fan_out_pcm_chunk(
    raw_pcm: &Arc<Mutex<Vec<u8>>>,
    pcm_sender: &mut Option<UnboundedSender<Vec<u8>>>,
    chunk: &[u8],
) {
    raw_pcm.lock().await.extend_from_slice(chunk);

    if let Some(sender) = pcm_sender {
        if sender.send(chunk.to_vec()).is_err() {
            debug!("raw PCM fan-out receiver dropped; continuing local capture");
            *pcm_sender = None;
        }
    }
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

/// Compute the maximum absolute amplitude of raw 16-bit signed little-endian PCM.
///
/// Scans every pair of bytes as a little-endian i16 and returns the largest
/// absolute value (0..32767). A value near 0 means the buffer is silent.
fn compute_peak_amplitude(pcm: &[u8]) -> i16 {
    let mut max = 0i16;
    for chunk in pcm.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        let abs = sample.abs();
        if abs > max {
            max = abs;
        }
    }
    max
}

/// Check whether a recording is effectively silent based on its peak amplitude.
pub fn is_silence(peak: i16) -> bool {
    peak < SILENCE_THRESHOLD
}

/// Save the WAV buffer to a debug file under `/tmp/` when debug logging is enabled.
///
/// The filename includes a UNIX timestamp so every recording is preserved.
/// This lets users inspect captured audio in any audio player to diagnose
/// microphone/source issues.
fn save_debug_wav(wav_data: &[u8]) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = format!("/tmp/daapstt-debug-{timestamp}.wav");

    match std::fs::write(&path, wav_data) {
        Ok(()) => debug!("saved debug WAV to {path}"),
        Err(e) => warn!("failed to save debug WAV to {path}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fan_out_appends_locally_and_sends_chunk() {
        let raw_pcm = Arc::new(Mutex::new(Vec::new()));
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut pcm_sender = Some(sender);
        let chunk = [1, 2, 3, 4];

        fan_out_pcm_chunk(&raw_pcm, &mut pcm_sender, &chunk).await;

        assert_eq!(*raw_pcm.lock().await, chunk);
        assert_eq!(receiver.try_recv().unwrap(), chunk);
        assert!(pcm_sender.is_some());
    }

    #[tokio::test]
    async fn test_fan_out_continues_locally_after_receiver_loss() {
        let raw_pcm = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let mut pcm_sender = Some(sender);

        fan_out_pcm_chunk(&raw_pcm, &mut pcm_sender, &[1, 2]).await;
        fan_out_pcm_chunk(&raw_pcm, &mut pcm_sender, &[3, 4]).await;

        assert_eq!(*raw_pcm.lock().await, [1, 2, 3, 4]);
        assert!(pcm_sender.is_none());
    }

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

    #[test]
    fn test_peak_amplitude_all_zeros() {
        let pcm = vec![0u8; 32000];
        assert_eq!(compute_peak_amplitude(&pcm), 0);
        assert!(is_silence(compute_peak_amplitude(&pcm)));
    }

    #[test]
    fn test_peak_amplitude_mixed_samples() {
        // Create PCM with samples: 0, 100, -200, 0
        let pcm: Vec<u8> = [
            0x00, 0x00, // 0
            0x64, 0x00, // 100
            0x38, 0xFF, // -200 (0xFF38 in le)
            0x00, 0x00, // 0
        ]
        .to_vec();
        assert_eq!(compute_peak_amplitude(&pcm), 200);
        assert!(!is_silence(compute_peak_amplitude(&pcm)));
    }

    #[test]
    fn test_peak_amplitude_near_threshold() {
        // Sample value of 49 should be considered silence
        let pcm = vec![0x31, 0x00]; // 49
        assert_eq!(compute_peak_amplitude(&pcm), 49);
        assert!(is_silence(compute_peak_amplitude(&pcm)));

        // Sample value of 50 should NOT be considered silence
        let pcm = vec![0x32, 0x00]; // 50
        assert_eq!(compute_peak_amplitude(&pcm), 50);
        assert!(!is_silence(compute_peak_amplitude(&pcm)));
    }

    #[test]
    fn test_peak_amplitude_odd_byte_ignored() {
        // Odd byte count: last byte should be ignored
        let pcm = vec![0x64, 0x00, 0x00]; // 100, then trailing 0
        assert_eq!(compute_peak_amplitude(&pcm), 100);
    }
}
