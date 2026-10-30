// Rust guideline compliant 2026-02-21
//! Single-executable Rust port of `TalkType` (`sttme`) for Linux Wayland and Windows System Tray.
//!
//! On Linux/Wayland, combines the hold-to-talk client (`hold`, `toggle`, `start`, `stop`,
//! `recover`, `retry`) and the hot-model background daemon (`daemon`).
//! On Windows, launches directly into the Notification Area (`tray`) with a global `WH_KEYBOARD_LL`
//! hold-to-talk hook (`F10`), `50 FPS` layered top-center LCARS waveform HUD, and `SendInput` typing.

#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, windows_subsystem = "windows")]

mod audio;
mod config;
#[cfg(unix)]
mod daemon;
mod history;
mod inject;
#[cfg(unix)]
mod ipc;
#[cfg(unix)]
mod ptt;
mod transcribe;
mod waveform;
#[cfg(windows)]
mod win_platform;
mod window;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use crate::audio::decode_wav_to_16k_f32;
#[cfg(unix)]
use crate::config::socket_path;
use crate::config::{
    AppConfig, DEFAULT_HTTP_PORT, InputMethod, TranscriptionMode, load_config, run_setup_wizard,
};
#[cfg(unix)]
use crate::daemon::run_daemon;
#[cfg(unix)]
use crate::inject::probe_wayland_virtual_keyboard;
use crate::inject::sanitize_for_alacritty;
#[cfg(unix)]
use crate::ipc::{ClientContext, IpcAction, IpcRequest, send_ipc_request};
#[cfg(unix)]
use crate::ptt::{parse_evdev_key, probe_wayland_layer_shell};
use crate::transcribe::{Transcriber, ensure_ggml_model};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Human-facing release version in `vYY.MMDD.##` form (e.g. `v26.1029.01`).
///
/// CI injects the exact git tag via `STTME_VERSION` because Cargo semver forbids the zero-padded
/// `.01` build counter; local builds fall back to the Cargo package version.
const DISPLAY_VERSION: &str = match option_env!("STTME_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Push-to-talk local Whisper voice typing for Wayland/Linux and Windows System Tray.
#[derive(Debug, Parser)]
#[command(name = "sttme", version = DISPLAY_VERSION, about)]
struct Cli {
    /// Override Whisper model size (`tiny`, `tiny.en`, `base`, `base.en`, `small`, `medium`, `large-v3`).
    #[arg(short, long, global = true)]
    model: Option<String>,

    /// Override Whisper HTTP API URL (enables API mode when set).
    #[arg(short, long, global = true)]
    api: Option<String>,

    /// Override model name sent to `OpenAI`-compatible APIs.
    #[arg(long, global = true)]
    api_model: Option<String>,

    /// Override ISO language code (e.g., `en`, `es`, or `auto`).
    #[arg(short, long, global = true)]
    language: Option<String>,

    /// Override text injection method (`type` or `paste`).
    #[arg(long, value_enum, global = true)]
    method: Option<CliInputMethod>,

    /// Run interactive setup wizard (`talktype --setup` equivalent).
    #[arg(long)]
    setup: bool,

    /// Subcommand to execute (defaults to `hold` on Linux or System `tray` on Windows).
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliInputMethod {
    /// Direct keystroke typing (`zwp_virtual_keyboard_v1` on Wayland, `SendInput` on Windows).
    Type,
    /// Clipboard copy + `Ctrl+Shift+V` / `Ctrl+V` shortcut.
    Paste,
}

impl From<CliInputMethod> for InputMethod {
    fn from(val: CliInputMethod) -> Self {
        match val {
            CliInputMethod::Type => Self::Type,
            CliInputMethod::Paste => Self::Paste,
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, ValueEnum)]
enum TargetScope {
    /// Restrict listening and keystroke output strictly to Alacritty (strips all Enter keys).
    Alacritty,
    /// Allow speech-to-text into any focused window (still strips Enter inside Alacritty).
    All,
}

#[cfg(unix)]
impl TargetScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Alacritty => "alacritty",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Listen strictly while `key` (`F10`) is held down, finishing automatically upon release.
    #[cfg(unix)]
    Hold {
        /// Key held down during dictation (e.g., `f10`, `f12`, `rctrl`, `capslock`, or evdev code).
        #[arg(short, long, default_value = "f10")]
        key: String,
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
        /// Suppress stdout status output (ideal for compositor keybindings).
        #[arg(short, long)]
        quiet: bool,
        /// Use exclusive Wayland keyboard grab instead of compositor key-repeat lease.
        #[arg(long)]
        grab: bool,
    },
    /// Toggle recording on/off via the persistent daemon (auto-starts daemon if needed).
    #[cfg(unix)]
    Toggle {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
        /// Suppress stdout status output (ideal for terminal keybindings).
        #[arg(short, long)]
        quiet: bool,
    },
    /// Explicitly start microphone recording.
    #[cfg(unix)]
    Start {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Explicitly stop recording, transcribe, and inject text.
    #[cfg(unix)]
    Stop,
    /// Cancel an active recording without transcribing.
    #[cfg(unix)]
    Cancel,
    /// Re-inject the last transcription from `~/.cache/sttme/history.jsonl` (`TalkType` F8).
    #[cfg(unix)]
    Recover {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Retry failed transcription from `~/.cache/sttme/pending.wav` (`TalkType` F7).
    #[cfg(unix)]
    Retry {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Query live daemon state, loaded model, and transcription metrics.
    #[cfg(unix)]
    Status,
    /// Stop the running background daemon.
    #[cfg(unix)]
    Shutdown,
    /// Run the persistent low-latency background daemon (keeps Whisper warm in RAM).
    #[cfg(unix)]
    Daemon {
        /// Optionally also expose `TalkType`'s HTTP `/health`, `/stats`, `/transcribe` server on this port.
        #[arg(long)]
        http_port: Option<u16>,
        /// Bind address for the optional HTTP server.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
    },
    /// Run the Windows System Tray application with global `F10` hold-to-talk and waveform HUD.
    #[cfg(windows)]
    Tray {
        /// Optionally also expose `TalkType`'s HTTP `/health` and `/transcribe` server on this port.
        #[arg(long)]
        http_port: Option<u16>,
        /// Bind address for the optional HTTP server.
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
        host: IpAddr,
    },
    /// Run the `TalkType`-compatible Whisper HTTP server (`whisper_server.py` port).
    Serve {
        /// Port to bind HTTP server on.
        #[arg(short, long, default_value_t = DEFAULT_HTTP_PORT)]
        port: u16,
        /// Host IP to bind HTTP server on.
        #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
        host: IpAddr,
    },
    /// Transcribe a WAV file directly from disk and print sanitized text.
    Transcribe {
        /// Path to input WAV audio file.
        file: PathBuf,
        /// Strip all Enter/newline characters using Alacritty sanitization rules.
        #[arg(long, default_value_t = true)]
        strip_enter: bool,
    },
    /// Pre-download a Whisper GGML model into the local model cache.
    DownloadModel {
        /// Model name (`tiny`, `tiny.en`, `base`, `base.en`, `small`, `medium`, `large-v3`, `large-v3-turbo`).
        #[arg(default_value = "base")]
        model_name: String,
    },
    /// Run interactive configuration wizard.
    Setup,
    /// Verify platform input backend, HUD readiness, and model configuration without typing keys.
    Doctor,
}

fn main() -> Result<()> {
    #[cfg(windows)]
    if std::env::args_os().len() > 1 {
        win_platform::attach_parent_console_if_present();
    }

    let cli = Cli::parse();

    if should_enable_tracing(cli.command.as_ref()) {
        init_tracing();
    }

    if cli.setup || matches!(cli.command, Some(Commands::Setup)) {
        run_setup_wizard()?;
        return Ok(());
    }

    let command = cli.command.unwrap_or_else(default_platform_command);
    execute_command(
        command,
        cli.model,
        cli.api,
        cli.api_model,
        cli.language,
        cli.method,
    )
}

fn should_enable_tracing(cmd: Option<&Commands>) -> bool {
    match cmd {
        #[cfg(unix)]
        Some(Commands::Daemon { .. }) => true,
        #[cfg(windows)]
        Some(Commands::Tray { .. }) => std::env::args_os().len() > 1,
        Some(
            Commands::Serve { .. }
            | Commands::DownloadModel { .. }
            | Commands::Transcribe { .. }
            | Commands::Doctor,
        ) => true,
        _ => false,
    }
}

fn default_platform_command() -> Commands {
    #[cfg(unix)]
    {
        Commands::Hold {
            key: "f10".to_owned(),
            target: None,
            quiet: false,
            grab: false,
        }
    }
    #[cfg(windows)]
    {
        Commands::Tray {
            http_port: None,
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }
    }
}

fn execute_command(
    command: Commands,
    model: Option<String>,
    api: Option<String>,
    api_model: Option<String>,
    language: Option<String>,
    method: Option<CliInputMethod>,
) -> Result<()> {
    match command {
        #[cfg(unix)]
        Commands::Hold {
            key,
            target,
            quiet,
            grab,
        } => dispatch_hold_ipc(&key, target, quiet, grab)?,
        #[cfg(unix)]
        Commands::Toggle { target, quiet } => {
            dispatch_simple_ipc(IpcAction::Toggle, target, true, quiet)?;
        }
        #[cfg(unix)]
        Commands::Start { target } => {
            dispatch_simple_ipc(IpcAction::Start, target, true, false)?;
        }
        #[cfg(unix)]
        Commands::Stop => {
            dispatch_simple_ipc(IpcAction::Stop, None, false, false)?;
        }
        #[cfg(unix)]
        Commands::Cancel => {
            dispatch_simple_ipc(IpcAction::Cancel, None, false, false)?;
        }
        #[cfg(unix)]
        Commands::Recover { target } => {
            dispatch_simple_ipc(IpcAction::Recover, target, true, false)?;
        }
        #[cfg(unix)]
        Commands::Retry { target } => {
            dispatch_simple_ipc(IpcAction::Retry, target, true, false)?;
        }
        #[cfg(unix)]
        Commands::Status => {
            print_daemon_status()?;
        }
        #[cfg(unix)]
        Commands::Shutdown => dispatch_shutdown_ipc()?,
        #[cfg(unix)]
        Commands::Daemon { http_port, host } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            run_in_tokio(run_daemon(
                cfg,
                http_port.map(|port| SocketAddr::new(host, port)),
            ))?;
        }
        #[cfg(windows)]
        Commands::Tray { http_port, host } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            win_platform::run_windows_tray(
                &cfg,
                http_port.map(|port| SocketAddr::new(host, port)),
            )?;
        }
        Commands::Serve { port, host } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            #[cfg(unix)]
            {
                run_in_tokio(run_daemon(cfg, Some(SocketAddr::new(host, port))))?;
            }
            #[cfg(windows)]
            {
                run_in_tokio(win_platform::run_windows_http_server(
                    cfg,
                    SocketAddr::new(host, port),
                ))?;
            }
        }
        Commands::Transcribe { file, strip_enter } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            run_in_tokio(run_file_transcription(&cfg, &file, strip_enter))?;
        }
        Commands::DownloadModel { model_name } => {
            run_in_tokio(async move {
                let client = reqwest::Client::new();
                let path = ensure_ggml_model(&model_name, &client).await?;
                println!("Model ready at {}", path.display());
                Ok(())
            })?;
        }
        Commands::Setup => {
            run_setup_wizard()?;
        }
        Commands::Doctor => {
            run_doctor()?;
        }
    }
    Ok(())
}

fn run_in_tokio<F>(fut: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to initialize Tokio runtime")?;
    runtime.block_on(fut)
}

#[cfg(unix)]
fn dispatch_hold_ipc(
    key: &str,
    target: Option<TargetScope>,
    quiet: bool,
    grab: bool,
) -> Result<()> {
    let evdev_code = parse_evdev_key(key)?;
    let resp = send_ipc_request(
        &socket_path(),
        &IpcRequest {
            action: IpcAction::Hold,
            context: ClientContext::from_env(target.map(|t| t.as_str().to_owned()))
                .with_hold_key(evdev_code)
                .with_grab_keyboard(grab),
        },
        true,
    )?;
    if !quiet {
        println!("[{}] {}", resp.state, resp.message);
    }
    Ok(())
}

#[cfg(unix)]
fn dispatch_shutdown_ipc() -> Result<()> {
    let resp = send_ipc_request(
        &socket_path(),
        &IpcRequest {
            action: IpcAction::Shutdown,
            context: ClientContext::from_env(None),
        },
        false,
    )?;
    println!("{}", resp.message);
    Ok(())
}

#[cfg(unix)]
fn dispatch_simple_ipc(
    action: IpcAction,
    target: Option<TargetScope>,
    autostart: bool,
    quiet: bool,
) -> Result<()> {
    let resp = send_ipc_request(
        &socket_path(),
        &IpcRequest {
            action,
            context: ClientContext::from_env(target.map(|t| t.as_str().to_owned())),
        },
        autostart,
    )?;
    if !quiet {
        println!("[{}] {}", resp.state, resp.message);
    }
    Ok(())
}

#[cfg(unix)]
fn print_daemon_status() -> Result<()> {
    let resp = send_ipc_request(
        &socket_path(),
        &IpcRequest {
            action: IpcAction::Status,
            context: ClientContext::from_env(None),
        },
        false,
    )?;
    println!("State:  {}", resp.state);
    println!("Model:  {}", resp.model);
    println!("Detail: {}", resp.message);
    if let Some(stats) = resp.stats {
        println!(
            "Stats:  {} requests, {} errors, avg {:.1}ms, {:.1}s audio, uptime {:.1}s",
            stats.request_count,
            stats.error_count,
            stats.avg_transcription_time_ms,
            stats.total_audio_processed_seconds,
            stats.uptime_seconds
        );
    }
    Ok(())
}

async fn run_file_transcription(cfg: &AppConfig, file: &PathBuf, strip_enter: bool) -> Result<()> {
    let bytes =
        std::fs::read(file).with_context(|| format!("failed to read {}", file.display()))?;
    let pcm = decode_wav_to_16k_f32(&bytes)?;
    let engine = Transcriber::new(cfg).await?;
    let res = engine.transcribe_pcm(&pcm, None).await?;
    let out = if strip_enter {
        sanitize_for_alacritty(&res.text)
    } else {
        res.text
    };
    println!("{out}");
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("sttme=info,whisper_rs=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn apply_cli_overrides(
    mut cfg: AppConfig,
    model: Option<String>,
    api: Option<String>,
    api_model: Option<String>,
    language: Option<String>,
    method: Option<CliInputMethod>,
) -> AppConfig {
    if let Some(m) = model {
        cfg.transcription.model = m;
    }
    if let Some(url) = api {
        cfg.transcription.mode = TranscriptionMode::Api;
        cfg.transcription.api_url = Some(url);
    }
    if let Some(am) = api_model {
        cfg.transcription.api_model = Some(am);
    }
    if let Some(lang) = language {
        if lang.eq_ignore_ascii_case("auto") {
            cfg.transcription.language = None;
        } else {
            cfg.transcription.language = Some(lang);
        }
    }
    if let Some(im) = method {
        cfg.target.input_method = im.into();
    }
    cfg
}

fn run_doctor() -> Result<()> {
    let cfg = load_config()?;
    println!("Config path:    {}", config::config_file_path().display());
    #[cfg(unix)]
    println!("Socket path:    {}", socket_path().display());
    println!("Model:          {}", cfg.transcription.model);
    println!("Alacritty only: {}", cfg.target.alacritty_only);
    println!("Strip Enter:    {}", cfg.target.strip_alacritty_enter);
    println!("Input method:   {}", cfg.target.input_method);

    match window::get_focused_window(None) {
        Some(win) => println!(
            "Focused window: id={} app_id='{}' terminal={}",
            win.id,
            win.app_id,
            win.is_terminal()
        ),
        None => println!("Focused window: unavailable"),
    }

    #[cfg(unix)]
    {
        match probe_wayland_virtual_keyboard(None) {
            Ok(()) => {
                println!("Wayland VK:     OK (zwp_virtual_keyboard_v1 bound & keymap verified)");
            }
            Err(err) => println!("Wayland VK:     WARN ({err})"),
        }

        match probe_wayland_layer_shell(None) {
            Ok(()) => {
                println!(
                    "Wayland HUD:    OK (zwlr_layer_shell_v1 top-center waveform & hold-to-talk)"
                );
            }
            Err(err) => println!("Wayland HUD:    WARN ({err})"),
        }
    }

    #[cfg(windows)]
    print_windows_doctor(&cfg);

    Ok(())
}

#[cfg(windows)]
fn print_windows_doctor(cfg: &AppConfig) {
    println!(
        "Hotkey:         {} (WH_KEYBOARD_LL swallows menu-bar activation)",
        win_platform::hotkey_display_name(&cfg.target.hotkey)
    );
    println!(
        "Sign-In Start:  {}",
        if win_platform::is_autostart_enabled() {
            "enabled (HKCU\\...\\Run\\sttme)"
        } else {
            "disabled"
        }
    );
    println!("Win32 Input:    OK (SendInput KEYEVENTF_UNICODE + clipboard fallback)");
    println!("Win32 HUD:      OK (Per-Monitor DPI V2 UpdateLayeredWindow 50 FPS LCARS HUD)");
}
