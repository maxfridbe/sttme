// Rust guideline compliant 2026-02-21
//! Configuration loading, persistence, path resolution, and interactive setup wizard.
//!
//! Settings are stored in `~/.config/sttme/config.toml`. For seamless migration from
//! Python `TalkType`, `~/.config/talktype/config.yaml` is automatically imported when
//! no native configuration file is present yet.

use std::fmt::{self, Debug, Display, Formatter};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{Level, event};

/// Default Whisper GGML model name when none is configured.
///
/// `base` balances accuracy (~142 MB) and sub-300ms CPU latency on modern `x86_64`.
/// Smaller (`tiny`) trades accuracy for speed; larger (`small`, `medium`) increases RAM/CPU.
pub const DEFAULT_MODEL: &str = "base";

/// Default maximum number of transcription entries retained in `history.jsonl`.
///
/// Matches `TalkType`'s 100-entry rolling window so disk usage stays bounded (< 64 KB).
pub const DEFAULT_HISTORY_LIMIT: usize = 100;

/// Default RMS speech energy threshold across 50ms audio frames.
///
/// Slightly below `TalkType`'s `0.01` (`0.008`) to avoid false negatives on laptop mics
/// while still rejecting ambient room silence before running Whisper.
pub const DEFAULT_ENERGY_THRESHOLD: f32 = 0.008;

/// Default port for the optional `TalkType`-compatible HTTP transcription server.
///
/// Matches `whisper_server.py` (`8002`) so existing `TalkType` clients can connect unchanged.
pub const DEFAULT_HTTP_PORT: u16 = 8002;

/// Redacted wrapper around an optional cloud API bearer token.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiSecret(String);

impl ApiSecret {
    /// Creates a new [`ApiSecret`] wrapping the provided token string.
    #[cfg(test)]
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// Borrows the raw secret string for outbound HTTP authorization headers.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns `true` when the secret string is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Debug for ApiSecret {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            write!(f, "ApiSecret(<empty>)")
        } else {
            write!(f, "ApiSecret([REDACTED])")
        }
    }
}

/// Method used to deliver transcribed text into the target window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputMethod {
    /// Type characters directly via Wayland `zwp_virtual_keyboard_v1` without touching clipboard.
    #[default]
    Type,
    /// Copy text to clipboard and trigger `Ctrl+Shift+V` (terminals) or `Ctrl+V` (apps).
    Paste,
}

impl Display for InputMethod {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type => f.write_str("type"),
            Self::Paste => f.write_str("paste"),
        }
    }
}

/// Transcription execution mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionMode {
    /// Run Whisper locally in-process inside the `sttme` daemon via `whisper.cpp`.
    #[default]
    Local,
    /// Send recorded WAV audio to an `OpenAI`-compatible or `TalkType` HTTP endpoint.
    Api,
}

impl Display for TranscriptionMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Api => f.write_str("api"),
        }
    }
}

/// Top-level application configuration persisted at `~/.config/sttme/config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Speech-to-text model and backend settings.
    pub transcription: TranscriptionConfig,
    /// Window targeting, Enter-keystroke stripping, and injection settings.
    pub target: TargetConfig,
    /// Audio feedback and UI preferences.
    pub ui: UiConfig,
    /// Recovery history and pending audio retry preferences.
    pub history: HistoryConfig,
}

/// Speech-to-text backend settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptionConfig {
    /// Whether to run local embedded Whisper or call a remote HTTP API.
    pub mode: TranscriptionMode,
    /// Whisper model identifier (`tiny`, `tiny.en`, `base`, `base.en`, `small`, `medium`, `large-v3`).
    pub model: String,
    /// Optional ISO-639-1 language code (e.g., `"en"`). `None` enables auto-detection.
    pub language: Option<String>,
    /// Optional HTTP endpoint URL when `mode` is [`TranscriptionMode::Api`].
    pub api_url: Option<String>,
    /// Optional model name sent to OpenAI-compatible transcription APIs.
    pub api_model: Option<String>,
    /// Optional bearer token for cloud APIs (falls back to `GROQ_API_KEY` / `OPENAI_API_KEY`).
    pub api_key: Option<ApiSecret>,
    /// Minimum 50ms segment RMS energy required to treat audio as speech.
    pub energy_threshold: f32,
}

impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            mode: TranscriptionMode::Local,
            model: DEFAULT_MODEL.to_owned(),
            language: Some("en".to_owned()),
            api_url: None,
            api_model: None,
            api_key: None,
            energy_threshold: DEFAULT_ENERGY_THRESHOLD,
        }
    }
}

/// Window targeting and text sanitization configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetConfig {
    /// Global hold-to-talk key name (`"f10"`, `"f9"`, `"f12"`, `"rctrl"`, `"ralt"`).
    pub hotkey: String,
    /// Restrict speech-to-text output strictly to Alacritty windows.
    pub alacritty_only: bool,
    /// Strip all Enter/Return/newline characters when typing or pasting into terminals.
    pub strip_alacritty_enter: bool,
    /// Append a trailing space after each dictated phrase so consecutive dictations separate cleanly.
    pub trailing_space: bool,
    /// Preferred Wayland/X11/Win32 delivery mechanism.
    pub input_method: InputMethod,
}

impl Default for TargetConfig {
    fn default() -> Self {
        Self {
            hotkey: "f10".to_owned(),
            alacritty_only: false,
            strip_alacritty_enter: true,
            trailing_space: true,
            input_method: InputMethod::Type,
        }
    }
}

/// User feedback preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Play embedded LCARS `computerbeep_50` / `computerbeep_48` audio cues.
    pub audio_beeps: bool,
    /// Render a live top-center Wayland audio waveform overlay while listening.
    pub waveform_overlay: bool,
    /// Minimal terminal output mode.
    pub minimal: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            audio_beeps: true,
            waveform_overlay: true,
            minimal: false,
        }
    }
}

/// Transcription recovery history configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryConfig {
    /// Maximum number of transcriptions stored in `history.jsonl`.
    pub limit: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            limit: DEFAULT_HISTORY_LIMIT,
        }
    }
}

/// Legacy `~/.config/talktype/config.yaml` schema for seamless import.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyTalkTypeYaml {
    transcription: LegacyTranscriptionYaml,
    ui: LegacyUiYaml,
    history: LegacyHistoryYaml,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyTranscriptionYaml {
    mode: Option<String>,
    model: Option<String>,
    language: Option<String>,
    api_url: Option<String>,
    api_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyUiYaml {
    minimal: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LegacyHistoryYaml {
    limit: Option<usize>,
}

/// Returns the native `sttme` configuration file path (`~/.config/sttme/config.toml`).
#[must_use]
pub fn config_file_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sttme")
        .join("config.toml")
}

/// Returns the legacy `TalkType` YAML path (`~/.config/talktype/config.yaml`).
#[must_use]
pub fn legacy_talktype_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("talktype")
        .join("config.yaml")
}

/// Returns the `sttme` cache directory (`~/.cache/sttme` on Linux, `%LOCALAPPDATA%\sttme` on Windows).
#[must_use]
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("sttme")
}

/// Returns the local Whisper GGML model cache directory (`<cache_dir>/models`).
#[must_use]
pub fn models_dir() -> PathBuf {
    cache_dir().join("models")
}

/// Returns the Unix domain socket path used for instant client-to-daemon IPC on Linux.
#[cfg(unix)]
#[must_use]
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("sttme.sock")
    } else {
        cache_dir().join("sttme.sock")
    }
}

/// Returns the daemon single-instance lock file path (`~/.cache/sttme/sttme.lock`).
#[cfg(unix)]
#[must_use]
pub fn lock_file_path() -> PathBuf {
    cache_dir().join("sttme.lock")
}

