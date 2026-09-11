//! AtomCode CLI session parser
//!
//! Parses `.meta` files from `~/.atomcode/sessions/{projectHash}/*.meta`
//!
//! ## Why this one is different
//!
//! AtomCode splits its usage data across two files, and neither is complete alone:
//!
//! - `*.meta`  — pretty-printed JSON (NOT JSONL). Carries `turn_stats[].model_usage[]`
//!   with the real model id, per-call `input` / `output` / `cached_input`, plus
//!   `pricing` and `ctx_window`. But it has **no per-turn timestamp** — only
//!   session-level `created_at` / `updated_at`.
//! - `*.jsonl` — one JSON object per line, each with an exact epoch `ts` and a
//!   `turn_id`. But it carries **no model name** and no cache breakdown.
//!
//! So the parser reads the `.meta` for tokens/models and joins the sibling
//! `.jsonl` on `turn_id` to recover real timestamps. Without that join every
//! turn in a long session would be stamped with the session's last-update time,
//! which buckets days of work onto a single day.
//!
//! Also worth knowing: the multi-megabyte `*.snapshot` files in the same
//! directory hold only conversation text — they contain zero token data. Do not
//! point a scanner at them.

use super::utils::{file_modified_timestamp_ms, parse_timestamp_str};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

const DEFAULT_MODEL: &str = "unknown";
const DEFAULT_PROVIDER: &str = "atomcode";

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
    /// Session-wide token counter for this turn. Used only as a fallback for
    /// pre-0.9 records, which predate `model_usage` and carry no model name.
    used_tokens: Option<i64>,
    #[serde(default)]
    model_usage: Vec<AtomCodeModelUsage>,
}

#[derive(Debug, Deserialize)]
struct AtomCodeModelUsage {
    provider_id: Option<String>,
    model_id: Option<String>,
    tokens: Option<AtomCodeTokens>,
}

/// `cached_input` is a *separate* counter from `input`, not a subset of it.
/// Verified against real data: tokens.input can be ~64K while
/// tokens.cached_input is ~331K in the same turn, so adding them is correct.
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
/// earliest observation wins — that is when the turn actually began.
fn load_turn_timestamps(meta_path: &Path) -> HashMap<i64, i64> {
    let mut map: HashMap<i64, i64> = HashMap::new();
    let jsonl_path = meta_path.with_extension("jsonl");
    if !jsonl_path.exists() {
        return map;
    }

    let file = match fs::File::open(&jsonl_path) {
        Ok(f) => f,
        Err(_) => return map,
    };

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Plain serde_json here: these lines are small and this runs once per
        // session, so simd_json's speedup is not worth the mutable-buffer dance.
        if let Ok(rec) = serde_json::from_str::<AtomCodeJsonlLine>(trimmed) {
            if let (Some(turn_id), Some(ts)) = (rec.turn_id, rec.ts) {
                map.entry(turn_id)
                    .and_modify(|existing| {
                        if ts < *existing {
                            *existing = ts;
                        }
                    })
                    .or_insert(ts);
            }
        }
    }

    map
}

/// Resolve the workspace from the session's own `working_dir` field.
///
/// AtomCode records the absolute project path in the `.meta`, which is strictly
/// better than reverse-engineering it from the directory name: sessions live
/// under a hashed folder (`.atomcode/sessions/<hash>/`) that carries no
/// readable project name.
fn atomcode_workspace(meta: &AtomCodeMeta) -> (Option<String>, Option<String>) {
    match meta.working_dir.as_deref() {
        Some(dir) if !dir.trim().is_empty() => {
            let key = normalize_workspace_key(dir);
            let label = key.as_deref().and_then(workspace_label_from_key);
            (key, label)
        }
        _ => (None, None),
    }
}

