//! AtomCode data source: reads AtomCode session files under
//! `~/.atomcode/sessions/`. Two formats coexist and both are parsed:
//! - legacy `.meta` — pretty-printed whole-file JSON with
//!   `turn_stats[].model_usage[]` (real model id + per-call token breakdown);
//! - current `.json` — full session with `messages` and `turn_stats[]` carrying
//!   only an aggregate `total_tokens`. The model name lives in the System prompt
//!   text, not a structured field (see `parse_json_new`).
//!
//! ## Why `.meta`, not `.jsonl`
//!
//! AtomCode splits its usage data across two sibling files, and neither is
//! complete alone:
//!
//! - `*.meta` — a **pretty-printed whole-file JSON** (hundreds of lines, NOT
//!   JSONL). It carries `turn_stats[].model_usage[]` with the real model id,
//!   per-call `input` / `output` / `cached_input`, and the session's
//!   `working_dir`. But it has **no per-turn timestamp** — only session-level
//!   `created_at` / `updated_at`.
//! - `*.jsonl` — one JSON object per line, each with an exact epoch `ts` and a
//!   `turn_id`. But it carries **no model name** and no cache breakdown.
//!
//! So this adapter reads the `.meta` for tokens/models and joins the sibling
//! `.jsonl` on `turn_id` to recover real per-turn timestamps. Without that join
//! every turn in a long session would be stamped with the session's last-update
//! time, which buckets days of work onto a single day and wrecks the daily
//! trend.
//!
//! The multi-megabyte `*.snapshot` files in the same directory hold only
//! conversation text — zero token data — so the scanner only matches `*.meta`.
//!
//! Field mappings and edge-case handling below follow the verified spec in
//! `doc/增加AtomCode支持_完整指南.md` (its section 六):
//! - `cached_input` is a *separate* counter from `input`, not a subset of it, so
//!   it maps to `cache_read` and is safe to add to `input`.
//! - Pre-`model_usage` turns (no model name) fall back to `used_tokens`, kept
//!   under model `"unknown"` rather than dropped, so ~half the history survives.
//! - Cost is left at 0 here; the scan pipeline stamps `cost_micros` via the
//!   embedded `Pricer` from the model name, consistent with every other adapter.
//!
//! Unlike the JSONL-based adapters this one cannot reuse `scan_roots*` (those
//! walk `*.jsonl` and parse line-by-line), so the walk, fingerprint and parse
//! are spelled out here — but they use the same `fingerprint()` helper so the
//! cheap check and a full scan stay in lockstep.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json;
use walkdir::WalkDir;

use crate::core::model::{Provider, Usage};
use crate::core::usage::UsageRecord;

use super::roots::discover_roots;
use super::source::{
    fingerprint, for_each_line, ProviderConfig, ProviderError, ProviderSource, ScanOutput, ScanRoot,
};

/// Model label for legacy turns that predate `model_usage` and carry no name.
const DEFAULT_MODEL: &str = "unknown";

/// Data directory suffix under the user home (`~/.atomcode/sessions`). The walk
/// is recursive, so the `<project-hash>` layer below it needs no mention.
const SESSIONS_SUFFIX: &[&str] = &[".atomcode", "sessions"];

// ---------------------------------------------------------------------------
// `.meta` (whole-file JSON)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AtomCodeMeta {
    id: Option<String>,
    working_dir: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    #[serde(default)]
    turn_stats: Vec<AtomCodeTurnStat>,
}

#[derive(Debug, Deserialize)]
struct AtomCodeTurnStat {
    turn_id: Option<i64>,
    /// Turn-wide token counter. Used only as a fallback for pre-`model_usage`
    /// records, which carry no model name and no per-call breakdown.
    used_tokens: Option<i64>,
    #[serde(default)]
    model_usage: Vec<AtomCodeModelUsage>,
}

#[derive(Debug, Deserialize)]
struct AtomCodeModelUsage {
    model_id: Option<String>,
    tokens: Option<AtomCodeTokens>,
}

/// `cached_input` is a *separate* counter from `input`, not a subset of it.
/// Verified against real data: `tokens.input` can be ~64K while
/// `tokens.cached_input` is ~331K in the same turn, so adding them is correct.
#[derive(Debug, Deserialize)]
struct AtomCodeTokens {
    input: Option<i64>,
    output: Option<i64>,
    cached_input: Option<i64>,
}

