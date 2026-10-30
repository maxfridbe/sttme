// Rust guideline compliant 2026-02-21
//! Native Windows system tray application, low-level hotkey hook, layered waveform HUD, and `SendInput` engine.
//!
//! Runs `sttme.exe` as a lightweight zero-console Notification Area (system tray) app with:
//! - Global `WH_KEYBOARD_LL` hold-to-talk hook (`F10` by default, swallowing menu-bar activation)
//! - Per-monitor DPI-aware `50 FPS` top-center `UpdateLayeredWindow` LCARS live speech waveform HUD
//! - Direct `SendInput` (`KEYEVENTF_UNICODE`) UTF-16 text injection + terminal Enter stripping
//! - Live right-click tray menu for hotkey, Whisper model, waveform/audio toggles, and sign-in autostart

#![expect(
    unsafe_code,
    reason = "Win32 system tray, WH_KEYBOARD_LL hook, UpdateLayeredWindow HUD, and SendInput FFI"
)]

use std::ffi::c_void;
use std::mem::zeroed;
use std::net::SocketAddr;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::{Level, event};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, FALSE, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM,
    LRESULT, POINT, RECT, SIZE, TRUE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    ReleaseDC, SelectObject,
};
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    VK_CONTROL, VK_ESCAPE, VK_F9, VK_F10, VK_F11, VK_F12, VK_RCONTROL, VK_RETURN, VK_RMENU,
    VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindowTextW, GetWindowThreadProcessId, HHOOK, HICON,
    HMENU, ICONINFO, IDC_ARROW, KBDLLHOOKSTRUCT, KillTimer, LLKHF_INJECTED, LoadCursorW,
    MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetForegroundWindow, SetTimer,
    SetWindowsHookExW, ShowWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, ULW_ALPHA, UnhookWindowsHookEx, UpdateLayeredWindow, WH_KEYBOARD_LL, WM_APP,
    WM_COMMAND, WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONUP, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::audio::{
    ActiveCapture, BeepKind, MIN_SPEECH_SAMPLES, WHISPER_SAMPLE_RATE, WaveformMeter,
    decode_wav_to_16k_f32, encode_wav_16k, has_speech, play_beep,
};
use crate::config::{AppConfig, cache_dir, config_file_path, load_config, save_config};
use crate::history::HistoryStore;
use crate::inject::{inject_text, prepare_output_text};
use crate::transcribe::{Transcriber, is_hallucination};
use crate::waveform::{HUD_HEIGHT, HUD_TOP_MARGIN, HUD_WIDTH, WaveformAnimator};
use crate::window::{WindowBackend, WindowInfo, focus_window, get_focused_window};

/// Custom window message delivered when the user interacts with the Notification Area icon.
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;

/// Custom window message posted by `WH_KEYBOARD_LL` when the hold-to-talk key is pressed.
const WM_PTT_DOWN: u32 = WM_APP + 2;

/// Custom window message posted by `WH_KEYBOARD_LL` when the hold-to-talk key is released.
const WM_PTT_UP: u32 = WM_APP + 3;

/// Custom window message posted by `WH_KEYBOARD_LL` when `Escape` cancels an active hold.
const WM_PTT_CANCEL: u32 = WM_APP + 4;

/// Custom window message posted by background workers to refresh the tray icon and tooltip.
const WM_REFRESH_TRAY: u32 = WM_APP + 5;

/// Timer identifier driving `50 FPS` (`20ms`) layered HUD waveform updates while recording.
const HUD_TIMER_ID: usize = 1;

/// Frame interval (`20ms` = `50 FPS`) for the layered Win32 waveform HUD timer.
const HUD_FRAME_INTERVAL_MS: u32 = 20;

/// Clipboard format constant for UTF-16 text (`CF_UNICODETEXT = 13`).
const CF_UNICODETEXT_ID: u32 = 13;

/// Virtual-key code for `V` (`0x56`) used for `Ctrl+V` / `Ctrl+Shift+V` paste fallback.
const VK_KEY_V: u16 = 0x56;

/// Maximum UTF-16 code units sent per `SendInput` batch to avoid overflowing terminal input queues.
const WIN_KEYSTROKE_BATCH: usize = 32;

/// Brief yield (`2ms`) between `SendInput` batches for long transcriptions.
const WIN_BATCH_PAUSE: Duration = Duration::from_millis(2);

/// Anti-double-inject debounce window (`500ms`) matching the Linux daemon.
const INJECT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Maximum multipart upload size (`50 MiB`) for the optional `sttme serve` HTTP API.
const MAX_HTTP_AUDIO_BYTES: usize = 50 * 1024 * 1024;

/// Tray state encoding stored in [`TRAY_PHASE`]: Idle (`0`), Recording (`1`), Transcribing (`2`).
const PHASE_IDLE: u8 = 0;
const PHASE_RECORDING: u8 = 1;
const PHASE_TRANSCRIBING: u8 = 2;

// Menu command IDs
const CMD_HOTKEY_F10: usize = 1001;
const CMD_HOTKEY_F9: usize = 1002;
const CMD_HOTKEY_F12: usize = 1003;
const CMD_HOTKEY_RCTRL: usize = 1004;
const CMD_HOTKEY_RALT: usize = 1005;

const CMD_MODEL_TINY: usize = 1101;
const CMD_MODEL_BASE: usize = 1102;
const CMD_MODEL_BASE_EN: usize = 1103;
const CMD_MODEL_SMALL: usize = 1104;
const CMD_MODEL_TURBO: usize = 1105;

const CMD_TOGGLE_HUD: usize = 1201;
const CMD_TOGGLE_BEEPS: usize = 1202;
const CMD_TOGGLE_STRIP_ENTER: usize = 1203;
const CMD_TOGGLE_ALACRITTY_ONLY: usize = 1204;
const CMD_TOGGLE_AUTOSTART: usize = 1205;

const CMD_RECOVER_LAST: usize = 1301;
const CMD_OPEN_CONFIG: usize = 1302;
const CMD_RETRY_PENDING: usize = 1303;
const CMD_QUIT: usize = 1399;

/// Registry subkey under `HKCU` controlling sign-in startup apps on Windows.
const RUN_REGISTRY_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

/// Global atomic state shared with the low-level `WH_KEYBOARD_LL` callback (`< 0.1ms` execution).
static ACTIVE_HOTKEY_VK: AtomicU16 = AtomicU16::new(VK_F10);
static IS_HOTKEY_HELD: AtomicBool = AtomicBool::new(false);
static TRAY_HOST_HWND: AtomicIsize = AtomicIsize::new(0);
static TRAY_PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);

static APP_STATE: OnceLock<Arc<WinTrayApp>> = OnceLock::new();

struct RecordingSession {
    capture: ActiveCapture,
    target_window: Option<WindowInfo>,
}

struct MutableTrayState {
    cfg: AppConfig,
    transcriber: Option<Arc<Transcriber>>,
    recording: Option<RecordingSession>,
    hud_meter: Option<Arc<WaveformMeter>>,
    hud_started_at: Instant,
    animator: WaveformAnimator,
    history: HistoryStore,
    last_inject_at: Option<Instant>,
    last_external_window: Option<WindowInfo>,
}

struct WinTrayApp {
    rt: tokio::runtime::Runtime,
    inner: Mutex<MutableTrayState>,
    hud_hwnd: AtomicIsize,
    icon_idle: AtomicIsize,
    icon_rec: AtomicIsize,
    icon_transcribe: AtomicIsize,
}

impl WinTrayApp {
    fn beep(&self, kind: BeepKind) {
        let enabled = self.inner.lock().map_or(true, |g| g.cfg.ui.audio_beeps);
        if enabled {
            play_beep(kind);
        }
    }
}

/// Attaches `stdout`, `stderr`, and `stdin` to the parent console when run from PowerShell/cmd.
pub fn attach_parent_console_if_present() {
    // SAFETY: `AttachConsole(ATTACH_PARENT_PROCESS)` is safe to invoke at process startup;
    // when launched from Explorer with no parent console, it simply returns FALSE.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Maps a human-readable hotkey name (`"f10"`, `"f9"`, `"f12"`, `"rctrl"`, `"ralt"`) to Win32 `VK_*`.
#[must_use]
pub fn hotkey_name_to_vk(name: &str) -> u16 {
    match name.trim().to_ascii_lowercase().as_str() {
        "f9" => VK_F9,
        "f11" => VK_F11,
        "f12" => VK_F12,
        "rctrl" | "rightctrl" => VK_RCONTROL,
        "ralt" | "rightalt" => VK_RMENU,
        _ => VK_F10,
    }
}

/// Formats a canonical display label for the configured hotkey.
#[must_use]
pub fn hotkey_display_name(name: &str) -> &'static str {
    match hotkey_name_to_vk(name) {
        VK_F9 => "F9",
        VK_F11 => "F11",
        VK_F12 => "F12",
        VK_RCONTROL => "Right Ctrl",
        VK_RMENU => "Right Alt",
        _ => "F10",
    }
}

