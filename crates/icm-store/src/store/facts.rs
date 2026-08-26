//! SQLite backend — split out of the former monolithic `store.rs`.
//!
//! `SqliteStore` and the shared row/parse helpers live in `super`
//! (`store/mod.rs`); each submodule here holds one trait impl (or a
//! coherent group of inherent methods) on that type.

use super::*;
use rusqlite::OptionalExtension;

fn row_to_fact(row: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    let created_at: String = row.get("created_at")?;
    let superseded_at: Option<String> = row.get("superseded_at")?;
    Ok(Fact {
        id: row.get("id")?,
        entity: row.get("entity")?,
        key: row.get("key")?,
        value: row.get("value")?,
        source: row.get("source")?,
        created_at: chrono::DateTime::parse_from_rfc3339(&created_at)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
        superseded_at: superseded_at.and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
        }),
    })
}

impl SqliteStore {
    /// List every **active** fact in the database across all entities.
    /// Used by `icm export` to produce a full snapshot.
    pub fn list_all_facts(&self) -> IcmResult<Vec<icm_core::Fact>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, entity, key, value, source, created_at, superseded_at
                 FROM facts
                 WHERE superseded_at IS NULL
                 ORDER BY entity ASC, key ASC",
            )
            .map_err(db_err)?;
        let rows = stmt.query_map([], row_to_fact).map_err(db_err)?;
        collect_rows(rows)
    }

    fn set_fact_inner(
        &self,
        entity: &str,
        key: &str,
        value: &str,
        source: &str,
    ) -> IcmResult<String> {
        let conn = &self.conn;
        // Active row lookup
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, value FROM facts
                 WHERE entity = ?1 AND key = ?2 AND superseded_at IS NULL",
                params![entity, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db_err)?;

        if let Some((id, current_value)) = existing {
            if current_value == value {
                // No-op: same value re-asserted.
                return Ok(id);
            }
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE facts SET superseded_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(db_err)?;
        }

        let new = Fact::new(
            entity.to_string(),
            key.to_string(),
            value.to_string(),
            source.to_string(),
        );
        conn.execute(
            "INSERT INTO facts (id, entity, key, value, source, created_at, superseded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                new.id,
                new.entity,
                new.key,
                new.value,
                new.source,
                new.created_at.to_rfc3339(),
            ],
        )
        .map_err(db_err)?;
        Ok(new.id)
    }
}

impl FactsStore for SqliteStore {
    fn set_fact(&self, entity: &str, key: &str, value: &str, source: &str) -> IcmResult<String> {
        if entity.is_empty() || key.is_empty() {
            return Err(IcmError::InvalidInput(
                "entity and key must be non-empty".into(),
            ));
        }
        // SELECT (find active row) -> UPDATE (supersede it) -> INSERT (new
        // row) was three separate statements with no transaction around
        // them, unlike every other multi-statement write in this file
        // (audit finding: 0 BEGIN IMMEDIATE occurrences across
        // Facts/Feedback/Transcript vs 5 in MemoryStore). A crash or
        // concurrent writer between the UPDATE and the INSERT could leave
        // the entity/key with no active fact at all.
        self.conn
            .execute_batch("BEGIN IMMEDIATE;")
            .map_err(db_err)?;
        match self.set_fact_inner(entity, key, value, source) {
            Ok(id) => {
                self.conn.execute_batch("COMMIT;").map_err(db_err)?;
                Ok(id)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(e)
            }
        }
    }

    fn get_fact(&self, entity: &str, key: &str) -> IcmResult<Option<Fact>> {
        let conn = &self.conn;
        let row = conn
            .query_row(
                "SELECT id, entity, key, value, source, created_at, superseded_at
                 FROM facts
                 WHERE entity = ?1 AND key = ?2 AND superseded_at IS NULL",
                params![entity, key],
                row_to_fact,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })?;
        Ok(row)
    }

    fn list_facts(&self, entity: &str, key_prefix: Option<&str>) -> IcmResult<Vec<Fact>> {
        let conn = &self.conn;
        let rows = match key_prefix {
            Some(prefix) if !prefix.is_empty() => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, entity, key, value, source, created_at, superseded_at
                         FROM facts
                         WHERE entity = ?1 AND key LIKE ?2 AND superseded_at IS NULL
                         ORDER BY key ASC",
                    )
                    .map_err(db_err)?;
                let pattern = format!("{prefix}%");
                let rows = stmt
                    .query_map(params![entity, pattern], row_to_fact)
                    .map_err(db_err)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?
            }
            _ => {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, entity, key, value, source, created_at, superseded_at
                         FROM facts
                         WHERE entity = ?1 AND superseded_at IS NULL
                         ORDER BY key ASC",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![entity], row_to_fact)
                    .map_err(db_err)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?
            }
        };
        Ok(rows)
    }

    fn history(&self, entity: &str, key: &str) -> IcmResult<Vec<Fact>> {
        let conn = &self.conn;
        let mut stmt = conn
            .prepare(
                "SELECT id, entity, key, value, source, created_at, superseded_at
                 FROM facts
                 WHERE entity = ?1 AND key = ?2
                 ORDER BY created_at DESC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![entity, key], row_to_fact)
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    fn forget_fact(&self, entity: &str, key: &str) -> IcmResult<usize> {
        let conn = &self.conn;
        let n = conn
            .execute(
                "DELETE FROM facts WHERE entity = ?1 AND key = ?2",
                params![entity, key],
            )
            .map_err(db_err)?;
        Ok(n)
    }

    fn facts_stats(&self) -> IcmResult<FactsStats> {
        let conn = &self.conn;
        let active_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM facts WHERE superseded_at IS NULL",
                [],
                |r| r.get::<_, i64>(0).map(|v| v as usize),
            )
            .map_err(db_err)?;
        let total_count: usize = conn
            .query_row("SELECT COUNT(*) FROM facts", [], |r| {
                r.get::<_, i64>(0).map(|v| v as usize)
            })
            .map_err(db_err)?;
        let distinct_entities: usize = conn
            .query_row(
                "SELECT COUNT(DISTINCT entity) FROM facts WHERE superseded_at IS NULL",
                [],
                |r| r.get::<_, i64>(0).map(|v| v as usize),
            )
            .map_err(db_err)?;

        let mut stmt = conn
            .prepare(
                "SELECT entity, COUNT(*) as n FROM facts
                 WHERE superseded_at IS NULL
                 GROUP BY entity
                 ORDER BY n DESC
                 LIMIT 10",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let entity: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                Ok((entity, n as usize))
            })
            .map_err(db_err)?;
        let top_entities: Vec<(String, usize)> =
            rows.collect::<Result<Vec<_>, _>>().map_err(db_err)?;

        Ok(FactsStats {
            active_count,
            total_count,
            distinct_entities,
            top_entities,
        })
    }
}
