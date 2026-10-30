// Rust guideline compliant 2026-02-21
//! Low-latency Unix domain socket IPC protocol and daemon auto-start client.
//!
//! When holding `F10` in Niri (or invoking any CLI subcommand), `sttme` connects synchronously to
//! `$XDG_RUNTIME_DIR/sttme.sock`, sends a single newline-delimited JSON [`IpcRequest`] containing
//! the live session environment, and receives an [`IpcResponse`] in under half a millisecond.

use std::fmt::{self, Display, Formatter};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Maximum time `sttme` waits for an auto-spawned background daemon to bind its socket.
///
/// Loading a `base` GGML model on modern CPUs takes ~150-600ms, while first-run download of
/// `ggml-base.bin` over broadband can take up to ~60-90s. Setting 120s prevents premature client
/// timeout on cold first-run model downloads while returning immediately once ready.
const DAEMON_AUTOSTART_TIMEOUT: Duration = Duration::from_secs(120);

/// Polling interval while waiting for the newly spawned daemon socket to appear.
///
/// 25ms balances near-instant wakeup after model load against negligible CPU usage.
const DAEMON_AUTOSTART_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Current operational phase of the `sttme` state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecorderState {
    /// Ready and waiting for hotkey/command trigger.
    Idle,
    /// Actively capturing audio from the default input device.
    Recording,
    /// Running Whisper transcription and injecting text into the target window.
    Transcribing,
}

impl Display for RecorderState {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Recording => f.write_str("recording"),
            Self::Transcribing => f.write_str("transcribing"),
        }
    }
}

/// Command sent from the short-lived `sttme` CLI client to `sttme daemon`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpcAction {
    /// Record strictly while the push-to-talk key is held down, finishing upon key release.
    Hold,
    /// Start recording if idle, or stop recording and transcribe if currently recording.
    Toggle,
    /// Begin recording if currently idle.
    Start,
    /// Stop recording, transcribe, and inject text if currently recording.
    Stop,
    /// Abort recording without transcribing.
    Cancel,
    /// Re-inject the last successful transcription from `history.jsonl`.
    Recover,
    /// Re-transcribe `pending.wav` from a failed attempt.
    Retry,
    /// Query live daemon state and metrics without mutating state.
    Status,
    /// Gracefully shut down the running daemon.
    Shutdown,
}

/// Live client session environment forwarded with each IPC trigger.
///
/// Passing these values from the keybinding subprocess ensures the daemon always uses
/// the active Wayland display, Niri compositor socket, and Alacritty window even if the
/// daemon was started earlier by `systemd --user`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientContext {
    /// Value of `$WAYLAND_DISPLAY` in the caller's environment.
    pub wayland_display: Option<String>,
    /// Value of `$NIRI_SOCKET` in the caller's environment.
    pub niri_socket: Option<String>,
    /// Value of `$ALACRITTY_WINDOW_ID` when invoked from an Alacritty keybinding.
    pub alacritty_window_id: Option<String>,
    /// Optional explicit target filter (`"alacritty"` or `"all"`).
    pub target_filter: Option<String>,
    /// Optional evdev keycode (`68` = `F10`) monitored during `Hold`.
    #[serde(default)]
    pub hold_key: Option<u32>,
    /// Whether `Hold` should request an exclusive `wl_keyboard` grab instead of compositor repeat lease.
    #[serde(default)]
    pub grab_keyboard: bool,
}

impl ClientContext {
    /// Captures Wayland, Niri, and Alacritty environment variables from the current process.
    #[must_use]
    pub fn from_env(target_filter: Option<String>) -> Self {
        Self {
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            niri_socket: std::env::var("NIRI_SOCKET").ok(),
            alacritty_window_id: std::env::var("ALACRITTY_WINDOW_ID").ok(),
            target_filter,
            hold_key: None,
            grab_keyboard: false,
        }
    }

    /// Attaches the evdev keycode monitored during `Hold`.
    #[must_use]
    pub const fn with_hold_key(mut self, evdev_key: u32) -> Self {
        self.hold_key = Some(evdev_key);
        self
    }

    /// Configures whether `Hold` uses an exclusive Wayland keyboard grab.
    #[must_use]
    pub const fn with_grab_keyboard(mut self, grab: bool) -> Self {
        self.grab_keyboard = grab;
        self
    }
}

/// Wire request sent over `$XDG_RUNTIME_DIR/sttme.sock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRequest {
    /// Requested state transition or query.
    pub action: IpcAction,
    /// Caller's live desktop environment metadata.
    pub context: ClientContext,
}

/// Cumulative daemon metrics matching `whisper_server.py`'s `/stats` endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStats {
    /// Total completed transcription requests.
    pub request_count: u64,
    /// Total failed transcription requests.
    pub error_count: u64,
    /// Average transcription latency in milliseconds.
    pub avg_transcription_time_ms: f64,
    /// Cumulative seconds of speech audio processed.
    pub total_audio_processed_seconds: f64,
    /// Daemon uptime in seconds.
    pub uptime_seconds: f64,
}

/// Wire response returned by `sttme daemon` over `$XDG_RUNTIME_DIR/sttme.sock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Whether the request succeeded.
    pub ok: bool,
    /// Resulting [`RecorderState`] after processing the request.
    pub state: RecorderState,
    /// Human-readable summary message.
    pub message: String,
    /// Loaded Whisper model or API target name.
    pub model: String,
    /// Optional cumulative server statistics (populated for `status` requests).
    pub stats: Option<DaemonStats>,
}

/// Sends an [`IpcRequest`] to `socket_path`, optionally spawning `sttme daemon` on cold start.
///
/// Uses synchronous Unix domain socket I/O so hotkey and key-repeat triggers complete in `< 0.5ms`
/// without initializing an async thread-pool runtime.
///
/// # Errors
///
/// Returns an error if the daemon cannot be reached or started, or if socket I/O fails.
pub fn send_ipc_request(
    socket_path: &Path,
    request: &IpcRequest,
    autostart_daemon: bool,
) -> Result<IpcResponse> {
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(first_err) => {
            if !autostart_daemon {
                return Err(first_err).with_context(|| {
                    format!("daemon is not running at {}", socket_path.display())
                });
            }
            spawn_detached_daemon()?;
            wait_for_daemon_socket(socket_path)?
        }
    };

    let mut payload = serde_json::to_string(request).context("failed to serialize IPC request")?;
    payload.push('\n');
    stream
        .write_all(payload.as_bytes())
        .context("failed to write IPC request")?;
    stream
        .shutdown(Shutdown::Write)
        .context("failed to flush IPC socket")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .context("failed to read IPC response")?;
    if bytes == 0 {
        bail!("daemon closed IPC connection without replying");
    }

    let resp: IpcResponse =
        serde_json::from_str(line.trim()).context("invalid JSON response from daemon")?;
    Ok(resp)
}

fn spawn_detached_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("failed to determine current sttme executable")?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to auto-spawn background `sttme daemon`")?;
    Ok(())
}

fn wait_for_daemon_socket(socket_path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + DAEMON_AUTOSTART_TIMEOUT;
    loop {
        if let Ok(stream) = UnixStream::connect(socket_path) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for auto-started `sttme daemon` at {}",
                socket_path.display()
            );
        }
        std::thread::sleep(DAEMON_AUTOSTART_POLL_INTERVAL);
    }
}
