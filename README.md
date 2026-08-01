# daapSTT — Voice Input Daemon

Global push-to-talk speech-to-text dictation for Wayland. Hold **Alt+Space**, speak, release — your words appear in the active window.

Uses [ElevenLabs Scribe v2](https://elevenlabs.io/docs/speech-to-text) for transcription: no GPU, CUDA, or local model files are required.

## How It Works

```
Alt+Space held ──► § marker appears ──► you speak ──► release key
                                                          │
             transcribed text appears ◄── ElevenLabs API ◄┘
```

## Prerequisites

- **Linux** with Wayland (tested on Hyprland)
- **PipeWire** for audio capture
- **wtype**, **pw-record**, **notify-send**, **keyd**, and **wl-clipboard** (`wl-paste`) installed
- **keyd** enabled and configured in its existing `/etc/keyd/default.conf` (see Quick Start)
- **input** group membership for `/dev/input/event*` access
- An [ElevenLabs API key](https://elevenlabs.io/app/settings/api-keys)
- **Rust** toolchain (for building from source)

## Quick Start

First, add this mapping to the existing `/etc/keyd/default.conf` (do not create a second or wildcard keyd configuration):

```ini
[alt]
space = f24
```

Then enable or restart keyd after saving that file, and install the daemon:

```bash
# Enable keyd if needed, or restart it after updating default.conf
sudo systemctl enable --now keyd
sudo systemctl restart keyd

# 1. Put your ElevenLabs API key in the env file
mkdir -p ~/.config/voice-daemon
echo 'ELEVENLABS_API_KEY=your_key_here' > ~/.config/voice-daemon/env

# 2. Build and install
make install

# 3. Start the daemon
systemctl --user daemon-reload
systemctl --user enable --now voice-daemon

# 4. Watch logs
journalctl --user -u voice-daemon -f
```

## Usage

| Action | Result |
| --- | --- |
| Hold **Alt+Space** | `§` appears at cursor |
| Speak into mic | Audio captured |
| Release **Alt+Space** | `§` removed, text appears |
| Quick tap (< 0.05s) | `§` removed, no action |

Batch transcription is the default. In both Batch and Realtime modes, the user hotkey remains **hold Alt+Space and release it to stop**. `F24` is only keyd's internal normalized signal; it is not a second user-facing hotkey.

Speaking standalone `banana` (case-insensitive) triggers a native clipboard paste rather than typing a replacement. Text is wrapped in double quotes, while images get one space before and after. It supports multiline text and images; text uses **Ctrl+Shift+V** only in Kitty and **Ctrl+V** in every other app, while images use **Ctrl+V**. All occurrences are recognized, including punctuation-adjacent ones but not those inside longer words. In Realtime mode, paste happens when recognition becomes a stable committed segment, before the hotkey is released. Batch mode necessarily waits for the provider transcription result, but pastes immediately once `banana` is recognized.

Realtime mode streams audio to Scribe v2 Realtime and displays live partial text while you speak. It removes filler words, false starts, and non-speech sounds:

```bash
daapstt realtime on      # Enable realtime and restart the user service
daapstt realtime off     # Return to batch mode and restart the service
daapstt realtime status  # Print Batch or Realtime
```

Both Batch and Realtime transcripts remove boundary ASCII/smart double quotes and terminal ellipses. Internal punctuation and normal terminal periods remain unchanged.

In Realtime mode, the current partial transcript is mutable: revisions replace only its changed suffix, while committed segments become immutable. Keep the cursor and window focus where dictation started until releasing the hotkey; moving either during dictation can revise text in the wrong location.

If realtime fails before any committed text appears, the provisional partial is removed and the buffered recording falls back to batch transcription after release. If it fails after a commit, the mutable tail is removed, committed text is kept, and the recording is not batch-transcribed again, avoiding duplicate output. Batch mode behavior is unchanged.

## Keyterms

Keyterms help Scribe recognize names, product names, and other specialized vocabulary. By default, they are stored locally in `~/.config/voice-daemon/keyterms.txt`; set `VOICE_KEYTERMS_FILE` in the env file to use a different file. The keyterms file contains one trimmed phrase per line. Empty lines are ignored; terms are case-sensitive, deduplicated in insertion order, limited to 1,000 entries, and each term may contain at most 50 Unicode characters and five words. ElevenLabs does not accept `<`, `>`, `{`, `}`, `[`, `]`, or `\` in a term.

Manage them from an interactive terminal (not from the background daemon):

```bash
daapstt keyterms
```

Controls: **↑/↓** or **j/k** move the selection; **D** (uppercase) asks to delete the selected term; only **y** or **Y** confirms; **q** or **Esc** exits.

Scriptable commands are also available:

```bash
daapstt keyterms list
daapstt keyterms add "ElevenLabs Scribe"
daapstt keyterms remove "ElevenLabs Scribe"
```

Batch transcription accepts up to 1,000 stored keyterms. Realtime sessions send only the first 50, matching the realtime API limit.

ElevenLabs Scribe v2 transcription costs $0.22/hour base. Keyterm prompting adds $0.05/hour.

## Configuration

Set these variables in `~/.config/voice-daemon/env`:

| Variable | Default | Description |
| --- | --- | --- |
| `ELEVENLABS_API_KEY` | *(required)* | ElevenLabs API key |
| `ELEVENLABS_API_URL` | `https://api.elevenlabs.io/v1/speech-to-text` | Batch API endpoint (normally leave unchanged) |
| `ELEVENLABS_REALTIME_URL` | `wss://api.elevenlabs.io/v1/speech-to-text/realtime` | Realtime WebSocket endpoint (normally leave unchanged) |
| `VOICE_MARKER_CHAR` | `§` | Recording indicator character |
| `VOICE_MAX_RECORDING_SECS` | `60` | Maximum recording duration |
| `VOICE_KEYTERMS_FILE` | `~/.config/voice-daemon/keyterms.txt` | File containing one keyterm per line; keyterm commands use this path |

## Architecture

```
physical Alt+Space ──► keyd (`[alt] space = f24`) ──► evdev F24 ──► hotkey.rs
                                                                  │
                                                  (Press/two-stage release channel)
                                                                  │
                                                          main.rs (orchestrator)
                                     │
                   ┌─────────────────┼─────────────────┐
                   ▼                 ▼                  ▼
             audio.rs      transcribe/realtime     deliver.rs
          (pw-record)       (ElevenLabs API)        (wtype)
                   │                 │                  │
                   └─────────────────┴──────────────────┘
                                     │
                               notify.rs
```

keyd temporarily removes Alt while it emits the internal `F24` event. If Space is released before physical Alt, keyd restores Alt after `F24`-up; the daemon therefore stops recording immediately at `F24`-up but defers final text reconciliation and cleanup until the restored Alt is released. If Alt is released first, finalization proceeds after a short event-settle interval. Users continue to use only Alt+Space.

## Troubleshooting

- **Alt+Space does not start recording:** Confirm keyd is running with `systemctl status keyd`, then verify the existing `/etc/keyd/default.conf` contains the `[alt]` section and `space = f24` mapping shown above. Restart keyd after changes.
- **Live text is affected by Alt shortcuts:** Ensure the mapping is in `/etc/keyd/default.conf`, not a separate wildcard configuration. The daemon observes both `F24` and Alt transitions from keyd's virtual keyboard so it can defer release-time `wtype` edits until Alt is safe.

## Development

```bash
# Run in debug mode (foreground, verbose logging)
RUST_LOG=debug cargo run

# Run tests
cargo test

# Build release
cargo build --release
```

## License

MIT
