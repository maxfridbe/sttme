// Rust guideline compliant 2026-02-21
//! Local embedded `whisper.cpp` transcription, remote HTTP API client, model downloader, and hallucination filter.
//!
//! Keeps `WhisperContext` resident in RAM inside `sttme daemon` so consecutive dictations run
//! with zero model initialization overhead, while also supporting OpenAI/Groq/TalkType HTTP APIs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{Level, event};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::audio::encode_wav_16k;
use crate::config::{AppConfig, TranscriptionMode, models_dir};

/// Maximum physical/logical threads assigned to a single `whisper.cpp` inference pass.
///
/// `whisper.cpp` scales nearly linearly up to physical core count (`8` on Zen 4 laptops),
/// whereas oversubscribing SMT hyperthreads increases barrier synchronization latency.
const MAX_WHISPER_THREADS: usize = 8;

/// Common Whisper silence hallucination substrings ported from `talktype.py` + `whisper.cpp`.
const HALLUCINATION_PHRASES: &[&str] = &[
    "thanks for watching",
    "thank you for watching",
    "thanks for listening",
    "thank you for listening",
    "thank you",
    "thanks",
    "subscribe",
    "like and subscribe",
    "see you next time",
    "see you later",
    "the end",
    "silence",
    "no speech",
    "inaudible",
    "[music]",
    "(music)",
    "[blank_audio]",
    "(blank audio)",
    "[silence]",
    "please subscribe",
    "don't forget to subscribe",
    "hit the bell",
    "leave a comment",
    "see you in the next",
    "bye bye",
    "good bye",
    "take care",
    "have a nice day",
    "have a good day",
    "peace out",
    "cheers",
    "ciao",
    "adios",
    "auf wiedersehen",
    "さようなら",
    "...",
    "♪",
    "music playing",
    "background noise",
    "applause",
];

/// Single-word Whisper silence hallucinations when they constitute the entire transcript.
const HALLUCINATION_WORDS: &[&str] = &[
    "you", "i", "so", "uh", "um", "hmm", "huh", "ah", "oh", "bye", "goodbye", "thanks", "okay",
    "ok", "yes", "no", "yeah", "yep", "nope", "well", "right", "hey", "hi", "hello", "what", "hm",
];

/// Timestamped transcription segment matching `whisper_server.py` JSON responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// Segment start offset in seconds.
    pub start: f64,
    /// Segment end offset in seconds.
    pub end: f64,
    /// Segment UTF-8 text.
    pub text: String,
}

/// Detailed output returned by [`Transcriber::transcribe_pcm`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptResult {
    /// Cleaned concatenated transcript text.
    pub text: String,
    /// Detected or configured ISO language code.
    pub language: String,
    /// Timestamped segment breakdown.
    pub segments: Vec<TranscriptSegment>,
}

/// Unified speech-to-text engine supporting in-process `whisper.cpp` and remote HTTP APIs.
pub struct Transcriber {
    mode: TranscriptionMode,
    model_name: String,
    language: Option<String>,
    api_url: Option<String>,
    api_model: Option<String>,
    api_key: Option<String>,
    local_ctx: Option<Mutex<WhisperContext>>,
    http_client: reqwest::Client,
}

impl std::fmt::Debug for Transcriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transcriber")
            .field("mode", &self.mode)
            .field("model_name", &self.model_name)
            .field("language", &self.language)
            .field("api_url", &self.api_url)
            .field("local_loaded", &self.local_ctx.is_some())
            .finish_non_exhaustive()
    }
}

impl Transcriber {
    /// Initializes a [`Transcriber`] from [`AppConfig`], downloading the GGML model if needed.
    ///
    /// # Errors
    ///
    /// Returns an error if model download fails or `whisper.cpp` rejects the model file.
    pub async fn new(cfg: &AppConfig) -> Result<Self> {
        whisper_rs::install_logging_hooks();

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(240))
            .build()
            .context("failed to build HTTP client")?;

        let mode = if cfg.transcription.api_url.is_some() {
            TranscriptionMode::Api
        } else {
            cfg.transcription.mode
        };

        let local_ctx = if mode == TranscriptionMode::Local {
            let model_path = ensure_ggml_model(&cfg.transcription.model, &http_client).await?;
            let path_str = model_path
                .to_str()
                .context("model path contains non-UTF-8 characters")?
                .to_owned();

            event!(
                name: "whisper.model.loading",
                Level::INFO,
                whisper.model = %cfg.transcription.model,
                file.path = %path_str,
                "loading local Whisper model '{{whisper.model}}' from {{file.path}}",
            );

            let ctx = tokio::task::spawn_blocking(move || {
                let params = WhisperContextParameters::default();
                WhisperContext::new_with_params(&path_str, params)
                    .context("whisper.cpp failed to load GGML model")
            })
            .await
            .context("model loader task panicked")??;

            event!(
                name: "whisper.model.loaded",
                Level::INFO,
                whisper.model = %cfg.transcription.model,
                "local Whisper model '{{whisper.model}}' ready in memory",
            );

            Some(Mutex::new(ctx))
        } else {
            None
        };

