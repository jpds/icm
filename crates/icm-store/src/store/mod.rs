use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Mutex, Once};

use chrono::{DateTime, Utc};
use lru::LruCache;
use rusqlite::{ffi::sqlite3_auto_extension, params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use zerocopy::IntoBytes;

use icm_core::{
    Concept, ConceptLink, Embedder, Fact, FactsStats, FactsStore, Feedback, FeedbackStats,
    FeedbackStore, IcmError, IcmResult, Importance, Label, Memoir, MemoirStats, MemoirStore,
    Memory, MemorySource, MemoryStore, Message, PatternCluster, Relation, Role, Session,
    StoreStats, TopicHealth, TranscriptHit, TranscriptStats, TranscriptStore,
};

use crate::schema::init_db_with_dims;

/// Convert rusqlite::Error to IcmError::Database
pub(crate) fn db_err(e: rusqlite::Error) -> IcmError {
    IcmError::Database(e.to_string())
}

/// True when a rusqlite error is a "no such table" (a legacy DB missing an
/// FTS shadow table we optionally rebuild — issue #313).
fn is_missing_table(e: &rusqlite::Error) -> bool {
    e.to_string().contains("no such table")
}

/// FTS5 shadow tables maintained by ICM, checked/rebuilt during repair (#313).
const FTS_TABLES: [&str; 4] = [
    "memories_fts",
    "concepts_fts",
    "feedback_fts",
    "messages_fts",
];

// Shared public row types live in `crate::common` so all backends can be
// compiled into one binary without colliding definitions (issue #301).
pub use crate::common::{CodeArea, HookEvent, HookEventInsert, HookStatsRow, PendingRow};

/// Collect mapped rows into a Vec, converting rusqlite errors.
fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> IcmResult<Vec<T>> {
    rows.collect::<Result<Vec<T>, _>>().map_err(db_err)
}

static SQLITE_VEC_INIT: Once = Once::new();

fn ensure_sqlite_vec() {
    SQLITE_VEC_INIT.call_once(|| unsafe {
        #[allow(clippy::missing_transmute_annotations)]
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

/// URI-encode a filesystem path for a SQLite `file:` URI so a backslash on
/// Windows or a `?`/`#`/`%` in a pathological filename can't break the parser.
fn encode_sqlite_uri_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| match c {
            '?' | '#' | '%' => format!("%{:02X}", c as u32),
            // Normalize Windows backslashes; SQLite URIs accept "/".
            '\\' => "/".into(),
            other => other.to_string(),
        })
        .collect()
}

/// Open `path` strictly read-only. When `immutable` is set, add the
/// `immutable=1` URI flag — SQLite then assumes the file never changes and
/// touches no `-shm`/`-wal` sidecars, which is required on a `chmod -w`
/// parent directory (issue #263) but serves a permanently stale snapshot and
/// eventually reports spurious `SQLITE_CORRUPT` on a live DB (issue #319).
/// Plain `mode=ro` (immutable = false) is WAL-aware and sees committed
/// writes, at the cost of needing a writable directory for the sidecars.
fn open_readonly_uri(path: &Path, immutable: bool) -> IcmResult<Connection> {
    let encoded = encode_sqlite_uri_path(path);
    let uri = if immutable {
        format!("file:{encoded}?mode=ro&immutable=1")
    } else {
        format!("file:{encoded}?mode=ro")
    };
    Connection::open_with_flags(
        uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| IcmError::Database(format!("cannot open database read-only: {e}")))
}

/// Open a long-lived read-only connection (issue #319).
///
/// Prefer a normal WAL-aware `mode=ro` connection: it respects locking and
/// sees writes committed after it opened — the actual deployment model for
/// `icm --read-only serve`, where hooks keep writing the same DB. Fall back to
/// `immutable=1` only when the live open can't even read the DB, e.g. a
/// `chmod -w` sandbox where SQLite can't create the `-shm` sidecar for a
/// WAL-mode file (issue #263). The read probe is essential: on such a
/// directory the open may *succeed* yet the first real read fails, so opening
/// alone is not a sufficient signal.
fn open_readonly_connection(path: &Path) -> IcmResult<Connection> {
    if let Ok(conn) = open_readonly_uri(path, false) {
        // Give a momentarily-locked writer time to release before deciding
        // the live open is unusable.
        let _ = conn.execute_batch("PRAGMA busy_timeout=30000;");
        // Exercise a real table read (touches the WAL/-shm path) — `SELECT 1`
        // would not.
        if conn
            .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .is_ok()
        {
            return Ok(conn);
        }
    }
    // Live open unusable (e.g. a read-only sandbox dir, #263). Fall back to an
    // immutable snapshot — but warn, because a long-lived reader on this
    // connection will NOT see subsequent writes (the #319 staleness tradeoff).
    tracing::warn!(
        path = %path.display(),
        "read-only DB opened immutable (sandbox fallback): writes committed after \
         this point will not be visible until the connection is reopened"
    );
    open_readonly_uri(path, true)
}

/// In-process LRU cache size for hot memories. Each entry is one
/// fully-hydrated `Memory` (incl. optional 384×f32 embedding ≈ 1.5KB),
/// so 256 entries cap RAM at ~400KB worst case. Helps long-running
/// processes (`icm serve`, TUI) where the same memories are read
/// repeatedly; zero benefit in one-shot CLI invocations beyond the
/// single recall flow.
const MEMORY_CACHE_CAP: usize = 256;

pub struct SqliteStore {
    conn: Connection,
    cache: Mutex<LruCache<String, Memory>>,
    /// `true` when opened through [`Self::open_readonly`]. Read-like
    /// methods that would otherwise dirty the DB (auto-decay,
    /// `update_access`) check this and skip silently; mutation methods
    /// (`store`, `update`, `delete`, etc.) check this and return
    /// `IcmError::ReadOnly`. Issue #263.
    readonly: bool,
}

impl SqliteStore {
    pub fn new(path: &Path) -> IcmResult<Self> {
        Self::with_dims(path, icm_core::DEFAULT_EMBEDDING_DIMS)
    }

    /// Open an existing database in read-only mode (issue #263).
    ///
    /// Differences vs [`Self::with_dims`]:
    /// - The parent directory is NOT created.
    /// - The connection is opened with `SQLITE_OPEN_READ_ONLY` — SQLite
    ///   itself refuses any DDL/DML that the application might miss.
    /// - No `PRAGMA journal_mode=WAL` (WAL requires writable access).
    /// - No `init_db_with_dims` (schema migration would mutate the DB).
    ///
    /// Returns an error if the file is absent (caller may want to fall
    /// through to writable mode then). Use [`std::path::Path::exists`]
    /// at the call site if you need a missing-DB fast path.
    pub fn open_readonly(path: &Path) -> IcmResult<Self> {
        ensure_sqlite_vec();
        if !path.exists() {
            return Err(IcmError::NotFound(format!(
                "database not found at {}",
                path.display()
            )));
        }
        let conn = open_readonly_connection(path)?;
        // foreign_keys is a no-op for reads; busy_timeout is still useful
        // when another writer holds the file.
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=30000;")
            .map_err(db_err)?;
        Ok(Self {
            conn,
            cache: Mutex::new(new_cache()),
            readonly: true,
        })
    }

    /// Open an existing database for maintenance — integrity check and
    /// repair (issue #313).
    ///
    /// Writable (so `REINDEX` and FTS `'rebuild'` can run) but, unlike
    /// [`Self::with_dims`], it deliberately does NOT:
    /// - run `init_db_with_dims` — schema migration would fail on, or mutate,
    ///   a corrupt DB before it can even be inspected;
    /// - switch `journal_mode` — a damaged file's on-disk format is left
    ///   exactly as found so recovery reasons about the real state.
    ///
    /// Returns [`IcmError::NotFound`] when the file is absent.
    pub fn open_maintenance(path: &Path) -> IcmResult<Self> {
        ensure_sqlite_vec();
        if !path.exists() {
            return Err(IcmError::NotFound(format!(
                "database not found at {}",
                path.display()
            )));
        }
        let conn = Connection::open(path)
            .map_err(|e| IcmError::Database(format!("cannot open database: {e}")))?;
        conn.execute_batch("PRAGMA busy_timeout=30000;")
            .map_err(db_err)?;
        Ok(Self {
            conn,
            cache: Mutex::new(new_cache()),
            readonly: false,
        })
    }

    /// True when the store was opened read-only (issue #263). Read-like
    /// methods skip side-effect mutations; write methods return
    /// `IcmError::ReadOnly`.
    #[must_use]
    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Peek `icm_metadata.embedding_dims` without running any schema
    /// migration. Returns `Ok(None)` when the DB file is absent, the
    /// metadata table doesn't exist (legacy DB), or the row is missing.
    ///
    /// Use this *before* calling [`Self::with_dims`] when running in a
    /// mode that must not trigger a destructive vector recreate — most
    /// notably the `--no-embeddings` path (issue #267): if the caller
    /// has no embedder loaded, `with_dims` would otherwise fall back to
    /// `DEFAULT_EMBEDDING_DIMS`, mismatch the stored value, and silently
    /// DROP `vec_memories` while NULL-ing every `memories.embedding`.
    pub fn read_stored_embedding_dims(path: &Path) -> IcmResult<Option<usize>> {
        if !path.exists() {
            return Ok(None);
        }
        // Open strictly immutable so this helper survives a `chmod -w`
        // sandbox (issue #263 interaction). `SQLITE_OPEN_READ_ONLY`
        // alone is NOT enough — SQLite still tries to create/update
        // the `-shm` / `-wal` companion files for any WAL-mode DB,
        // which fails when the parent directory is non-writable.
        // The `immutable=1` URI flag tells SQLite the file will not
        // change during the connection's lifetime and stops it from
        // touching WAL infrastructure entirely. This is a one-shot probe
        // (not a long-lived connection), so the staleness that #319 fixes
        // for `open_readonly` does not apply here.
        let conn = open_readonly_uri(path, true)?;
        // Probe for the metadata table — legacy DBs predate it.
        let has_table: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master
                 WHERE type = 'table' AND name = 'icm_metadata'",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        if !has_table {
            return Ok(None);
        }
        let row: Option<String> = conn
            .query_row(
                "SELECT value FROM icm_metadata WHERE key = 'embedding_dims'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)?;
        Ok(row.and_then(|s| s.parse().ok()))
    }

    /// Open or create a store with a specific embedding dimension.
    pub fn with_dims(path: &Path, embedding_dims: usize) -> IcmResult<Self> {
        ensure_sqlite_vec();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| IcmError::Database(format!("cannot create db directory: {e}")))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| IcmError::Database(format!("cannot open database: {e}")))?;
        // Schema/PRAGMA setup races with other processes opening the same
        // brand-new DB simultaneously (found via real concurrent testing:
        // 10 processes opening one fresh DB, several hung, others errored,
        // zero succeeded). Both the WAL-mode switch (needs a brief
        // exclusive lock to convert a fresh file — busy_timeout must be
        // set first in the same batch, or this statement itself has no
        // timeout active yet) and init_db_with_dims's schema creation
        // (BEGIN IMMEDIATE-wrapped in schema.rs, but SQLite's FTS5
        // virtual-table module can still surface a transient error on the
        // loser even so) are retried together here: whatever the winner
        // already committed, a fresh attempt's PRAGMA + existence checks
        // correctly see and no-op past it. Jittered, not just linear,
        // backoff: a fixed schedule lets many racing processes retry in
        // near-lockstep and collide again and again.
        let mut last_err = None;
        for attempt in 0..40u32 {
            if attempt > 0 {
                let base_ms = (attempt as u64).min(20) * 15;
                let jitter_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 40)
                    .unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(base_ms + jitter_ms));
            }
            // A short busy_timeout during this retry loop, not the normal
            // 30s: 30s is meant to tolerate *ordinary* write contention
            // during real use (e.g. a hook write racing a consolidate),
            // but stacked with up to 40 outer attempts here it turns into
            // a potentially multi-minute worst case under real multi-
            // process contention (measured: several real `icm` processes
            // hung well past 60s with the 30s inner timeout) — the outer
            // jittered loop is what actually provides the robustness here,
            // so the inner SQLite-level wait only needs to be long enough
            // to smooth over a single competing transaction, not to be a
            // retry mechanism in its own right. Restored to 30s below once
            // the schema is confirmed present.
            let attempt_result = conn
                .execute_batch(
                    "PRAGMA busy_timeout=1000; PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;",
                )
                .map_err(db_err)
                .and_then(|()| init_db_with_dims(&conn, embedding_dims));
            match attempt_result {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    let msg = e.to_string();
                    let transient = msg.contains("vtable constructor failed")
                        || msg.contains("already exists")
                        || msg.contains("database is locked")
                        || msg.contains("database is busy");
                    last_err = Some(e);
                    if !transient {
                        break;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        conn.execute_batch("PRAGMA busy_timeout=30000;")
            .map_err(db_err)?;

        Ok(Self {
            conn,
            cache: Mutex::new(new_cache()),
            readonly: false,
        })
    }

    /// Atomically increment the hook call counter and return the new value.
    pub fn increment_hook_counter(&self) -> IcmResult<usize> {
        let count: usize = self
            .conn
            .query_row(
                "INSERT INTO icm_metadata (key, value) VALUES ('hook_counter', '1')
                 ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)
                 RETURNING CAST(value AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)?;
        Ok(count)
    }

    /// Reset the hook call counter to 0.
    pub fn reset_hook_counter(&self) -> IcmResult<()> {
        self.conn
            .execute(
                "INSERT INTO icm_metadata (key, value) VALUES ('hook_counter', '0')
                 ON CONFLICT(key) DO UPDATE SET value = '0'",
                [],
            )
            .map_err(db_err)?;
        Ok(())
    }

    // ── Async extraction queue ─────────────────────────────────────────
    //
    // Row tuple shape: `(id, project, tool_name, raw_output, captured_at)`
    //
    // When `[extraction.summarizer].provider` is set to something other
    // than `"none"`, PostToolUse hooks INSERT raw tool output here in
    // ~50ms (no embedder load) and a worker (`icm extract-pending` or
    // the SessionEnd async fork) dequeues batches and runs the LLM CLI.

    /// Enqueue raw tool output for later LLM extraction. Returns the
    /// generated row id so the caller can correlate logs.
    pub fn enqueue_pending_extraction(
        &self,
        project: &str,
        tool_name: &str,
        raw_output: &str,
    ) -> IcmResult<String> {
        let id = ulid::Ulid::new().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO pending_extractions (id, project, tool_name, raw_output, captured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![id, project, tool_name, raw_output, now],
            )
            .map_err(db_err)?;
        Ok(id)
    }

    /// Pop up to `limit` oldest pending rows. Caller is expected to call
    /// `delete_pending_extractions` after successful processing.
    pub fn list_pending_extractions(&self, limit: usize) -> IcmResult<Vec<PendingRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, tool_name, raw_output, captured_at
                 FROM pending_extractions
                 ORDER BY captured_at ASC
                 LIMIT ?1",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Delete pending rows by id. Used after a worker has processed them.
    pub fn delete_pending_extractions(&self, ids: &[String]) -> IcmResult<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM pending_extractions WHERE id IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let n = self.conn.execute(&sql, params.as_slice()).map_err(db_err)?;
        Ok(n)
    }

    /// Total rows currently waiting in the queue. Used by `icm doctor`.
    pub fn pending_extraction_count(&self) -> IcmResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pending_extractions", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok(n as usize)
    }

    // ── Code areas (auto-captured file edits — issue #196) ────────────
    //
    // `cmd_hook_post` calls `upsert_code_area` whenever the upstream
    // tool was Edit / Write / MultiEdit / NotebookEdit. Same project +
    // file_path => touch_count++ via ON CONFLICT.

    /// Insert or refresh a row for `(project, file_path)`. On conflict
    /// bumps `touch_count`, updates `last_touched_at`, refreshes
    /// `session_id` / `tool_name`, and only overwrites `description` if
    /// the caller passes `Some` (so the most recent meaningful hint
    /// wins without clobbering an existing one with `None`).
    pub fn upsert_code_area(
        &self,
        project: &str,
        file_path: &str,
        description: Option<&str>,
        session_id: Option<&str>,
        tool_name: Option<&str>,
    ) -> IcmResult<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO code_areas (project, file_path, description,
                    session_id, tool_name, touch_count,
                    first_touched_at, last_touched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
                 ON CONFLICT(project, file_path) DO UPDATE SET
                    touch_count = touch_count + 1,
                    last_touched_at = excluded.last_touched_at,
                    session_id = COALESCE(excluded.session_id, session_id),
                    tool_name = COALESCE(excluded.tool_name, tool_name),
                    description = COALESCE(excluded.description, description)",
                rusqlite::params![project, file_path, description, session_id, tool_name, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// List code areas, optionally filtered by project / file_path /
    /// since timestamp. `limit` caps the result count (use `usize::MAX`
    /// to disable). Ordered by `last_touched_at DESC` so the freshest
    /// edits come first.
    pub fn list_code_areas(
        &self,
        project: Option<&str>,
        in_file: Option<&str>,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> IcmResult<Vec<CodeArea>> {
        let mut sql = String::from(
            "SELECT id, project, file_path, description, session_id, tool_name,
                    touch_count, first_touched_at, last_touched_at
             FROM code_areas
             WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = project {
            sql.push_str(" AND project = ?");
            params.push(Box::new(p.to_string()));
        }
        if let Some(f) = in_file {
            // Match either an exact file_path or a path that ends with
            // the provided fragment so users can pass a short suffix.
            sql.push_str(" AND (file_path = ? OR file_path LIKE ?)");
            params.push(Box::new(f.to_string()));
            params.push(Box::new(format!("%/{f}")));
        }
        if let Some(t) = since {
            sql.push_str(" AND last_touched_at >= ?");
            params.push(Box::new(t.to_rfc3339()));
        }
        sql.push_str(" ORDER BY last_touched_at DESC LIMIT ?");
        params.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|p| p.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let first: String = row.get(7)?;
                let last: String = row.get(8)?;
                Ok(CodeArea {
                    id: row.get(0)?,
                    project: row.get(1)?,
                    file_path: row.get(2)?,
                    description: row.get(3)?,
                    session_id: row.get(4)?,
                    tool_name: row.get(5)?,
                    touch_count: row.get(6)?,
                    first_touched_at: DateTime::parse_from_rfc3339(&first)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    last_touched_at: DateTime::parse_from_rfc3339(&last)
                        .map(|d| d.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    /// Total rows in `code_areas`. Cheap; used by stats / doctor.
    pub fn code_area_count(&self) -> IcmResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM code_areas", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok(n as usize)
    }

    // ── Hook telemetry ─────────────────────────────────────────────────
    //
    // Every `icm hook <event>` fire writes one row to `hook_events`. Read
    // back via `hook_events_recent` / `hook_stats`. Inserts are designed
    // to be cheap (single statement, no FTS) so they stay well under the
    // <50ms async-path budget.

    /// Append one hook telemetry row. Errors are swallowed by callers in
    /// hook paths (logging must never block the user), but tests can
    /// inspect the `Result`.
    pub fn record_hook_event(&self, ev: &HookEventInsert) -> IcmResult<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                "INSERT INTO hook_events
                 (ts, event, project, session_id, tool_name,
                  duration_ms, exit_code, payload_size, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    now,
                    ev.event,
                    ev.project,
                    ev.session_id,
                    ev.tool_name,
                    ev.duration_ms,
                    ev.exit_code,
                    ev.payload_size,
                    ev.note,
                ],
            )
            .map_err(db_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent `limit` hook events, newest first. Optional `event`
    /// filter (e.g. `Some("end")` to see only SessionEnd hooks).
    pub fn hook_events_recent(
        &self,
        limit: usize,
        event_filter: Option<&str>,
    ) -> IcmResult<Vec<HookEvent>> {
        let limit_i64 = limit as i64;
        let row_to_event = |row: &rusqlite::Row<'_>| -> rusqlite::Result<HookEvent> {
            let ts_str: String = row.get(1)?;
            let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(HookEvent {
                id: row.get(0)?,
                ts,
                event: row.get(2)?,
                project: row.get(3)?,
                session_id: row.get(4)?,
                tool_name: row.get(5)?,
                duration_ms: row.get(6)?,
                exit_code: row.get(7)?,
                payload_size: row.get(8)?,
                note: row.get(9)?,
            })
        };
        match event_filter {
            Some(e) => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, ts, event, project, session_id, tool_name,
                                duration_ms, exit_code, payload_size, note
                         FROM hook_events
                         WHERE event = ?1
                         ORDER BY id DESC
                         LIMIT ?2",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(rusqlite::params![e, limit_i64], row_to_event)
                    .map_err(db_err)?;
                collect_rows(rows)
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT id, ts, event, project, session_id, tool_name,
                                duration_ms, exit_code, payload_size, note
                         FROM hook_events
                         ORDER BY id DESC
                         LIMIT ?1",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(rusqlite::params![limit_i64], row_to_event)
                    .map_err(db_err)?;
                collect_rows(rows)
            }
        }
    }

    /// Aggregate counts and latency percentiles per event type, over a
    /// time window starting `since` (RFC3339). Used by `icm hook-stats`.
    pub fn hook_stats(&self, since_rfc3339: &str) -> IcmResult<Vec<HookStatsRow>> {
        // Pull each event type and compute percentiles in Rust — SQLite
        // has no native percentile function and the row count is small
        // enough (~1k/day worst case) that an in-process sort is fine.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event, duration_ms, exit_code
                 FROM hook_events
                 WHERE ts >= ?1
                 ORDER BY event",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([since_rfc3339], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i32>(2)?,
                ))
            })
            .map_err(db_err)?;
        let mut by_event: std::collections::BTreeMap<String, Vec<(Option<i64>, i32)>> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (ev, dur, exit) = r.map_err(db_err)?;
            by_event.entry(ev).or_default().push((dur, exit));
        }
        let mut out = Vec::with_capacity(by_event.len());
        for (event, mut items) in by_event {
            let count = items.len() as i64;
            let error_count = items.iter().filter(|(_, e)| *e != 0).count() as i64;
            let mut durations: Vec<i64> = items.iter().filter_map(|(d, _)| *d).collect();
            durations.sort_unstable();
            let avg = if durations.is_empty() {
                0.0
            } else {
                durations.iter().sum::<i64>() as f64 / durations.len() as f64
            };
            let p = |q: f64| -> i64 {
                if durations.is_empty() {
                    0
                } else {
                    let idx = ((durations.len() as f64 - 1.0) * q).round() as usize;
                    durations[idx.min(durations.len() - 1)]
                }
            };
            out.push(HookStatsRow {
                event,
                count,
                error_count,
                avg_duration_ms: avg,
                p50_duration_ms: p(0.50),
                p99_duration_ms: p(0.99),
            });
            // Avoid clippy 'unused variable' on items after move
            let _ = &mut items;
        }
        Ok(out)
    }

    /// Total rows currently in `hook_events`. Used by tests and `icm doctor`.
    pub fn hook_event_count(&self) -> IcmResult<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM hook_events", [], |r| r.get(0))
            .map_err(db_err)?;
        Ok(n as usize)
    }

    pub fn in_memory() -> IcmResult<Self> {
        Self::in_memory_with_dims(icm_core::DEFAULT_EMBEDDING_DIMS)
    }

    /// Open an in-memory store with a specific embedding dimension.
    /// Useful for tests that exercise the dim-migration / dim-drift paths.
    pub fn in_memory_with_dims(embedding_dims: usize) -> IcmResult<Self> {
        ensure_sqlite_vec();
        let conn = Connection::open_in_memory()
            .map_err(|e| IcmError::Database(format!("cannot open in-memory db: {e}")))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=30000;")
            .map_err(db_err)?;
        init_db_with_dims(&conn, embedding_dims)?;
        Ok(Self {
            conn,
            cache: Mutex::new(new_cache()),
            readonly: false,
        })
    }

    fn cache_get(&self, id: &str) -> Option<Memory> {
        self.cache.lock().ok().and_then(|mut c| c.get(id).cloned())
    }

    fn cache_put(&self, m: &Memory) {
        if let Ok(mut c) = self.cache.lock() {
            c.put(m.id.clone(), m.clone());
        }
    }

    fn cache_invalidate(&self, id: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.pop(id);
        }
    }

    fn cache_invalidate_many(&self, ids: &[&str]) {
        if let Ok(mut c) = self.cache.lock() {
            for id in ids {
                c.pop(*id);
            }
        }
    }

    fn cache_clear(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }
}