/// Loads [`AppConfig`] from disk, falling back to legacy `TalkType` YAML or defaults.
///
/// # Errors
///
/// Returns an error if `~/.config/sttme/config.toml` exists on disk but contains
/// malformed TOML syntax.
pub fn load_config() -> Result<AppConfig> {
    let native_path = config_file_path();
    if native_path.exists() {
        let raw = fs::read_to_string(&native_path)
            .with_context(|| format!("failed to read {}", native_path.display()))?;
        let cfg: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {}", native_path.display()))?;
        return Ok(cfg);
    }

    let legacy_path = legacy_talktype_config_path();
    if legacy_path.exists()
        && let Ok(raw) = fs::read_to_string(&legacy_path)
        && let Ok(legacy) = serde_yaml_ng::from_str::<LegacyTalkTypeYaml>(&raw)
    {
        let mut cfg = AppConfig::default();
        if legacy.transcription.mode.as_deref() == Some("api") {
            cfg.transcription.mode = TranscriptionMode::Api;
        }
        if let Some(model) = legacy.transcription.model {
            cfg.transcription.model = model;
        }
        if legacy.transcription.language.is_some() {
            cfg.transcription.language = legacy.transcription.language;
        }
        cfg.transcription.api_url = legacy.transcription.api_url;
        cfg.transcription.api_model = legacy.transcription.api_model;
        if let Some(minimal) = legacy.ui.minimal {
            cfg.ui.minimal = minimal;
        }
        if let Some(limit) = legacy.history.limit {
            cfg.history.limit = limit;
        }
        event!(
            name: "config.legacy.imported",
            Level::INFO,
            file.path = %legacy_path.display(),
            "imported legacy TalkType configuration from {{file.path}}",
        );
        return Ok(cfg);
    }

    Ok(AppConfig::default())
}

