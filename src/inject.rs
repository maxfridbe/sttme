// Rust guideline compliant 2026-02-21
//! Text sanitization (Enter-keystroke stripping) and Wayland/X11 input injection.
//!
//! Provides two defense-in-depth guarantees when targeting Alacritty:
//! 1. [`sanitize_for_alacritty`] strips all CR/LF, Unicode line/paragraph separators, and
//!    control characters so no newline can ever execute a terminal command prematurely.
//! 2. [`WaylandVirtualKeyboard`] maps Unicode codepoints exclusively onto high XKB keycodes
//!    (`128..=228`, evdev scancodes `120..=220`), avoiding physical scancode collisions with
//!    `KEY_ENTER` (`28`), `KEY_KPENTER` (`96`), `KEY_ESC` (`1`), and `KEY_F12` (`88`).

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use tracing::{Level, event};
use wayland_client::protocol::wl_keyboard::KeymapFormat;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::{
    self, ZwpVirtualKeyboardManagerV1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::{
    self, ZwpVirtualKeyboardV1,
};

use crate::config::InputMethod;

/// Starting XKB keycode (`128`, corresponding to evdev scancode `120`) for synthesized characters.
///
/// Chosen well above standard PC keys (`1..=119`, which include `KEY_ENTER = 28`, `KEY_F12 = 88`,
/// and `KEY_KPENTER = 96`) so `winit`/Alacritty physical-key bindings are never triggered.
const SAFE_XKB_KEYCODE_BASE: u32 = 128;

/// Maximum distinct Unicode characters packed into a single dynamic XKB keymap upload.
///
/// Keeps XKB keycodes strictly within `128..=228` (`<= 255` XKB 8-bit keycode ceiling).
const MAX_UNIQUE_CHARS_PER_CHUNK: usize = 100;

/// Wayland `wl_keyboard::key_state` value representing a key press.
///
/// Defined by the core Wayland `wl_keyboard.key_state` protocol enum (`pressed = 1`).
const WL_KEY_PRESSED: u32 = 1;

/// Wayland `wl_keyboard::key_state` value representing a key release.
///
/// Defined by the core Wayland `wl_keyboard.key_state` protocol enum (`released = 0`).
const WL_KEY_RELEASED: u32 = 0;

/// Linux evdev scancode for `KEY_LEFTCTRL` (`29`, XKB keycode `37`).
///
/// Used when synthesizing `Ctrl+Shift+V` or `Ctrl+V` clipboard paste shortcuts.
const EVDEV_KEY_LEFTCTRL: u32 = 29;

/// Linux evdev scancode for `KEY_LEFTSHIFT` (`42`, XKB keycode `50`).
///
/// Used alongside [`EVDEV_KEY_LEFTCTRL`] when synthesizing terminal `Ctrl+Shift+V` paste.
const EVDEV_KEY_LEFTSHIFT: u32 = 42;

/// Linux evdev scancode for `KEY_V` (`47`, XKB keycode `55`).
///
/// Triggers paste when combined with `Ctrl` or `Ctrl+Shift` on a US XKB layout.
const EVDEV_KEY_V: u32 = 47;

/// XKB depressed modifier bitmask for `Control` (`bit 2 = 4`) in the standard `complete` map.
///
/// Sent via `zwp_virtual_keyboard_v1.modifiers` during paste shortcut synthesis.
const XKB_MOD_CTRL_MASK: u32 = 4;

/// XKB depressed modifier bitmask for `Shift` (`bit 0 = 1`) in the standard `complete` map.
///
/// Combined with [`XKB_MOD_CTRL_MASK`] (`1 | 4 = 5`) for `Ctrl+Shift+V` terminal paste.
const XKB_MOD_SHIFT_MASK: u32 = 1;

/// Number of characters emitted before performing a Wayland socket flush + brief yield.
///
/// Prevents overflowing client PTY input buffers on long multi-sentence transcriptions.
const KEYSTROKE_BATCH_SIZE: usize = 24;

/// Pause between keystroke batches (`2ms`) to preserve strict ordering in terminal PTYs.
///
/// Small enough that 240 characters complete in ~20ms while keeping Wayland queues smooth.
const KEYSTROKE_BATCH_PAUSE: Duration = Duration::from_millis(2);

/// Strips all Enter/Return keystrokes, Unicode line breaks, and control characters for Alacritty.
///
/// Line breaks (`\r`, `\n`, `\x0b`, `\x0c`, `\u{0085}`, `\u{2028}`, `\u{2029}`) are converted to
/// spaces, all remaining control characters (`char::is_control`) are removed, and consecutive
/// whitespace is collapsed into single ASCII spaces.
///
/// # Examples
///
/// ```ignore
/// let cleaned = sanitize_for_alacritty("git status\r\nrm -rf /\n");
/// assert_eq!(cleaned, "git status rm -rf /");
/// ```
#[must_use]
pub fn sanitize_for_alacritty(raw: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if is_line_break_char(ch) || ch == '\t' {
            normalized.push(' ');
        } else if !ch.is_control() {
            normalized.push(ch);
        }
    }

    let mut out = String::with_capacity(normalized.len());
    for word in normalized.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

const fn is_line_break_char(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r' | '\x0b' | '\x0c' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

/// Prepares final dictated text according to target window type and user spacing rules.
#[must_use]
pub fn prepare_output_text(raw: &str, strip_enter: bool, trailing_space: bool) -> String {
    let base = if strip_enter {
        sanitize_for_alacritty(raw)
    } else {
        raw.trim().to_owned()
    };

    if base.is_empty() {
        return base;
    }

    if trailing_space {
        let mut with_space = base;
        with_space.push(' ');
        with_space
    } else {
        base
    }
}

/// Injects text into the currently focused window using Wayland virtual keyboard or clipboard paste.
///
/// # Errors
///
/// Returns an error if neither Wayland virtual keyboard injection nor clipboard/X11 fallback succeeds.
pub fn inject_text(
    text: &str,
    method: InputMethod,
    is_terminal: bool,
    forbid_enter_keys: bool,
    wayland_display_override: Option<&str>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    let effective_text = if forbid_enter_keys {
        let has_trailing_space = text.ends_with(' ');
        let mut clean = sanitize_for_alacritty(text);
        if has_trailing_space && !clean.is_empty() {
            clean.push(' ');
        }
        clean
    } else {
        text.to_owned()
    };

    if effective_text.is_empty() {
        return Ok(());
    }

    match method {
        InputMethod::Type => {
            if let Err(vk_err) =
                wayland_type_text(&effective_text, forbid_enter_keys, wayland_display_override)
            {
                event!(
                    name: "inject.wayland_type.fallback",
                    Level::WARN,
                    error.message = %vk_err,
                    "Wayland direct typing failed ({{error.message}}), falling back to clipboard paste",
                );
                paste_via_clipboard(&effective_text, is_terminal, wayland_display_override)?;
            }
        }
        InputMethod::Paste => {
            paste_via_clipboard(&effective_text, is_terminal, wayland_display_override)?;
        }
    }

    Ok(())
}

/// Builds a self-contained XKB keymap mapping `unique_chars` onto safe high keycodes.
///
/// Exposed at module level so unit tests can validate every generated map with `xkbcomp`.
#[must_use]
pub fn build_unicode_xkb_keymap(unique_chars: &[char]) -> (String, HashMap<char, u32>) {
    let mut keycodes_sec = String::from(
        "xkb_keymap {\n  xkb_keycodes \"sttme\" {\n    minimum = 8;\n    maximum = 255;\n",
    );
    let mut symbols_sec = String::from("  xkb_symbols \"sttme\" {\n");
    let mut char_to_evdev = HashMap::with_capacity(unique_chars.len());

    for (idx, &ch) in unique_chars.iter().enumerate() {
        if is_line_break_char(ch) || ch.is_control() {
            continue;
        }
        let xkb_code = SAFE_XKB_KEYCODE_BASE + (idx as u32);
        let evdev_code = xkb_code - 8;
        char_to_evdev.insert(ch, evdev_code);

        let _ = writeln!(keycodes_sec, "    <C{idx}> = {xkb_code};");
        let keysym = format_xkb_keysym(ch);
        let _ = writeln!(symbols_sec, "    key <C{idx}> {{[ {keysym} ]}};");
    }

    keycodes_sec.push_str("  };\n");
    symbols_sec.push_str("  };\n};\n");

    let mut full = keycodes_sec;
    full.push_str("  xkb_types \"sttme\" { include \"complete\" };\n");
    full.push_str("  xkb_compatibility \"sttme\" { include \"complete\" };\n");
    full.push_str(&symbols_sec);

    (full, char_to_evdev)
}

fn format_xkb_keysym(ch: char) -> String {
    if ch == ' ' {
        "space".to_owned()
    } else {
        format!("U{:04X}", u32::from(ch))
    }
}

#[derive(Debug, Default)]
struct WaylandVkState {
    seat: Option<WlSeat>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<WlRegistry, ()> for WaylandVkState {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_seat" && state.seat.is_none() {
                state.seat = Some(proxy.bind::<WlSeat, _, _>(name, version.min(7), qhandle, ()));
            } else if interface == "zwp_virtual_keyboard_manager_v1" && state.vk_manager.is_none() {
                state.vk_manager = Some(proxy.bind::<ZwpVirtualKeyboardManagerV1, _, _>(
                    name,
                    version.min(1),
                    qhandle,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandVkState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WaylandVkState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: zwp_virtual_keyboard_manager_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WaylandVkState {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: zwp_virtual_keyboard_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn connect_wayland(display_override: Option<&str>) -> Result<Connection> {
    if let Some(disp) = display_override
        && let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
    {
        let sock_path = PathBuf::from(runtime_dir).join(disp);
        if sock_path.exists() {
            let stream = UnixStream::connect(&sock_path)
                .with_context(|| format!("failed to connect to {}", sock_path.display()))?;
            return Connection::from_socket(stream).context("failed to init Wayland connection");
        }
    }
    Connection::connect_to_env().context("failed to connect to Wayland display")
}

/// Verifies that the compositor exposes `zwp_virtual_keyboard_manager_v1` without sending keys.
///
/// # Errors
///
/// Returns an error if Wayland connection fails or `zwp_virtual_keyboard_manager_v1` is absent.
pub fn probe_wayland_virtual_keyboard(display_override: Option<&str>) -> Result<()> {
    let conn = connect_wayland(display_override)?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = WaylandVkState::default();
    event_queue
        .roundtrip(&mut state)
        .context("Wayland registry roundtrip failed")?;

    let (Some(seat), Some(manager)) = (&state.seat, &state.vk_manager) else {
        bail!("Wayland compositor does not advertise wl_seat + zwp_virtual_keyboard_manager_v1");
    };

    let vk = manager.create_virtual_keyboard(seat, &qh, ());
    let (keymap_str, _) = build_unicode_xkb_keymap(&['a']);
    upload_keymap(&vk, &keymap_str)?;
    vk.modifiers(0, 0, 0, 0);
    event_queue
        .roundtrip(&mut state)
        .context("Wayland virtual keyboard probe roundtrip failed")?;
    vk.destroy();
    let _ = event_queue.roundtrip(&mut state);
    Ok(())
}

fn wayland_type_text(
    text: &str,
    forbid_enter_keys: bool,
    display_override: Option<&str>,
) -> Result<()> {
    let conn = connect_wayland(display_override)?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = WaylandVkState::default();
    event_queue
        .roundtrip(&mut state)
        .context("Wayland registry roundtrip failed")?;

    let (Some(seat), Some(manager)) = (&state.seat, &state.vk_manager) else {
        bail!("compositor missing zwp_virtual_keyboard_manager_v1");
    };

    let vk = manager.create_virtual_keyboard(seat, &qh, ());
    let chars: Vec<char> = text.chars().collect();
    let chunks = split_by_unique_char_budget(&chars, MAX_UNIQUE_CHARS_PER_CHUNK);

    let mut tick_ms = 1_u32;
    for chunk in chunks {
        let mut unique = Vec::new();
        for &ch in chunk {
            if forbid_enter_keys && (is_line_break_char(ch) || ch.is_control()) {
                continue;
            }
            if !unique.contains(&ch) {
                unique.push(ch);
            }
        }
        if unique.is_empty() {
            continue;
        }

        let (keymap_str, code_map) = build_unicode_xkb_keymap(&unique);
        upload_keymap(&vk, &keymap_str)?;
        vk.modifiers(0, 0, 0, 0);
        event_queue
            .roundtrip(&mut state)
            .context("Wayland keymap sync failed")?;

        for (i, &ch) in chunk.iter().enumerate() {
            let Some(&evdev_code) = code_map.get(&ch) else {
                continue;
            };
            // Defense-in-depth: refuse Enter/NumpadEnter scancodes when targeting Alacritty.
            if forbid_enter_keys && matches!(evdev_code, 28 | 96) {
                continue;
            }
            vk.key(tick_ms, evdev_code, WL_KEY_PRESSED);
            tick_ms = tick_ms.wrapping_add(1);
            vk.key(tick_ms, evdev_code, WL_KEY_RELEASED);
            tick_ms = tick_ms.wrapping_add(1);

            if (i + 1).is_multiple_of(KEYSTROKE_BATCH_SIZE) {
                event_queue
                    .roundtrip(&mut state)
                    .context("Wayland keystroke batch sync failed")?;
                thread::sleep(KEYSTROKE_BATCH_PAUSE);
            }
        }
        event_queue
            .roundtrip(&mut state)
            .context("Wayland chunk completion sync failed")?;
    }

    vk.destroy();
    let _ = event_queue.roundtrip(&mut state);
    Ok(())
}

fn split_by_unique_char_budget(chars: &[char], max_unique: usize) -> Vec<&[char]> {
    let mut result = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut seen = Vec::with_capacity(max_unique);
        let mut end = start;
        while end < chars.len() {
            let ch = chars[end];
            if !seen.contains(&ch) {
                if seen.len() >= max_unique {
                    break;
                }
                seen.push(ch);
            }
            end += 1;
        }
        result.push(&chars[start..end]);
        start = end;
    }
    result
}

fn upload_keymap(vk: &ZwpVirtualKeyboardV1, keymap_text: &str) -> Result<()> {
    let name = CString::new("sttme-xkb").context("invalid memfd name")?;
    let fd = memfd_create(&name, MemfdFlags::CLOEXEC).context("memfd_create failed")?;
    let mut file = std::fs::File::from(fd);

    let mut bytes = Vec::with_capacity(keymap_text.len() + 1);
    bytes.extend_from_slice(keymap_text.as_bytes());
    bytes.push(0);

    file.write_all(&bytes)
        .context("failed to write XKB keymap to memfd")?;
    file.flush().context("failed to flush XKB memfd")?;

    vk.keymap(KeymapFormat::XkbV1.into(), file.as_fd(), bytes.len() as u32);
    Ok(())
}

fn paste_via_clipboard(
    text: &str,
    is_terminal: bool,
    wayland_display_override: Option<&str>,
) -> Result<()> {
    let old_clipboard = read_clipboard(wayland_display_override);
    write_clipboard(text, wayland_display_override)?;
    thread::sleep(Duration::from_millis(35));

    if wayland_send_paste_shortcut(is_terminal, wayland_display_override).is_err() {
        let combo = if is_terminal {
            "ctrl+shift+v"
        } else {
            "ctrl+v"
        };
        let status = Command::new("xdotool")
            .args(["key", "--clearmodifiers", "--delay", "40", combo])
            .stderr(Stdio::null())
            .status()
            .context("neither Wayland virtual keyboard nor xdotool succeeded for paste")?;
        if !status.success() {
            bail!("xdotool exited with non-zero status while pasting");
        }
    }

    if let Some(previous) = old_clipboard {
        let len = text.len();
        let disp_owned = wayland_display_override.map(str::to_owned);
        drop(thread::spawn(move || {
            let delay_secs = (1.0 + (len as f64) * 0.0001).clamp(1.0, 3.0);
            thread::sleep(Duration::from_secs_f64(delay_secs));
            drop(write_clipboard(&previous, disp_owned.as_deref()));
        }));
    }

    Ok(())
}

fn wayland_send_paste_shortcut(is_terminal: bool, display_override: Option<&str>) -> Result<()> {
    let conn = connect_wayland(display_override)?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = WaylandVkState::default();
    event_queue.roundtrip(&mut state)?;

    let (Some(seat), Some(manager)) = (&state.seat, &state.vk_manager) else {
        bail!("compositor missing zwp_virtual_keyboard_manager_v1");
    };

    let us_keymap = concat!(
        "xkb_keymap {\n",
        "  xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n",
        "  xkb_types { include \"complete\" };\n",
        "  xkb_compatibility { include \"complete\" };\n",
        "  xkb_symbols { include \"pc+us+inet(evdev)\" };\n",
        "};\n"
    );

    let vk = manager.create_virtual_keyboard(seat, &qh, ());
    upload_keymap(&vk, us_keymap)?;
    vk.modifiers(0, 0, 0, 0);
    event_queue.roundtrip(&mut state)?;

    vk.key(1, EVDEV_KEY_LEFTCTRL, WL_KEY_PRESSED);
    if is_terminal {
        vk.key(2, EVDEV_KEY_LEFTSHIFT, WL_KEY_PRESSED);
        vk.modifiers(XKB_MOD_CTRL_MASK | XKB_MOD_SHIFT_MASK, 0, 0, 0);
    } else {
        vk.modifiers(XKB_MOD_CTRL_MASK, 0, 0, 0);
    }
    event_queue.roundtrip(&mut state)?;

    vk.key(3, EVDEV_KEY_V, WL_KEY_PRESSED);
    vk.key(4, EVDEV_KEY_V, WL_KEY_RELEASED);

    if is_terminal {
        vk.key(5, EVDEV_KEY_LEFTSHIFT, WL_KEY_RELEASED);
    }
    vk.key(6, EVDEV_KEY_LEFTCTRL, WL_KEY_RELEASED);
    vk.modifiers(0, 0, 0, 0);
    event_queue.roundtrip(&mut state)?;

    vk.destroy();
    let _ = event_queue.roundtrip(&mut state);
    Ok(())
}

fn read_clipboard(wayland_display_override: Option<&str>) -> Option<String> {
    let mut cmd = Command::new("wl-paste");
    cmd.arg("--no-newline").stderr(Stdio::null());
    if let Some(disp) = wayland_display_override {
        cmd.env("WAYLAND_DISPLAY", disp);
    }
    if let Ok(out) = cmd.output()
        && out.status.success()
    {
        return String::from_utf8(out.stdout).ok();
    }

    let out = Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn write_clipboard(text: &str, wayland_display_override: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("wl-copy");
    if let Some(disp) = wayland_display_override {
        cmd.env("WAYLAND_DISPLAY", disp);
    }
    if let Ok(mut child) = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
    }

    let mut xclip = Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("neither `wl-copy` nor `xclip` is available")?;
    if let Some(mut stdin) = xclip.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = xclip.wait()?;
    if !status.success() {
        bail!("xclip failed to write clipboard");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_for_alacritty_strips_all_enter_and_line_breaks() {
        let tricky =
            "echo hello\r\nrm -rf /\nline3\rline4\u{2028}line5\u{2029}line6\u{0085}\x03\x1b";
        let sanitized = sanitize_for_alacritty(tricky);
        assert_eq!(sanitized, "echo hello rm -rf / line3 line4 line5 line6");
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\r'));
        assert!(!sanitized.chars().any(char::is_control));
    }

    #[test]
    fn prepare_output_text_respects_trailing_space_without_newlines() {
        let out = prepare_output_text("  hello\nworld\r\n", true, true);
        assert_eq!(out, "hello world ");
        assert!(!out.contains('\n'));
        assert!(!out.contains('\r'));
    }

    #[test]
    fn generated_unicode_xkb_keymap_compiles_cleanly_with_xkbcomp() {
        let sample_chars = [
            'H', 'e', 'l', 'o', ' ', ',', '.', '!', '?', '-', '_', '/', 'é',
        ];
        let (keymap_text, map) = build_unicode_xkb_keymap(&sample_chars);

        // Verify no keycode maps to Enter (28), NumpadEnter (96), Esc (1), or F12 (88).
        for &evdev in map.values() {
            assert!(evdev >= 120, "unexpected low evdev scancode {evdev}");
        }

        let out_path =
            std::env::temp_dir().join(format!("sttme-test-keymap-{}.xkb", std::process::id()));
        if let Ok(mut child) = Command::new("xkbcomp")
            .arg("-xkb")
            .arg("-")
            .arg(&out_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(keymap_text.as_bytes())
                    .expect("write keymap to xkbcomp");
            }
            let output = child.wait_with_output().expect("wait xkbcomp");
            drop(std::fs::remove_file(&out_path));
            assert!(
                output.status.success(),
                "xkbcomp rejected generated keymap: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
