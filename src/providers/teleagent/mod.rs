//! TeleAgent data source: reads the SQLite store at
//! `~/.local/share/TeleAgent/users/<user_id>/teleagent.db` (one store per user
//! directory; a `teleagent.db` sitting directly under `TeleAgent/` is also
//! picked up). See `doc/teleagent-token-parsing.md` for the data-source survey
//! this adapter implements.
//!
//! TeleAgent is an Electron app whose usage data lives in a single SQLite
//! database per user: every message is one `message` row and the token counts
//! ride inside the `data` JSON column — there is no dedicated column. Only
//! assistant messages carry `tokens`; user/tool messages are skipped, and
//! zero-token assistant rows (placeholders, interrupted calls with
//! `finish: null`) are dropped so they cannot pollute the aggregates. Each
//! remaining row emits one `UsageRecord`, keyed for dedup by the row's
//! primary-key `message.id` — stable across rescans, exactly like the
//! OpenCode adapter's `msg_<id>` keys.
//!
//! The app holds its store open while running, so the connection is opened
//! read-only (falling back to `query_only` when the WAL sidecars make a
//! genuinely read-only open impossible) and waits out transient
//! `SQLITE_BUSY` instead of failing the scan: TeleAgent's own writes always
//! win, TokenMonitor just waits its turn.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::core::model::{Provider, Usage};
use crate::core::usage::UsageRecord;

use super::roots::discover_roots;
use super::source::{
    fingerprint, ProviderConfig, ProviderError, ProviderSource, ScanOutput, ScanRoot,
};

/// SQLite store file name inside each user directory.
const DB_FILE: &str = "teleagent.db";

/// Minimal view of a `message` row's `data` JSON. Only the fields aggregated
/// here are named; everything else — the full prompt and tool-call payloads —
/// is skipped by serde without being allocated.
#[derive(Deserialize)]
struct MessageData {
    role: Option<String>,
    tokens: Option<Tokens>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    time: Option<TimeData>,
}

#[derive(Deserialize)]
struct Tokens {
    input: Option<i64>,
    output: Option<i64>,
    /// TeleAgent reports reasoning tokens separately from input; TokenMonitor
    /// has no reasoning bucket, so they are folded into input (the Anthropic
    /// convention, matching the OpenCode adapter) to keep `total_tokens()`
    /// aligned with the sum the app itself reports.
    reasoning: Option<i64>,
    cache: Option<Cache>,
}

#[derive(Deserialize)]
struct Cache {
    read: Option<i64>,
    write: Option<i64>,
}

#[derive(Deserialize)]
struct TimeData {
    created: Option<i64>,
}

pub struct TeleAgentSource {
    config: ProviderConfig,
    /// Discovered scan roots, computed lazily on the first scan (background
    /// thread) so the two-level directory walk never blocks UI startup.
    roots: OnceLock<Vec<ScanRoot>>,
}

impl TeleAgentSource {
    pub fn new(config: ProviderConfig) -> Self {
        TeleAgentSource {
            config,
            roots: OnceLock::new(),
        }
    }

    /// Expand one candidate directory into the roots that actually hold a
    /// store: the directory itself when it directly contains `teleagent.db`,
    /// plus each one-level subdirectory that does (the documented per-user
    /// layout `users/<user_id>/teleagent.db`).
    ///
    /// A store found in a subdirectory gets that directory's name (joined onto
    /// the parent's label, if any) as its root label, so the same message id
    /// in two user stores can never share a dedup fingerprint.
    fn expand_roots(root: ScanRoot) -> Vec<ScanRoot> {
        let mut out = Vec::new();
        if root.dir.join(DB_FILE).is_file() {
            out.push(root.clone());
        }
        let Ok(entries) = std::fs::read_dir(&root.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dir = entry.path();
            if !dir.join(DB_FILE).is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let label = match &root.label {
                Some(parent) => Path::new(parent).join(&name).to_string_lossy().into_owned(),
                None => name,
            };
            out.push(ScanRoot {
                dir,
                label: Some(label),
            });
        }
        out
    }

