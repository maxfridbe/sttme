# sttme

Single-executable Rust port of [TalkType](https://github.com/lmacan1/talktype) providing sub-millisecond push-to-talk local Whisper speech-to-text for Alacritty and Linux (Wayland / Niri / X11).

## Highlights

- **Single standalone executable**: Combines the push-to-talk client, hot-model background daemon (`whisper.cpp` via `whisper-rs`), TalkType-compatible HTTP API server (`/health`, `/stats`, `/transcribe`), history/retry recovery, and setup wizard into one Rust binary.
- **Instant hold-to-talk (`< 2ms`)**: Holding `F12` in Alacritty talks to `sttme daemon` over `$XDG_RUNTIME_DIR/sttme.sock`, listens strictly while `F12` remains physically held down (via Wayland `zwlr_layer_shell_v1` + `wl_keyboard` release tracking with zero `/dev/input` root permissions), and finishes listening the instant `F12` is released.
- **Safe Alacritty dictation**: Restricts STT output to Alacritty by default and **strips out all Enter/Return/newline keystrokes** (`\r`, `\n`, `\u2028`, `\u2029`, `\u0085`, and control characters) in both text sanitization and dynamic Wayland XKB keymaps so dictated text never auto-executes terminal commands.
- **Zero external Wayland tools required**: Uses native Wayland `zwp_virtual_keyboard_v1`, `zwlr_layer_shell_v1`, and `$NIRI_SOCKET` IPC directly in safe Rust (no `xdotool`, `ydotool`, or `wtype` required).
- **Top-center Wayland live waveform HUD**: Renders an anti-aliased `272x50` `50 FPS` live LCARS audio waveform pill anchored at the top-center of your screen (`zwlr_layer_shell_v1` `Layer::Overlay` + double-buffered `wl_shm` `ARGB8888`) while `F12` is held down, vanishing the instant `F12` is released.
- **Embedded LCARS audio cues**: Plays Star Trek `computerbeep_50` when listening begins (`F12` held) and `computerbeep_48` when finished listening and processed (`F12` released).

## Quick Usage

```bash
# Hold-to-talk while F12 is held down (wired to F12 in Alacritty)
sttme hold --key f12 --target alacritty --quiet

# Check daemon status, loaded model, and latency metrics
sttme status

# Recover (re-type) last transcription (TalkType F8 equivalent)
sttme recover

# Retry failed transcription from ~/.cache/sttme/pending.wav (TalkType F7 equivalent)
sttme retry

# Run persistent daemon (automatically managed by systemd --user sttme.service)
sttme daemon

# Run TalkType-compatible Whisper HTTP server on port 8002
sttme serve --port 8002

# Verify Wayland virtual keyboard, Niri IPC, and configuration
sttme doctor

# Interactive setup wizard
sttme --setup
```

## Configuration

Settings live in `~/.config/sttme/config.toml` (with automatic migration from `~/.config/talktype/config.yaml` if present):

```toml
[transcription]
mode = "local"
model = "base"
language = "en"
energy_threshold = 0.008

[target]
alacritty_only = true
strip_alacritty_enter = true
trailing_space = true
input_method = "type"

[ui]
audio_beeps = true
minimal = false

[history]
limit = 100
```

## Alacritty `F12` Hold-to-Talk Keybinding

### `alacritty.yml` (Alacritty `0.12.x`)

```yaml
key_bindings:
  - { key: F12, command: { program: "/var/home/maxfridbe/.local/bin/sttme", args: ["hold", "--key", "f12", "--target", "alacritty", "--quiet"] } }
```

### `alacritty.toml` (Alacritty `0.13+`)

```toml
[[keyboard.bindings]]
key = "F12"
command = { program = "/var/home/maxfridbe/.local/bin/sttme", args = ["hold", "--key", "f12", "--target", "alacritty", "--quiet"] }
```
