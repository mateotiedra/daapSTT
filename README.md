# daapSTT — Voice Input Daemon

Global push-to-talk speech-to-text dictation for Wayland. Hold **Alt+Space**, speak, release — your words appear in the active window.

Uses [ElevenLabs Scribe v2](https://elevenlabs.io/docs/speech-to-text) for transcription: no GPU, CUDA, or local model files are required.

## How It Works

```
Batch: Alt+Space → § marker → speak → release → final text
Chrome/Discord realtime: Alt+Space → § preedit → live words → release → one final commit
```

Native IBus delivery shows `§` immediately, then replaces it with recognized words. Other realtime targets follow their delivery policy described below.

## Prerequisites

- **Linux** with Wayland (tested on Hyprland)
- **PipeWire** for audio capture
- **wtype**, **pw-record**, **notify-send**, **keyd**, and **wl-clipboard** (`wl-paste`) installed
- **pactl** for temporary output muting and per-app microphone privacy (both features degrade to a no-op if PipeWire's PulseAudio compatibility layer is unavailable)
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

While the hotkey is held, the daemon uses PipeWire's PulseAudio compatibility layer (`pactl`) to mute every available audio output that was not already muted. This silences Spotify, YouTube, Discord, and new playback routed to those outputs without pausing or changing playback position. Outputs that were already muted stay muted, and only outputs successfully muted by daapSTT are restored as soon as recording stops—before transcription finishes. A new output device that appears mid-dictation is not part of the initial snapshot.

The daemon also mutes other apps' currently active microphone capture streams while the hotkey is held. This prevents Discord, browsers, and call apps from receiving your dictation while leaving the physical microphone available to daapSTT. Streams that capture system/monitor audio are ignored, streams that were already muted stay muted, and streams muted by daapSTT are restored as soon as recording stops. This does not toggle the mute control inside Discord or other apps.

Outside native IBus realtime, speaking standalone `banana` (case-insensitive) triggers a native clipboard paste rather than typing a replacement. Text is wrapped in double quotes, while images get one space before and after. It supports multiline text and images; text uses **Ctrl+Shift+V** only in Kitty and **Ctrl+V** in every other app, while images use **Ctrl+V**. All occurrences are recognized, including punctuation-adjacent ones but not those inside longer words. Live keyboard delivery pastes when recognition becomes a stable committed segment. Deferred and Batch delivery wait until their final delivery phase.

Realtime mode streams audio to Scribe v2 Realtime and displays live partial text while you speak. It removes filler words, false starts, and non-speech sounds. Chrome/Chromium on native Wayland or XWayland and Discord on XWayland use the opt-in native IBus transport below. Other native Wayland windows receive live keyboard edits through `wtype`. Other XWayland windows and native Wayland Discord retain deferred delivery: no marker or partial text while Alt+Space is held, then the stable transcript after Alt is released.

Keyboard delivery inserts `§` at recording start and removes it before inserting the first transcript. Chrome never uses this path: physical-key mappings can drop spaces, trigger shortcuts or corrupt non-BMP characters. A missing or failed Chrome IME session is reported, without recording on begin failure and without keyboard fallback.

```bash
daapstt realtime on      # Enable realtime and restart the user service
daapstt realtime off     # Return to batch mode and restart the service
daapstt realtime status  # Print Batch or Realtime
```

Both Batch and Realtime transcripts remove boundary ASCII/smart double quotes and terminal ellipses. Internal punctuation and normal terminal periods remain unchanged.

For live keyboard delivery outside native IBus targets, the current partial transcript is mutable: revisions replace only its changed suffix, while committed segments become immutable. Keep the cursor and window focus where dictation started until releasing the hotkey; moving either during keyboard-based dictation can revise text in the wrong location.

For keyboard-based realtime delivery, if recognition fails before any committed text appears, the provisional partial is removed and the buffered recording falls back to batch transcription after release. If it fails after a commit, the mutable tail is removed, committed text is kept, and the recording is not batch-transcribed again, avoiding duplicate output. Explicit Batch mode behavior is unchanged.

### Native Wayland Chrome and XWayland Chrome/Discord voice input

Install the optional bridge separately from the daemon:

```bash
sudo apt-get install ibus ibus-gtk3 ibus-gtk4 gir1.2-ibus-1.0 python3-gi gcc pkg-config patch libglib2.0-dev libwayland-dev libxkbcommon-dev wayland-protocols
make install-ime
```

Back up your user desktop launchers, then prefix each application's executable in every `Exec=` line with the absolute path `$HOME/.local/bin/daapstt-ime-run` (expand `$HOME` yourself; desktop files do not expand it). Native Wayland Chrome uses `/home/YOU/.local/bin/daapstt-ime-run /usr/bin/google-chrome-stable --ozone-platform=wayland --enable-wayland-ime --wayland-ime-version=3`. Preserve `%U`, incognito actions, profile and scale factor. For Discord, keep `/usr/bin/env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11 /home/YOU/.local/bin/daapstt-ime-run /usr/bin/discord --ozone-platform=x11`: its updater can restart without forwarding flags, so the X11 environment must survive in child processes. Quit each app normally after saving work and reopen it through its launcher; opening another window does not reload IME flags.

The shared wrapper reuses an existing IBus daemon or starts `daapstt-ibus.service`, then starts `daapstt-ime.service`. When `WAYLAND_DISPLAY` is set and the arguments contain exactly `--ozone-platform=wayland` and `--enable-wayland-ime`, it also starts `daapstt-ibus-wayland.service`. This frontend owns the seat's input-method-v2 role; do not run it alongside another seat input method. It never replaces the bus, changes global keyboard preferences or adds flags to other applications. Frontend startup failure warns independently of bridge socket readiness, then opens the application normally. `GTK_IM_MODULE=ibus` and `XMODIFIERS=@im=ibus` remain scoped to the launched application.

`make install-ime` builds an isolated upstream IBus 1.5.32 Wayland frontend against Ubuntu amd64 `libibus-1.0-5` version `1.5.32-2build1`. Source and extracted development headers are checksum-pinned; no system packages or libraries are replaced. The patch forwards double-buffered v2 content purposes/hints (including password/PIN) and handles rapid engine-change notifications using the current engine descriptor. The stock 1.5.32 GTK frontend discards content types and can assert during rapid engine changes, so it is not used. Build artifacts remain under `target/ibus-wayland`; an incompatible installed IBus ABI fails the build explicitly. The standalone frontend has no GTK candidate popup and is intended for this composition engine.

Native IBus realtime behavior:

- Hold Alt+Space: `§` appears as native preedit, then recognized words replace it progressively. The marker is never included in the final commit. Provider-confirmed segments accumulate in that preedit; no intermediate application commits occur. **Docs may preserve intermediate compositions, including the marker, in its Undo history.**
- Release: queued audio is drained, the last server segment is explicitly finalized, and exactly one native commit inserts the provider-confirmed transcript as soon as its acknowledgement arrives. There is no fixed three-second grace period for native IBus delivery. Network/recognition latency can still keep the preedit underlined briefly; an unconfirmed partial is never promoted to final text.
- During composition, Chromium/Docs displays a temporary underline. It disappears on final commit; it is not document formatting. Chromium's GTK transport supplies this underline even when the engine supplies no underline attribute.
- Native Chrome's rich-text editor may normalize ordinary spaces to non-breaking spaces to preserve their visible layout. Textarea Unicode scalars remain exact; rich-text space normalization is accepted.
- Change field/tab/window or type manually: the dictation session is invalidated and late results are refused. **Chrome/Docs may retain the visible composition in the old field on focus loss.** The daemon does not delete that text or reopen a session in the new field.
- An IME failure or ambiguous ACK timeout never retries the commit and never falls back to keyboard injection. A provider failure before any confirmed segment may use batch recognition, but only through the same still-valid IME session.
- `banana` clipboard insertion is **unsupported in native IBus realtime**: a final transcript containing the placeholder is rejected with a notification, not pasted or typed literally. Keyboard-based clipboard behavior and explicit Batch mode are unchanged.

Native IBus sessions use the provider's manual commit strategy. Audio is segmented at 20 seconds, below its automatic ~36-second boundary, with only one outstanding commit acknowledgement. This prevents an earlier segment from being mistaken for the final answer after release. The three-second acknowledgement timeout is an error bound, not a mandatory delay. Other realtime delivery paths retain VAD segmentation.

The bridge requires an owned `$XDG_RUNTIME_DIR`, uses a private `daapstt/ime.sock` socket, rejects password/PIN contexts when reported, and selects its engine only at dictation start. It does not make every XWayland application an IBus target.

```bash
systemctl --user status daapstt-ibus daapstt-ime daapstt-ibus-wayland voice-daemon
journalctl --user -u daapstt-ime -u daapstt-ibus-wayland -u voice-daemon -f
# Both manual probes modify the focused field after a five-second pause:
python3 contrib/ibus/smoke.py
cargo test ime_manual_smoke -- --ignored --nocapture
python3 -m unittest discover -s contrib/ibus/tests
```

If native input is unavailable, focus an editable field and check the bridge service and the app's scoped launcher. Discord must run under XWayland for this path; native Wayland Discord remains deferred. `make install` does not install or enable the optional IBus services.

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
| `VOICE_RECORD_TARGET` | *(default source)* | Optional PipeWire target node ID for `pw-record` |
| `VOICE_MUTE_AUDIO_OUTPUTS` | `true` | Mute all available audio outputs during dictation; set to `false` to disable |
| `VOICE_MUTE_OTHER_MIC_APPS` | `true` | Mute other apps' active microphone streams during dictation; set to `false` to disable |
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
- **Live text is affected by Alt shortcuts:** Ensure the mapping is in `/etc/keyd/default.conf`, not a separate wildcard configuration. Native Wayland targets receive live edits except Discord; XWayland targets outside Chrome and Discord use deferred delivery. The IBus bridge recognizes keyd's F24 by both keysym and normalized Linux hardware code 194, so an XKB layout without an F24 keysym does not treat shortcut repeats as manual typing.
- **Chrome preedit disappears on hotkey release:** Modifier-only events (including Control restored during release) must not cancel the session. The bridge consumes those transitions while dictating; an actual non-modifier key or focus change still cancels. Reinstall with `make install-ime` and restart `daapstt-ime` after updating the bridge.

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
