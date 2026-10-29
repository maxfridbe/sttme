// Rust guideline compliant 2026-02-21
//! Persistent transcription history and pending audio store for recovery and retry.
//!
//! Mirrors `TalkType`'s two safety nets:
//! 1. `history.jsonl` keeps the last `N` completed transcriptions so `sttme recover`
//!    can re-inject text if focus was lost.
//! 2. `pending.wav` stores the raw 16 kHz PCM WAV prior to transcription (expiring after
//!    1 hour) so `sttme retry` can re-run failed API or model transcriptions without
//!    speaking again.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Maximum age of `pending.wav` before it is treated as stale and discarded.
///
/// Matches `TalkType`'s 1-hour (`3600` seconds) expiration policy so old audio from
/// an earlier session is never unexpectedly retried hours later.
const PENDING_AUDIO_MAX_AGE: Duration = Duration::from_secs(3600);

/// Trim hysteresis multiplier applied to `max_entries` before rewriting `history.jsonl`.
///
/// Waiting until the file reaches `1.5x` `max_entries` avoids rewriting the entire
/// JSONL file on every single dictation while keeping disk growth bounded.
const TRIM_THRESHOLD_NUMERATOR: usize = 3;

/// Denominator paired with [`TRIM_THRESHOLD_NUMERATOR`] (`3 / 2 == 1.5x`).
///
/// Kept as integer arithmetic to avoid floating-point conversions on entry counts.
const TRIM_THRESHOLD_DENOMINATOR: usize = 2;

/// Single JSONL record persisted to `history.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unix epoch timestamp in seconds when the transcription succeeded.
    pub unix_timestamp_secs: u64,
    /// Transcribed text prior to target-specific prefix/suffix spacing.
    pub text: String,
}

/// Manages `history.jsonl` and `pending.wav` inside `~/.cache/sttme`.
#[derive(Debug, Clone)]
pub struct HistoryStore {
    max_entries: usize,
    history_file: PathBuf,
    pending_wav: PathBuf,
    last_text: Option<String>,
}

impl HistoryStore {
    /// Creates a new [`HistoryStore`] rooted at `cache_dir` retaining up to `max_entries`.
    ///
    /// # Errors
    ///
    /// Returns an error if `cache_dir` cannot be created on disk.
    pub fn new(cache_dir: &Path, max_entries: usize) -> Result<Self> {
        fs::create_dir_all(cache_dir)
            .with_context(|| format!("failed to create cache dir {}", cache_dir.display()))?;
        Ok(Self {
            max_entries: max_entries.max(1),
            history_file: cache_dir.join("history.jsonl"),
            pending_wav: cache_dir.join("pending.wav"),
            last_text: None,
        })
    }

    /// Appends a completed transcription to memory and `history.jsonl`.
    ///
    /// # Errors
    ///
    /// Returns an error if appending to `history.jsonl` fails.
    pub fn record(&mut self, text: &str) -> Result<()> {
        let unix_timestamp_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = HistoryEntry {
            unix_timestamp_secs,
            text: text.to_owned(),
        };
        self.last_text = Some(text.to_owned());

        let mut line =
            serde_json::to_string(&entry).context("failed to serialize history entry")?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)
            .with_context(|| format!("failed to open {}", self.history_file.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("failed to append {}", self.history_file.display()))?;

        self.maybe_trim();
        Ok(())
    }

    /// Returns the most recent transcription text from memory or `history.jsonl`.
    #[must_use]
    pub fn last(&self) -> Option<String> {
        if let Some(cached) = &self.last_text {
            return Some(cached.clone());
        }
        let file = fs::File::open(&self.history_file).ok()?;
        let reader = BufReader::new(file);
        let last_line = reader.lines().map_while(Result::ok).last()?;
        let parsed: HistoryEntry = serde_json::from_str(&last_line).ok()?;
        Some(parsed.text)
    }

    /// Saves encoded WAV bytes to `pending.wav` prior to transcription.
    ///
    /// # Errors
    ///
    /// Returns an error if writing `pending.wav` fails.
    pub fn save_pending_wav(&self, wav_bytes: &[u8]) -> Result<()> {
        fs::write(&self.pending_wav, wav_bytes)
            .with_context(|| format!("failed to write {}", self.pending_wav.display()))
    }

    /// Loads `pending.wav` if present and younger than 1 hour.
    #[must_use]
    pub fn load_pending_wav(&self) -> Option<Vec<u8>> {
        let meta = fs::metadata(&self.pending_wav).ok()?;
        if let Ok(modified) = meta.modified()
            && let Ok(age) = SystemTime::now().duration_since(modified)
            && age > PENDING_AUDIO_MAX_AGE
        {
            self.clear_pending_wav();
            return None;
        }
        fs::read(&self.pending_wav).ok()
    }

    /// Removes `pending.wav` after a successful transcription or silent clip.
    pub fn clear_pending_wav(&self) {
        drop(fs::remove_file(&self.pending_wav));
    }

    fn maybe_trim(&self) {
        let Ok(file) = fs::File::open(&self.history_file) else {
            return;
        };
        let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        let threshold = (self.max_entries * TRIM_THRESHOLD_NUMERATOR) / TRIM_THRESHOLD_DENOMINATOR;
        if lines.len() > threshold {
            let keep_from = lines.len().saturating_sub(self.max_entries);
            let mut rewritten = String::new();
            for line in &lines[keep_from..] {
                rewritten.push_str(line);
                rewritten.push('\n');
            }
            drop(fs::write(&self.history_file, rewritten));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_records_recovers_and_trims() {
        let temp_dir = std::env::temp_dir().join(format!("sttme-hist-test-{}", std::process::id()));
        drop(fs::remove_dir_all(&temp_dir));

        let mut store = HistoryStore::new(&temp_dir, 2).expect("create history store");
        store.record("first phrase").expect("record 1");
        store.record("second phrase").expect("record 2");
        store.record("third phrase").expect("record 3");
        store.record("fourth phrase").expect("record 4");

        assert_eq!(store.last().as_deref(), Some("fourth phrase"));

        let reopened = HistoryStore::new(&temp_dir, 2).expect("reopen history store");
        assert_eq!(reopened.last().as_deref(), Some("fourth phrase"));

        let raw = fs::read_to_string(temp_dir.join("history.jsonl")).expect("read jsonl");
        assert!(raw.lines().count() <= 3);

        drop(fs::remove_dir_all(&temp_dir));
    }
}