        let resolved_key = cfg
            .transcription
            .api_key
            .as_ref()
            .filter(|k| !k.is_empty())
            .map(|k| k.expose().to_owned())
            .or_else(|| std::env::var("STTME_API_KEY").ok())
            .or_else(|| std::env::var("GROQ_API_KEY").ok())
            .or_else(|| std::env::var("OPENAI_API_KEY").ok());

        Ok(Self {
            mode,
            model_name: cfg.transcription.model.clone(),
            language: cfg.transcription.language.clone(),
            api_url: cfg.transcription.api_url.clone(),
            api_model: cfg.transcription.api_model.clone(),
            api_key: resolved_key,
            local_ctx,
            http_client,
        })
    }

    /// Returns a human-readable identifier for the active model or API endpoint.
    #[must_use]
    pub fn model_label(&self) -> String {
        match self.mode {
            TranscriptionMode::Local => self.model_name.clone(),
            TranscriptionMode::Api => self
                .api_url
                .clone()
                .unwrap_or_else(|| "remote-api".to_owned()),
        }
    }

    /// Transcribes mono `16,000 Hz` `f32` PCM samples using the configured backend.
    ///
    /// # Errors
    ///
    /// Returns an error if local `whisper.cpp` inference or the remote HTTP request fails.
    pub async fn transcribe_pcm(
        &self,
        pcm_16k: &[f32],
        language_override: Option<&str>,
    ) -> Result<TranscriptResult> {
        let effective_lang = language_override
            .map(str::to_owned)
            .or_else(|| self.language.clone());

        match self.mode {
            TranscriptionMode::Local => self.transcribe_local(pcm_16k, effective_lang).await,
            TranscriptionMode::Api => {
                let wav_bytes = encode_wav_16k(pcm_16k)?;
                self.transcribe_api(&wav_bytes, effective_lang.as_deref())
                    .await
            }
        }
    }

    async fn transcribe_local(
        &self,
        pcm_16k: &[f32],
        language: Option<String>,
    ) -> Result<TranscriptResult> {
        let Some(ctx_mutex) = &self.local_ctx else {
            bail!("local Whisper context is not initialized");
        };

        let samples = pcm_16k.to_vec();
        let is_en_only_model = Path::new(&self.model_name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("en"));

        let mut state = {
            let guard = ctx_mutex
                .lock()
                .map_err(|_poison| anyhow::anyhow!("Whisper context mutex poisoned"))?;
            guard
                .create_state()
                .context("failed to allocate Whisper inference state")?
        };

        tokio::task::spawn_blocking(move || {
            let n_threads = std::thread::available_parallelism()
                .map_or(4, |n| (n.get() / 2).clamp(2, MAX_WHISPER_THREADS));

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_n_threads(i32::try_from(n_threads).unwrap_or(4));
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_no_context(true);
            params.set_suppress_blank(true);
            params.set_suppress_nst(true);

            let resolved_lang = if is_en_only_model {
                Some("en")
            } else {
                language.as_deref()
            };
            params.set_language(resolved_lang);

            state
                .full(params, &samples)
                .context("whisper.cpp inference failed")?;

            let mut segments = Vec::new();
            let mut full_text = String::new();

            for seg in state.as_iter() {
                if seg.no_speech_probability() > 0.85 {
                    continue;
                }
                let raw_seg = seg.to_str_lossy().unwrap_or_default();
                let cleaned = strip_whisper_markers(&raw_seg);
                if cleaned.is_empty() {
                    continue;
                }
                if !full_text.is_empty() && !full_text.ends_with(' ') && !cleaned.starts_with(' ') {
                    full_text.push(' ');
                }
                full_text.push_str(&cleaned);
                segments.push(TranscriptSegment {
                    start: (seg.start_timestamp() as f64) * 0.01,
                    end: (seg.end_timestamp() as f64) * 0.01,
                    text: cleaned,
                });
            }

            let lang_id = state.full_lang_id_from_state();
            let detected_lang = whisper_rs::get_lang_str(lang_id).unwrap_or("en").to_owned();

            Ok(TranscriptResult {
                text: full_text.trim().to_owned(),
                language: detected_lang,
                segments,
            })
        })
        .await
        .context("Whisper blocking worker panicked")?
    }

    async fn transcribe_api(
        &self,
        wav_bytes: &[u8],
        language: Option<&str>,
    ) -> Result<TranscriptResult> {
        let url = self
            .api_url
            .as_deref()
            .context("API transcription requested without `api_url`")?;

        let file_part = Part::bytes(wav_bytes.to_vec())
            .file_name("audio.wav")
            .mime_str("audio/wav")
            .context("invalid MIME type")?;

        let mut form = Form::new().part("file", file_part);
        if is_openai_compatible_url(url) {
            let model = self.api_model.as_deref().unwrap_or("whisper-1").to_owned();
            form = form.text("model", model).text("response_format", "json");
        }
        if let Some(lang) = language {
            form = form.text("language", lang.to_owned());
        }

        let mut req = self.http_client.post(url).multipart(form);
        if let Some(token) = &self.api_key {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("HTTP transcription request to {url} failed"))?
            .error_for_status()
            .context("HTTP transcription server returned error status")?;

        let body = resp
            .text()
            .await
            .context("failed to read API response body")?;
        if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&body) {
            let text = json_val
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let lang = json_val
                .get("language")
                .and_then(serde_json::Value::as_str)
                .or(language)
                .unwrap_or("en")
                .to_owned();
            return Ok(TranscriptResult {
                text,
                language: lang,
                segments: Vec::new(),
            });
        }

        Ok(TranscriptResult {
            text: body.trim().to_owned(),
            language: language.unwrap_or("en").to_owned(),
            segments: Vec::new(),
        })
    }
}

