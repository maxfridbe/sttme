// Rust guideline compliant 2026-02-21
//! Persistent low-latency speech-to-text daemon, state machine, and HTTP server endpoints.
//!
//! Coordinates microphone capture, `whisper.cpp` inference, Niri/X11 window focus restoration,
//! Alacritty Enter-stripping text injection, `history.jsonl` / `pending.wav` recovery, and
//! optional `TalkType`-compatible HTTP endpoints (`/health`, `/stats`, `/transcribe`).

use std::fs::{self, File};
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use rustix::fs::{FlockOperation, flock};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::Notify;
use tracing::{Level, event};

use crate::audio::{
    ActiveCapture, BeepKind, MIN_SPEECH_SAMPLES, WHISPER_SAMPLE_RATE, decode_wav_to_16k_f32,
    encode_wav_16k, has_speech, play_beep,
};
use crate::config::{AppConfig, cache_dir, lock_file_path, socket_path};
use crate::history::HistoryStore;
use crate::inject::{inject_text, prepare_output_text};
use crate::ipc::{ClientContext, DaemonStats, IpcAction, IpcRequest, IpcResponse, RecorderState};
use crate::ptt::{EVDEV_KEY_F12, HoldOutcome, OverlayOptions, run_recording_overlay};
use crate::transcribe::{Transcriber, is_hallucination};
use crate::window::{WindowInfo, focus_window, get_focused_window};

/// Minimum interval (`300ms`) between `Toggle` triggers to reject mechanical key bounce.
///
/// Matches `talktype.py`'s `HOTKEY_DEBOUNCE_MS = 300`.
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(300);

/// Anti-chatter cooldown (`80ms`) after push-to-talk key release before accepting a new `Hold`.
///
/// Rejects mechanical switch bounce on key-up without delaying intentional back-to-back dictation.
const RELEASE_COOLDOWN: Duration = Duration::from_millis(80);

/// Minimum interval (`500ms`) between consecutive text injections to prevent double-typing.
///
/// Matches `talktype.py`'s `PASTE_DEBOUNCE_MS = 500`.
const INJECT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Maximum multipart upload size (`50 MiB`) accepted by the optional `/transcribe` HTTP endpoint.
///
/// Matches `whisper_server.py`'s `50 * 1024 * 1024` byte ceiling.
const MAX_HTTP_AUDIO_BYTES: usize = 50 * 1024 * 1024;

/// Brief settle delay (`30ms`) after compositor `FocusWindow` before emitting virtual keystrokes.
///
/// Gives Niri and Alacritty one frame to route Wayland `wl_keyboard.enter` to the target surface.
const WINDOW_FOCUS_SETTLE: Duration = Duration::from_millis(30);

struct ActiveSession {
    capture: ActiveCapture,
    target_window: Option<WindowInfo>,
    client_ctx: ClientContext,
    alacritty_restricted: bool,
    abort_hold: Arc<AtomicBool>,
}

struct MutableDaemonState {
    phase: RecorderState,
    active_session: Option<ActiveSession>,
    last_toggle_at: Option<Instant>,
    last_release_at: Option<Instant>,
    last_inject_at: Option<Instant>,
    history: HistoryStore,
    request_count: u64,
    error_count: u64,
    total_transcribe_secs: f64,
    total_audio_secs: f64,
}

struct SharedDaemon {
    cfg: AppConfig,
    transcriber: Transcriber,
    started_at: Instant,
    shutdown: Notify,
    inner: Mutex<MutableDaemonState>,
}

