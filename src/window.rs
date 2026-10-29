// Rust guideline compliant 2026-02-21
//! Active window detection and focus restoration for Niri Wayland and X11.
//!
//! Communicates directly with `$NIRI_SOCKET` over a Unix domain socket in sub-millisecond
//! time on Niri Wayland, falling back to `xdotool` / `xprop` when running under X11.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

/// Timeout applied to Niri IPC Unix socket queries so window checks never stall audio capture.
///
/// Niri normally responds in `< 0.5ms`; `250ms` guards against compositor hangs.
const NIRI_IPC_TIMEOUT: Duration = Duration::from_millis(250);

/// Known Linux terminal emulator identifiers matching `talktype.py`.
const LINUX_TERMINALS: &[&str] = &[
    "alacritty",
    "gnome-terminal",
    "xterm",
    "konsole",
    "kitty",
    "terminator",
    "tilix",
    "xfce4-terminal",
    "urxvt",
    "st",
    "sakura",
    "guake",
    "tilda",
    "hyper",
    "wezterm",
    "foot",
    "ghostty",
];

/// Display server backend that produced a [`WindowInfo`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowBackend {
    /// Niri Wayland compositor native JSON IPC (`$NIRI_SOCKET`).
    Niri,
    /// X11 `xdotool` + `xprop` fallback.
    X11,
}

/// Snapshot of a desktop window captured when recording begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// Compositor or X11 window identifier.
    pub id: String,
    /// Wayland `app_id` or X11 `WM_CLASS` string.
    pub app_id: String,
    /// Window title at capture time.
    pub title: String,
    /// Owning process ID when reported by the compositor.
    pub pid: Option<u32>,
    /// Backend used to query and refocus this window.
    pub backend: WindowBackend,
}

impl WindowInfo {
    /// Returns `true` when this window belongs to Alacritty.
    #[must_use]
    pub fn is_alacritty(&self) -> bool {
        self.app_id.eq_ignore_ascii_case("alacritty")
            || self.app_id.to_ascii_lowercase().contains("alacritty")
    }

    /// Returns `true` when this window is a terminal emulator (uses `Ctrl+Shift+V` paste).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        let lower = self.app_id.to_ascii_lowercase();
        LINUX_TERMINALS.iter().any(|term| lower.contains(term))
    }
}

#[derive(Debug, Deserialize)]
struct NiriReply {
    #[serde(rename = "Ok")]
    ok: Option<NiriOkPayload>,
}

#[derive(Debug, Deserialize)]
struct NiriOkPayload {
    #[serde(rename = "FocusedWindow")]
    focused_window: Option<NiriWindow>,
}

#[derive(Debug, Deserialize)]
struct NiriWindow {
    id: u64,
    title: Option<String>,
    app_id: Option<String>,
    pid: Option<u32>,
}

/// Queries the currently focused window using `$NIRI_SOCKET` (or `niri_socket_override`) or X11.
#[must_use]
pub fn get_focused_window(niri_socket_override: Option<&str>) -> Option<WindowInfo> {
    if let Some(sock_path) = resolve_niri_socket(niri_socket_override)
        && let Some(win) = query_niri_focused_window(&sock_path)
    {
        return Some(win);
    }
    query_x11_focused_window()
}

/// Restores focus to a previously captured [`WindowInfo`].
pub fn focus_window(window: &WindowInfo, niri_socket_override: Option<&str>) {
    match window.backend {
        WindowBackend::Niri => {
            if let Ok(id) = window.id.parse::<u64>()
                && let Some(sock_path) = resolve_niri_socket(niri_socket_override)
            {
                focus_niri_window(&sock_path, id);
            }
        }
        WindowBackend::X11 => {
            drop(
                Command::new("xdotool")
                    .args(["windowactivate", "--sync", &window.id])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status(),
            );
        }
    }
}

fn resolve_niri_socket(override_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(env_sock) = std::env::var("NIRI_SOCKET") {
        let path = PathBuf::from(env_sock);
        if path.exists() {
            return Some(path);
        }
    }
    // Discover active niri socket in $XDG_RUNTIME_DIR when daemon started prior to Niri.
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok()?;
    let entries = std::fs::read_dir(runtime_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("niri.")
            && Path::new(name_str.as_ref())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sock"))
        {
            return Some(entry.path());
        }
    }
    None
}

fn query_niri_focused_window(sock_path: &Path) -> Option<WindowInfo> {
    let mut stream = UnixStream::connect(sock_path).ok()?;
    drop(stream.set_read_timeout(Some(NIRI_IPC_TIMEOUT)));
    drop(stream.set_write_timeout(Some(NIRI_IPC_TIMEOUT)));
    stream.write_all(b"\"FocusedWindow\"\n").ok()?;
    stream.flush().ok()?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let parsed: NiriReply = serde_json::from_str(line.trim()).ok()?;
    let win = parsed.ok?.focused_window?;

    Some(WindowInfo {
        id: win.id.to_string(),
        app_id: win.app_id.unwrap_or_default(),
        title: win.title.unwrap_or_default(),
        pid: win.pid,
        backend: WindowBackend::Niri,
    })
}

fn focus_niri_window(sock_path: &Path, id: u64) {
    let Ok(mut stream) = UnixStream::connect(sock_path) else {
        return;
    };
    drop(stream.set_read_timeout(Some(NIRI_IPC_TIMEOUT)));
    drop(stream.set_write_timeout(Some(NIRI_IPC_TIMEOUT)));
    let req = format!("{{\"Action\":{{\"FocusWindow\":{{\"id\":{id}}}}}}}\n");
    if stream.write_all(req.as_bytes()).is_ok() {
        drop(stream.flush());
        let mut reply = String::new();
        drop(BufReader::new(stream).read_line(&mut reply));
    }
}

fn query_x11_focused_window() -> Option<WindowInfo> {
    let out = Command::new("xdotool")
        .arg("getactivewindow")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if id.is_empty() {
        return None;
    }

    let wm_class = Command::new("xprop")
        .args(["-id", &id, "WM_CLASS"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    Some(WindowInfo {
        id,
        app_id: wm_class,
        title: String::new(),
        pid: None,
        backend: WindowBackend::X11,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_alacritty_and_terminal_classes() {
        let win = WindowInfo {
            id: "72".to_owned(),
            app_id: "Alacritty".to_owned(),
            title: "sttme".to_owned(),
            pid: Some(1234),
            backend: WindowBackend::Niri,
        };
        assert!(win.is_alacritty());
        assert!(win.is_terminal());

        let browser = WindowInfo {
            id: "10".to_owned(),
            app_id: "firefox".to_owned(),
            title: "Mozilla Firefox".to_owned(),
            pid: None,
            backend: WindowBackend::Niri,
        };
        assert!(!browser.is_alacritty());
        assert!(!browser.is_terminal());
    }
}
