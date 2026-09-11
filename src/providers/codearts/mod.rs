//! CodeArts (华为云码道 / CodeArts 代码智能体) data source.
//!
//! ## Where the usage lives
//!
//! CodeArts persists every session/message in a single **SQLite** database
//! rather than the JSONL/JSON files the other adapters walk. On this machine
//! the live database is at:
//!
//! ```text
//! %USERPROFILE%/.local/share/.codeartsdoer/opencode.db
//! ```
//!
//! (Some docs reference `~/.codeartsdoer/codearts-data/opencode.db`; the
//! adapter probes several candidate locations so either layout works.)
//!
//! ## Schema (verified against the real DB)
//!
//! - `session(id, project_id, directory, title, time_created, time_updated)`
//!   — `directory` is the absolute project path; its last component is the
//!   project name we surface.
//! - `message(id, session_id, time_created, time_updated, data)`
//!   — `data` is a JSON blob. Assistant messages that completed a model call
//!   carry `tokens`:
//!
//!   ```json
//!   {
//!     "role": "assistant",
//!     "modelID": "deepseek-v4-pro",
//!     "providerID": "inferhub-provider",
//!     "tokens": { "total": 24605, "input": 24291, "output": 314,
//!                 "reasoning": 0, "cache": { "write": 0, "read": 0 },
//!                 "context": { "...": 0 } },
//!     "time": { "created": 1787880271126, "completed": 1787880396842 },
//!     "cost": 0
//!   }
//!   ```
//!
//! User messages also carry a `model` but **no** `tokens`, so token-bearing
//! records are exactly the assistant messages with a `tokens` object. Token
//! fields are all separate counters (cache read/write are NOT subsets of
//! input), so they map 1:1 onto `Usage` and may be summed freely.
//!
//! `cost` is left at 0 here; the scan pipeline stamps `cost_micros` from the
//! embedded `Pricer` keyed on the model name, consistent with every other
//! adapter.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::core::model::{Provider, Usage};
use crate::core::usage::UsageRecord;

use super::source::{fingerprint, ProviderConfig, ProviderError, ProviderSource, ScanOutput};

/// Candidate database paths, relative to the user home dir, in priority order.
/// Covers the observed Windows layout plus the documented Linux/macOS layout.
const DB_REL_PATHS: &[&str] = &[
    ".local/share/.codeartsdoer/opencode.db",
    ".codeartsdoer/codearts-data/opencode.db",
    ".local/state/codeartsdoer/opencode.db",
    ".codeartsdoer/opencode.db",
];

pub struct CodeArtsSource {
    config: ProviderConfig,
    /// Discovered opencode.db paths, computed lazily on the first scan so home
    /// dir probing never blocks UI startup.
    db_paths: OnceLock<Vec<PathBuf>>,
}

impl CodeArtsSource {
    pub fn new(config: ProviderConfig) -> Self {
        CodeArtsSource {
            config,
            db_paths: OnceLock::new(),
        }
    }

    fn db_paths(&self) -> &[PathBuf] {
        self.db_paths.get_or_init(|| {
            if let Some(dir) = &self.config.data_dir_override {
                // An explicit override is treated as the directory holding
                // (or expected to hold) opencode.db.
                let candidate = dir.join("opencode.db");
                return if candidate.is_file() {
                    vec![candidate]
                } else {
                    vec![dir.clone()]
                };
            }
            let mut found = Vec::new();
            if let Some(home) = dirs::home_dir() {
                for rel in DB_REL_PATHS {
                    let p = home.join(rel);
                    if p.is_file() {
                        found.push(p);
                    }
                }
            }
            found
        })
    }

    fn existing_db_paths(&self) -> Vec<PathBuf> {
        self.db_paths()
            .iter()
            .filter(|p| p.is_file())
            .cloned()
            .collect()
    }

    /// Cheap change detector over the db file(s): file count + newest mtime +
    /// total size. Mirrors the `fingerprint()` helper other adapters use so
    /// the scheduler skips rescanning an untouched database.
    fn db_fingerprint(paths: &[PathBuf]) -> String {
        let mut found = 0u64;
        let mut max_mtime = 0i64;
        let mut total_bytes = 0u64;
        for p in paths {
            if let Ok(meta) = std::fs::metadata(p) {
                found += 1;
                total_bytes += meta.len();
                if let Ok(modified) = meta.modified() {
                    if let Ok(unix) = modified.duration_since(std::time::UNIX_EPOCH) {
                        max_mtime = max_mtime.max(unix.as_secs() as i64);
                    }
                }
            }
        }
        fingerprint(found, max_mtime, total_bytes)
    }