/// Queries the currently focused Win32 foreground window (`HWND`, `.exe` basename, and title).
#[must_use]
pub fn query_foreground_window() -> Option<WindowInfo> {
    // SAFETY: `GetForegroundWindow` takes no pointers and returns either a valid `HWND` or null.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }

    let mut pid: u32 = 0;
    // SAFETY: `hwnd` is non-null and `&mut pid` is a valid aligned `u32` out-pointer.
    let _ = unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };

    let app_id = query_process_basename(pid).unwrap_or_default();
    let title = query_window_title(hwnd);

    Some(WindowInfo {
        id: (hwnd as isize).to_string(),
        app_id,
        title,
        pid: (pid != 0).then_some(pid),
        backend: WindowBackend::Win32,
    })
}

/// Restores foreground focus to a previously captured Win32 `HWND` string.
pub fn restore_foreground_window(hwnd_str: &str) {
    let Ok(raw) = hwnd_str.parse::<isize>() else {
        return;
    };
    if raw == 0 {
        return;
    }
    let hwnd = raw as HWND;
    // SAFETY: `SetForegroundWindow` safely ignores stale or invalid window handles in Win32.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

fn query_process_basename(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` opens a read-only handle
    // that we close unconditionally before returning.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
    if handle.is_null() {
        return None;
    }

    let mut buf = [0_u16; 512];
    let mut size = u32::try_from(buf.len()).unwrap_or(512);
    // SAFETY: `buf` has capacity `size` UTF-16 code units; `handle` is valid and closed below.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &raw mut size)
    };
    // SAFETY: `handle` was returned by `OpenProcess` above and is not used after `CloseHandle`.
    unsafe {
        let _ = CloseHandle(handle);
    }
    if ok == FALSE || size == 0 {
        return None;
    }

    let full_path = String::from_utf16_lossy(&buf[..size as usize]);
    Path::new(&full_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
}

fn query_window_title(hwnd: HWND) -> String {
    let mut buf = [0_u16; 256];
    // SAFETY: `buf` provides 256 writable `u16` elements for `GetWindowTextW`.
    let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), 256) };
    if len <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..(len as usize)])
    }
}

/// Types UTF-8 `text` directly into the focused Win32 window via `SendInput` (`KEYEVENTF_UNICODE`).
///
/// # Errors
///
/// Returns an error if `SendInput` rejects the synthesized keyboard events.
pub fn win32_type_text(text: &str, forbid_enter_keys: bool) -> Result<()> {
    let mut units: Vec<u16> = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        if forbid_enter_keys && (ch == '\n' || ch.is_control()) {
            continue;
        }
        let mut tmp = [0_u16; 2];
        for &u in ch.encode_utf16(&mut tmp).iter() {
            units.push(u);
        }
    }

    if units.is_empty() {
        return Ok(());
    }

    for chunk in units.chunks(WIN_KEYSTROKE_BATCH) {
        let mut inputs = Vec::with_capacity(chunk.len() * 2);
        for &unit in chunk {
            if unit == u16::from(b'\n') && !forbid_enter_keys {
                inputs.push(make_vk_input(VK_RETURN, false));
                inputs.push(make_vk_input(VK_RETURN, true));
            } else {
                inputs.push(make_unicode_input(unit, false));
                inputs.push(make_unicode_input(unit, true));
            }
        }

        let count = u32::try_from(inputs.len()).unwrap_or(0);
        // SAFETY: `inputs` is a contiguous slice of valid `INPUT` structs and `size_of::<INPUT>()`
        // matches the exact Win32 ABI structure size.
        let sent = unsafe {
            SendInput(
                count,
                inputs.as_ptr(),
                i32::try_from(size_of::<INPUT>()).unwrap_or(40),
            )
        };
        if sent == 0 {
            bail!("Win32 SendInput injected 0 events (UIPI or locked desktop)");
        }
        if units.len() > WIN_KEYSTROKE_BATCH {
            std::thread::sleep(WIN_BATCH_PAUSE);
        }
    }

    Ok(())
}

/// Copies `text` to the Win32 clipboard and synthesizes `Ctrl+Shift+V` (terminals) or `Ctrl+V`.
///
/// # Errors
///
/// Returns an error if opening or writing the Win32 clipboard fails.
pub fn win32_paste_via_clipboard(text: &str, is_terminal: bool) -> Result<()> {
    let previous = read_win32_clipboard();
    write_win32_clipboard(text)?;
    std::thread::sleep(Duration::from_millis(30));

    let mut shortcut = Vec::with_capacity(6);
    shortcut.push(make_vk_input(VK_CONTROL, false));
    if is_terminal {
        shortcut.push(make_vk_input(VK_SHIFT, false));
    }
    shortcut.push(make_vk_input(VK_KEY_V, false));
    shortcut.push(make_vk_input(VK_KEY_V, true));
    if is_terminal {
        shortcut.push(make_vk_input(VK_SHIFT, true));
    }
    shortcut.push(make_vk_input(VK_CONTROL, true));

    // SAFETY: `shortcut` is a contiguous slice of initialized `INPUT_KEYBOARD` records.
    let sent = unsafe {
        SendInput(
            u32::try_from(shortcut.len()).unwrap_or(0),
            shortcut.as_ptr(),
            i32::try_from(size_of::<INPUT>()).unwrap_or(40),
        )
    };
    if sent == 0 {
        bail!("Win32 SendInput failed to emit paste shortcut");
    }

    if let Some(prev) = previous {
        drop(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(900));
            drop(write_win32_clipboard(&prev));
        }));
    }

    Ok(())
}

fn make_unicode_input(scan_unit: u16, key_up: bool) -> INPUT {
    let flags = if key_up {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    } else {
        KEYEVENTF_UNICODE
    };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan_unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn make_vk_input(vk: u16, key_up: bool) -> INPUT {
    let flags = if key_up { KEYEVENTF_KEYUP } else { 0 };
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn read_win32_clipboard() -> Option<String> {
    // SAFETY: `OpenClipboard(null_mut())` opens the current task clipboard; closed before return.
    if unsafe { OpenClipboard(null_mut()) } == FALSE {
        return None;
    }
    let result = (|| {
        // SAFETY: Clipboard is open on this thread; `CF_UNICODETEXT` returns a global memory handle.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT_ID) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: Locking global handle owned by clipboard while clipboard remains open.
        let ptr = unsafe { GlobalLock(handle) }.cast::<u16>();
        if ptr.is_null() {
            return None;
        }
        // SAFETY: `GlobalSize` gives byte capacity of the locked UTF-16 buffer.
        let max_u16 = unsafe { GlobalSize(handle) } / 2;
        let mut len = 0_usize;
        while len < max_u16 {
            // SAFETY: `len < max_u16` stays strictly within the allocated global block.
            let ch = unsafe { *ptr.add(len) };
            if ch == 0 {
                break;
            }
            len += 1;
        }
        // SAFETY: `ptr` is valid for `len` `u16` elements while locked.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        let text = String::from_utf16_lossy(slice);
        // SAFETY: Balances `GlobalLock` above.
        unsafe {
            let _ = GlobalUnlock(handle);
        }
        Some(text)
    })();
    // SAFETY: Balances `OpenClipboard` above.
    unsafe {
        let _ = CloseClipboard();
    }
    result
}

fn write_win32_clipboard(text: &str) -> Result<()> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let byte_len = wide.len() * 2;

    // SAFETY: `OpenClipboard(null_mut())` opens the current task clipboard.
    if unsafe { OpenClipboard(null_mut()) } == FALSE {
        bail!("failed to open Windows clipboard");
    }

    // SAFETY: `EmptyClipboard` is valid while clipboard is open.
    unsafe {
        let _ = EmptyClipboard();
    }

    // SAFETY: Allocates moveable global block for `SetClipboardData` ownership transfer.
    let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if hmem.is_null() {
        // SAFETY: Balances `OpenClipboard`.
        unsafe {
            let _ = CloseClipboard();
        }
        bail!("GlobalAlloc failed for clipboard text");
    }

    // SAFETY: `hmem` was freshly allocated with `byte_len` bytes.
    let dst = unsafe { GlobalLock(hmem) }.cast::<u16>();
    if dst.is_null() {
        // SAFETY: Balances `OpenClipboard`.
        unsafe {
            let _ = CloseClipboard();
        }
        bail!("GlobalLock failed for clipboard text");
    }
    // SAFETY: `dst` has exact capacity `wide.len()` `u16` elements and does not overlap `wide`.
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);
    }

    // SAFETY: Transfers ownership of `hmem` to the system clipboard upon success.
    let set_res = unsafe { SetClipboardData(CF_UNICODETEXT_ID, hmem) };
    // SAFETY: Balances `OpenClipboard`.
    unsafe {
        let _ = CloseClipboard();
    }
    if set_res.is_null() {
        bail!("SetClipboardData failed");
    }
    Ok(())
}

/// Checks whether `sttme` is registered under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
#[must_use]
pub fn is_autostart_enabled() -> bool {
    let subkey = to_wide_null(RUN_REGISTRY_SUBKEY);
    let val_name = to_wide_null("sttme");
    let mut hkey: HKEY = null_mut();
    // SAFETY: `subkey` is null-terminated UTF-16 and `hkey` receives the opened registry key.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ,
            &raw mut hkey,
        )
    };
    if status != 0 || hkey.is_null() {
        return false;
    }
    let mut data_len: u32 = 0;
    // SAFETY: Passing null data buffer queries whether value `"sttme"` exists.
    let q = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            null(),
            null_mut(),
            null_mut(),
            &raw mut data_len,
        )
    };
    // SAFETY: Closes opened `hkey`.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    q == 0 && data_len > 0
}

/// Enables or disables launching `sttme.exe` automatically at Windows Sign-In.
///
/// # Errors
///
/// Returns an error if the current executable path or `HKCU` Run registry key cannot be updated.
pub fn set_autostart_enabled(enable: bool) -> Result<()> {
    let subkey = to_wide_null(RUN_REGISTRY_SUBKEY);
    let val_name = to_wide_null("sttme");
    let mut hkey: HKEY = null_mut();
    // SAFETY: Creates or opens `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` for write.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            null(),
            &raw mut hkey,
            null_mut(),
        )
    };
    if status != 0 || hkey.is_null() {
        bail!("failed to open HKCU Run registry key (Win32 status {status})");
    }

    let res = if enable {
        let exe = std::env::current_exe().context("failed to resolve sttme.exe path")?;
        let quoted = format!("\"{}\"", exe.display());
        let wide = to_wide_null(&quoted);
        let byte_len = u32::try_from(wide.len() * 2).unwrap_or(0);
        // SAFETY: `wide` is null-terminated UTF-16 of `byte_len` bytes.
        let rc = unsafe {
            RegSetValueExW(
                hkey,
                val_name.as_ptr(),
                0,
                REG_SZ,
                wide.as_ptr().cast::<u8>(),
                byte_len,
            )
        };
        if rc != 0 {
            Err(anyhow::anyhow!("RegSetValueExW failed with status {rc}"))
        } else {
            Ok(())
        }
    } else {
        // SAFETY: Deletes `"sttme"` value if present; missing value is harmless.
        unsafe {
            let _ = RegDeleteValueW(hkey, val_name.as_ptr());
        }
        Ok(())
    };

    // SAFETY: Closes opened `hkey`.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    res
}

/// Runs the native Windows System Tray application (`sttme.exe` default mode).
///
/// # Errors
///
/// Returns an error if single-instance mutex creation, window registration, or keyboard hook setup fails.
pub fn run_windows_tray(cfg: &AppConfig, http_addr: Option<SocketAddr>) -> Result<()> {
    // Enable crisp per-monitor HiDPI scaling on 4K laptops and external monitors.
    // SAFETY: Safe process-wide DPI context call before any `HWND` is created.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let mutex_name = to_wide_null("Local\\sttme-tray-singleton-v1");
    // SAFETY: `mutex_name` is a valid null-terminated UTF-16 string; handle lives until process exit.
    let single_mutex: HANDLE = unsafe { CreateMutexW(null(), TRUE, mutex_name.as_ptr()) };
    if single_mutex.is_null() {
        bail!("failed to create single-instance mutex");
    }
    // SAFETY: Queries thread-local Win32 error immediately after `CreateMutexW`.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: Closes duplicate mutex handle before exiting.
        unsafe {
            let _ = CloseHandle(single_mutex);
        }
        return Ok(());
    }

    ACTIVE_HOTKEY_VK.store(hotkey_name_to_vk(&cfg.target.hotkey), Ordering::Relaxed);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to initialize Tokio runtime")?;
    let history = HistoryStore::new(&cache_dir(), cfg.history.limit)?;

    let icon_idle = create_lcars_tray_icon(88, 200, 255)?;
    let icon_rec = create_lcars_tray_icon(255, 96, 46)?;
    let icon_transcribe = create_lcars_tray_icon(255, 184, 58)?;

    let app = Arc::new(WinTrayApp {
        rt,
        inner: Mutex::new(MutableTrayState {
            cfg: cfg.clone(),
            transcriber: None,
            recording: None,
            hud_meter: None,
            hud_started_at: Instant::now(),
            animator: WaveformAnimator::new(),
            history,
            last_inject_at: None,
            last_external_window: None,
        }),
        hud_hwnd: AtomicIsize::new(0),
        icon_idle: AtomicIsize::new(icon_idle as isize),
        icon_rec: AtomicIsize::new(icon_rec as isize),
        icon_transcribe: AtomicIsize::new(icon_transcribe as isize),
    });
    let _ = APP_STATE.set(Arc::clone(&app));

    let (tray_hwnd, hud_hwnd) = create_tray_and_hud_windows()?;
    TRAY_HOST_HWND.store(tray_hwnd as isize, Ordering::Release);
    app.hud_hwnd.store(hud_hwnd as isize, Ordering::Release);

    add_or_update_tray_icon(
        tray_hwnd,
        icon_idle,
        &format!("sttme — Loading '{}' model...", cfg.transcription.model),
        true,
        None,
    )?;

    // Load Whisper model asynchronously so the tray icon appears in < 15ms!
    spawn_model_loader(&app, cfg.clone(), http_addr);

    // Install global low-level keyboard hook (`WH_KEYBOARD_LL`) to intercept & swallow `F10`.
    // SAFETY: `ll_keyboard_proc` has exact `HOOKPROC` ABI and pumps on this thread's message loop.
    let hook: HHOOK =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), null_mut(), 0) };
    if hook.is_null() {
        remove_tray_icon(tray_hwnd);
        bail!("SetWindowsHookExW(WH_KEYBOARD_LL) failed");
    }

    run_win32_message_loop();

    // SAFETY: Unhooks `WH_KEYBOARD_LL`, removes tray icon, and releases procedural `HICON` handles.
    unsafe {
        let _ = UnhookWindowsHookEx(hook);
        remove_tray_icon(tray_hwnd);
        let _ = DestroyWindow(hud_hwnd);
        let _ = DestroyWindow(tray_hwnd);
        let _ = DestroyIcon(icon_idle);
        let _ = DestroyIcon(icon_rec);
        let _ = DestroyIcon(icon_transcribe);
        let _ = CloseHandle(single_mutex);
    }

    Ok(())
}

/// Runs the standalone `TalkType`-compatible HTTP API (`sttme.exe serve --port 8002`) on Windows.
///
/// # Errors
///
/// Returns an error if Whisper initialization or TCP port binding fails.
pub async fn run_windows_http_server(cfg: AppConfig, addr: SocketAddr) -> Result<()> {
    let transcriber = Arc::new(Transcriber::new(&cfg).await?);
    serve_http_with_transcriber(transcriber, addr).await
}

fn spawn_model_loader(app: &Arc<WinTrayApp>, cfg: AppConfig, http_addr: Option<SocketAddr>) {
    let app_clone = Arc::clone(app);
    app.rt.spawn(async move {
        match Transcriber::new(&cfg).await {
            Ok(engine) => {
                let engine_arc = Arc::new(engine);
                if let Some(addr) = http_addr {
                    let http_engine = Arc::clone(&engine_arc);
                    tokio::spawn(async move {
                        drop(serve_http_with_transcriber(http_engine, addr).await);
                    });
                }
                if let Ok(mut guard) = app_clone.inner.lock() {
                    guard.transcriber = Some(engine_arc);
                }
                post_refresh_tray();
            }
            Err(err) => {
                event!(
                    name: "win.model.load_failed",
                    Level::ERROR,
                    error.message = %err,
                    "failed to initialize Whisper model: {{error.message}}",
                );
                let hwnd = TRAY_HOST_HWND.load(Ordering::Acquire) as HWND;
                if !hwnd.is_null() {
                    let icon = app_clone.icon_idle.load(Ordering::Relaxed) as HICON;
                    let msg = format!("Model load error: {err}");
                    drop(add_or_update_tray_icon(
                        hwnd,
                        icon,
                        "sttme — Model Error",
                        false,
                        Some(("sttme Model Error", &msg)),
                    ));
                }
            }
        }
    });
}

fn post_refresh_tray() {
    let hwnd = TRAY_HOST_HWND.load(Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        // SAFETY: `PostMessageW` is thread-safe for valid `HWND`.
        unsafe {
            let _ = PostMessageW(hwnd, WM_REFRESH_TRAY, 0, 0);
        }
    }
}

fn run_win32_message_loop() {
    // SAFETY: `MSG` is a plain-old-data Win32 C struct whose all-zero bit pattern is valid.
    let mut msg: MSG = unsafe { zeroed() };
    // SAFETY: Standard Win32 message pump until `WM_QUIT` (`GetMessageW <= 0`).
    unsafe {
        while GetMessageW(&raw mut msg, null_mut(), 0, 0) > 0 {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam != 0 {
        // SAFETY: When `code >= 0`, Windows guarantees `lparam` points to a valid `KBDLLHOOKSTRUCT`.
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let is_injected = (kb.flags & LLKHF_INJECTED) != 0;
        if !is_injected {
            let vk = u16::try_from(kb.vkCode).unwrap_or(0);
            let target_vk = ACTIVE_HOTKEY_VK.load(Ordering::Relaxed);
            let msg = u32::try_from(wparam).unwrap_or(0);
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

            if vk == target_vk {
                let hwnd = TRAY_HOST_HWND.load(Ordering::Acquire) as HWND;
                if is_down {
                    if !IS_HOTKEY_HELD.swap(true, Ordering::AcqRel) && !hwnd.is_null() {
                        // SAFETY: Asynchronous `PostMessageW` returns immediately without stalling hook.
                        unsafe {
                            let _ = PostMessageW(hwnd, WM_PTT_DOWN, 0, 0);
                        }
                    }
                    // Swallow `F10` key-down and key-repeats so Windows app menu bars never pop up!
                    return 1;
                } else if is_up {
                    if IS_HOTKEY_HELD.swap(false, Ordering::AcqRel) && !hwnd.is_null() {
                        // SAFETY: Asynchronous `PostMessageW` to tray host window.
                        unsafe {
                            let _ = PostMessageW(hwnd, WM_PTT_UP, 0, 0);
                        }
                    }
                    // Swallow `F10` key-up as well to prevent `WM_SYSCOMMAND` (`SC_KEYMENU`).
                    return 1;
                }
            } else if vk == VK_ESCAPE && is_down && IS_HOTKEY_HELD.swap(false, Ordering::AcqRel) {
                let hwnd = TRAY_HOST_HWND.load(Ordering::Acquire) as HWND;
                if !hwnd.is_null() {
                    // SAFETY: Asynchronous `PostMessageW` to cancel recording.
                    unsafe {
                        let _ = PostMessageW(hwnd, WM_PTT_CANCEL, 0, 0);
                    }
                }
                return 1;
            }
        }
    }
    // SAFETY: Chains to next hook in Win32 hook chain.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

fn create_tray_and_hud_windows() -> Result<(HWND, HWND)> {
    // SAFETY: `GetModuleHandleW(null())` returns the current `.exe` module instance.
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(null()) };
    let tray_class = to_wide_null("sttmeTrayHostClass");
    let hud_class = to_wide_null("sttmeWaveformHudClass");

    let wc_tray = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(tray_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        // SAFETY: Standard system arrow cursor.
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: tray_class.as_ptr(),
    };
    let wc_hud = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(hud_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null(),
        lpszClassName: hud_class.as_ptr(),
    };

    // SAFETY: Both `WNDCLASSW` structs point to valid module-lifetime procedures and UTF-16 names.
    unsafe {
        let _ = RegisterClassW(&raw const wc_tray);
        let _ = RegisterClassW(&raw const wc_hud);
    };

    let title = to_wide_null("sttme");
    // SAFETY: Creates hidden top-level helper window for tray callbacks and timers.
    let tray_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            tray_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        )
    };
    if tray_hwnd.is_null() {
        bail!("failed to create hidden tray host window");
    }

    // SAFETY: Creates topmost, layered, click-through, non-activating popup window for the 50 FPS HUD.
    let hud_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            hud_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            HUD_WIDTH.cast_signed(),
            HUD_HEIGHT.cast_signed(),
            null_mut(),
            null_mut(),
            hinstance,
            null(),
        )
    };
    if hud_hwnd.is_null() {
        bail!("failed to create layered waveform HUD window");
    }

    Ok((tray_hwnd, hud_hwnd))
}

unsafe extern "system" fn hud_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: Default window procedure for click-through layered HUD popup.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PTT_DOWN => {
            if let Some(app) = APP_STATE.get() {
                handle_ptt_down(hwnd, app);
            }
            0
        }
        WM_PTT_UP => {
            if let Some(app) = APP_STATE.get() {
                handle_ptt_up(hwnd, app);
            }
            0
        }
        WM_PTT_CANCEL => {
            if let Some(app) = APP_STATE.get() {
                handle_ptt_cancel(hwnd, app);
            }
            0
        }
        WM_TIMER if wparam == HUD_TIMER_ID => {
            if let Some(app) = APP_STATE.get() {
                render_hud_frame(app);
            }
            0
        }
        WM_REFRESH_TRAY => {
            if let Some(app) = APP_STATE.get() {
                sync_tray_visuals(hwnd, app);
            }
            0
        }
        WM_TRAY_CALLBACK => {
            let mouse_msg = u32::try_from(lparam as usize & 0xFFFF).unwrap_or(0);
            if (mouse_msg == WM_RBUTTONUP || mouse_msg == WM_LBUTTONUP)
                && let Some(app) = APP_STATE.get()
            {
                show_tray_context_menu(hwnd, app);
            }
            0
        }
        WM_COMMAND => {
            let cmd_id = wparam & 0xFFFF;
            if let Some(app) = APP_STATE.get() {
                handle_menu_command(hwnd, app, cmd_id);
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: Signals main thread `GetMessageW` loop to exit cleanly.
            unsafe { PostQuitMessage(0) };
            0
        }
        // SAFETY: Delegates unhandled Win32 messages to default window procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_ptt_down(tray_hwnd: HWND, app: &Arc<WinTrayApp>) {
    if TRAY_PHASE.load(Ordering::Acquire) != PHASE_IDLE {
        return;
    }

    let focused = get_focused_window(None);
    let (alacritty_only, show_hud, ready) = if let Ok(mut guard) = app.inner.lock() {
        if let Some(win) = &focused {
            guard.last_external_window = Some(win.clone());
        }
        (
            guard.cfg.target.alacritty_only,
            guard.cfg.ui.waveform_overlay,
            guard.transcriber.is_some(),
        )
    } else {
        return;
    };

    if !ready {
        app.beep(BeepKind::Error);
        return;
    }

    if alacritty_only
        && let Some(win) = &focused
        && !win.is_alacritty()
    {
        app.beep(BeepKind::Error);
        return;
    }

    match ActiveCapture::start() {
        Ok(capture) => {
            let meter = capture.meter();
            if let Ok(mut guard) = app.inner.lock() {
                guard.recording = Some(RecordingSession {
                    capture,
                    target_window: focused,
                });
                guard.hud_meter = Some(meter);
                guard.hud_started_at = Instant::now();
                guard.animator = WaveformAnimator::new();
            }

            TRAY_PHASE.store(PHASE_RECORDING, Ordering::Release);
            sync_tray_visuals(tray_hwnd, app);
            app.beep(BeepKind::Start);

            if show_hud {
                show_and_start_hud(tray_hwnd, app);
            }
        }
        Err(err) => {
            event!(
                name: "win.capture.failed",
                Level::ERROR,
                error.message = %err,
                "failed to open Windows microphone: {{error.message}}",
            );
            app.beep(BeepKind::Error);
        }
    }
}

fn handle_ptt_up(tray_hwnd: HWND, app: &Arc<WinTrayApp>) {
    stop_and_hide_hud(tray_hwnd, app);

    let (session_opt, transcriber_opt, threshold) = if let Ok(mut guard) = app.inner.lock() {
        guard.hud_meter = None;
        (
            guard.recording.take(),
            guard.transcriber.as_ref().map(Arc::clone),
            guard.cfg.transcription.energy_threshold,
        )
    } else {
        return;
    };

    let Some(session) = session_opt else {
        TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
        sync_tray_visuals(tray_hwnd, app);
        return;
    };
    let Some(transcriber) = transcriber_opt else {
        drop(session.capture.stop());
        TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
        sync_tray_visuals(tray_hwnd, app);
        return;
    };

    TRAY_PHASE.store(PHASE_TRANSCRIBING, Ordering::Release);
    sync_tray_visuals(tray_hwnd, app);

    let app_clone = Arc::clone(app);
    app.rt.spawn(async move {
        process_windows_recording(app_clone, transcriber, session, threshold).await;
    });
}

fn handle_ptt_cancel(tray_hwnd: HWND, app: &Arc<WinTrayApp>) {
    stop_and_hide_hud(tray_hwnd, app);
    if let Ok(mut guard) = app.inner.lock() {
        guard.hud_meter = None;
        if let Some(session) = guard.recording.take() {
            drop(session.capture.stop());
        }
    }
    TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
    sync_tray_visuals(tray_hwnd, app);
    app.beep(BeepKind::Success);
}

async fn process_windows_recording(
    app: Arc<WinTrayApp>,
    transcriber: Arc<Transcriber>,
    session: RecordingSession,
    energy_threshold: f32,
) {
    let RecordingSession {
        capture,
        target_window,
    } = session;

    let Ok(Ok(samples)) = tokio::task::spawn_blocking(move || capture.stop()).await else {
        TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
        post_refresh_tray();
        app.beep(BeepKind::Error);
        return;
    };

    if samples.len() < MIN_SPEECH_SAMPLES || !has_speech(&samples, energy_threshold) {
        TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
        post_refresh_tray();
        app.beep(BeepKind::Success);
        return;
    }

    if let Ok(wav_bytes) = encode_wav_16k(&samples)
        && let Ok(guard) = app.inner.lock()
    {
        drop(guard.history.save_pending_wav(&wav_bytes));
    }

    match transcriber.transcribe_pcm(&samples, None).await {
        Ok(result) => {
            let raw_text = result.text.trim().to_owned();
            if raw_text.is_empty() || is_hallucination(&raw_text) {
                if let Ok(guard) = app.inner.lock() {
                    guard.history.clear_pending_wav();
                }
                TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
                post_refresh_tray();
                app.beep(BeepKind::Success);
                return;
            }

            if let Ok(mut guard) = app.inner.lock() {
                guard.history.clear_pending_wav();
                drop(guard.history.record(&raw_text));
            }

            let app_inject = Arc::clone(&app);
            let _ = tokio::task::spawn_blocking(move || {
                deliver_windows_text(&app_inject, &raw_text, target_window.as_ref());
            })
            .await;

            TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
            post_refresh_tray();
        }
        Err(err) => {
            event!(
                name: "win.transcribe.failed",
                Level::ERROR,
                error.message = %err,
                "Whisper transcription failed: {{error.message}}",
            );
            TRAY_PHASE.store(PHASE_IDLE, Ordering::Release);
            post_refresh_tray();
            app.beep(BeepKind::Error);
        }
    }
}

fn deliver_windows_text(app: &WinTrayApp, raw_text: &str, captured_target: Option<&WindowInfo>) {
    let (input_method, strip_terminal_enter, trailing_space, alacritty_only) =
        if let Ok(mut guard) = app.inner.lock() {
            let now = Instant::now();
            if let Some(prev) = guard.last_inject_at
                && now.duration_since(prev) < INJECT_DEBOUNCE
            {
                return;
            }
            guard.last_inject_at = Some(now);
            (
                guard.cfg.target.input_method,
                guard.cfg.target.strip_alacritty_enter,
                guard.cfg.target.trailing_space,
                guard.cfg.target.alacritty_only,
            )
        } else {
            return;
        };

    let mut current_focus = get_focused_window(None);
    if let Some(win) = captured_target {
        let same = current_focus.as_ref().is_some_and(|cur| cur.id == win.id);
        if !same {
            focus_window(win, None);
            std::thread::sleep(Duration::from_millis(25));
            current_focus = get_focused_window(None);
        }
    }

    if alacritty_only
        && let Some(active) = &current_focus
        && !active.is_alacritty()
    {
        app.beep(BeepKind::Error);
        return;
    }

    let is_terminal = current_focus
        .as_ref()
        .is_some_and(|w| w.is_alacritty() || w.is_terminal())
        || captured_target.is_some_and(|w| w.is_alacritty() || w.is_terminal());
    let strip_enter = is_terminal && strip_terminal_enter;

    let formatted = prepare_output_text(raw_text, strip_enter, trailing_space);
    if formatted.is_empty() {
        app.beep(BeepKind::Success);
        return;
    }

    if inject_text(&formatted, input_method, is_terminal, strip_enter, None).is_ok() {
        app.beep(BeepKind::Success);
    } else {
        app.beep(BeepKind::Error);
    }
}

fn show_and_start_hud(tray_hwnd: HWND, app: &WinTrayApp) {
    let hud_hwnd = app.hud_hwnd.load(Ordering::Acquire) as HWND;
    if hud_hwnd.is_null() {
        return;
    }
    render_hud_frame(app);
    // SAFETY: `SW_SHOWNOACTIVATE` displays the topmost layered pill without stealing focus.
    unsafe {
        let _ = ShowWindow(hud_hwnd, SW_SHOWNOACTIVATE);
        let _ = SetTimer(tray_hwnd, HUD_TIMER_ID, HUD_FRAME_INTERVAL_MS, None);
    }
}

fn stop_and_hide_hud(tray_hwnd: HWND, app: &WinTrayApp) {
    let hud_hwnd = app.hud_hwnd.load(Ordering::Acquire) as HWND;
    // SAFETY: Stops `HUD_TIMER_ID` and hides `hud_hwnd` so idle GPU/CPU cost is 0%.
    unsafe {
        let _ = KillTimer(tray_hwnd, HUD_TIMER_ID);
        if !hud_hwnd.is_null() {
            let _ = ShowWindow(hud_hwnd, SW_HIDE);
        }
    }
}

fn render_hud_frame(app: &WinTrayApp) {
    let hud_hwnd = app.hud_hwnd.load(Ordering::Acquire) as HWND;
    if hud_hwnd.is_null() {
        return;
    }

    let mut base_pixels = vec![0_u8; (HUD_WIDTH * HUD_HEIGHT * 4) as usize];
    if !sample_and_render_hud_pixels(app, &mut base_pixels) {
        return;
    }

    // Position top-center on the monitor containing the active foreground window.
    // SAFETY: `GetForegroundWindow` + `MonitorFromWindow` + `GetMonitorInfoW` query work area safely.
    let (mon_rect, dpi) = unsafe {
        let fg = GetForegroundWindow();
        let hmon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = u32::try_from(size_of::<MONITORINFO>()).unwrap_or(40);
        let rect = if GetMonitorInfoW(hmon, &raw mut mi) == FALSE {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        } else {
            mi.rcWork
        };
        let target_dpi = if fg.is_null() {
            96
        } else {
            GetDpiForWindow(fg)
        };
        (rect, target_dpi.max(96))
    };

    let scale = ((dpi as f32) / 96.0).clamp(1.0, 2.5);
    let dst_w = ((HUD_WIDTH as f32) * scale).round() as u32;
    let dst_h = ((HUD_HEIGHT as f32) * scale).round() as u32;
    let scaled_pixels = if (scale - 1.0).abs() < 0.05 {
        base_pixels
    } else {
        upscale_argb_bilinear(&base_pixels, HUD_WIDTH, HUD_HEIGHT, dst_w, dst_h)
    };

    let dst_w_i32 = dst_w.cast_signed();
    let mon_w = (mon_rect.right - mon_rect.left).max(dst_w_i32);
    let pos_x = mon_rect.left + (mon_w - dst_w_i32) / 2;
    let pos_y = mon_rect.top + (((HUD_TOP_MARGIN as f32) * scale).round() as i32);

    present_layered_dib(hud_hwnd, pos_x, pos_y, dst_w, dst_h, &scaled_pixels);
}

fn sample_and_render_hud_pixels(app: &WinTrayApp, base_pixels: &mut [u8]) -> bool {
    let Ok(mut guard) = app.inner.lock() else {
        return false;
    };
    let elapsed = guard.hud_started_at.elapsed().as_secs_f32();
    if let Some(meter) = &guard.hud_meter
        && let Some(level) = meter.take_rms()
    {
        guard.animator.step(level, elapsed);
    }
    guard.animator.render_argb8888(base_pixels, elapsed);
    true
}

fn upscale_argb_bilinear(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0_u8; (dst_w * dst_h * 4) as usize];
    let sw = src_w as usize;
    let sh = src_h as usize;
    let dw = dst_w as usize;
    let dh = dst_h as usize;

    for dy in 0..dh {
        let gy = ((dy as f32 + 0.5) * (src_h as f32) / (dst_h as f32) - 0.5).max(0.0);
        let y0 = (gy.floor() as usize).min(sh.saturating_sub(1));
        let y1 = (y0 + 1).min(sh.saturating_sub(1));
        let fy = gy - (y0 as f32);

        for dx in 0..dw {
            let gx = ((dx as f32 + 0.5) * (src_w as f32) / (dst_w as f32) - 0.5).max(0.0);
            let x0 = (gx.floor() as usize).min(sw.saturating_sub(1));
            let x1 = (x0 + 1).min(sw.saturating_sub(1));
            let fx = gx - (x0 as f32);

            let i00 = (y0 * sw + x0) * 4;
            let i10 = (y0 * sw + x1) * 4;
            let i01 = (y1 * sw + x0) * 4;
            let i11 = (y1 * sw + x1) * 4;
            let out_idx = (dy * dw + dx) * 4;

            for c in 0..4 {
                let top = f32::from(src[i00 + c]) * (1.0 - fx) + f32::from(src[i10 + c]) * fx;
                let bot = f32::from(src[i01 + c]) * (1.0 - fx) + f32::from(src[i11 + c]) * fx;
                let val = top * (1.0 - fy) + bot * fy;
                dst[out_idx + c] = val.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    dst
}

fn present_layered_dib(hwnd: HWND, x: i32, y: i32, width: u32, height: u32, bgra: &[u8]) {
    let w_i32 = i32::try_from(width).unwrap_or(320);
    let h_i32 = i32::try_from(height).unwrap_or(62);

    // SAFETY: Standard Win32 per-pixel alpha `CreateDIBSection` + `UpdateLayeredWindow` sequence;
    // all GDI handles (`mem_dc`, `dib`, `screen_dc`) are released before returning.
    unsafe {
        let screen_dc = GetDC(null_mut());
        if screen_dc.is_null() {
            return;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            let _ = ReleaseDC(null_mut(), screen_dc);
            return;
        }

        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40);
        bmi.bmiHeader.biWidth = w_i32;
        bmi.bmiHeader.biHeight = -h_i32; // Top-down premultiplied BGRA
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits_ptr: *mut c_void = null_mut();
        let dib = CreateDIBSection(
            screen_dc,
            &raw const bmi,
            DIB_RGB_COLORS,
            &raw mut bits_ptr,
            null_mut(),
            0,
        );
        if !dib.is_null() && !bits_ptr.is_null() {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits_ptr.cast::<u8>(), bgra.len());
            let old_obj = SelectObject(mem_dc, dib);

            let pt_dst = POINT { x, y };
            let size = SIZE {
                cx: w_i32,
                cy: h_i32,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                hwnd,
                screen_dc,
                &raw const pt_dst,
                &raw const size,
                mem_dc,
                &raw const pt_src,
                0,
                &raw const blend,
                ULW_ALPHA,
            );

            let _ = SelectObject(mem_dc, old_obj);
            let _ = DeleteObject(dib);
        }

        let _ = DeleteDC(mem_dc);
        let _ = ReleaseDC(null_mut(), screen_dc);
    }
}

fn sync_tray_visuals(tray_hwnd: HWND, app: &WinTrayApp) {
    let phase = TRAY_PHASE.load(Ordering::Acquire);
    let (model, hotkey_label) = app.inner.lock().map_or_else(
        |_| ("base".to_owned(), "F10"),
        |g| {
            (
                g.cfg.transcription.model.clone(),
                hotkey_display_name(&g.cfg.target.hotkey),
            )
        },
    );

    let (icon, tip) = match phase {
        PHASE_RECORDING => (
            app.icon_rec.load(Ordering::Relaxed) as HICON,
            format!("sttme — Listening (Release {hotkey_label} to Transcribe)"),
        ),
        PHASE_TRANSCRIBING => (
            app.icon_transcribe.load(Ordering::Relaxed) as HICON,
            format!("sttme — Transcribing with '{model}'..."),
        ),
        _ => (
            app.icon_idle.load(Ordering::Relaxed) as HICON,
            format!("sttme — Ready (Hold {hotkey_label} to Dictate • Model: {model})"),
        ),
    };

    drop(add_or_update_tray_icon(tray_hwnd, icon, &tip, false, None));
}

fn show_tray_context_menu(tray_hwnd: HWND, app: &WinTrayApp) {
    let Ok(guard) = app.inner.lock() else {
        return;
    };
    let phase = TRAY_PHASE.load(Ordering::Acquire);
    let status_label = match phase {
        PHASE_RECORDING => format!("sttme — Listening ({})", guard.cfg.transcription.model),
        PHASE_TRANSCRIBING => format!("sttme — Transcribing ({})", guard.cfg.transcription.model),
        _ if guard.transcriber.is_some() => {
            format!("sttme — Ready (Model: {})", guard.cfg.transcription.model)
        }
        _ => format!(
            "sttme — Loading '{}' model...",
            guard.cfg.transcription.model
        ),
    };
    let current_vk = ACTIVE_HOTKEY_VK.load(Ordering::Relaxed);
    let current_model = guard.cfg.transcription.model.clone();
    let hud_on = guard.cfg.ui.waveform_overlay;
    let beeps_on = guard.cfg.ui.audio_beeps;
    let strip_enter = guard.cfg.target.strip_alacritty_enter;
    let alacritty_only = guard.cfg.target.alacritty_only;
    drop(guard);

    let autostart = is_autostart_enabled();

    // SAFETY: Builds popup menus for `TrackPopupMenu` and destroys `root_menu` (which recursively
    // destroys attached submenus) immediately after `TrackPopupMenu` returns.
    unsafe {
        let root_menu: HMENU = CreatePopupMenu();
        let hotkey_menu: HMENU = CreatePopupMenu();
        let model_menu: HMENU = CreatePopupMenu();
        if root_menu.is_null() || hotkey_menu.is_null() || model_menu.is_null() {
            return;
        }

        append_menu_item(root_menu, MF_STRING | MF_GRAYED, 0, &status_label);
        let _ = AppendMenuW(root_menu, MF_SEPARATOR, 0, null());

        populate_hotkey_and_model_submenus(
            root_menu,
            hotkey_menu,
            model_menu,
            current_vk,
            &current_model,
        );
        let _ = AppendMenuW(root_menu, MF_SEPARATOR, 0, null());

        append_toggle_item(root_menu, CMD_TOGGLE_HUD, "Top-Center Waveform HUD", hud_on);
        append_toggle_item(root_menu, CMD_TOGGLE_BEEPS, "LCARS Audio Cues", beeps_on);
        append_toggle_item(
            root_menu,
            CMD_TOGGLE_STRIP_ENTER,
            "Strip Enter in Terminals",
            strip_enter,
        );
        append_toggle_item(
            root_menu,
            CMD_TOGGLE_ALACRITTY_ONLY,
            "Alacritty Only",
            alacritty_only,
        );
        append_toggle_item(
            root_menu,
            CMD_TOGGLE_AUTOSTART,
            "Launch at Windows Sign-In",
            autostart,
        );

        let _ = AppendMenuW(root_menu, MF_SEPARATOR, 0, null());
        append_menu_item(
            root_menu,
            MF_STRING,
            CMD_RECOVER_LAST,
            "Copy & Recover Last Dictation",
        );
        append_menu_item(
            root_menu,
            MF_STRING,
            CMD_RETRY_PENDING,
            "Retry Failed Audio (pending.wav)",
        );
        append_menu_item(root_menu, MF_STRING, CMD_OPEN_CONFIG, "Open config.toml");
        let _ = AppendMenuW(root_menu, MF_SEPARATOR, 0, null());
        append_menu_item(root_menu, MF_STRING, CMD_QUIT, "Quit sttme");

        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&raw mut pt);
        let _ = SetForegroundWindow(tray_hwnd);
        let _ = TrackPopupMenu(
            root_menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            tray_hwnd,
            null(),
        );
        let _ = DestroyMenu(root_menu);
    }
}

unsafe fn populate_hotkey_and_model_submenus(
    root_menu: HMENU,
    hotkey_menu: HMENU,
    model_menu: HMENU,
    current_vk: u16,
    current_model: &str,
) {
    // SAFETY: Caller supplies valid `HMENU` handles owned by `root_menu`.
    unsafe {
        for (vk, cmd, label) in [
            (VK_F10, CMD_HOTKEY_F10, "F10 (Default — Swallows Menu Bar)"),
            (VK_F9, CMD_HOTKEY_F9, "F9"),
            (VK_F12, CMD_HOTKEY_F12, "F12"),
            (VK_RCONTROL, CMD_HOTKEY_RCTRL, "Right Ctrl"),
            (VK_RMENU, CMD_HOTKEY_RALT, "Right Alt"),
        ] {
            let flag = if current_vk == vk {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING | MF_UNCHECKED
            };
            append_menu_item(hotkey_menu, flag, cmd, label);
        }
        let hk_title = to_wide_null("Hold-to-Talk Hotkey");
        let _ = AppendMenuW(root_menu, MF_POPUP, hotkey_menu as usize, hk_title.as_ptr());

        for (name, cmd, label) in [
            ("tiny", CMD_MODEL_TINY, "tiny (~75 MB — Fastest)"),
            ("base", CMD_MODEL_BASE, "base (~147 MB — Default)"),
            ("base.en", CMD_MODEL_BASE_EN, "base.en (~147 MB — English)"),
            ("small", CMD_MODEL_SMALL, "small (~466 MB — High Accuracy)"),
            (
                "large-v3-turbo",
                CMD_MODEL_TURBO,
                "large-v3-turbo (~1.6 GB — Best)",
            ),
        ] {
            let flag = if current_model.eq_ignore_ascii_case(name) {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING | MF_UNCHECKED
            };
            append_menu_item(model_menu, flag, cmd, label);
        }
        let mdl_title = to_wide_null("Whisper Model");
        let _ = AppendMenuW(root_menu, MF_POPUP, model_menu as usize, mdl_title.as_ptr());
    }
}

unsafe fn append_menu_item(menu: HMENU, flags: u32, id: usize, label: &str) {
    let wide = to_wide_null(label);
    // SAFETY: `menu` is valid and `wide` lives across `AppendMenuW` (which copies the string).
    unsafe {
        let _ = AppendMenuW(menu, flags, id, wide.as_ptr());
    }
}

unsafe fn append_toggle_item(menu: HMENU, id: usize, label: &str, checked: bool) {
    let flags = if checked {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING | MF_UNCHECKED
    };
    // SAFETY: Caller provides valid `menu`.
    unsafe {
        append_menu_item(menu, flags, id, label);
    }
}

fn handle_menu_command(tray_hwnd: HWND, app: &Arc<WinTrayApp>, cmd_id: usize) {
    match cmd_id {
        CMD_HOTKEY_F10 | CMD_HOTKEY_F9 | CMD_HOTKEY_F12 | CMD_HOTKEY_RCTRL | CMD_HOTKEY_RALT => {
            switch_tray_hotkey(tray_hwnd, app, cmd_id);
        }
        CMD_MODEL_TINY | CMD_MODEL_BASE | CMD_MODEL_BASE_EN | CMD_MODEL_SMALL | CMD_MODEL_TURBO => {
            switch_tray_model(tray_hwnd, app, cmd_id);
        }
        CMD_TOGGLE_HUD => {
            if let Ok(mut guard) = app.inner.lock() {
                guard.cfg.ui.waveform_overlay = !guard.cfg.ui.waveform_overlay;
                drop(save_config(&guard.cfg));
            }
        }
        CMD_TOGGLE_BEEPS => {
            if let Ok(mut guard) = app.inner.lock() {
                guard.cfg.ui.audio_beeps = !guard.cfg.ui.audio_beeps;
                drop(save_config(&guard.cfg));
            }
        }
        CMD_TOGGLE_STRIP_ENTER => {
            if let Ok(mut guard) = app.inner.lock() {
                guard.cfg.target.strip_alacritty_enter = !guard.cfg.target.strip_alacritty_enter;
                drop(save_config(&guard.cfg));
            }
        }
        CMD_TOGGLE_ALACRITTY_ONLY => {
            if let Ok(mut guard) = app.inner.lock() {
                guard.cfg.target.alacritty_only = !guard.cfg.target.alacritty_only;
                drop(save_config(&guard.cfg));
            }
        }
        CMD_TOGGLE_AUTOSTART => {
            let target = !is_autostart_enabled();
            drop(set_autostart_enabled(target));
        }
        CMD_RECOVER_LAST => recover_last_dictation(app),
        CMD_RETRY_PENDING => retry_pending_audio(app),
        CMD_OPEN_CONFIG => {
            let path = config_file_path();
            if !path.exists() {
                let cfg = load_config().unwrap_or_default();
                drop(save_config(&cfg));
            }
            drop(std::process::Command::new("notepad.exe").arg(&path).spawn());
        }
        CMD_QUIT => {
            // SAFETY: Destroying the tray host window posts `WM_QUIT` via `WM_DESTROY`.
            unsafe {
                let _ = DestroyWindow(tray_hwnd);
            }
        }
        _ => {}
    }
}

fn switch_tray_hotkey(tray_hwnd: HWND, app: &WinTrayApp, cmd_id: usize) {
    let new_key = match cmd_id {
        CMD_HOTKEY_F9 => "f9",
        CMD_HOTKEY_F12 => "f12",
        CMD_HOTKEY_RCTRL => "rctrl",
        CMD_HOTKEY_RALT => "ralt",
        _ => "f10",
    };
    ACTIVE_HOTKEY_VK.store(hotkey_name_to_vk(new_key), Ordering::Relaxed);
    IS_HOTKEY_HELD.store(false, Ordering::Relaxed);
    if let Ok(mut guard) = app.inner.lock() {
        new_key.clone_into(&mut guard.cfg.target.hotkey);
        drop(save_config(&guard.cfg));
    }
    sync_tray_visuals(tray_hwnd, app);
}

fn switch_tray_model(tray_hwnd: HWND, app: &Arc<WinTrayApp>, cmd_id: usize) {
    let model_name = match cmd_id {
        CMD_MODEL_TINY => "tiny",
        CMD_MODEL_BASE_EN => "base.en",
        CMD_MODEL_SMALL => "small",
        CMD_MODEL_TURBO => "large-v3-turbo",
        _ => "base",
    };
    let updated_cfg = if let Ok(mut guard) = app.inner.lock() {
        model_name.clone_into(&mut guard.cfg.transcription.model);
        guard.transcriber = None;
        drop(save_config(&guard.cfg));
        Some(guard.cfg.clone())
    } else {
        None
    };
    if let Some(cfg) = updated_cfg {
        let icon = app.icon_transcribe.load(Ordering::Relaxed) as HICON;
        let note = format!("Downloading / loading Whisper '{model_name}' model...");
        drop(add_or_update_tray_icon(
            tray_hwnd,
            icon,
            &note,
            false,
            Some(("sttme Switching Model", &note)),
        ));
        spawn_model_loader(app, cfg, None);
    }
}

fn recover_last_dictation(app: &Arc<WinTrayApp>) {
    let (last_opt, prev_win) = if let Ok(guard) = app.inner.lock() {
        (guard.history.last(), guard.last_external_window.clone())
    } else {
        (None, None)
    };
    if let Some(text) = last_opt {
        drop(write_win32_clipboard(&text));
        if let Some(win) = prev_win {
            let app_clone = Arc::clone(app);
            drop(std::thread::spawn(move || {
                focus_window(&win, None);
                std::thread::sleep(Duration::from_millis(60));
                deliver_windows_text(&app_clone, &text, Some(&win));
            }));
        } else {
            app.beep(BeepKind::Success);
        }
    } else {
        app.beep(BeepKind::Error);
    }
}

fn retry_pending_audio(app: &Arc<WinTrayApp>) {
    let (wav_opt, transcriber_opt, prev_win) = if let Ok(guard) = app.inner.lock() {
        (
            guard.history.load_pending_wav(),
            guard.transcriber.as_ref().map(Arc::clone),
            guard.last_external_window.clone(),
        )
    } else {
        return;
    };
    let (Some(wav_bytes), Some(transcriber)) = (wav_opt, transcriber_opt) else {
        app.beep(BeepKind::Error);
        return;
    };
    let Ok(pcm) = decode_wav_to_16k_f32(&wav_bytes) else {
        app.beep(BeepKind::Error);
        return;
    };
    let app_clone = Arc::clone(app);
    app.rt.spawn(async move {
        if let Ok(res) = transcriber.transcribe_pcm(&pcm, None).await {
            let text = res.text.trim().to_owned();
            if !text.is_empty() && !is_hallucination(&text) {
                if let Ok(mut guard) = app_clone.inner.lock() {
                    guard.history.clear_pending_wav();
                    drop(guard.history.record(&text));
                }
                let _ = tokio::task::spawn_blocking(move || {
                    deliver_windows_text(&app_clone, &text, prev_win.as_ref());
                })
                .await;
                return;
            }
        }
        app_clone.beep(BeepKind::Error);
    });
}

fn create_lcars_tray_icon(r_accent: u8, g_accent: u8, b_accent: u8) -> Result<HICON> {
    const ICON_DIM: i32 = 32;
    let mut pixels = [0_u8; (32 * 32 * 4)];

    for py in 0..32_usize {
        let rel_y = (py as f32) + 0.5 - 16.0;
        for px in 0..32_usize {
            let rel_x = (px as f32) + 0.5 - 16.0;
            let dist = rel_x.hypot(rel_y);
            let idx = (py * 32 + px) * 4;
            if dist <= 15.0 {
                let edge_cov = (15.0 - dist).clamp(0.0, 1.0);
                // Dark LCARS badge background (#121826) with bright ring and center waveform bars
                let mut red = 18.0_f32;
                let mut green = 24.0_f32;
                let mut blue = 38.0_f32;

                let ring_cov = (1.1 - (dist - 13.4).abs()).clamp(0.0, 1.0);
                red += (f32::from(r_accent) - red) * ring_cov;
                green += (f32::from(g_accent) - green) * ring_cov;
                blue += (f32::from(b_accent) - blue) * ring_cov;

                // Three crisp vertical voice-spike bars inside the badge
                for (bar_x, bar_h) in [(-5.5_f32, 4.8_f32), (0.0_f32, 8.2_f32), (5.5_f32, 5.8_f32)]
                {
                    let dx = (rel_x - bar_x).abs() - 1.5;
                    let dy = rel_y.abs() - bar_h;
                    let sdf_dist = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0);
                    let cov = (0.6 - sdf_dist).clamp(0.0, 1.0);
                    if cov > 0.0 {
                        red += (f32::from(r_accent) - red) * cov;
                        green += (f32::from(g_accent) - green) * cov;
                        blue += (f32::from(b_accent) - blue) * cov;
                    }
                }

                let alpha = (255.0 * edge_cov).round() as u8;
                let scale = f32::from(alpha) / 255.0;
                pixels[idx] = (blue * scale).round().clamp(0.0, 255.0) as u8;
                pixels[idx + 1] = (green * scale).round().clamp(0.0, 255.0) as u8;
                pixels[idx + 2] = (red * scale).round().clamp(0.0, 255.0) as u8;
                pixels[idx + 3] = alpha;
            }
        }
    }

    // SAFETY: Creates 32x32 32-bit `ARGB` color DIB + 1bpp mask bitmap for `CreateIconIndirect`,
    // releasing temporary GDI bitmaps immediately after `HICON` creation.
    unsafe {
        let screen_dc = GetDC(null_mut());
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40);
        bmi.bmiHeader.biWidth = ICON_DIM;
        bmi.bmiHeader.biHeight = -ICON_DIM;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut c_void = null_mut();
        let hbm_color = CreateDIBSection(
            screen_dc,
            &raw const bmi,
            DIB_RGB_COLORS,
            &raw mut bits,
            null_mut(),
            0,
        );
        let hbm_mask = CreateCompatibleBitmap(screen_dc, ICON_DIM, ICON_DIM);
        let _ = ReleaseDC(null_mut(), screen_dc);

        if hbm_color.is_null() || hbm_mask.is_null() || bits.is_null() {
            if !hbm_color.is_null() {
                let _ = DeleteObject(hbm_color);
            }
            if !hbm_mask.is_null() {
                let _ = DeleteObject(hbm_mask);
            }
            bail!("failed to allocate tray icon bitmaps");
        }

        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };
        let hicon = CreateIconIndirect(&raw const icon_info);
        let _ = DeleteObject(hbm_color);
        let _ = DeleteObject(hbm_mask);

        if hicon.is_null() {
            bail!("CreateIconIndirect failed");
        }
        Ok(hicon)
    }
}

fn add_or_update_tray_icon(
    hwnd: HWND,
    hicon: HICON,
    tooltip: &str,
    is_add: bool,
    balloon: Option<(&str, &str)>,
) -> Result<()> {
    // SAFETY: Zero-initializes `NOTIFYICONDATAW` and sets `cbSize` before `Shell_NotifyIconW`.
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0);
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = hicon;

        copy_wide_truncated(&mut nid.szTip, tooltip);

        if let Some((title, body)) = balloon {
            nid.uFlags |= NIF_INFO;
            copy_wide_truncated(&mut nid.szInfoTitle, title);
            copy_wide_truncated(&mut nid.szInfo, body);
        }

        let action = if is_add { NIM_ADD } else { NIM_MODIFY };
        if Shell_NotifyIconW(action, &raw const nid) == FALSE {
            bail!("Shell_NotifyIconW failed");
        }
    }
    Ok(())
}

fn remove_tray_icon(hwnd: HWND) {
    // SAFETY: Removes tray icon `uID = 1` registered to `hwnd`.
    unsafe {
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = u32::try_from(size_of::<NOTIFYICONDATAW>()).unwrap_or(0);
        nid.hWnd = hwnd;
        nid.uID = 1;
        let _ = Shell_NotifyIconW(NIM_DELETE, &raw const nid);
    }
}

fn copy_wide_truncated(dst: &mut [u16], text: &str) {
    if dst.is_empty() {
        return;
    }
    let max_chars = dst.len() - 1;
    let mut idx = 0;
    for unit in text.encode_utf16() {
        if idx >= max_chars {
            break;
        }
        dst[idx] = unit;
        idx += 1;
    }
    dst[idx] = 0;
}

fn to_wide_null(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

#[derive(Debug, Serialize)]
struct WinHealthResponse {
    status: &'static str,
    model: String,
    device: &'static str,
    vad_enabled: bool,
}

#[derive(Debug, Serialize)]
struct WinTranscribeResponse {
    text: String,
    language: String,
    model: String,
    segments: Vec<crate::transcribe::TranscriptSegment>,
    duration: f64,
}

async fn serve_http_with_transcriber(
    transcriber: Arc<Transcriber>,
    addr: SocketAddr,
) -> Result<()> {
    let app = Router::new()
        .route("/health", get(win_http_health))
        .route("/transcribe", post(win_http_transcribe))
        .layer(DefaultBodyLimit::max(MAX_HTTP_AUDIO_BYTES))
        .with_state(transcriber);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP server on {addr}"))?;
    axum::serve(listener, app)
        .await
        .context("Windows HTTP server error")
}

async fn win_http_health(State(transcriber): State<Arc<Transcriber>>) -> impl IntoResponse {
    Json(WinHealthResponse {
        status: "ok",
        model: transcriber.model_label(),
        device: "cpu",
        vad_enabled: true,
    })
}

async fn win_http_transcribe(
    State(transcriber): State<Arc<Transcriber>>,
    mut multipart: Multipart,
) -> Result<Json<WinTranscribeResponse>, (StatusCode, Json<serde_json::Value>)> {
    let mut wav_bytes: Option<Vec<u8>> = None;
    let mut language: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("file") => {
                let bytes = field.bytes().await.map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "detail": format!("invalid file upload: {e}") })),
                    )
                })?;
                wav_bytes = Some(bytes.to_vec());
            }
            Some("language") => {
                if let Ok(txt) = field.text().await
                    && !txt.trim().is_empty()
                {
                    language = Some(txt.trim().to_owned());
                }
            }
            _ => {}
        }
    }

    let Some(raw_wav) = wav_bytes.filter(|b| !b.is_empty()) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "detail": "Empty or missing audio file" })),
        ));
    };

    let pcm = decode_wav_to_16k_f32(&raw_wav).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "detail": format!("Invalid WAV file: {e}") })),
        )
    })?;

    let duration = (pcm.len() as f64) / f64::from(WHISPER_SAMPLE_RATE);
    let res = transcriber
        .transcribe_pcm(&pcm, language.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "detail": format!("Transcription failed: {e}") })),
            )
        })?;

    Ok(Json(WinTranscribeResponse {
        text: res.text,
        language: res.language,
        model: transcriber.model_label(),
        segments: res.segments,
        duration,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_names_map_to_expected_virtual_keys() {
        assert_eq!(hotkey_name_to_vk("f10"), VK_F10);
        assert_eq!(hotkey_name_to_vk("F9"), VK_F9);
        assert_eq!(hotkey_name_to_vk("f12"), VK_F12);
        assert_eq!(hotkey_name_to_vk("rctrl"), VK_RCONTROL);
        assert_eq!(hotkey_name_to_vk("ralt"), VK_RMENU);
        assert_eq!(hotkey_display_name("f10"), "F10");
        assert_eq!(hotkey_display_name("rctrl"), "Right Ctrl");
    }

    #[test]
    fn copy_wide_truncated_always_null_terminates() {
        let mut buf = [0xFFFF_u16; 6];
        copy_wide_truncated(&mut buf, "sttme-tray");
        assert_eq!(buf[5], 0);
        let decoded = String::from_utf16_lossy(&buf[..5]);
        assert_eq!(decoded, "sttme");
    }
}