impl SharedDaemon {
    fn snapshot_stats(&self, guard: &MutableDaemonState) -> DaemonStats {
        let avg_ms = if guard.request_count == 0 {
            0.0
        } else {
            (guard.total_transcribe_secs / (guard.request_count as f64)) * 1000.0
        };
        DaemonStats {
            request_count: guard.request_count,
            error_count: guard.error_count,
            avg_transcription_time_ms: (avg_ms * 10.0).round() / 10.0,
            total_audio_processed_seconds: (guard.total_audio_secs * 10.0).round() / 10.0,
            uptime_seconds: (self.started_at.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        }
    }

    fn beep(&self, kind: BeepKind) {
        if self.cfg.ui.audio_beeps {
            play_beep(kind);
        }
    }
}

/// Acquires an exclusive non-blocking lock on `~/.cache/sttme/sttme.lock`.
///
/// # Errors
///
/// Returns an error if another `sttme daemon` instance already holds the lock.
pub fn acquire_daemon_lock() -> Result<File> {
    let lock_path = lock_file_path();
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = File::create(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    if flock(&file, FlockOperation::NonBlockingLockExclusive).is_err() {
        bail!(
            "sttme daemon is already running (lock held at {})",
            lock_path.display()
        );
    }
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(file)
}

/// Starts the persistent `sttme` daemon listening on `$XDG_RUNTIME_DIR/sttme.sock` and optional HTTP.
///
/// # Errors
///
/// Returns an error if the single-instance lock is busy, model initialization fails, or socket binding fails.
pub async fn run_daemon(cfg: AppConfig, http_addr: Option<SocketAddr>) -> Result<()> {
    let _instance_lock = acquire_daemon_lock()?;
    let sock_path = socket_path();
    if sock_path.exists() {
        drop(fs::remove_file(&sock_path));
    }
    if let Some(parent) = sock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let history = HistoryStore::new(&cache_dir(), cfg.history.limit)?;
    let transcriber = Transcriber::new(&cfg).await?;

    let unix_listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind IPC socket {}", sock_path.display()))?;
    let _ = fs::set_permissions(&sock_path, fs::Permissions::from_mode(0o600));

    let shared = Arc::new(SharedDaemon {
        cfg,
        transcriber,
        started_at: Instant::now(),
        shutdown: Notify::new(),
        inner: Mutex::new(MutableDaemonState {
            phase: RecorderState::Idle,
            active_session: None,
            last_toggle_at: None,
            last_release_at: None,
            last_inject_at: None,
            history,
            request_count: 0,
            error_count: 0,
            total_transcribe_secs: 0.0,
            total_audio_secs: 0.0,
        }),
    });

    if let Some(addr) = http_addr {
        let http_shared = Arc::clone(&shared);
        tokio::spawn(async move {
            if let Err(err) = serve_http(http_shared, addr).await {
                event!(
                    name: "daemon.http.failed",
                    Level::ERROR,
                    error.message = %err,
                    "HTTP server failed: {{error.message}}",
                );
            }
        });
    }

    event!(
        name: "daemon.ready",
        Level::INFO,
        socket.path = %sock_path.display(),
        whisper.model = %shared.transcriber.model_label(),
        "sttme daemon listening at {{socket.path}} with model '{{whisper.model}}'",
    );

    loop {
        tokio::select! {
            accept_res = unix_listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let daemon = Arc::clone(&shared);
                        tokio::spawn(async move {
                            handle_ipc_client(daemon, stream).await;
                        });
                    }
                    Err(err) => {
                        event!(
                            name: "daemon.ipc.accept_error",
                            Level::WARN,
                            error.message = %err,
                            "IPC accept error: {{error.message}}",
                        );
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                event!(name: "daemon.shutdown.signal", Level::INFO, "received Ctrl+C, shutting down");
                break;
            }
            () = shared.shutdown.notified() => {
                event!(name: "daemon.shutdown.ipc", Level::INFO, "received IPC shutdown command");
                break;
            }
        }
    }

    cleanup_socket(&sock_path);
    Ok(())
}

fn cleanup_socket(sock_path: &Path) {
    drop(fs::remove_file(sock_path));
}

async fn handle_ipc_client(daemon: Arc<SharedDaemon>, stream: UnixStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let response = match serde_json::from_str::<IpcRequest>(line.trim()) {
        Ok(req) => dispatch_ipc_action(&daemon, req),
        Err(err) => IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: format!("invalid IPC request: {err}"),
            model: daemon.transcriber.model_label(),
            stats: None,
        },
    };

    if let Ok(mut encoded) = serde_json::to_string(&response) {
        encoded.push('\n');
        drop(write_half.write_all(encoded.as_bytes()).await);
        drop(write_half.shutdown().await);
    }
}

