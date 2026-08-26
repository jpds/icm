//! SQLite backend — split out of the former monolithic `store.rs`.
//!
//! `SqliteStore` and the shared row/parse helpers live in `super`
//! (`store/mod.rs`); each submodule here holds one trait impl (or a
//! coherent group of inherent methods) on that type.

use super::*;

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let started_at: String = row.get("started_at")?;
    let updated_at: String = row.get("updated_at")?;
    Ok(Session {
        id: row.get("id")?,
        agent: row.get("agent")?,
        project: row.get("project")?,
        started_at: parse_ts(&started_at),
        updated_at: parse_ts(&updated_at),
        metadata: row.get("metadata")?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let role_str: String = row.get("role")?;
    let ts: String = row.get("ts")?;
    let role = Role::parse(&role_str).unwrap_or(Role::Tool);
    Ok(Message {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        role,
        content: row.get("content")?,
        tool_name: row.get("tool_name")?,
        tokens: row.get("tokens")?,
        ts: parse_ts(&ts),
        metadata: row.get("metadata")?,
    })
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl TranscriptStore for SqliteStore {
    fn create_session(
        &self,
        agent: &str,
        project: Option<&str>,
        metadata: Option<&str>,
    ) -> IcmResult<String> {
        let metadata = metadata.map(|s| truncate_at_char_boundary(s, MAX_METADATA_BYTES));
        let session = Session::new(
            agent.to_string(),
            project.map(|s| s.to_string()),
            metadata.map(|s| s.to_string()),
        );
        let conn = &self.conn;
        conn.execute(
            "INSERT INTO sessions (id, agent, project, started_at, updated_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session.id,
                session.agent,
                session.project,
                session.started_at.to_rfc3339(),
                session.updated_at.to_rfc3339(),
                session.metadata,
            ],
        )
        .map_err(db_err)?;
        Ok(session.id)
    }

    fn get_session(&self, id: &str) -> IcmResult<Option<Session>> {
        let conn = &self.conn;
        let row = conn
            .query_row(
                "SELECT id, agent, project, started_at, updated_at, metadata
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })?;
        Ok(row)
    }

    fn ensure_session(
        &self,
        id: &str,
        agent: &str,
        project: Option<&str>,
        metadata: Option<&str>,
    ) -> IcmResult<String> {
        let conn = &self.conn;
        let now = chrono::Utc::now().to_rfc3339();
        let metadata = metadata.map(|s| truncate_at_char_boundary(s, MAX_METADATA_BYTES));
        // INSERT OR IGNORE keeps the first row stable across re-fires.
        // The `id` is the host agent's session id (Claude Code, Codex,
        // Gemini, etc.) so repeated hook posts route to the same row.
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, agent, project, started_at, updated_at, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, agent, project, now, now, metadata.unwrap_or("{}")],
        )
        .map_err(db_err)?;
        Ok(id.to_string())
    }

    fn list_sessions(&self, project: Option<&str>, limit: usize) -> IcmResult<Vec<Session>> {
        let conn = &self.conn;
        match project {
            Some(p) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, agent, project, started_at, updated_at, metadata
                         FROM sessions WHERE project = ?1
                         ORDER BY updated_at DESC LIMIT ?2",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![p, limit as i64], row_to_session)
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                Ok(rows)
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, agent, project, started_at, updated_at, metadata
                         FROM sessions ORDER BY updated_at DESC LIMIT ?1",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![limit as i64], row_to_session)
                    .map_err(db_err)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                Ok(rows)
            }
        }
    }

    fn record_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
        tool_name: Option<&str>,
        tokens: Option<i64>,
        metadata: Option<&str>,
    ) -> IcmResult<String> {
        /// Cap on a single transcript message. Unlike memory writes (which
        /// reject oversized input), transcripts are best-effort logs — a
        /// multi-MB tool dump is truncated rather than lost entirely. The
        /// MCP `icm_transcript_record` path previously had no bound at all
        /// (audit finding).
        const MAX_MESSAGE_BYTES: usize = 256 * 1024;
        // metadata/tool_name got no cap at all even after content was capped
        // in round 2 (audit finding) - same best-effort-truncate rationale,
        // just smaller since they're auxiliary, not the main payload.
        const MAX_TOOL_NAME_BYTES: usize = 512;

        let content = truncate_at_char_boundary(content, MAX_MESSAGE_BYTES);
        let tool_name = tool_name.map(|s| truncate_at_char_boundary(s, MAX_TOOL_NAME_BYTES));
        let metadata = metadata.map(|s| truncate_at_char_boundary(s, MAX_METADATA_BYTES));

        let msg = Message::new(
            session_id.to_string(),
            role,
            content.to_string(),
            tool_name.map(|s| s.to_string()),
            tokens,
            metadata.map(|s| s.to_string()),
        );
        let conn = &self.conn;

        // Ensure the session exists — referential integrity check is friendlier
        // than raw FK failure.
        let session_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if !session_exists {
            return Err(IcmError::NotFound(format!(
                "session {} does not exist",
                session_id
            )));
        }

        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, tool_name, tokens, ts, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                msg.id,
                msg.session_id,
                msg.role.as_str(),
                msg.content,
                msg.tool_name,
                msg.tokens,
                msg.ts.to_rfc3339(),
                msg.metadata,
            ],
        )
        .map_err(db_err)?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![msg.ts.to_rfc3339(), session_id],
        )
        .map_err(db_err)?;
        Ok(msg.id)
    }

    fn list_session_messages(
        &self,
        session_id: &str,
        limit: usize,
        offset: usize,
    ) -> IcmResult<Vec<Message>> {
        let conn = &self.conn;
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, role, content, tool_name, tokens, ts, metadata
                 FROM messages WHERE session_id = ?1
                 ORDER BY ts ASC LIMIT ?2 OFFSET ?3",
            )
            .map_err(db_err)?;
        let rows: Vec<Message> = stmt
            .query_map(
                params![session_id, limit as i64, offset as i64],
                row_to_message,
            )
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;
        Ok(rows)
    }

    fn search_transcripts(
        &self,
        query: &str,
        session_id: Option<&str>,
        project: Option<&str>,
        limit: usize,
    ) -> IcmResult<Vec<TranscriptHit>> {
        // Unlike search_feedback, raw query text is bound straight to
        // `messages_fts MATCH ?1` — intentionally, since callers rely on
        // real FTS5 syntax here (boolean OR, phrase quoting; see
        // test_transcript_search_fts5_boolean_and_phrase). Sanitizing like
        // search_feedback does would silently break that. Malformed syntax
        // (e.g. "hello AND", "(hello") threw a raw sqlite error instead of
        // degrading gracefully (audit finding) — that's handled below by
        // treating an FTS5 syntax error as "no results" rather than
        // propagating the sqlite internals to the caller.
        let conn = &self.conn;
        // Build dynamic WHERE filters. FTS MATCH comes first for index usage.
        let mut sql = String::from(
            "SELECT m.id, m.session_id, m.role, m.content, m.tool_name, m.tokens, m.ts, m.metadata,
                    s.id AS s_id, s.agent AS s_agent, s.project AS s_project,
                    s.started_at AS s_started_at, s.updated_at AS s_updated_at,
                    s.metadata AS s_metadata,
                    bm25(messages_fts) AS score
             FROM messages_fts
             JOIN messages m ON m.rowid = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_id
             WHERE messages_fts MATCH ?1",
        );
        // Param numbering: ?1 = query (always). Session if present is ?2.
        // Project is ?2 if no session, else ?3.
        if session_id.is_some() {
            sql.push_str(" AND m.session_id = ?2");
        }
        if project.is_some() {
            if session_id.is_some() {
                sql.push_str(" AND s.project = ?3");
            } else {
                sql.push_str(" AND s.project = ?2");
            }
        }
        sql.push_str(" ORDER BY score ASC LIMIT ?");
        sql.push_str(match (session_id.is_some(), project.is_some()) {
            (true, true) => "4",
            (true, false) | (false, true) => "3",
            (false, false) => "2",
        });

        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let limit_i = limit as i64;
        // Compute against raw rusqlite errors so a malformed FTS5 query
        // (syntax error) can be distinguished from a real db failure below,
        // instead of eagerly converting to IcmError via `?`.
        let mut compute = || -> rusqlite::Result<Vec<TranscriptHit>> {
            Ok(match (session_id, project) {
                (Some(sid), Some(p)) => stmt
                    .query_map(params![query, sid, p, limit_i], |row| {
                        let msg = Message {
                            id: row.get("id")?,
                            session_id: row.get("session_id")?,
                            role: Role::parse(&row.get::<_, String>("role")?).unwrap_or(Role::Tool),
                            content: row.get("content")?,
                            tool_name: row.get("tool_name")?,
                            tokens: row.get("tokens")?,
                            ts: parse_ts(&row.get::<_, String>("ts")?),
                            metadata: row.get("metadata")?,
                        };
                        let sess = Session {
                            id: row.get("s_id")?,
                            agent: row.get("s_agent")?,
                            project: row.get("s_project")?,
                            started_at: parse_ts(&row.get::<_, String>("s_started_at")?),
                            updated_at: parse_ts(&row.get::<_, String>("s_updated_at")?),
                            metadata: row.get("s_metadata")?,
                        };
                        let raw_score: f64 = row.get("score")?;
                        Ok(TranscriptHit {
                            message: msg,
                            session: sess,
                            score: -raw_score, // FTS5 bm25 returns negative for better rank
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
                (Some(sid), None) => stmt
                    .query_map(params![query, sid, limit_i], |row| {
                        let msg = Message {
                            id: row.get("id")?,
                            session_id: row.get("session_id")?,
                            role: Role::parse(&row.get::<_, String>("role")?).unwrap_or(Role::Tool),
                            content: row.get("content")?,
                            tool_name: row.get("tool_name")?,
                            tokens: row.get("tokens")?,
                            ts: parse_ts(&row.get::<_, String>("ts")?),
                            metadata: row.get("metadata")?,
                        };
                        let sess = Session {
                            id: row.get("s_id")?,
                            agent: row.get("s_agent")?,
                            project: row.get("s_project")?,
                            started_at: parse_ts(&row.get::<_, String>("s_started_at")?),
                            updated_at: parse_ts(&row.get::<_, String>("s_updated_at")?),
                            metadata: row.get("s_metadata")?,
                        };
                        let raw_score: f64 = row.get("score")?;
                        Ok(TranscriptHit {
                            message: msg,
                            session: sess,
                            score: -raw_score,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
                (None, Some(p)) => stmt
                    .query_map(params![query, p, limit_i], |row| {
                        let msg = Message {
                            id: row.get("id")?,
                            session_id: row.get("session_id")?,
                            role: Role::parse(&row.get::<_, String>("role")?).unwrap_or(Role::Tool),
                            content: row.get("content")?,
                            tool_name: row.get("tool_name")?,
                            tokens: row.get("tokens")?,
                            ts: parse_ts(&row.get::<_, String>("ts")?),
                            metadata: row.get("metadata")?,
                        };
                        let sess = Session {
                            id: row.get("s_id")?,
                            agent: row.get("s_agent")?,
                            project: row.get("s_project")?,
                            started_at: parse_ts(&row.get::<_, String>("s_started_at")?),
                            updated_at: parse_ts(&row.get::<_, String>("s_updated_at")?),
                            metadata: row.get("s_metadata")?,
                        };
                        let raw_score: f64 = row.get("score")?;
                        Ok(TranscriptHit {
                            message: msg,
                            session: sess,
                            score: -raw_score,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
                (None, None) => stmt
                    .query_map(params![query, limit_i], |row| {
                        let msg = Message {
                            id: row.get("id")?,
                            session_id: row.get("session_id")?,
                            role: Role::parse(&row.get::<_, String>("role")?).unwrap_or(Role::Tool),
                            content: row.get("content")?,
                            tool_name: row.get("tool_name")?,
                            tokens: row.get("tokens")?,
                            ts: parse_ts(&row.get::<_, String>("ts")?),
                            metadata: row.get("metadata")?,
                        };
                        let sess = Session {
                            id: row.get("s_id")?,
                            agent: row.get("s_agent")?,
                            project: row.get("s_project")?,
                            started_at: parse_ts(&row.get::<_, String>("s_started_at")?),
                            updated_at: parse_ts(&row.get::<_, String>("s_updated_at")?),
                            metadata: row.get("s_metadata")?,
                        };
                        let raw_score: f64 = row.get("score")?;
                        Ok(TranscriptHit {
                            message: msg,
                            session: sess,
                            score: -raw_score,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?,
            })
        };
        let rows = match compute() {
            Ok(rows) => rows,
            Err(e) if is_fts5_syntax_error(&e) => Vec::new(),
            Err(e) => return Err(db_err(e)),
        };
        Ok(rows)
    }

    fn forget_session(&self, id: &str) -> IcmResult<()> {
        let conn = &self.conn;
        // Explicit delete of messages (in case FK cascade isn't enabled on older DBs).
        conn.execute("DELETE FROM messages WHERE session_id = ?1", params![id])
            .map_err(db_err)?;
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(db_err)?;
        Ok(())
    }

    fn transcript_stats(&self) -> IcmResult<TranscriptStats> {
        let conn = &self.conn;

        let total_sessions: usize = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .map_err(db_err)?;
        let total_messages: usize = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .map_err(db_err)?;
        let total_bytes: u64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM messages",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(db_err)? as u64;

        let mut stmt_role = conn
            .prepare("SELECT role, COUNT(*) FROM messages GROUP BY role ORDER BY 2 DESC")
            .map_err(db_err)?;
        let by_role: Vec<(String, usize)> = stmt_role
            .query_map([], |r: &rusqlite::Row<'_>| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;

        let mut stmt_agent = conn
            .prepare("SELECT agent, COUNT(*) FROM sessions GROUP BY agent ORDER BY 2 DESC")
            .map_err(db_err)?;
        let by_agent: Vec<(String, usize)> = stmt_agent
            .query_map([], |r: &rusqlite::Row<'_>| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;

        let mut stmt_top = conn
            .prepare(
                "SELECT session_id, COUNT(*) FROM messages
                 GROUP BY session_id ORDER BY 2 DESC LIMIT 10",
            )
            .map_err(db_err)?;
        let top_sessions: Vec<(String, usize)> = stmt_top
            .query_map([], |r: &rusqlite::Row<'_>| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })
            .map_err(db_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_err)?;

        let (oldest, newest): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = conn
            .query_row(
                "SELECT MIN(ts), MAX(ts) FROM messages",
                [],
                |r: &rusqlite::Row<'_>| {
                    let o: Option<String> = r.get(0)?;
                    let n: Option<String> = r.get(1)?;
                    Ok((o.as_deref().map(parse_ts), n.as_deref().map(parse_ts)))
                },
            )
            .unwrap_or((None, None));

        Ok(TranscriptStats {
            total_sessions,
            total_messages,
            total_bytes,
            by_role,
            by_agent,
            top_sessions,
            oldest,
            newest,
        })
    }
}