    /// Parse one assistant message's `data` JSON into a usage record. Returns
    /// `None` for user messages, in-flight assistant messages (no `tokens`), or
    /// malformed blobs — those are simply skipped, never fatal.
    fn parse_message(
        session_id: &str,
        project: &str,
        message_id: &str,
        data: &str,
        raw_bytes: u64,
    ) -> Option<UsageRecord> {
        let v: Value = serde_json::from_str(data).ok()?;
        let obj = v.as_object()?;

        // Token-bearing records are assistant messages with a `tokens` object.
        let tokens = obj.get("tokens")?.as_object()?;
        let input = json_i64(tokens.get("input")).max(0) as u64;
        let output = json_i64(tokens.get("output")).max(0) as u64;
        let cache = tokens.get("cache").and_then(|c| c.as_object());
        let cache_read = cache
            .and_then(|c| Some(json_i64_opt(c.get("read"))))
            .unwrap_or(0)
            .max(0) as u64;
        let cache_write = cache
            .and_then(|c| Some(json_i64_opt(c.get("write"))))
            .unwrap_or(0)
            .max(0) as u64;
        if input + output + cache_read + cache_write == 0 {
            return None;
        }

        // Model id: top-level `modelID`, else nested `model.modelID`.
        let model = obj
            .get("modelID")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .or_else(|| {
                obj.get("model")
                    .and_then(|m| m.get("modelID"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_string());

        // Timestamp: prefer `time.created` (ms) inside the JSON; fall back to
        // the row's `time_created` column (also ms).
        let ts_ms = obj
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|t| t.as_i64())
            .or_else(|| obj.get("time_created").and_then(|t| t.as_i64()))
            .unwrap_or(0);
        let started_at = Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(Utc::now);

        Some(UsageRecord::new(
            Provider::CodeArts,
            project.to_string(),
            session_id.to_string(),
            Usage {
                model,
                started_at,
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                cost_micros: 0, // priced by the scan pipeline
            },
            raw_bytes,
            format!("message:{message_id}"),
        ))
    }

    /// Scan one database file, emitting every token-bearing assistant message.
    fn scan_db(path: &Path, emit: &mut dyn FnMut(UsageRecord), errors: &mut Vec<String>) -> u64 {
        let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("open {:?}: {e}", path));
                return 0;
            }
        };

        // Join message -> session so we can resolve the project directory.
        // `directory` is the project path; its last component is the name.
        let sql = "\
            SELECT m.id, m.session_id, m.data, s.directory \
            FROM message m \
            JOIN session s ON s.id = m.session_id";

        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("prepare {:?}: {e}", path));
                return 0;
            }
        };

        let mut found = 0u64;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?, // message id
                row.get::<_, String>(1)?, // session id
                row.get::<_, String>(2)?, // data json
                row.get::<_, String>(3)?, // session directory
            ))
        });

        let Ok(rows) = rows else {
            errors.push(format!("query {:?}: failed", path));
            return 0;
        };

        for r in rows.flatten() {
            let (msg_id, session_id, data, directory) = r;
            let project = project_name(&directory);
            let raw_bytes = data.len() as u64;
            if let Some(rec) = Self::parse_message(&session_id, &project, &msg_id, &data, raw_bytes)
            {
                emit(rec);
                found += 1;
            }
        }
        found
    }
}

/// Last path component of a project directory, or "unknown" when absent.
fn project_name(directory: &str) -> String {
    if directory.trim().is_empty() {
        return "unknown".to_string();
    }
    Path::new(directory)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn json_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(val) => val
            .as_i64()
            .or_else(|| val.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        None => 0,
    }
}

fn json_i64_opt(v: Option<&Value>) -> i64 {
    json_i64(v)
}

impl ProviderSource for CodeArtsSource {
    fn provider(&self) -> Provider {
        Provider::CodeArts
    }