fn dispatch_ipc_action(daemon: &Arc<SharedDaemon>, req: IpcRequest) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    let Ok(mut guard) = daemon.inner.lock() else {
        return IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: "daemon state mutex poisoned".to_owned(),
            model,
            stats: None,
        };
    };

    match req.action {
        IpcAction::Status => {
            let stats = daemon.snapshot_stats(&guard);
            IpcResponse {
                ok: true,
                state: guard.phase,
                message: format!("daemon is {}", guard.phase),
                model,
                stats: Some(stats),
            }
        }
        IpcAction::Shutdown => {
            daemon.shutdown.notify_one();
            IpcResponse {
                ok: true,
                state: guard.phase,
                message: "shutting down daemon".to_owned(),
                model,
                stats: None,
            }
        }
        IpcAction::Cancel => {
            if let Some(session) = guard.active_session.take() {
                session.abort_hold.store(true, Ordering::Relaxed);
                drop(session.capture.stop());
            }
            guard.phase = RecorderState::Idle;
            guard.last_release_at = Some(Instant::now());
            daemon.beep(BeepKind::Success);
            IpcResponse {
                ok: true,
                state: RecorderState::Idle,
                message: "recording cancelled".to_owned(),
                model,
                stats: None,
            }
        }
        IpcAction::Hold => handle_hold_locked(daemon, &mut guard, req.context),
        IpcAction::Toggle => {
            let now = Instant::now();
            if let Some(prev) = guard.last_toggle_at
                && now.duration_since(prev) < HOTKEY_DEBOUNCE
            {
                return IpcResponse {
                    ok: true,
                    state: guard.phase,
                    message: "debounced rapid hotkey press".to_owned(),
                    model,
                    stats: None,
                };
            }
            guard.last_toggle_at = Some(now);

            match guard.phase {
                RecorderState::Idle => {
                    start_recording_locked(daemon, &mut guard, req.context, false)
                }
                RecorderState::Recording => stop_and_spawn_transcribe_locked(daemon, &mut guard),
                RecorderState::Transcribing => IpcResponse {
                    ok: false,
                    state: RecorderState::Transcribing,
                    message: "currently transcribing previous recording".to_owned(),
                    model,
                    stats: None,
                },
            }
        }
        IpcAction::Start => match guard.phase {
            RecorderState::Idle => start_recording_locked(daemon, &mut guard, req.context, false),
            other => IpcResponse {
                ok: false,
                state: other,
                message: format!("cannot start while {other}"),
                model,
                stats: None,
            },
        },
        IpcAction::Stop => match guard.phase {
            RecorderState::Recording => stop_and_spawn_transcribe_locked(daemon, &mut guard),
            other => IpcResponse {
                ok: false,
                state: other,
                message: format!("not currently recording (state: {other})"),
                model,
                stats: None,
            },
        },
        IpcAction::Recover => handle_recover_locked(daemon, &guard, req.context),
        IpcAction::Retry => handle_retry_locked(daemon, &mut guard, req.context),
    }
}

fn handle_recover_locked(
    daemon: &Arc<SharedDaemon>,
    guard: &MutableDaemonState,
    ctx: ClientContext,
) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    if guard.phase != RecorderState::Idle {
        return IpcResponse {
            ok: false,
            state: guard.phase,
            message: "can only recover while idle".to_owned(),
            model,
            stats: None,
        };
    }
    let Some(last_text) = guard.history.last() else {
        daemon.beep(BeepKind::Error);
        return IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: "no transcription history available".to_owned(),
            model,
            stats: None,
        };
    };
    let daemon_clone = Arc::clone(daemon);
    let text_preview: String = last_text.chars().take(48).collect();
    tokio::task::spawn_blocking(move || {
        let target = get_focused_window(ctx.niri_socket.as_deref());
        let alacritty_only = resolve_alacritty_restriction(&daemon_clone.cfg, &ctx);
        let _ = deliver_text_to_window(
            &daemon_clone,
            &last_text,
            target.as_ref(),
            &ctx,
            alacritty_only,
        );
    });
    IpcResponse {
        ok: true,
        state: RecorderState::Idle,
        message: format!("recovered: {text_preview}"),
        model,
        stats: None,
    }
}

fn handle_retry_locked(
    daemon: &Arc<SharedDaemon>,
    guard: &mut MutableDaemonState,
    ctx: ClientContext,
) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    if guard.phase != RecorderState::Idle {
        return IpcResponse {
            ok: false,
            state: guard.phase,
            message: "can only retry while idle".to_owned(),
            model,
            stats: None,
        };
    }
    let Some(pending_wav) = guard.history.load_pending_wav() else {
        daemon.beep(BeepKind::Error);
        return IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: "no pending failed audio to retry".to_owned(),
            model,
            stats: None,
        };
    };
    guard.phase = RecorderState::Transcribing;
    let daemon_clone = Arc::clone(daemon);
    tokio::spawn(async move {
        process_retry_audio(daemon_clone, pending_wav, ctx).await;
    });
    IpcResponse {
        ok: true,
        state: RecorderState::Transcribing,
        message: "retrying pending audio transcription".to_owned(),
        model,
        stats: None,
    }
}

