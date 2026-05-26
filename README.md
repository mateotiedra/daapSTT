# daapSTT — Voice Input Daemon

Global push-to-talk speech-to-text dictation for Wayland. Hold **Alt+Space**, speak, release — your words appear in the active window.

Uses [Groq's Whisper API](https://console.groq.com) (free tier: 2,000 req/day) — no GPU, no CUDA, no model files.

## How It Works

```
Alt+Space held ──► § marker appears ──► you speak ──► release key
                                                          │
                 transcribed text appears ◄── Groq API ◄──┘
```

## Prerequisites

- **Linux** with Wayland (tested on Hyprland)
- **PipeWire** for audio capture
- **wtype**, **pw-record**, **notify-send** installed
- **input** group membership for `/dev/input/event*` access
- **Rust** toolchain (for building from source)

## Quick Start

```bash
# 1. Get a Groq API key (free) at https://console.groq.com/keys
# 2. Set it in the env file
echo 'GROQ_API_KEY=gsk_your_key_here' > ~/.config/voice-daemon/env

# 3. Build and install
make install

# 4. Start the daemon
systemctl --user daemon-reload
systemctl --user enable --now voice-daemon

# 5. Watch logs
journalctl --user -u voice-daemon -f
```

## Usage

| Action | Result |
|--------|--------|
| Hold **Alt+Space** | `§` appears at cursor |
| Speak into mic | Audio captured |
| Release **Alt+Space** | `§` removed, text appears |
| Quick tap (< 0.05s) | `§` removed, no action |

## Configuration

All via environment variables in `~/.config/voice-daemon/env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `GROQ_API_KEY` | *(required)* | Groq API key |
| `GROQ_API_URL` | `https://api.groq.com/openai/v1/audio/transcriptions` | API endpoint |
| `GROQ_MODEL` | `whisper-large-v3-turbo` | Whisper model |
| `GROQ_LANGUAGE` | `en` | Transcription language |
| `VOICE_MARKER_CHAR` | `§` | Recording indicator character |
| `VOICE_MAX_RECORDING_SECS` | `60` | Max recording duration |

## Architecture

```
evdev keyboard ──► hotkey.rs ──► (Press/Release via channel)
                                     │
                             main.rs (orchestrator)
                                     │
                   ┌─────────────────┼─────────────────┐
                   ▼                 ▼                  ▼
             audio.rs         transcribe.rs        deliver.rs
          (pw-record)         (Groq API)           (wtype)
                   │                 │                  │
                   └─────────────────┴──────────────────┘
                                     │
                               notify.rs
                            (notify-send)
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
