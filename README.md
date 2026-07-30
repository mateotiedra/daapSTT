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
- **wtype**, **pw-record**, and **notify-send** installed
- **input** group membership for `/dev/input/event*` access
- An [ElevenLabs API key](https://elevenlabs.io/app/settings/api-keys)
- **Rust** toolchain (for building from source)

## Quick Start

```bash
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

## Keyterms

Keyterms help Scribe recognize names, product names, and other specialized vocabulary. They are stored locally in `~/.config/voice-daemon/keyterms.txt`, one trimmed phrase per line. Empty lines are ignored; terms are case-sensitive, deduplicated in insertion order, limited to 1,000 entries, and each term may contain at most 50 Unicode characters and five words. ElevenLabs does not accept `<`, `>`, `{`, `}`, `[`, `]`, or `\` in a term.

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

ElevenLabs Scribe v2 transcription costs $0.22/hour base. Keyterm prompting adds $0.05/hour.

## Configuration

Set these variables in `~/.config/voice-daemon/env`:

| Variable | Default | Description |
| --- | --- | --- |
| `ELEVENLABS_API_KEY` | *(required)* | ElevenLabs API key |
| `ELEVENLABS_API_URL` | `https://api.elevenlabs.io/v1/speech-to-text` | API endpoint (normally leave unchanged) |
| `VOICE_MARKER_CHAR` | `§` | Recording indicator character |
| `VOICE_MAX_RECORDING_SECS` | `60` | Maximum recording duration |

## Architecture

```
evdev keyboard ──► hotkey.rs ──► (Press/Release via channel)
                                     │
                             main.rs (orchestrator)
                                     │
                   ┌─────────────────┼─────────────────┐
                   ▼                 ▼                  ▼
             audio.rs         transcribe.rs        deliver.rs
          (pw-record)       (ElevenLabs API)        (wtype)
                   │                 │                  │
                   └─────────────────┴──────────────────┘
                                     │
                               notify.rs
```

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