fn new_cache() -> LruCache<String, Memory> {
    let cap = NonZeroUsize::new(MEMORY_CACHE_CAP)
        .expect("MEMORY_CACHE_CAP must be non-zero — see store.rs");
    LruCache::new(cap)
}

// ---------------------------------------------------------------------------
// Memory helpers
// ---------------------------------------------------------------------------

fn source_type(source: &MemorySource) -> &'static str {
    match source {
        MemorySource::ClaudeCode { .. } => "claude_code",
        MemorySource::Conversation { .. } => "conversation",
        MemorySource::Manual => "manual",
    }
}

fn source_data(source: &MemorySource) -> Option<String> {
    match source {
        MemorySource::Manual => None,
        other => serde_json::to_string(other).ok(),
    }
}

fn parse_source(source_type_str: &str, source_data_str: Option<String>) -> MemorySource {
    match source_type_str {
        "manual" => MemorySource::Manual,
        _ => source_data_str
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(MemorySource::Manual),
    }
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.as_bytes().to_vec()
}

fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        tracing::warn!(
            blob_size = blob.len(),
            "embedding blob size not divisible by 4, truncating"
        );
    }
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    // Column order: id(0), created_at(1), updated_at(2), last_accessed(3),
    //   access_count(4), weight(5), topic(6), summary(7), raw_excerpt(8),
    //   keywords(9), importance(10), source_type(11), source_data(12),
    //   related_ids(13), embedding(14)
    let keywords_json: String = row.get::<_, Option<String>>(9)?.unwrap_or_default();
    let keywords: Vec<String> = serde_json::from_str(&keywords_json).unwrap_or_default();

    let importance_str: String = row.get(10)?;
    let importance = importance_str.parse().unwrap_or(Importance::Medium);

    let source_type_str: String = row.get(11)?;
    let source_data_str: Option<String> = row.get(12)?;
    let source = parse_source(&source_type_str, source_data_str);

    let related_json: String = row.get::<_, Option<String>>(13)?.unwrap_or_default();
    let related_ids: Vec<String> = serde_json::from_str(&related_json).unwrap_or_default();

    let embedding: Option<Vec<f32>> = row
        .get::<_, Option<Vec<u8>>>(14)?
        .map(|b| blob_to_embedding(&b));

    let created_at_str: String = row.get(1)?;
    let updated_at_str: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
    let last_accessed_str: String = row.get(3)?;

    let created_at = parse_dt(&created_at_str);

    Ok(Memory {
        id: row.get(0)?,
        created_at,
        updated_at: if updated_at_str.is_empty() {
            created_at
        } else {
            parse_dt(&updated_at_str)
        },
        last_accessed: parse_dt(&last_accessed_str),
        access_count: row.get::<_, u32>(4)?,
        weight: row.get(5)?,
        topic: row.get(6)?,
        summary: row.get(7)?,
        raw_excerpt: row.get(8)?,
        keywords,
        importance,
        source,
        related_ids,
        embedding,
        scope: icm_core::Scope::User, // default for existing local memories
    })
}