// ---------------------------------------------------------------------------
// `.jsonl` (timestamp source)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AtomCodeJsonlLine {
    ts: Option<i64>,
    turn_id: Option<i64>,
}

/// Build a `turn_id -> start timestamp (ms)` map from the sibling `.jsonl`.
///
/// A turn can be written more than once (streaming, retries, undo), so the
/// earliest observation wins — that is when the turn actually began. A missing
/// or unreadable `.jsonl` is not an error: older sessions have none, and the
/// caller falls back to the session-level timestamp.
fn load_turn_timestamps(meta_path: &Path) -> HashMap<i64, i64> {
    let mut map: HashMap<i64, i64> = HashMap::new();
    let jsonl_path = meta_path.with_extension("jsonl");
    let _ = for_each_line(&jsonl_path, |line, _idx| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(rec) = serde_json::from_str::<AtomCodeJsonlLine>(trimmed) else {
            return;
        };
        if let (Some(turn_id), Some(ts)) = (rec.turn_id, rec.ts) {
            map.entry(turn_id)
                .and_modify(|existing| {
                    if ts < *existing {
                        *existing = ts;
                    }
                })
                .or_insert(ts);
        }
    });
    map
}

/// The current AtomCode `.json` format stamps `created_at`/`updated_at` in
/// **seconds**, whereas the legacy `.meta` used milliseconds. Normalize both to
/// millis so session timestamps land on the right day.
fn to_millis(v: Option<i64>) -> Option<i64> {
    let v = v?;
    if v < 1_000_000_000_000 {
        Some(v * 1000)
    } else {
        Some(v)
    }
}

/// Recover the model name from a new-format session's `messages`. AtomCode only
/// embeds it in the System prompt, e.g. "...running the deepseek-v4-flash
/// model." Without this, every new-format turn collapses to "unknown".
fn extract_model_from_messages(messages: Option<&Vec<serde_json::Value>>) -> Option<String> {
    let messages = messages?;
    for msg in messages {
        let is_system = msg.get("role").and_then(|r| r.as_str()) == Some("System");
        if !is_system {
            continue;
        }
        let text = match msg.get("content") {
            Some(c) if c.is_string() => c.as_str(),
            Some(c) if c.is_object() => c.get("Text").and_then(|t| t.as_str()),
            _ => None,
        };
        if let Some(t) = text {
            if let Some(m) = extract_model_from_prompt(t) {
                return Some(m);
            }
        }
    }
    None
}