fn resolve_alacritty_restriction(cfg: &AppConfig, ctx: &ClientContext) -> bool {
    match ctx.target_filter.as_deref() {
        Some("all") => false,
        Some("alacritty") => true,
        _ => cfg.target.alacritty_only,
    }
}

fn handle_hold_locked(
    daemon: &Arc<SharedDaemon>,
    guard: &mut MutableDaemonState,
    ctx: ClientContext,
) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    match guard.phase {
        RecorderState::Recording => IpcResponse {
            ok: true,
            state: RecorderState::Recording,
            message: "already listening while key is held".to_owned(),
            model,
            stats: None,
        },
        RecorderState::Transcribing => IpcResponse {
            ok: false,
            state: RecorderState::Transcribing,
            message: "currently transcribing previous recording".to_owned(),
            model,
            stats: None,
        },
        RecorderState::Idle => {
            if let Some(released_at) = guard.last_release_at
                && Instant::now().duration_since(released_at) < RELEASE_COOLDOWN
            {
                return IpcResponse {
                    ok: true,
                    state: RecorderState::Idle,
                    message: "ignored post-release switch bounce".to_owned(),
                    model,
                    stats: None,
                };
            }
            start_recording_locked(daemon, guard, ctx, true)
        }
    }
}

fn start_recording_locked(
    daemon: &Arc<SharedDaemon>,
    guard: &mut MutableDaemonState,
    ctx: ClientContext,
    hold_until_release: bool,
) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    let alacritty_restricted = resolve_alacritty_restriction(&daemon.cfg, &ctx);
    let focused = get_focused_window(ctx.niri_socket.as_deref());

    if alacritty_restricted
        && ctx.alacritty_window_id.is_none()
        && let Some(win) = &focused
        && !win.is_alacritty()
    {
        daemon.beep(BeepKind::Error);
        return IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: format!(
                "alacritty-only mode active (focused window is '{}')",
                win.app_id
            ),
            model,
            stats: None,
        };
    }

    match ActiveCapture::start() {
        Ok(capture) => {
            let abort_hold = Arc::new(AtomicBool::new(false));
            let meter = capture.meter();
            let hold_key = hold_until_release.then(|| ctx.hold_key.unwrap_or(EVDEV_KEY_F12));
            let show_waveform = daemon.cfg.ui.waveform_overlay;
            let wayland_display = ctx.wayland_display.clone();

            guard.phase = RecorderState::Recording;
            guard.active_session = Some(ActiveSession {
                capture,
                target_window: focused,
                client_ctx: ctx,
                alacritty_restricted,
                abort_hold: Arc::clone(&abort_hold),
            });
            daemon.beep(BeepKind::Start);
            event!(
                name: "daemon.recording.started",
                Level::INFO,
                alacritty.only = alacritty_restricted,
                ptt.hold = hold_until_release,
                "microphone recording started (alacritty_only={{alacritty.only}}, hold={{ptt.hold}})",
            );

            if hold_until_release || show_waveform {
                spawn_recording_overlay_worker(
                    Arc::clone(daemon),
                    OverlayOptions {
                        hold_key,
                        show_waveform,
                        wayland_display,
                        meter,
                    },
                    abort_hold,
                );
            }

            IpcResponse {
                ok: true,
                state: RecorderState::Recording,
                message: "listening while key is held".to_owned(),
                model,
                stats: None,
            }
        }
        Err(err) => {
            daemon.beep(BeepKind::Error);
            event!(
                name: "daemon.recording.failed",
                Level::ERROR,
                error.message = %err,
                "failed to start microphone capture: {{error.message}}",
            );
            IpcResponse {
                ok: false,
                state: RecorderState::Idle,
                message: format!("microphone error: {err}"),
                model,
                stats: None,
            }
        }
    }
}