    fn data_dirs(&self) -> Result<Vec<PathBuf>, ProviderError> {
        let paths = self.existing_db_paths();
        if paths.is_empty() {
            Err(ProviderError::DataDirNotFound(Provider::CodeArts))
        } else {
            Ok(paths)
        }
    }

    fn scan(&self, emit: &mut dyn FnMut(UsageRecord)) -> Result<ScanOutput, ProviderError> {
        let paths = self.existing_db_paths();
        if paths.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::CodeArts));
        }
        let mut errors = Vec::new();
        let mut found_files = 0u64;
        for p in &paths {
            found_files += Self::scan_db(p, emit, &mut errors);
        }
        Ok(ScanOutput {
            found_files,
            fingerprint: Self::db_fingerprint(&paths),
            errors,
            ..Default::default()
        })
    }

    fn scan_fingerprint(&self) -> Result<String, ProviderError> {
        let paths = self.existing_db_paths();
        if paths.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::CodeArts));
        }
        Ok(Self::db_fingerprint(&paths))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn write_db(path: &Path, messages: &[(&str, &str, &str, &str)]) {
        // (message_id, session_id, directory, data_json)
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, directory TEXT); \
             CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session(id, project_id, directory) VALUES ('ses_1', 'p1', 'E:/project/dbpt-zw')",
            [],
        )
        .unwrap();
        for (mid, sid, dir, data) in messages {
            conn.execute(
                "INSERT INTO message(id, session_id, time_created, data) VALUES (?,?,?,?)",
                params![mid, sid, 1787880271126i64, data],
            )
            .unwrap();
            let _ = dir;
        }
    }

    fn source_for(dir: &Path) -> CodeArtsSource {
        CodeArtsSource::new(ProviderConfig {
            provider: Provider::CodeArts,
            data_dir_override: Some(dir.to_path_buf()),
            ..ProviderConfig::default()
        })
    }

    fn scan_collect(src: &CodeArtsSource) -> (ScanOutput, Vec<UsageRecord>) {
        let mut records = Vec::new();
        let out = src.scan(&mut |r| records.push(r)).unwrap();
        (out, records)
    }

    #[test]
    fn parses_assistant_tokens_and_skips_user() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        write_db(
            &db,
            &[
                (
                    "msg_user",
                    "ses_1",
                    "",
                    r#"{"role":"user","modelID":"deepseek-v4-pro","summary":{"diffs":[]}}"#,
                ),
                (
                    "msg_asst",
                    "ses_1",
                    "",
                    r#"{"role":"assistant","modelID":"deepseek-v4-pro","providerID":"inferhub-provider","tokens":{"total":24605,"input":24291,"output":314,"reasoning":0,"cache":{"write":0,"read":42}},"time":{"created":1787880271126,"completed":1787880396842},"cost":0}"#,
                ),
            ],
        );

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        // Only the assistant message with `tokens` yields a record.
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.provider, Provider::CodeArts);
        assert_eq!(r.project, "dbpt-zw");
        assert_eq!(r.session_id, "ses_1");
        assert_eq!(r.usage.model, "deepseek-v4-pro");
        assert_eq!(r.usage.input_tokens, 24291);
        assert_eq!(r.usage.output_tokens, 314);
        assert_eq!(r.usage.cache_read_tokens, 42);
        assert_eq!(r.usage.cache_write_tokens, 0);
        assert_eq!(r.usage.started_at.timestamp_millis(), 1787880271126);
        assert!(r.fingerprint.starts_with("message:msg_asst"));
    }

    #[test]
    fn zero_token_message_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        write_db(
            &db,
            &[(
                "msg_asst_zero",
                "ses_1",
                "",
                r#"{"role":"assistant","modelID":"x","tokens":{"input":0,"output":0,"cache":{"read":0,"write":0}}}"#,
            )],
        );
        let (_out, records) = scan_collect(&source_for(dir.path()));
        assert!(records.is_empty());
    }

    #[test]
    fn missing_db_reports_not_found() {
        let src = CodeArtsSource::new(ProviderConfig {
            provider: Provider::CodeArts,
            data_dir_override: Some(PathBuf::from("/nonexistent/codearts")),
            ..ProviderConfig::default()
        });
        assert!(matches!(
            src.data_dirs().unwrap_err(),
            ProviderError::DataDirNotFound(Provider::CodeArts)
        ));
    }
}
