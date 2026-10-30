# sttme

Single-executable Rust port of [TalkType](https://github.com/lmacan1/talktype) providing sub-millisecond global push-to-talk local Whisper speech-to-text for Wayland / Niri and Linux.

## Highlights

- **Single standalone executable**: Combines the push-to-talk client, hot-model background daemon (`whisper.cpp` via `whisper-rs`), TalkType-compatible HTTP API server (`/health`, `/stats`, `/transcribe`), history/retry recovery, and setup wizard into one Rust binary.
- **Global instant hold-to-talk (`< 0.5ms` IPC)**: Holding `F10` anywhere in Niri talks synchronously to `sttme daemon` over `$XDG_RUNTIME_DIR/sttme.sock`, listens strictly while `F10` remains held down (with `KeyboardInteractivity::None` so active apps never lose focus), and finishes listening automatically upon releasing `F10`.
- **Works across every app + safe terminal dictation**: Dictates into any focused Wayland/XWayland window (`alacritty_only = false`) while still **stripping out all Enter/Return/newline keystrokes** (`\r`, `\n`, `\u2028`, `\u2029`, `\u0085`, and control characters) in Alacritty and terminals so dictated text never auto-executes shell commands.
- **Zero external Wayland tools required**: Uses native Wayland `zwp_virtual_keyboard_v1`, `zwlr_layer_shell_v1`, PipeWire `pw-record --raw` live PCM streaming, and `$NIRI_SOCKET` IPC directly in safe Rust (no `xdotool`, `ydotool`, or `wtype` required).
- **Top-center Wayland live waveform HUD**: Renders an anti-aliased `320x62` `50 FPS` high-contrast adaptive-gain LCARS audio waveform pill anchored at the top-center of your screen (`zwlr_layer_shell_v1` `Layer::Overlay` + double-buffered `wl_shm` `ARGB8888`) while `F10` is held down, vanishing the instant `F10` is released.
- **Embedded LCARS audio cues**: Plays Star Trek `computerbeep_50` when listening begins (`F10` held) and `computerbeep_48` when finished listening and processed (`F10` released).

## Quick Usage

```bash
# Global hold-to-talk while F10 is held down (wired to F10 in Niri)
sttme hold --key f10 --target all --quiet

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
alacritty_only = false
strip_alacritty_enter = true
trailing_space = true
input_method = "type"

[ui]
audio_beeps = true
waveform_overlay = true
minimal = false

[history]
limit = 100
```

## Global `F10` Hold-to-Talk Keybinding in Niri

In `~/.config/niri/config.kdl` inside `binds { ... }`:

```kdl
    F10 repeat=true cooldown-ms=70 hotkey-overlay-title="Hold to Dictate (sttme)" { spawn "/var/home/maxfridbe/.local/bin/sttme" "hold" "--key" "f10" "--target" "all" "--quiet"; }
```