const SELECT_COLS: &str = "id, created_at, updated_at, last_accessed, access_count, weight, \
                           topic, summary, raw_excerpt, keywords, \
                           importance, source_type, source_data, related_ids, embedding";

/// Sanitize a query string for FTS5 MATCH.
///
/// FTS5 treats characters like `-`, `*`, `"`, `:`, `^`, `+`, `~` as operators.
/// A query like `"sqlite-vec"` makes FTS5 interpret `-` as NOT and `vec` as a
/// column name, causing "no such column: vec".
///
/// Escape `%`, `_`, and the escape character itself so a keyword can be
/// safely wrapped in a `%...%` LIKE pattern. Pair with `ESCAPE '\'` in the
/// SQL — without it, a keyword containing `%` matches every row and `_`
/// matches any single character (audit finding).
fn escape_like_wildcards(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Cap on auxiliary metadata (transcript sessions/messages) - best-effort
/// truncation, not rejection, matching MAX_MESSAGE_BYTES's rationale.
const MAX_METADATA_BYTES: usize = 8 * 1024;

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// This function strips special chars and wraps each token in double quotes.
fn sanitize_fts_query(query: &str) -> String {
    // Limit input length to prevent abuse (UTF-8 safe truncation)
    let query = if query.len() > 10_000 {
        let mut end = 10_000;
        while end > 0 && !query.is_char_boundary(end) {
            end -= 1;
        }
        &query[..end]
    } else {
        query
    };

    // Replace FTS5 operator chars with spaces, then quote each resulting token.
    // FTS5 tokenizer (unicode61) splits on `-` too, so we must keep tokens separate.
    let cleaned: String = query
        .chars()
        .map(|c| {
            if matches!(
                c,
                '-' | '*' | '"' | '(' | ')' | '{' | '}' | ':' | '^' | '+' | '~' | '\\'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();

    let tokens: Vec<String> = cleaned
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .take(100) // Limit token count to prevent excessive query complexity
        .map(|w| {
            // Strip any remaining quotes from tokens before wrapping in quotes
            let stripped = w.replace('"', "");
            format!("\"{stripped}\"")
        })
        .collect();
    tokens.join(" ")
}

/// Whether `e` is FTS5 rejecting a malformed MATCH query (e.g. "hello AND",
/// unbalanced parens) rather than a genuine database error. Used by
/// `search_transcripts` to degrade to "no results" instead of surfacing a
/// raw sqlite error, without pre-sanitizing the query text away from valid
/// FTS5 syntax (which callers rely on — see
/// `test_transcript_search_fts5_boolean_and_phrase`).
fn is_fts5_syntax_error(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(_, Some(msg)) if msg.contains("fts5: syntax error")
    )
}

// ---------------------------------------------------------------------------
// MemoryStore impl
// ---------------------------------------------------------------------------

/// Maximum byte length of a stored summary. Audit finding: a transcript
/// containing a 1 MB unbroken text block landed as a single memory whose
/// summary was the full 1 MB blob. Caps the cost of a single bad write
/// (memory bloat, embedding compute, FTS5 index growth) to a generous
/// but bounded 64 KB.
const MAX_SUMMARY_BYTES: usize = 64 * 1024;

/// Maximum byte length of a stored topic. Topics surface in `icm
/// topics` listings and as the routing key for project filters; a
/// thousand-byte topic is always a bug, never legitimate user input.
const MAX_TOPIC_BYTES: usize = 256;

/// Validate and normalize a `Memory` before insertion. Trims topic
/// whitespace and rejects inputs that we know corrupt or break the
/// store:
///
/// - Empty or whitespace-only `topic` / `summary` — these would surface
///   as blank rows in `icm topics` / `icm list` and pollute the FTS5
///   index without conveying information.
/// - NUL byte (`\0`) in `topic` or `summary` — libsql binds text via a
///   NUL-terminated C string, so anything past the first `\0` is
///   silently dropped. Rather than silently truncate, refuse the
///   write so the caller knows.
/// - Newline / CR / tab in `topic` — these break the `icm topics`
///   tabular layout and could enable display-spoofing of topic names
///   (e.g. a topic that visually overlaps another in TUI/log output).
///   Allowed in `summary` since it's free-form prose.
/// - `topic` longer than `MAX_TOPIC_BYTES` or `summary` longer than
///   `MAX_SUMMARY_BYTES` — see the constant docs for rationale.
fn validate_and_normalize(mut memory: Memory) -> IcmResult<Memory> {
    memory.topic = memory.topic.trim().to_string();
    validate_fields(&memory.topic, &memory.summary)?;
    Ok(memory)
}

/// The borrowed core of [`validate_and_normalize`], shared with `update()`
/// (audit finding: the update path previously bypassed every size/content
/// check, so oversized or NUL-carrying payloads could enter the store by
/// storing small then updating big).
fn validate_fields(topic: &str, summary: &str) -> IcmResult<()> {
    if topic.is_empty() {
        return Err(IcmError::InvalidInput("topic cannot be empty".into()));
    }
    if summary.trim().is_empty() {
        return Err(IcmError::InvalidInput("summary cannot be empty".into()));
    }
    if topic.contains('\0') {
        return Err(IcmError::InvalidInput(
            "topic must not contain NUL bytes".into(),
        ));
    }
    if summary.contains('\0') {
        return Err(IcmError::InvalidInput(
            "summary must not contain NUL bytes".into(),
        ));
    }
    if topic.contains(['\n', '\r', '\t']) {
        return Err(IcmError::InvalidInput(
            "topic must not contain newline / CR / tab characters".into(),
        ));
    }
    if topic.len() > MAX_TOPIC_BYTES {
        return Err(IcmError::InvalidInput(format!(
            "topic exceeds {} bytes",
            MAX_TOPIC_BYTES
        )));
    }
    if summary.len() > MAX_SUMMARY_BYTES {
        return Err(IcmError::InvalidInput(format!(
            "summary exceeds {} bytes",
            MAX_SUMMARY_BYTES
        )));
    }
    Ok(())
}

/// Local total order on `Importance` (Critical > High > Medium > Low).
/// `Importance` does not implement `Ord` because the project did not
/// want to imply a globally meaningful ordering across all uses
/// (e.g. presentation, filtering). For the dedup-merge path we *do*
/// want to take the maximum so re-storing with a higher priority
/// upgrades the existing row.
fn importance_rank(i: Importance) -> u8 {
    match i {
        Importance::Critical => 4,
        Importance::High => 3,
        Importance::Medium => 2,
        Importance::Low => 1,
    }
}

/// Return the higher-priority importance. Used by the dedup path so
/// `store(...)` semantics are "re-store with critical upgrades, never
/// downgrades".
fn max_importance(a: Importance, b: Importance) -> Importance {
    if importance_rank(a) >= importance_rank(b) {
        a
    } else {
        b
    }
}

/// SHA-256 over the normalized `(topic, summary)` pair, hex-encoded.
/// Normalization: trim + lowercase + collapse whitespace runs to single
/// spaces. Topic and summary are joined by `\0` to prevent boundary
/// ambiguity (e.g. `"a"|"bc"` vs `"ab"|"c"` would otherwise hash the
/// same). Used by the dedup `INSERT OR IGNORE` path.
pub(crate) fn summary_hash(topic: &str, summary: &str) -> String {
    let topic_n = topic.trim().to_lowercase();
    let summary_n: String = summary
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase();
    let mut h = Sha256::new();
    h.update(topic_n.as_bytes());
    h.update(b"\0");
    h.update(summary_n.as_bytes());
    format!("{:x}", h.finalize())
}

impl SqliteStore {
    /// Insert a memory into the database without transaction management.
    /// Callers are responsible for wrapping this in a transaction.
    ///
    /// Dedup contract: an INSERT that collides with an existing memory on
    /// `(topic, summary_hash)` is silently ignored, and the **existing**
    /// row's id is returned. The caller's `memory.id` is forgotten in
    /// that case. This keeps `store(...)` idempotent: writing the same
    /// fact 100× ends up with one row, not 100.
    fn store_inner(&self, memory: &Memory) -> IcmResult<String> {
        let keywords_json = serde_json::to_string(&memory.keywords)?;
        let related_json = serde_json::to_string(&memory.related_ids)?;
        let st = source_type(&memory.source);
        let sd = source_data(&memory.source);
        let emb_blob = memory.embedding.as_deref().map(embedding_to_blob);
        let hash = summary_hash(&memory.topic, &memory.summary);

        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO memories (id, created_at, updated_at, last_accessed, access_count, weight,
                 topic, summary, raw_excerpt, keywords,
                 importance, source_type, source_data, related_ids, embedding, summary_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    memory.id,
                    memory.created_at.to_rfc3339(),
                    memory.updated_at.to_rfc3339(),
                    memory.last_accessed.to_rfc3339(),
                    memory.access_count,
                    memory.weight,
                    memory.topic,
                    memory.summary,
                    memory.raw_excerpt,
                    keywords_json,
                    memory.importance.to_string(),
                    st,
                    sd,
                    related_json,
                    emb_blob,
                    hash,
                ],
            )
            .map_err(db_err)?;

        if inserted == 0 {
            // Dedup hit: a row with the same (topic, summary_hash)
            // already exists. Audit #185 H2: the previous behaviour
            // returned the existing id and silently dropped the
            // caller's importance / keywords / raw_excerpt. So
            // running `icm store -t T -c "X" -i medium` then `icm
            // store -t T -c "X" -i critical` left the importance at
            // medium without warning the user.
            //
            // New behaviour: merge the caller's metadata into the
            // existing row before returning the id.
            // - importance: take the max (critical > high > medium >
            //   low). Re-storing with a *higher* priority upgrades.
            //   Re-storing with a *lower* priority is a no-op so a
            //   careless write can't downgrade an already-flagged
            //   critical memory.
            // - keywords: union, preserving existing order then
            //   appending new ones not already present.
            // - raw_excerpt: prefer the new value if non-None,
            //   otherwise keep existing.
            // - updated_at: bumped whenever any field actually changed.
            let (existing_id, existing_importance_str, existing_keywords_json, existing_raw): (
                String,
                String,
                String,
                Option<String>,
            ) = self
                .conn
                .query_row(
                    // Audit finding: `summary_hash` already encodes the topic
                    // (Rust `to_lowercase()`, full Unicode) as part of the
                    // hash input — an additional `LOWER(topic) = LOWER(?)`
                    // comparison here used SQLite's built-in `LOWER()`,
                    // which is ASCII-only and does not fold e.g. 'É' → 'é'.
                    // For an all-caps accented topic like "DÉCISIONS" that
                    // mismatch meant this SELECT could fail to find the row
                    // the `INSERT OR IGNORE` conflict was already about,
                    // even though `summary_hash` alone uniquely identifies
                    // it. `summary_hash` is sufficient on its own.
                    "SELECT id, importance, keywords, raw_excerpt FROM memories
                     WHERE summary_hash = ?1",
                    params![hash],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(db_err)?;

            let existing_importance: Importance = existing_importance_str
                .parse()
                .unwrap_or(Importance::Medium);
            let merged_importance = max_importance(existing_importance, memory.importance);

            let existing_keywords: Vec<String> =
                serde_json::from_str(&existing_keywords_json).unwrap_or_default();
            let mut merged_keywords = existing_keywords.clone();
            for kw in &memory.keywords {
                if !merged_keywords.contains(kw) {
                    merged_keywords.push(kw.clone());
                }
            }

            let merged_raw = memory.raw_excerpt.clone().or(existing_raw.clone());

            let importance_changed = merged_importance != existing_importance;
            let keywords_changed = merged_keywords != existing_keywords;
            let raw_changed = merged_raw != existing_raw;
            if importance_changed || keywords_changed || raw_changed {
                let merged_keywords_json = serde_json::to_string(&merged_keywords)?;
                self.conn
                    .execute(
                        "UPDATE memories
                         SET importance = ?1, keywords = ?2, raw_excerpt = ?3, updated_at = ?4
                         WHERE id = ?5",
                        params![
                            merged_importance.to_string(),
                            merged_keywords_json,
                            merged_raw,
                            Utc::now().to_rfc3339(),
                            existing_id,
                        ],
                    )
                    .map_err(db_err)?;
                self.cache_invalidate(&existing_id);
            }

            tracing::debug!(
                topic = %memory.topic,
                existing = %existing_id,
                attempted = %memory.id,
                imp_changed = importance_changed,
                kw_changed = keywords_changed,
                raw_changed = raw_changed,
                "store: dedup'd duplicate memory (metadata merged)"
            );
            return Ok(existing_id);
        }

        // Sync to vec_memories for KNN search (only on a fresh insert).
        if let Some(ref blob) = emb_blob {
            self.conn
                .execute(
                    "INSERT INTO vec_memories (memory_id, embedding) VALUES (?1, ?2)",
                    params![memory.id, blob],
                )
                .map_err(db_err)?;
        }

        Ok(memory.id.clone())
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ---------------------------------------------------------------------------
// Test helpers (visible to other modules in crate for test use)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::ensure_sqlite_vec;

    pub fn ensure_vec_init() {
        ensure_sqlite_vec();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Submodules (formerly monolithic `store.rs`, split for reviewability).
mod facts;
mod feedback;
mod maintenance;
mod memoir;
mod memory;
mod patterns;
#[cfg(test)]
mod tests;
mod transcript;