fn is_openai_compatible_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    [
        "/v1/audio/transcriptions",
        "/v1/audio/",
        "openai",
        "groq",
        "deepgram",
    ]
    .iter()
    .any(|pat| lower.contains(pat))
}

fn strip_whisper_markers(segment: &str) -> String {
    let mut cleaned = segment.trim().to_owned();
    for marker in [
        "[BLANK_AUDIO]",
        "[blank_audio]",
        "(blank audio)",
        "[Silence]",
        "[silence]",
        "[MUSIC]",
        "[Music]",
    ] {
        cleaned = cleaned.replace(marker, "");
    }
    cleaned.trim().to_owned()
}

/// Checks whether a transcription is a likely Whisper silence hallucination (port of `talktype.py`).
#[must_use]
pub fn is_hallucination(text: &str) -> bool {
    let normalized = text
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .trim()
        .to_ascii_lowercase();
    if normalized.len() < 2 {
        return true;
    }
    if HALLUCINATION_WORDS.contains(&normalized.as_str()) {
        return true;
    }
    let raw_lower = text.trim().to_ascii_lowercase();
    if raw_lower.len() < 40 {
        return HALLUCINATION_PHRASES
            .iter()
            .any(|phrase| raw_lower.contains(phrase));
    }
    false
}

/// Resolves or downloads the requested Whisper GGML model into `~/.cache/sttme/models`.
///
/// # Errors
///
/// Returns an error if the model name is unrecognized or the `HuggingFace` download fails.
pub async fn ensure_ggml_model(model: &str, client: &reqwest::Client) -> Result<PathBuf> {
    let candidate = Path::new(model);
    if candidate.is_file() {
        return Ok(candidate.to_path_buf());
    }

    let dir = models_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create models directory {}", dir.display()))?;

    let filename = model_to_ggml_filename(model)?;
    let target = dir.join(&filename);
    if target.is_file() {
        return Ok(target);
    }

    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{filename}");
    event!(
        name: "whisper.model.download.started",
        Level::INFO,
        whisper.model = %model,
        url.full = %url,
        file.path = %target.display(),
        "downloading Whisper model '{{whisper.model}}' from {{url.full}} to {{file.path}}",
    );

    let part_path = dir.join(format!("{filename}.part"));
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("HuggingFace returned error for {url}"))?;

    let mut file = tokio::fs::File::create(&part_path)
        .await
        .with_context(|| format!("failed to create {}", part_path.display()))?;

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("network error while streaming GGML model")?;
        file.write_all(&bytes)
            .await
            .context("failed to write model chunk to disk")?;
    }
    file.flush().await?;
    drop(file);

    tokio::fs::rename(&part_path, &target)
        .await
        .with_context(|| format!("failed to finalize {}", target.display()))?;

    event!(
        name: "whisper.model.download.completed",
        Level::INFO,
        whisper.model = %model,
        file.path = %target.display(),
        "downloaded Whisper model '{{whisper.model}}' to {{file.path}}",
    );
    Ok(target)
}

fn model_to_ggml_filename(model: &str) -> Result<String> {
    let normalized = model.trim().to_ascii_lowercase();
    let valid = matches!(
        normalized.as_str(),
        "tiny"
            | "tiny.en"
            | "base"
            | "base.en"
            | "small"
            | "small.en"
            | "medium"
            | "medium.en"
            | "large-v1"
            | "large-v2"
            | "large-v3"
            | "large-v3-turbo"
    );
    if !valid {
        bail!(
            "unsupported Whisper model '{model}' (expected tiny, tiny.en, base, base.en, small, small.en, medium, large-v3, or large-v3-turbo)"
        );
    }
    Ok(format!("ggml-{normalized}.bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallucination_filter_catches_common_false_positives_and_keeps_commands() {
        assert!(is_hallucination("Thanks for watching!"));
        assert!(is_hallucination("[BLANK_AUDIO]"));
        assert!(is_hallucination("uh"));
        assert!(is_hallucination("..."));

        assert!(!is_hallucination("cargo test"));
        assert!(!is_hallucination("git status"));
        assert!(!is_hallucination(
            "please refactor the authentication module"
        ));
    }
}
