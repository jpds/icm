//! PostgreSQL backend -- split out of the former monolithic postgres.rs.
//!
//! Decay/auto-consolidate maintenance methods.

use super::*;

impl PostgresStore {
    /// Apply decay if more than 24 hours since the last run. Mirrors the
    /// SQLite backend's atomic check-and-claim via `icm_metadata`.
    pub fn maybe_auto_decay(&self) -> IcmResult<()> {
        if self.readonly {
            return Ok(());
        }
        let now = Utc::now();
        let claimed = {
            let mut c = self.conn()?;
            c.execute(
                "INSERT INTO icm_metadata (key, value) VALUES ('last_decay_at', $1)
                 ON CONFLICT (key) DO UPDATE SET value = $1
                 WHERE icm_metadata.value IS NULL
                    OR ($1::timestamptz - icm_metadata.value::timestamptz) >= interval '1 day'",
                &[&now.to_rfc3339()],
            )
            .map_err(pg_err)?
        };
        if claimed > 0 {
            self.apply_decay(0.95)?;
        }
        Ok(())
    }
}