    fn roots(&self) -> &[ScanRoot] {
        self.roots.get_or_init(|| {
            if let Some(dir) = &self.config.data_dir_override {
                return Self::expand_roots(ScanRoot {
                    dir: dir.clone(),
                    label: None,
                });
            }
            let mut roots = Vec::new();
            // The documented layout: users/<user_id>/teleagent.db.
            for r in discover_roots(&[".local", "share", "TeleAgent", "users"]) {
                roots.extend(Self::expand_roots(r));
            }
            // A store sitting directly under TeleAgent/ (no users layer).
            for r in discover_roots(&[".local", "share", "TeleAgent"]) {
                if r.dir.join(DB_FILE).is_file() {
                    roots.push(r);
                }
            }
            roots
        })
    }

    fn existing_roots(&self) -> Vec<ScanRoot> {
        self.roots()
            .iter()
            .filter(|r| r.dir.join(DB_FILE).is_file())
            .cloned()
            .collect()
    }

    /// Open the store without writing to it. Prefers a genuinely read-only
    /// connection so TokenMonitor never contends with TeleAgent's live
    /// database; when that fails (a WAL store whose `-shm` is missing, which
    /// read-only SQLite refuses), falls back to a `query_only` connection.
    /// The busy timeout is the `SQLITE_BUSY` tolerance the data-source survey
    /// calls for: TeleAgent's writes win, the scan waits instead of failing.
    fn open_db(path: &Path) -> Result<Connection, String> {
        let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(_) => {
                let c = Connection::open(path).map_err(|e| format!("open {path:?}: {e}"))?;
                c.pragma_update(None, "query_only", "ON")
                    .map_err(|e| format!("query_only {path:?}: {e}"))?;
                c
            }
        };
        conn.busy_timeout(Duration::from_secs(2))
            .map_err(|e| format!("busy_timeout {path:?}: {e}"))?;
        Ok(conn)
    }

    /// Stream one record per assistant message with a non-zero token count.
    ///
    /// The session title is joined in so the record's project dimension shows
    /// the conversation's own name; a store without the `session` table (or
    /// with an unexpected shape) falls back to a titleless scan instead of
    /// failing the whole pass.
    fn scan_db(
        conn: &Connection,
        root: &ScanRoot,
        emit: &mut dyn FnMut(UsageRecord),
    ) -> Result<(), String> {
        let with_title = "SELECT m.id, m.session_id, m.time_created, m.data, s.title \
                          FROM message m LEFT JOIN \"session\" s ON s.id = m.session_id \
                          ORDER BY m.id";
        let without_title =
            "SELECT id, session_id, time_created, data, NULL FROM message ORDER BY id";
        let query = match conn.prepare(with_title) {
            Ok(_) => with_title,
            Err(_) => without_title,
        };
        let mut stmt = conn
            .prepare(query)
            .map_err(|e| format!("prepare message query: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(|e| format!("query message: {e}"))?;
        for row in rows {
            let (id, session_id, created_ms, data, title) = row.map_err(|e| format!("row: {e}"))?;
            if let Some(r) =
                Self::record_from_message(&id, &session_id, created_ms, &data, title, root)
            {
                emit(r);
            }
        }
        Ok(())
    }

    fn record_from_message(
        id: &str,
        session_id: &str,
        created_ms: i64,
        data: &str,
        title: Option<String>,
        root: &ScanRoot,
    ) -> Option<UsageRecord> {
        let d: MessageData = serde_json::from_str(data).ok()?;
        if d.role.as_deref() != Some("assistant") {
            return None;
        }
        let tokens = d.tokens?;
        let cache = tokens.cache.as_ref();
        let reasoning = tokens.reasoning.unwrap_or(0).max(0);
        let input = tokens.input.unwrap_or(0).max(0) + reasoning;
        let output = tokens.output.unwrap_or(0).max(0);
        let cache_read = cache.and_then(|c| c.read).unwrap_or(0).max(0);
        let cache_write = cache.and_then(|c| c.write).unwrap_or(0).max(0);
        // Zero across every bucket: a placeholder row (sample conversation) or
        // an interrupted call (`finish: null`) — excluded so the aggregates
        // count only real exchanges.
        if input + output + cache_read + cache_write == 0 {
            return None;
        }

        let started_at = d
            .time
            .and_then(|t| t.created)
            .or(Some(created_ms))
            .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
            .unwrap_or_else(Utc::now);
        // The conversation title is the only human-readable grouping the store
        // offers (the data JSON carries no working directory), so it maps to
        // the record's project dimension; untitled sessions fall back to a
        // stable placeholder.
        let project = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        let model = d.model_id.unwrap_or_default();

        // Namespace the dedup key per root so identically-named store files
        // from the local home and WSL distros never collide.
        let rel = match &root.label {
            Some(label) => Path::new(label).join(DB_FILE),
            None => Path::new(DB_FILE).to_path_buf(),
        };

        Some(UsageRecord::new(
            Provider::TeleAgent,
            project,
            session_id.to_string(),
            Usage {
                model,
                started_at,
                input_tokens: input as u64,
                output_tokens: output as u64,
                cache_read_tokens: cache_read as u64,
                cache_write_tokens: cache_write as u64,
                cost_micros: 0, // `cost` in the store is locally always 0
            },
            data.len() as u64,
            format!("{}:{id}", rel.display()),
        ))
    }

    /// Change-detection stats over one root's store file (plus its WAL
    /// sidecars). Shared by `scan` and `scan_fingerprint` so the cheap check
    /// and a full scan always agree.
    fn store_stats(root: &ScanRoot) -> (u64, i64, u64) {
        let mut found = 0u64;
        let mut max_mtime = 0i64;
        let mut total_bytes = 0u64;
        let db = root.dir.join(DB_FILE);
        for f in [
            &db,
            &db.with_extension("db-wal"),
            &db.with_extension("db-shm"),
        ] {
            let Ok(meta) = std::fs::metadata(f) else {
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
        (found, max_mtime, total_bytes)
    }
}

impl ProviderSource for TeleAgentSource {
    fn provider(&self) -> Provider {
        Provider::TeleAgent
    }

    fn data_dirs(&self) -> Result<Vec<PathBuf>, ProviderError> {
        let dirs: Vec<PathBuf> = self.existing_roots().into_iter().map(|r| r.dir).collect();
        if dirs.is_empty() {
            Err(ProviderError::DataDirNotFound(Provider::TeleAgent))
        } else {
            Ok(dirs)
        }
    }

    fn scan(&self, emit: &mut dyn FnMut(UsageRecord)) -> Result<ScanOutput, ProviderError> {
        let roots = self.existing_roots();
        if roots.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::TeleAgent));
        }
        let mut errors = Vec::new();
        let mut found_files = 0u64;
        let mut max_mtime = 0i64;
        let mut total_bytes = 0u64;
        for root in &roots {
            let (found, mtime, bytes) = Self::store_stats(root);
            found_files += found;
            max_mtime = max_mtime.max(mtime);
            total_bytes += bytes;

            let db = root.dir.join(DB_FILE);
            let conn = match Self::open_db(&db) {
                Ok(c) => c,
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            };
            if let Err(e) = Self::scan_db(&conn, root, emit) {
                errors.push(e);
            }
        }
        Ok(ScanOutput {
            found_files,
            fingerprint: fingerprint(found_files, max_mtime, total_bytes),
            errors,
            ..Default::default()
        })
    }

    fn scan_fingerprint(&self) -> Result<String, ProviderError> {
        let roots = self.existing_roots();
        if roots.is_empty() {
            return Err(ProviderError::DataDirNotFound(Provider::TeleAgent));
        }
        let mut max_mtime = 0i64;
        let mut total_bytes = 0u64;
        let mut found = 0u64;
        for root in &roots {
            let (f, m, b) = Self::store_stats(root);
            found += f;
            max_mtime = max_mtime.max(m);
            total_bytes += b;
        }
        Ok(fingerprint(found, max_mtime, total_bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::{params, Connection};
    use tempfile::tempdir;

    use super::*;

    /// A completed assistant message, mirroring the real TeleAgent `message`
    /// row's `data` JSON (see `doc/teleagent-token-parsing.md` §3): tokens
    /// with a separate reasoning bucket and a cache sub-object, model id, and
    /// millisecond timestamps.
    fn assistant_data(input: i64, reasoning: i64, output: i64, created_ms: i64) -> String {
        format!(
            r#"{{"role":"assistant","agent":"opencowork-default","modelID":"chat-pro","providerID":"NewApi","cost":0,"finish":"tool-calls","time":{{"created":{created_ms},"completed":{created_ms}}},"tokens":{{"input":{input},"output":{output},"reasoning":{reasoning},"cache":{{"read":0,"write":0}},"total":0}}}}"#
        )
    }

    /// Create a store (`teleagent.db`) under `dir` with a `session` table and
    /// the given `(id, session_id, data)` message rows.
    fn write_store(dir: &Path, title: Option<&str>, messages: &[(&str, &str, &str)]) {
        let conn = Connection::open(dir.join(DB_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"session\" (
                id TEXT PRIMARY KEY,
                project_id TEXT,
                title TEXT,
                time_created INTEGER NOT NULL,
                time_updated INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        if let Some(title) = title {
            conn.execute(
                "INSERT INTO \"session\" (id, title, time_created) VALUES (?1, ?2, 1)",
                params![messages[0].1, title],
            )
            .unwrap();
        }
        for (id, session_id, data) in messages {
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data)
                 VALUES (?1, ?2, 1786800000000, ?3)",
                params![id, session_id, data],
            )
            .unwrap();
        }
    }

    fn source_for(dir: &Path) -> TeleAgentSource {
        TeleAgentSource::new(ProviderConfig {
            provider: Provider::TeleAgent,
            data_dir_override: Some(dir.to_path_buf()),
            ..ProviderConfig::default()
        })
    }

    fn scan_collect(src: &TeleAgentSource) -> (ScanOutput, Vec<UsageRecord>) {
        let mut records = Vec::new();
        let out = src.scan(&mut |r| records.push(r)).unwrap();
        (out, records)
    }

    #[test]
    fn parses_completed_assistant_messages_into_records() {
        let dir = tempdir().unwrap();
        write_store(
            dir.path(),
            Some("查看项目情况"),
            &[
                ("msg_1", "ses_1", &assistant_data(100, 5, 20, 1792395818864)),
                ("msg_2", "ses_1", &assistant_data(10, 0, 3, 1792395819000)),
            ],
        );

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert_eq!(out.found_files, 1, "one store file per root");
        assert!(out.errors.is_empty());
        assert_eq!(records.len(), 2);

        let first = &records[0];
        assert_eq!(first.provider, Provider::TeleAgent);
        // The session title maps to the project dimension.
        assert_eq!(first.project, "查看项目情况");
        assert_eq!(first.session_id, "ses_1");
        assert_eq!(first.usage.model, "chat-pro");
        // Reasoning is folded into input, matching the app's own totals.
        assert_eq!(first.usage.input_tokens, 105);
        assert_eq!(first.usage.output_tokens, 20);
        assert_eq!(first.usage.cache_read_tokens, 0);
        assert_eq!(first.usage.cache_write_tokens, 0);
        assert_eq!(first.usage.total_tokens(), 125);
        assert_eq!(first.usage.started_at.timestamp_millis(), 1792395818864);
        // Dedup key is the store path + message id (the row's primary key).
        assert!(first.fingerprint.ends_with(":msg_1"));
    }

    #[test]
    fn skips_user_and_zero_token_messages() {
        let dir = tempdir().unwrap();
        let placeholder = r#"{"role":"assistant","modelID":"chat-pro","finish":null,"time":{"created":1792395818864},"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0},"total":0}}"#;
        write_store(
            dir.path(),
            Some("t"),
            &[
                ("msg_u", "ses_1", r#"{"role":"user","time":{"created":1}}"#),
                // Zero-token assistant placeholder / interrupted call.
                ("msg_0", "ses_1", placeholder),
                ("msg_1", "ses_1", &assistant_data(50, 0, 0, 2)),
            ],
        );

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "ses_1");
    }

    #[test]
    fn maps_cache_buckets() {
        let dir = tempdir().unwrap();
        let data = r#"{"role":"assistant","tokens":{"input":10,"output":1,"reasoning":0,"cache":{"read":90,"write":5}},"modelID":"chat-flash","time":{"created":1,"completed":2}}"#;
        write_store(dir.path(), Some("t"), &[("msg_c", "ses_c", data)]);

        let (_, records) = scan_collect(&source_for(dir.path()));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].usage.cache_read_tokens, 90);
        assert_eq!(records[0].usage.cache_write_tokens, 5);
        assert_eq!(records[0].usage.total_tokens(), 106);
    }

    #[test]
    fn untitled_sessions_fall_back_to_unknown() {
        let dir = tempdir().unwrap();
        write_store(
            dir.path(),
            None,
            &[("msg_1", "ses_1", &assistant_data(1, 0, 1, 1))],
        );

        let (_, records) = scan_collect(&source_for(dir.path()));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].project, "unknown");
    }

    #[test]
    fn scans_without_the_session_table() {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join(DB_FILE)).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('msg_1', 'ses_1', 1786800000000, ?1)",
            params![assistant_data(7, 0, 3, 1792395818864)],
        )
        .unwrap();

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty(), "no-session stores scan, not fail");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].project, "unknown");
        assert_eq!(records[0].usage.total_tokens(), 10);
    }

    #[test]
    fn data_dir_missing_is_data_dir_not_found() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("no-such-dir");
        let err = source_for(&missing).data_dirs().unwrap_err();
        assert!(matches!(
            err,
            ProviderError::DataDirNotFound(Provider::TeleAgent)
        ));
    }

    #[test]
    fn expands_user_directories_and_namespaces_fingerprints() {
        let dir = tempdir().unwrap();
        let users = dir.path().join("users");
        let u1 = users.join("v1_public_111");
        let u2 = users.join("v1_public_222");
        std::fs::create_dir_all(&u1).unwrap();
        std::fs::create_dir_all(&u2).unwrap();
        write_store(
            &u1,
            Some("会话 A"),
            &[("msg_1", "ses", &assistant_data(1, 0, 1, 1))],
        );
        write_store(
            &u2,
            Some("会话 B"),
            &[("msg_1", "ses", &assistant_data(2, 0, 2, 2))],
        );

        let (out, records) = {
            // The override points at the users root; each one-level
            // subdirectory holding a store becomes a labelled scan root.
            let src = TeleAgentSource::new(ProviderConfig {
                provider: Provider::TeleAgent,
                data_dir_override: Some(users),
                ..ProviderConfig::default()
            });
            scan_collect(&src)
        };
        assert!(out.errors.is_empty());
        assert_eq!(out.found_files, 2, "one store per user directory");
        assert_eq!(records.len(), 2);
        // Same message id in two user stores must not collide — the label
        // namespaces each store's dedup keys.
        let fps: Vec<_> = records.iter().map(|r| r.fingerprint.clone()).collect();
        assert_ne!(fps[0], fps[1]);
        assert!(fps.iter().all(|f| f.contains("teleagent.db:msg_1")));
        assert!(fps.iter().any(|f| f.starts_with("v1_public_111")));
        assert!(fps.iter().any(|f| f.starts_with("v1_public_222")));
    }

    #[test]
    fn reads_a_store_directly_under_the_root() {
        let dir = tempdir().unwrap();
        write_store(
            dir.path(),
            Some("直连"),
            &[("msg_1", "ses_1", &assistant_data(1, 0, 1, 1))],
        );

        let (out, records) = scan_collect(&source_for(dir.path()));
        assert!(out.errors.is_empty());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].fingerprint, "teleagent.db:msg_1");
    }

    #[test]
    fn fingerprint_changes_when_store_rewritten() {
        let dir = tempdir().unwrap();
        let src = source_for(dir.path());
        write_store(
            dir.path(),
            Some("t"),
            &[("msg_1", "ses_1", &assistant_data(1, 0, 0, 1))],
        );

        let fp1 = src.scan_fingerprint().unwrap();
        let fp2 = src.scan_fingerprint().unwrap();
        assert_eq!(fp1, fp2, "unchanged store keeps the same fingerprint");

        // Rewriting the store (new message) changes the fingerprint, so the
        // scheduler rescans and picks up the new record. The fingerprint only
        // has one-second resolution, so wait out the current second to ensure
        // the mtime actually differs.
        std::thread::sleep(Duration::from_secs(2));
        std::fs::remove_file(dir.path().join(DB_FILE)).unwrap();
        write_store(
            dir.path(),
            Some("t"),
            &[
                ("msg_1", "ses_1", &assistant_data(1, 0, 0, 1)),
                ("msg_2", "ses_1", &assistant_data(2, 0, 0, 2)),
            ],
        );
        assert_ne!(fp1, src.scan_fingerprint().unwrap());
    }
}