/// Saves [`AppConfig`] atomically to `~/.config/sttme/config.toml`.
///
/// # Errors
///
/// Returns an error if the parent directory cannot be created or writing the file fails.
pub fn save_config(cfg: &AppConfig) -> Result<PathBuf> {
    let path = config_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let serialized = toml::to_string_pretty(cfg).context("failed to serialize config to TOML")?;
    fs::write(&path, serialized).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// Runs the interactive terminal setup wizard (Rust port of `setup_wizard.py`).
///
/// # Errors
///
/// Returns an error if standard I/O fails or the resulting configuration cannot be saved.
pub fn run_setup_wizard() -> Result<AppConfig> {
    let mut cfg = load_config().unwrap_or_default();

    println!("\n==============================================");
    println!("  sttme Setup (Rust port of TalkType)");
    println!("==============================================\n");

    configure_wizard_mode(&mut cfg)?;

    let models = [
        "tiny.en",
        "tiny",
        "base.en",
        "base",
        "small.en",
        "small",
        "medium",
        "large-v3-turbo",
    ];
    let selected_model = prompt_choice(
        "Step 2 of 4 - Local Whisper GGML model:",
        &[
            "tiny.en        (~75 MB, fastest, English only)",
            "tiny           (~75 MB, fastest, multilingual)",
            "base.en        (~142 MB, fast & accurate, English only)",
            "base           (~142 MB, fast & accurate, multilingual - default)",
            "small.en       (~466 MB, higher accuracy, English only)",
            "small          (~466 MB, higher accuracy, multilingual)",
            "medium         (~1.5 GB, great accuracy)",
            "large-v3-turbo (~1.6 GB, best speed/accuracy large model)",
        ],
        3,
    )?;
    models[selected_model].clone_into(&mut cfg.transcription.model);

    let lang_idx = prompt_choice(
        "Step 3 of 4 - Language:",
        &[
            "English (en)",
            "Auto-detect",
            "Spanish (es)",
            "French (fr)",
            "German (de)",
        ],
        0,
    )?;
    cfg.transcription.language = match lang_idx {
        0 => Some("en".to_owned()),
        1 => None,
        2 => Some("es".to_owned()),
        3 => Some("fr".to_owned()),
        _ => Some("de".to_owned()),
    };

    let target_idx = prompt_choice(
        "Step 4 of 4 - Target window behavior:",
        &[
            "Alacritty only + strip all Enter keystrokes (default)",
            "All focused windows (still strip Enter in Alacritty)",
        ],
        0,
    )?;
    cfg.target.alacritty_only = target_idx == 0;
    cfg.target.strip_alacritty_enter = true;

    let saved = save_config(&cfg)?;
    println!("\nSaved configuration to {}", saved.display());
    Ok(cfg)
}

fn configure_wizard_mode(cfg: &mut AppConfig) -> Result<()> {
    let selected_mode = prompt_choice(
        "Step 1 of 4 - Transcription mode:",
        &[
            "Local Embedded Whisper (instant via sttme daemon, private, recommended)",
            "Local/Custom Whisper HTTP Server (e.g. http://127.0.0.1:8002/transcribe)",
            "Cloud OpenAI-Compatible API (Groq / OpenAI)",
        ],
        0,
    )?;

    match selected_mode {
        0 => {
            cfg.transcription.mode = TranscriptionMode::Local;
            cfg.transcription.api_url = None;
        }
        1 => {
            cfg.transcription.mode = TranscriptionMode::Api;
            let url = prompt_line(
                "API URL",
                cfg.transcription
                    .api_url
                    .as_deref()
                    .unwrap_or("http://127.0.0.1:8002/transcribe"),
            )?;
            cfg.transcription.api_url = Some(url);
        }
        _ => {
            cfg.transcription.mode = TranscriptionMode::Api;
            let provider = prompt_choice(
                "Cloud provider:",
                &[
                    "Groq (whisper-large-v3, uses GROQ_API_KEY)",
                    "OpenAI (whisper-1, uses OPENAI_API_KEY)",
                    "Custom URL",
                ],
                0,
            )?;
            match provider {
                0 => {
                    cfg.transcription.api_url =
                        Some("https://api.groq.com/openai/v1/audio/transcriptions".to_owned());
                    cfg.transcription.api_model = Some("whisper-large-v3".to_owned());
                }
                1 => {
                    cfg.transcription.api_url =
                        Some("https://api.openai.com/v1/audio/transcriptions".to_owned());
                    cfg.transcription.api_model = Some("whisper-1".to_owned());
                }
                _ => {
                    let url = prompt_line("Custom API URL", "")?;
                    let model = prompt_line("API Model name", "whisper-1")?;
                    cfg.transcription.api_url = Some(url);
                    cfg.transcription.api_model = Some(model);
                }
            }
        }
    }
    Ok(())
}

fn prompt_choice(title: &str, options: &[&str], default_idx: usize) -> Result<usize> {
    println!("{title}");
    for (idx, item) in options.iter().enumerate() {
        let marker = if idx == default_idx { "*" } else { " " };
        println!("  [{marker}] {}: {item}", idx + 1);
    }
    print!(
        "Select [1-{} (default {})]: ",
        options.len(),
        default_idx + 1
    );
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default_idx);
    }
    if let Ok(num) = trimmed.parse::<usize>()
        && (1..=options.len()).contains(&num)
    {
        return Ok(num - 1);
    }
    Ok(default_idx)
}

fn prompt_line(label: &str, default_val: &str) -> Result<String> {
    if default_val.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default_val}]: ");
    }
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default_val.to_owned())
    } else {
        Ok(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_secret_debug_redacts_sensitive_token() {
        let raw_token = "sk-live-987654321-secret-token";
        let secret = ApiSecret::new(raw_token);
        let debug_repr = format!("{secret:?}");
        assert!(debug_repr.contains("ApiSecret"));
        assert!(!debug_repr.contains(raw_token));
    }

    #[test]
    fn default_config_roundtrips_toml() {
        let cfg = AppConfig::default();
        assert!(!cfg.target.alacritty_only);
        assert!(cfg.target.strip_alacritty_enter);
        let encoded = toml::to_string_pretty(&cfg).expect("serialize default config");
        let decoded: AppConfig = toml::from_str(&encoded).expect("deserialize default config");
        assert_eq!(decoded.transcription.model, DEFAULT_MODEL);
        assert!(!decoded.target.alacritty_only);
        assert!(decoded.target.strip_alacritty_enter);
    }
}