/// Pull the model token out of "running the <model> model".
fn extract_model_from_prompt(text: &str) -> Option<String> {
    let needle = "running the ";
    let idx = text.find(needle)?;
    let rest = &text[idx + needle.len()..];
    let end = rest.find(" model").unwrap_or(rest.len());
    let token = rest[..end].split_whitespace().next()?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

pub struct AtomCodeSource {
    config: ProviderConfig,
    /// Discovered scan roots, computed lazily on the first scan (background
    /// thread) so WSL discovery never blocks UI startup.
    roots: OnceLock<Vec<ScanRoot>>,
}

impl AtomCodeSource {
    pub fn new(config: ProviderConfig) -> Self {
        AtomCodeSource {
            config,
            roots: OnceLock::new(),
        }
    }

    fn roots(&self) -> &[ScanRoot] {
        self.roots.get_or_init(|| {
            if let Some(dir) = &self.config.data_dir_override {
                vec![ScanRoot {
                    dir: dir.clone(),
                    label: None,
                }]
            } else {
                discover_roots(SESSIONS_SUFFIX)
            }
        })
    }

    fn existing_roots(&self) -> Vec<ScanRoot> {
        self.roots()
            .iter()
            .filter(|r| r.dir.is_dir())
            .cloned()
            .collect()
    }

    /// All AtomCode session files under a root, recursively: legacy `.meta` and
    /// current `.json`. Non-session siblings (`.snapshot`, `.jsonl`, `.ui.json`,
    /// `.rewind.json`, `.lease`) are dropped by the extension filter.
    fn meta_files(root: &ScanRoot, max_depth: usize, max_file_size: u64) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in WalkDir::new(&root.dir)
            .max_depth(max_depth)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            // AtomCode persists two session formats:
            //  - legacy `.meta` (pretty JSON with `turn_stats[].model_usage`)
            //  - current `.json` (full session with `messages` + aggregate
            //    `turn_stats[].total_tokens`). Skip the non-session siblings
            //    (`.ui.json`, `.rewind.json`, `.snapshot`, `.jsonl`, `.lease`).
            let is_meta = ext == "meta";
            let is_session_json =
                ext == "json" && !stem.ends_with(".ui") && !stem.ends_with(".rewind");
            if !is_meta && !is_session_json {
                continue;
            }
            if let Ok(meta) = fs::metadata(entry.path()) {
                if meta.len() > max_file_size {
                    continue;
                }
                out.push(entry.path().to_path_buf());
            }
        }
        out
    }

    /// Change-detection fingerprint over a root's `.meta` files. Shared by
    /// `scan` and `scan_fingerprint` so the cheap check and a full scan agree.
    fn meta_fingerprint(files: &[PathBuf]) -> String {
        let mut found = 0u64;
        let mut max_mtime = 0i64;
        let mut total_bytes = 0u64;
        for f in files {
            let Ok(meta) = fs::metadata(f) else {
                continue;
            };
            found += 1;
            total_bytes += meta.len();
            if let Ok(modified) = meta.modified() {
                if let Ok(unix) = modified.duration_since(std::time::UNIX_EPOCH) {
                    max_mtime = max_mtime.max(unix.as_secs() as i64);
                }
            }
        }
        fingerprint(found, max_mtime, total_bytes)
    }

    /// Parse one AtomCode session file (legacy `.meta` or current `.json`) into
    /// usage records.
    fn parse_session(
        path: &Path,
        root: &ScanRoot,
        emit: &mut dyn FnMut(UsageRecord),
    ) -> Result<(), String> {
        let raw = fs::read_to_string(path).map_err(|e| format!("read {:?}: {e}", path))?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("parse {:?}: {e}", path))?;

        // Route to the right parser. Prefer the rich per-call breakdown
        // (`model_usage[].tokens{input,output,cached_input}`) whenever *any*
        // turn carries one — that is the only place `cached_input` (缓存命中)
        // lives.
        //
        // Why this guard matters: the current AtomCode `.meta` (observed as
        // `"v": 1` in the wild) embeds BOTH a per-turn `total_tokens` aggregate
        // AND a `model_usage[]` array with the real token split. The old
        // heuristic keyed off `total_tokens` being present and routed such
        // files into `parse_json_new`, which only reads the aggregate and so
        // **dropped `cached_input` entirely** — cache hits were invisible. The
        // aggregate `total_tokens` also proved *not* to be the sum of the call
        // breakdown (e.g. 36857 vs input+output+cache ≈ 244k), so it must never
        // stand in for `input_tokens` when the split exists. Genuinely
        // aggregate-only files (legacy `.meta` without `model_usage`, and the
        // true new `.json` session whose model name lives only in the System
        // prompt) still fall through to `parse_json_new`.
        let has_rich_breakdown = v
            .get("turn_stats")
            .and_then(|ts| ts.as_array())
            .map(|arr| {
                arr.iter().any(|t| {
                    t.get("model_usage")
                        .and_then(|m| m.as_array())
                        .map_or(false, |a| !a.is_empty())
                })
            })
            .unwrap_or(false);
        let looks_new = !has_rich_breakdown
            && (v.get("messages").is_some()
                || v.get("turn_stats")
                    .and_then(|ts| ts.get(0))
                    .and_then(|t| t.get("total_tokens"))
                    .is_some());
        if looks_new {
            return Self::parse_json_new(&raw, &v, path, root, emit);
        }

        // Legacy `.meta` format.
        let meta: AtomCodeMeta =
            serde_json::from_str(&raw).map_err(|e| format!("parse {:?}: {e}", path))?;

        let file_mtime = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let fallback_ts = meta.updated_at.or(meta.created_at).unwrap_or(file_mtime);

        let session_id = meta
            .id
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        // Project name comes straight from the session's own `working_dir`
        // field — strictly better than reverse-engineering it from the hashed
        // directory name (`.atomcode/sessions/<hash>/` carries no readable name).
        let project = meta
            .working_dir
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .and_then(|d| Path::new(d).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        let turn_ts = load_turn_timestamps(path);

        // Namespace the dedup key per root so identically-named meta files from
        // the local home and a WSL distro never collide.
        let rel = path.strip_prefix(&root.dir).unwrap_or(path).to_path_buf();
        let rel = match &root.label {
            Some(label) => Path::new(label).join(&rel),
            None => rel,
        };

        // A `.meta` is one whole-file JSON, so no record owns a specific byte
        // range. Amortize the file size across turns (and across a turn's model
        // calls) instead of stamping every record with the full file length,
        // which would inflate `raw_bytes` by the record count.
        let per_turn_bytes = raw.len() as u64 / meta.turn_stats.len().max(1) as u64;

        for turn in &meta.turn_stats {
            let turn_id = turn.turn_id.unwrap_or(0);
            let timestamp_ms = turn_ts.get(&turn_id).copied().unwrap_or(fallback_ts);
            let started_at = Utc
                .timestamp_millis_opt(timestamp_ms)
                .single()
                .unwrap_or_else(Utc::now);

            if turn.model_usage.is_empty() {
                // Legacy turns (pre-`model_usage`) have no model name at all.
                // Keep the tokens rather than dropping them — silently losing
                // ~half the history is worse than attributing it to "unknown".
                let used = turn.used_tokens.unwrap_or(0).max(0);
                if used == 0 {
                    continue;
                }
                emit(UsageRecord::new(
                    Provider::AtomCode,
                    project.clone(),
                    session_id.clone(),
                    Usage {
                        model: DEFAULT_MODEL.to_string(),
                        started_at,
                        input_tokens: used as u64,
                        output_tokens: 0,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        cost_micros: 0, // priced by the scan pipeline
                    },
                    per_turn_bytes,
                    format!("{}:{}:legacy", rel.display(), turn_id),
                ));
                continue;
            }

            let per_call_bytes = per_turn_bytes / turn.model_usage.len() as u64;
            for (index, usage) in turn.model_usage.iter().enumerate() {
                let Some(tokens) = &usage.tokens else {
                    continue;
                };
                let input = tokens.input.unwrap_or(0).max(0);
                let output = tokens.output.unwrap_or(0).max(0);
                let cache_read = tokens.cached_input.unwrap_or(0).max(0);
                if input + output + cache_read == 0 {
                    continue;
                }

                let model = usage
                    .model_id
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string());

                emit(UsageRecord::new(
                    Provider::AtomCode,
                    project.clone(),
                    session_id.clone(),
                    Usage {
                        model,
                        started_at,
                        input_tokens: input as u64,
                        output_tokens: output as u64,
                        cache_read_tokens: cache_read as u64,
                        cache_write_tokens: 0,
                        cost_micros: 0, // priced by the scan pipeline
                    },
                    per_call_bytes,
                    format!("{}:{}:{}", rel.display(), turn_id, index),
                ));
            }
        }

        Ok(())
    }

    /// Parse the current AtomCode `.json` session format.
    ///
    /// The model name is recovered from the System prompt ("running the <model>
    /// model"); token data is only the per-turn aggregate `total_tokens`, which
    /// we record as `input_tokens` since the format exposes no input/output/
    /// cache split. Cost is left at 0 and stamped by the scan pipeline's
    /// `Pricer` from the recovered model name (deepseek-v4-flash is priced).
    fn parse_json_new(
        raw: &str,
        v: &serde_json::Value,
        path: &Path,
        root: &ScanRoot,
        emit: &mut dyn FnMut(UsageRecord),
    ) -> Result<(), String> {
        let session_id = v
            .get("id")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        let project = v
            .get("working_dir")
            .and_then(|x| x.as_str())
            .filter(|d| !d.trim().is_empty())
            .and_then(|d| Path::new(d).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        // The new format stamps `created_at`/`updated_at` in seconds; the legacy
        // `.meta` used milliseconds. `to_millis` normalizes both.
        let created = to_millis(v.get("created_at").and_then(|x| x.as_i64()));
        let updated = to_millis(v.get("updated_at").and_then(|x| x.as_i64()));
        let file_mtime = fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let fallback_ts = updated.or(created).unwrap_or(file_mtime);

        // Model name only appears inside the System prompt in this format.
        let model = extract_model_from_messages(v.get("messages").and_then(|m| m.as_array()))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let empty: Vec<serde_json::Value> = Vec::new();
        let turn_stats = v
            .get("turn_stats")
            .and_then(|t| t.as_array())
            .unwrap_or(&empty);

        let rel = path.strip_prefix(&root.dir).unwrap_or(path).to_path_buf();
        let rel = match &root.label {
            Some(label) => Path::new(label).join(&rel),
            None => rel,
        };
        let per_turn_bytes = raw.len() as u64 / turn_stats.len().max(1) as u64;

        for (index, turn) in turn_stats.iter().enumerate() {
            let total = turn
                .get("total_tokens")
                .and_then(|t| t.as_i64())
                .unwrap_or(0);
            if total <= 0 {
                continue;
            }
            let started_at = Utc
                .timestamp_millis_opt(fallback_ts)
                .single()
                .unwrap_or_else(Utc::now);

            emit(UsageRecord::new(
                Provider::AtomCode,
                project.clone(),
                session_id.clone(),
                Usage {
                    model: model.clone(),
                    started_at,
                    input_tokens: total as u64, // aggregate; format has no split
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    cost_micros: 0, // priced by the scan pipeline
                },
                per_turn_bytes,
                format!("{}:{}", rel.display(), index),
            ));
        }

        Ok(())
    }
}

