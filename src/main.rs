// Rust guideline compliant 2026-02-21
//! Single-executable Rust port of `TalkType` (`sttme`) with sub-millisecond daemon IPC.
//!
//! Combines the hold-to-talk client (`hold`, `toggle`, `start`, `stop`, `recover`, `retry`),
//! the hot-model background daemon (`daemon`), the `TalkType`-compatible HTTP transcription server
//! (`serve`), direct WAV transcription (`transcribe`), and the interactive setup wizard (`setup`)
//! into a single standalone binary.

mod audio;
mod config;
mod daemon;
mod history;
mod inject;
mod ipc;
mod ptt;
mod transcribe;
mod waveform;
mod window;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use mimalloc::MiMalloc;
use tracing_subscriber::EnvFilter;

use crate::audio::decode_wav_to_16k_f32;
use crate::config::{
    AppConfig, DEFAULT_HTTP_PORT, InputMethod, TranscriptionMode, load_config, run_setup_wizard,
    socket_path,
};
use crate::daemon::run_daemon;
use crate::inject::{probe_wayland_virtual_keyboard, sanitize_for_alacritty};
use crate::ipc::{ClientContext, IpcAction, IpcRequest, send_ipc_request};
use crate::ptt::{parse_evdev_key, probe_wayland_layer_shell};
use crate::transcribe::{Transcriber, ensure_ggml_model};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Push-to-talk local Whisper voice typing for Wayland/Niri and Linux (Rust port of `TalkType`).
#[derive(Debug, Parser)]
#[command(name = "sttme", version, about)]
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

    /// Subcommand to execute (defaults to `hold` on `F10` for instant hotkey binding).
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliInputMethod {
    /// Direct Wayland `zwp_virtual_keyboard_v1` typing (preserves clipboard).
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TargetScope {
    /// Restrict listening and keystroke output strictly to Alacritty (strips all Enter keys).
    Alacritty,
    /// Allow speech-to-text into any focused window (still strips Enter inside Alacritty).
    All,
}

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
    Toggle {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
        /// Suppress stdout status output (ideal for terminal keybindings).
        #[arg(short, long)]
        quiet: bool,
    },
    /// Explicitly start microphone recording.
    Start {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Explicitly stop recording, transcribe, and inject text.
    Stop,
    /// Cancel an active recording without transcribing.
    Cancel,
    /// Re-inject the last transcription from `~/.cache/sttme/history.jsonl` (`TalkType` F8).
    Recover {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Retry failed transcription from `~/.cache/sttme/pending.wav` (`TalkType` F7).
    Retry {
        /// Target window scope (`alacritty` or `all`).
        #[arg(long, value_enum)]
        target: Option<TargetScope>,
    },
    /// Query live daemon state, loaded model, and transcription metrics.
    Status,
    /// Stop the running background daemon.
    Shutdown,
    /// Run the persistent low-latency background daemon (keeps Whisper warm in RAM).
    Daemon {
        /// Optionally also expose `TalkType`'s HTTP `/health`, `/stats`, `/transcribe` server on this port.
        #[arg(long)]
        http_port: Option<u16>,
        /// Bind address for the optional HTTP server.
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
    },
    /// Run both the IPC daemon and `TalkType`-compatible Whisper HTTP server (`whisper_server.py` port).
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
    /// Pre-download a Whisper GGML model into `~/.cache/sttme/models`.
    DownloadModel {
        /// Model name (`tiny`, `tiny.en`, `base`, `base.en`, `small`, `medium`, `large-v3`, `large-v3-turbo`).
        #[arg(default_value = "base")]
        model_name: String,
    },
    /// Run interactive configuration wizard.
    Setup,
    /// Verify Wayland virtual keyboard, Niri IPC, and model readiness without typing keys.
    Doctor,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(
        cli.command,
        Some(
            Commands::Daemon { .. }
                | Commands::Serve { .. }
                | Commands::DownloadModel { .. }
                | Commands::Transcribe { .. }
                | Commands::Doctor
        )
    ) {
        init_tracing();
    }

    if cli.setup || matches!(cli.command, Some(Commands::Setup)) {
        run_setup_wizard()?;
        return Ok(());
    }

    let command = cli.command.unwrap_or(Commands::Hold {
        key: "f10".to_owned(),
        target: None,
        quiet: false,
        grab: false,
    });
    execute_command(
        command,
        cli.model,
        cli.api,
        cli.api_model,
        cli.language,
        cli.method,
    )
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
        Commands::Hold {
            key,
            target,
            quiet,
            grab,
        } => {
            let evdev_code = parse_evdev_key(&key)?;
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
        }
        Commands::Toggle { target, quiet } => {
            dispatch_simple_ipc(IpcAction::Toggle, target, true, quiet)?;
        }
        Commands::Start { target } => {
            dispatch_simple_ipc(IpcAction::Start, target, true, false)?;
        }
        Commands::Stop => {
            dispatch_simple_ipc(IpcAction::Stop, None, false, false)?;
        }
        Commands::Cancel => {
            dispatch_simple_ipc(IpcAction::Cancel, None, false, false)?;
        }
        Commands::Recover { target } => {
            dispatch_simple_ipc(IpcAction::Recover, target, true, false)?;
        }
        Commands::Retry { target } => {
            dispatch_simple_ipc(IpcAction::Retry, target, true, false)?;
        }
        Commands::Status => {
            print_daemon_status()?;
        }
        Commands::Shutdown => {
            let resp = send_ipc_request(
                &socket_path(),
                &IpcRequest {
                    action: IpcAction::Shutdown,
                    context: ClientContext::from_env(None),
                },
                false,
            )?;
            println!("{}", resp.message);
        }
        Commands::Daemon { http_port, host } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            run_in_tokio(run_daemon(
                cfg,
                http_port.map(|port| SocketAddr::new(host, port)),
            ))?;
        }
        Commands::Serve { port, host } => {
            let cfg = apply_cli_overrides(load_config()?, model, api, api_model, language, method);
            run_in_tokio(run_daemon(cfg, Some(SocketAddr::new(host, port))))?;
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
    println!("Socket path:    {}", socket_path().display());
    println!("Model:          {}", cfg.transcription.model);
    println!("Alacritty only: {}", cfg.target.alacritty_only);
    println!("Strip Enter:    {}", cfg.target.strip_alacritty_enter);
    println!("Input method:   {}", cfg.target.input_method);

    match window::get_focused_window(None) {
        Some(win) => println!(
            "Focused window: id={} app_id='{}' alacritty={}",
            win.id,
            win.app_id,
            win.is_alacritty()
        ),
        None => println!("Focused window: unavailable"),
    }

    match probe_wayland_virtual_keyboard(None) {
        Ok(()) => println!("Wayland VK:     OK (zwp_virtual_keyboard_v1 bound & keymap verified)"),
        Err(err) => println!("Wayland VK:     WARN ({err})"),
    }

    match probe_wayland_layer_shell(None) {
        Ok(()) => {
            println!("Wayland HUD:    OK (zwlr_layer_shell_v1 top-center waveform & hold-to-talk)");
        }
        Err(err) => println!("Wayland HUD:    WARN ({err})"),
    }

    Ok(())
}