fn spawn_recording_overlay_worker(
    daemon: Arc<SharedDaemon>,
    opts: OverlayOptions,
    abort_hold: Arc<AtomicBool>,
) {
    let rt_handle = tokio::runtime::Handle::current();
    let is_hold_mode = opts.hold_key.is_some();
    drop(
        std::thread::Builder::new()
            .name("sttme-hud".to_owned())
            .spawn(move || match run_recording_overlay(&opts, &abort_hold) {
                Ok(HoldOutcome::Released) if is_hold_mode => {
                    let session_opt = if let Ok(mut guard) = daemon.inner.lock() {
                        if guard.phase == RecorderState::Recording {
                            guard.phase = RecorderState::Transcribing;
                            guard.last_release_at = Some(Instant::now());
                            guard.active_session.take()
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some(session) = session_opt {
                        event!(
                            name: "daemon.recording.released",
                            Level::INFO,
                            "push-to-talk key released; finishing listening",
                        );
                        rt_handle.spawn(async move {
                            process_recorded_session(daemon, session).await;
                        });
                    }
                }
                Ok(HoldOutcome::Released) => {}
                Ok(HoldOutcome::Cancelled) => {
                    if let Ok(mut guard) = daemon.inner.lock() {
                        if let Some(session) = guard.active_session.take() {
                            drop(session.capture.stop());
                        }
                        guard.phase = RecorderState::Idle;
                        guard.last_release_at = Some(Instant::now());
                    }
                    daemon.beep(BeepKind::Success);
                }
                Err(err) => {
                    event!(
                        name: "daemon.ptt.watcher_failed",
                        Level::WARN,
                        error.message = %err,
                        "Wayland waveform/PTT overlay unavailable ({{error.message}}); use `sttme stop`",
                    );
                }
            }),
    );
}

fn stop_and_spawn_transcribe_locked(
    daemon: &Arc<SharedDaemon>,
    guard: &mut MutableDaemonState,
) -> IpcResponse {
    let model = daemon.transcriber.model_label();
    let Some(session) = guard.active_session.take() else {
        guard.phase = RecorderState::Idle;
        return IpcResponse {
            ok: false,
            state: RecorderState::Idle,
            message: "no active recording session".to_owned(),
            model,
            stats: None,
        };
    };

    session.abort_hold.store(true, Ordering::Relaxed);
    guard.phase = RecorderState::Transcribing;
    guard.last_release_at = Some(Instant::now());

    let daemon_clone = Arc::clone(daemon);
    tokio::spawn(async move {
        process_recorded_session(daemon_clone, session).await;
    });

    IpcResponse {
        ok: true,
        state: RecorderState::Transcribing,
        message: "stopped recording; transcribing speech".to_owned(),
        model,
        stats: None,
    }
}

async fn process_recorded_session(daemon: Arc<SharedDaemon>, session: ActiveSession) {
    let ActiveSession {
        capture,
        target_window,
        client_ctx,
        alacritty_restricted,
        ..
    } = session;

    let Some(samples) = collect_valid_speech(&daemon, capture).await else {
        return;
    };

    if let Ok(wav_bytes) = encode_wav_16k(&samples)
        && let Ok(guard) = daemon.inner.lock()
    {
        drop(guard.history.save_pending_wav(&wav_bytes));
    }

    let audio_secs = (samples.len() as f64) / f64::from(WHISPER_SAMPLE_RATE);
    let started = Instant::now();
    match daemon.transcriber.transcribe_pcm(&samples, None).await {
        Ok(result) => {
            let elapsed = started.elapsed().as_secs_f64();
            let raw_text = result.text.trim().to_owned();

            if raw_text.is_empty() || is_hallucination(&raw_text) {
                event!(
                    name: "daemon.transcribe.filtered",
                    Level::INFO,
                    "filtered silent or hallucinated transcription",
                );
                if let Ok(mut guard) = daemon.inner.lock() {
                    guard.history.clear_pending_wav();
                    guard.phase = RecorderState::Idle;
                }
                daemon.beep(BeepKind::Success);
                return;
            }

            if let Ok(mut guard) = daemon.inner.lock() {
                guard.request_count += 1;
                guard.total_transcribe_secs += elapsed;
                guard.total_audio_secs += audio_secs;
                guard.history.clear_pending_wav();
                drop(guard.history.record(&raw_text));
            }

            event!(
                name: "daemon.transcribe.completed",
                Level::INFO,
                duration.ms = (elapsed * 1000.0).round() as u64,
                audio.seconds = (audio_secs * 10.0).round() / 10.0,
                text.chars = raw_text.chars().count(),
                "transcribed {{text.chars}} chars in {{duration.ms}}ms ({{audio.seconds}}s audio)",
            );

            let daemon_inject = Arc::clone(&daemon);
            let _ = tokio::task::spawn_blocking(move || {
                deliver_text_to_window(
                    &daemon_inject,
                    &raw_text,
                    target_window.as_ref(),
                    &client_ctx,
                    alacritty_restricted,
                )
            })
            .await;

            if let Ok(mut guard) = daemon.inner.lock() {
                guard.phase = RecorderState::Idle;
            }
        }
        Err(err) => {
            event!(
                name: "daemon.transcribe.failed",
                Level::ERROR,
                error.message = %err,
                "transcription failed (pending.wav kept for retry): {{error.message}}",
            );
            if let Ok(mut guard) = daemon.inner.lock() {
                guard.error_count += 1;
                guard.phase = RecorderState::Idle;
            }
            daemon.beep(BeepKind::Error);
        }
    }
}

async fn collect_valid_speech(daemon: &SharedDaemon, capture: ActiveCapture) -> Option<Vec<f32>> {
    let samples = match tokio::task::spawn_blocking(move || capture.stop()).await {
        Ok(Ok(pcm)) => pcm,
        Ok(Err(err)) => {
            event!(
                name: "daemon.capture.stop_failed",
                Level::ERROR,
                error.message = %err,
                "audio capture stop failed: {{error.message}}",
            );
            finish_with_error(daemon);
            return None;
        }
        Err(_) => {
            finish_with_error(daemon);
            return None;
        }
    };

    if samples.len() < MIN_SPEECH_SAMPLES
        || !has_speech(&samples, daemon.cfg.transcription.energy_threshold)
    {
        event!(
            name: "daemon.speech.empty",
            Level::INFO,
            audio.samples = samples.len(),
            "no speech detected ({{audio.samples}} samples)",
        );
        if let Ok(mut guard) = daemon.inner.lock() {
            guard.history.clear_pending_wav();
            guard.phase = RecorderState::Idle;
        }
        daemon.beep(BeepKind::Success);
        return None;
    }

    Some(samples)
}

async fn process_retry_audio(
    daemon: Arc<SharedDaemon>,
    pending_wav: Vec<u8>,
    client_ctx: ClientContext,
) {
    let target_window = get_focused_window(client_ctx.niri_socket.as_deref());
    let alacritty_restricted = resolve_alacritty_restriction(&daemon.cfg, &client_ctx);

    let Ok(samples) = decode_wav_to_16k_f32(&pending_wav) else {
        finish_with_error(&daemon);
        return;
    };

    match daemon.transcriber.transcribe_pcm(&samples, None).await {
        Ok(result) if !result.text.is_empty() && !is_hallucination(&result.text) => {
            if let Ok(mut guard) = daemon.inner.lock() {
                guard.history.clear_pending_wav();
                drop(guard.history.record(&result.text));
            }
            let daemon_inject = Arc::clone(&daemon);
            let text = result.text;
            let _ = tokio::task::spawn_blocking(move || {
                deliver_text_to_window(
                    &daemon_inject,
                    &text,
                    target_window.as_ref(),
                    &client_ctx,
                    alacritty_restricted,
                )
            })
            .await;
            if let Ok(mut guard) = daemon.inner.lock() {
                guard.phase = RecorderState::Idle;
            }
        }
        Ok(_) => {
            if let Ok(mut guard) = daemon.inner.lock() {
                guard.history.clear_pending_wav();
                guard.phase = RecorderState::Idle;
            }
            daemon.beep(BeepKind::Error);
        }
        Err(err) => {
            event!(
                name: "daemon.retry.failed",
                Level::ERROR,
                error.message = %err,
                "retry transcription failed: {{error.message}}",
            );
            finish_with_error(&daemon);
        }
    }
}

fn finish_with_error(daemon: &SharedDaemon) {
    if let Ok(mut guard) = daemon.inner.lock() {
        guard.phase = RecorderState::Idle;
    }
    daemon.beep(BeepKind::Error);
}

fn deliver_text_to_window(
    daemon: &SharedDaemon,
    raw_text: &str,
    captured_target: Option<&WindowInfo>,
    client_ctx: &ClientContext,
    alacritty_restricted: bool,
) -> Result<()> {
    {
        if let Ok(mut guard) = daemon.inner.lock() {
            let now = Instant::now();
            if let Some(prev) = guard.last_inject_at
                && now.duration_since(prev) < INJECT_DEBOUNCE
            {
                return Ok(());
            }
            guard.last_inject_at = Some(now);
        }
    }

    if let Some(win) = captured_target {
        focus_window(win, client_ctx.niri_socket.as_deref());
        std::thread::sleep(WINDOW_FOCUS_SETTLE);
    }

    let current_focus = get_focused_window(client_ctx.niri_socket.as_deref());
    let is_alacritty = current_focus
        .as_ref()
        .map(WindowInfo::is_alacritty)
        .or_else(|| captured_target.map(WindowInfo::is_alacritty))
        .unwrap_or(alacritty_restricted || client_ctx.alacritty_window_id.is_some());

    if alacritty_restricted
        && let Some(active) = &current_focus
        && !active.is_alacritty()
    {
        daemon.beep(BeepKind::Error);
        bail!(
            "refusing to inject speech into non-Alacritty window '{}'",
            active.app_id
        );
    }

    let is_terminal = is_alacritty || current_focus.as_ref().is_some_and(WindowInfo::is_terminal);
    let strip_enter = is_alacritty && daemon.cfg.target.strip_alacritty_enter;

    let formatted = prepare_output_text(raw_text, strip_enter, daemon.cfg.target.trailing_space);
    if formatted.is_empty() {
        daemon.beep(BeepKind::Success);
        return Ok(());
    }

    match inject_text(
        &formatted,
        daemon.cfg.target.input_method,
        is_terminal,
        strip_enter,
        client_ctx.wayland_display.as_deref(),
    ) {
        Ok(()) => {
            daemon.beep(BeepKind::Success);
            Ok(())
        }
        Err(err) => {
            event!(
                name: "daemon.inject.failed",
                Level::ERROR,
                error.message = %err,
                "failed to inject text into window: {{error.message}}",
            );
            daemon.beep(BeepKind::Error);
            Err(err)
        }
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    model: String,
    device: &'static str,
    vad_enabled: bool,
}

#[derive(Debug, Serialize)]
struct HttpTranscribeResponse {
    text: String,
    language: String,
    model: String,
    segments: Vec<crate::transcribe::TranscriptSegment>,
    duration: f64,
}

async fn serve_http(daemon: Arc<SharedDaemon>, addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/health", get(http_health))
        .route("/stats", get(http_stats))
        .route("/transcribe", post(http_transcribe))
        .layer(DefaultBodyLimit::max(MAX_HTTP_AUDIO_BYTES))
        .with_state(daemon);

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP server on {addr}"))?;
    event!(
        name: "daemon.http.listening",
        Level::INFO,
        server.address = %addr,
        "TalkType-compatible Whisper HTTP API listening on http://{{server.address}}",
    );
    axum::serve(listener, app)
        .await
        .context("axum server error")
}

async fn http_health(State(daemon): State<Arc<SharedDaemon>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        model: daemon.transcriber.model_label(),
        device: "cpu",
        vad_enabled: true,
    })
}

async fn http_stats(State(daemon): State<Arc<SharedDaemon>>) -> impl IntoResponse {
    let stats = daemon
        .inner
        .lock()
        .map(|g| daemon.snapshot_stats(&g))
        .unwrap_or_default();
    Json(serde_json::json!({
        "request_count": stats.request_count,
        "error_count": stats.error_count,
        "avg_transcription_time_ms": stats.avg_transcription_time_ms,
        "total_audio_processed_seconds": stats.total_audio_processed_seconds,
        "uptime_seconds": stats.uptime_seconds,
        "model": daemon.transcriber.model_label(),
        "device": "cpu",
    }))
}

async fn http_transcribe(
    State(daemon): State<Arc<SharedDaemon>>,
    mut multipart: Multipart,
) -> Result<Json<HttpTranscribeResponse>, (StatusCode, Json<serde_json::Value>)> {
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
    let started = Instant::now();
    let res = daemon
        .transcriber
        .transcribe_pcm(&pcm, language.as_deref())
        .await
        .map_err(|e| {
            if let Ok(mut guard) = daemon.inner.lock() {
                guard.error_count += 1;
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "detail": format!("Transcription failed: {e}") })),
            )
        })?;

    let elapsed = started.elapsed().as_secs_f64();
    if let Ok(mut guard) = daemon.inner.lock() {
        guard.request_count += 1;
        guard.total_transcribe_secs += elapsed;
        guard.total_audio_secs += duration;
    }

    Ok(Json(HttpTranscribeResponse {
        text: res.text,
        language: res.language,
        model: daemon.transcriber.model_label(),
        segments: res.segments,
        duration,
    }))
}