impl ProviderSource for AtomCodeSource {
    fn provider(&self) -> Provider {
        Provider::AtomCode
    }

    fn data_dirs(&self) -> Result<Vec<PathBuf>, ProviderError> {
        let dirs: Vec<PathBuf> = self.existing_roots().into_iter().map(|r| r.dir).collect();
        if dirs.is_empty() {
            Err(ProviderError::DataDirNotFound(Provider::AtomCode))
        } else {
            Ok(dirs)
        }
    }

    fn scan(&self, emit: &mut dyn FnMut(UsageRecord)) -> Result<ScanOutput, ProviderError> {
        let roots = self.existing_roots();
        if roots.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::AtomCode));
        }
        let mut files: Vec<(ScanRoot, PathBuf)> = Vec::new();
        for root in &roots {
            for f in Self::meta_files(root, self.config.max_depth, self.config.max_file_size) {
                files.push((root.clone(), f));
            }
        }
        let paths: Vec<PathBuf> = files.iter().map(|(_, p)| p.clone()).collect();
        let fp = Self::meta_fingerprint(&paths);

        let mut errors = Vec::new();
        for (root, path) in &files {
            if let Err(e) = Self::parse_session(path, root, emit) {
                errors.push(e);
            }
        }

        Ok(ScanOutput {
            found_files: files.len() as u64,
            fingerprint: fp,
            errors,
            ..Default::default()
        })
    }

    fn scan_fingerprint(&self) -> Result<String, ProviderError> {
        let roots = self.existing_roots();
        if roots.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::AtomCode));
        }
        let mut paths = Vec::new();
        for root in &roots {
            paths.extend(Self::meta_files(
                root,
                self.config.max_depth,
                self.config.max_file_size,
            ));
        }
        Ok(Self::meta_fingerprint(&paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = fs::File::create(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
    }

    fn source_for(dir: &Path) -> AtomCodeSource {
        AtomCodeSource::new(ProviderConfig {
            provider: Provider::AtomCode,
            data_dir_override: Some(dir.to_path_buf()),
            ..ProviderConfig::default()
        })
    }

    fn scan_collect(src: &AtomCodeSource) -> (ScanOutput, Vec<UsageRecord>) {
        let mut records = Vec::new();
        let out = src.scan(&mut |r| records.push(r)).unwrap();
        (out, records)
    }

    /// A `.meta` with one `model_usage` turn plus a sibling `.jsonl` stamping
    /// that turn far from the session's `updated_at` — exactly the case the
    /// `turn_id` join exists to fix.
    #[test]
    fn parses_model_usage_with_jsonl_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-1");

        let meta = r#"{
  "id": "sess-1",
  "working_dir": "D:/project/dbpt",
  "created_at": 1787000000000,
  "updated_at": 1787999999999,
  "turn_stats": [
    {
      "turn_id": 1,
      "used_tokens": 27593,
      "model_usage": [
        {
          "provider_id": "AtomGit",
          "model_id": "qwen3.8-27b",
          "tokens": { "input": 64080, "output": 22518, "cached_input": 331776 }
        }
      ]
    }
  ]
}"#;
        write_file(&base.with_extension("meta"), meta);

        let jsonl = r#"{"ts":1787220850248,"session_id":"sess-1","turn_id":1}
"#;
        write_file(&base.with_extension("jsonl"), jsonl);

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(out.found_files, 1); // the sibling .jsonl is not counted
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.provider, Provider::AtomCode);
        assert_eq!(r.project, "dbpt");
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.usage.model, "qwen3.8-27b");
        // Must come from the jsonl, not from updated_at (1787999999999).
        assert_eq!(r.usage.started_at.timestamp_millis(), 1787220850248);
        assert_eq!(r.usage.input_tokens, 64080);
        assert_eq!(r.usage.output_tokens, 22518);
        // cached_input is a separate counter, mapped to cache_read.
        assert_eq!(r.usage.cache_read_tokens, 331776);
        assert_eq!(r.usage.cost_micros, 0); // priced later by the pipeline
    }

    /// The earliest `ts` for a turn wins: streaming/retries rewrite the same
    /// `turn_id`, and the turn actually began at the first observation.
    #[test]
    fn earliest_jsonl_timestamp_wins_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-6");

        write_file(
            &base.with_extension("meta"),
            r#"{
  "id": "sess-6",
  "updated_at": 1787999999999,
  "turn_stats": [
    { "turn_id": 4, "model_usage": [ { "model_id": "m", "tokens": { "input": 5 } } ] }
  ]
}"#,
        );
        write_file(
            &base.with_extension("jsonl"),
            "{\"ts\":1787300000000,\"turn_id\":4}\n{\"ts\":1787200000000,\"turn_id\":4}\n",
        );

        let (_out, records) = scan_collect(&source_for(dir.path()));
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].usage.started_at.timestamp_millis(),
            1787200000000
        );
    }

    #[test]
    fn falls_back_to_session_timestamp_without_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-2");

        let meta = r#"{
  "id": "sess-2",
  "updated_at": 1787100000000,
  "turn_stats": [
    { "turn_id": 7, "model_usage": [ { "model_id": "GLM-5.2", "tokens": { "input": 10, "output": 2 } } ] }
  ]
}"#;
        write_file(&base.with_extension("meta"), meta);

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(records.len(), 1);
        // No jsonl -> session updated_at is the fallback timestamp.
        assert_eq!(
            records[0].usage.started_at.timestamp_millis(),
            1787100000000
        );
        assert_eq!(records[0].usage.cache_read_tokens, 0);
    }

    /// Regression: the current in-the-wild AtomCode `.meta` (`"v": 1`) carries
    /// BOTH a per-turn `total_tokens` aggregate AND `model_usage[].tokens{
    /// input, output, cached_input }`. The router must take the rich split,
    /// not the aggregate — otherwise `cached_input` (缓存命中) is lost and
    /// `total_tokens` would be misread as `input_tokens` (it is ~6x smaller
    /// than input+output+cache in real data).
    #[test]
    fn rich_meta_with_total_tokens_keeps_cache_read() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("114d687823ac7f89/sess-rich");

        let meta = r#"{
  "v": 1,
  "id": "sess-rich",
  "working_dir": "E:/project/dbpt-zw",
  "created_at": 1787319280476,
  "updated_at": 1787321147793,
  "turn_count": 3,
  "turn_stats": [
    {
      "turn_id": 1,
      "total_tokens": 36857,
      "used_tokens": 34060,
      "model_usage": [
        { "provider_id": "AtomGit", "model_id": "LongCat-2.0",
          "tokens": { "input": 44206, "output": 1272, "cached_input": 170240 } }
      ]
    },
    {
      "turn_id": 2,
      "total_tokens": 40758,
      "used_tokens": 40610,
      "model_usage": [
        { "provider_id": "AtomGit", "model_id": "LongCat-2.0",
          "tokens": { "input": 5370, "output": 4331, "cached_input": 306304 } }
      ]
    }
  ]
}"#;
        write_file(&base.with_extension("meta"), meta);

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(records.len(), 2);
        for r in &records {
            assert_eq!(r.provider, Provider::AtomCode);
            assert_eq!(r.project, "dbpt-zw");
            // Model comes from `model_usage[].model_id`, not the System prompt.
            assert_eq!(r.usage.model, "LongCat-2.0");
        }
        // Cache split is preserved (the whole point of this test).
        assert_eq!(records[0].usage.cache_read_tokens, 170240);
        assert_eq!(records[0].usage.input_tokens, 44206);
        assert_eq!(records[0].usage.output_tokens, 1272);
        assert_eq!(records[1].usage.cache_read_tokens, 306304);
        // `total_tokens` must NOT leak into input_tokens.
        assert_ne!(records[0].usage.input_tokens, 36857);
    }

    #[test]
    fn legacy_turn_without_model_usage_is_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-3");

        let meta = r#"{
  "id": "sess-3",
  "updated_at": 1787100000000,
  "turn_stats": [
    { "turn_id": 1, "used_tokens": 27593, "model_usage": [] },
    { "turn_id": 2, "used_tokens": 0, "model_usage": [] }
  ]
}"#;
        write_file(&base.with_extension("meta"), meta);

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        // Turn 2 has zero tokens and is skipped; turn 1 survives as "unknown".
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].usage.model, DEFAULT_MODEL);
        assert_eq!(records[0].usage.input_tokens, 27593);
        assert!(records[0].fingerprint.ends_with(":1:legacy"));
    }

    /// A malformed `.meta` is reported in `errors` (matching `scan_roots`'s
    /// behaviour for unparsable files) but must never abort the whole scan.
    #[test]
    fn malformed_meta_is_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-4");
        write_file(&base.with_extension("meta"), "{ this is not json");

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert_eq!(out.found_files, 1);
        assert_eq!(out.errors.len(), 1);
        assert!(records.is_empty());
    }

    /// `.snapshot` holds only conversation text; token-looking numbers in it
    /// must never be scanned.
    #[test]
    fn ignores_snapshot_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-5");
        write_file(
            &base.with_extension("snapshot"),
            r#"{"model":"x","input":9999,"output":9999}"#,
        );

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(out.found_files, 0);
        assert!(records.is_empty());
    }

    /// The current `.json` session format: model comes from the System prompt,
    /// tokens are an aggregate `total_tokens` (no input/output/cache split), and
    /// `created_at`/`updated_at` are in seconds.
    #[test]
    fn parses_new_json_format_model_from_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("48d39d236dcc56e0/sess-json");

        let json = r#"{
  "id": "sess-json",
  "working_dir": "E:/project/dbpt-dw",
  "created_at": 1784268744,
  "updated_at": 1784268768,
  "messages": [
    { "role": "System", "content": { "Text": "You are AtomCode, an AI coding agent by AtomGit running the deepseek-v4-flash model. Never claim to be another model." } },
    { "role": "User", "content": { "Text": "hi" } }
  ],
  "turn_stats": [
    { "turn_count": 1, "total_tokens": 12345, "errored": false },
    { "turn_count": 2, "total_tokens": 0, "errored": true },
    { "turn_count": 3, "total_tokens": 678 }
  ]
}"#;
        write_file(&base.with_extension("json"), json);

        let (_out, records) = scan_collect(&source_for(dir.path()));
        // The zero-token (errored) turn is skipped; two remain.
        assert_eq!(records.len(), 2);
        for r in &records {
            assert_eq!(r.provider, Provider::AtomCode);
            assert_eq!(r.project, "dbpt-dw");
            assert_eq!(r.session_id, "sess-json");
            // Recovered from the System prompt, not collapsed to "unknown".
            assert_eq!(r.usage.model, "deepseek-v4-flash");
        }
        // total_tokens lands in input_tokens (no split available in this format).
        assert_eq!(records[0].usage.input_tokens, 12345);
        assert_eq!(records[1].usage.input_tokens, 678);
        // seconds-based updated_at -> milliseconds timestamp (1784268768 * 1000).
        assert_eq!(
            records[0].usage.started_at.timestamp_millis(),
            1784268768 * 1000
        );
    }

    #[test]
    fn missing_data_dir_is_reported() {
        let src = AtomCodeSource::new(ProviderConfig {
            provider: Provider::AtomCode,
            data_dir_override: Some(PathBuf::from("/nonexistent/atomcode/sessions")),
            ..ProviderConfig::default()
        });
        assert!(matches!(
            src.data_dirs().unwrap_err(),
            ProviderError::DataDirNotFound(Provider::AtomCode)
        ));
    }
}