/// Parse one AtomCode `.meta` session file.
pub fn parse_atomcode_file(path: &Path) -> Vec<UnifiedMessage> {
    // Whole-file JSON, not JSONL — `.meta` is pretty-printed across hundreds
    // of lines, so line-oriented readers would see fragments, not records.
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let meta: AtomCodeMeta = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };

    let file_mtime = file_modified_timestamp_ms(path);
    let fallback_ts = meta
        .updated_at
        .or(meta.created_at)
        .unwrap_or(file_mtime);

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

    let (workspace_key, workspace_label) = atomcode_workspace(&meta);
    let turn_ts = load_turn_timestamps(path);

    let mut messages: Vec<UnifiedMessage> = Vec::new();

    for turn in &meta.turn_stats {
        let turn_id = turn.turn_id.unwrap_or(0);
        let timestamp_ms = turn_ts.get(&turn_id).copied().unwrap_or(fallback_ts);

        if turn.model_usage.is_empty() {
            // Legacy turns (pre-`model_usage`) have no model name at all. Keep
            // the tokens rather than dropping them — silently losing ~half the
            // history is worse than attributing it to "unknown".
            let used = turn.used_tokens.unwrap_or(0).max(0);
            if used == 0 {
                continue;
            }

            let dedup_key = Some(format!("atomcode:{session_id}:{turn_id}:legacy"));
            let mut unified = UnifiedMessage::new_with_dedup(
                "atomcode",
                DEFAULT_MODEL,
                DEFAULT_PROVIDER,
                session_id.clone(),
                timestamp_ms,
                TokenBreakdown {
                    input: used,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0, // Cost is resolved later by the pricing resolver
                dedup_key,
            );
            unified.set_workspace(workspace_key.clone(), workspace_label.clone());
            messages.push(unified);
            continue;
        }

        for (index, usage) in turn.model_usage.iter().enumerate() {
            let tokens = match &usage.tokens {
                Some(t) => t,
                None => continue,
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

            // Prefer the vendor's own provider id when present, so multi-vendor
            // setups (e.g. a self-hosted gateway) are not flattened to "atomcode".
            let provider = usage
                .provider_id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_PROVIDER.to_string());

            let dedup_key = Some(format!("atomcode:{session_id}:{turn_id}:{index}"));

            let mut unified = UnifiedMessage::new_with_dedup(
                "atomcode",
                model,
                provider,
                session_id.clone(),
                timestamp_ms,
                TokenBreakdown {
                    input,
                    output,
                    cache_read,
                    cache_write: 0,
                    reasoning: 0,
                },
                0.0,
                dedup_key,
            );
            unified.set_workspace(workspace_key.clone(), workspace_label.clone());
            messages.push(unified);
        }
    }

    messages
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

    #[test]
    fn parses_model_usage_with_jsonl_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-1");

        let meta = r#"{
  "v": 1,
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

        // The jsonl stamps turn 1 at a time far from the session's updated_at,
        // which is exactly the case the join exists to fix.
        let jsonl = r#"{"v":1,"ts":1787220850248,"iso":"2026-08-20T10:14:10.248+00:00","session_id":"sess-1","turn_id":1,"usage":{"prompt":100,"completion":20,"cached":5}}
"#;
        write_file(&base.with_extension("jsonl"), jsonl);

        let messages = parse_atomcode_file(&base.with_extension("meta"));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "atomcode");
        assert_eq!(messages[0].model_id, "qwen3.8-27b");
        assert_eq!(messages[0].provider_id, "AtomGit");
        assert_eq!(messages[0].session_id, "sess-1");
        // Must come from the jsonl, not from updated_at (1787999999999).
        assert_eq!(messages[0].timestamp, 1787220850248);
        assert_eq!(messages[0].tokens.input, 64080);
        assert_eq!(messages[0].tokens.output, 22518);
        assert_eq!(messages[0].tokens.cache_read, 331776);
        // cached_input is additive with input, not a subset of it.
        assert_eq!(
            messages[0].tokens.input + messages[0].tokens.cache_read,
            64080 + 331776
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

        let messages = parse_atomcode_file(&base.with_extension("meta"));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1787100000000);
        // No provider_id in the record -> default provider.
        assert_eq!(messages[0].provider_id, "atomcode");
        assert_eq!(messages[0].tokens.cache_read, 0);
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

        let messages = parse_atomcode_file(&base.with_extension("meta"));

        // Turn 2 has zero tokens and must be skipped; turn 1 must survive.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "unknown");
        assert_eq!(messages[0].tokens.input, 27593);
    }

    #[test]
    fn malformed_meta_yields_empty_vec() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("abc123/sess-4");
        write_file(&base.with_extension("meta"), "{ this is not json");

        assert!(parse_atomcode_file(&base.with_extension("meta")).is_empty());
    }
}
