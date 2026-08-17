//! OpenSearch backend -- split out of the former monolithic opensearch.rs.
//!
//! Decay/auto-consolidate maintenance methods.

use super::*;

// Inherent methods used by the cli/mcp store/recall/hook path

impl OpenSearchStore {
    pub fn maybe_auto_decay(&self) -> IcmResult<()> {
        if self.readonly {
            return Ok(());
        }
        // Atomic-ish claim via a scripted upsert on a metadata doc: only the
        // caller that flips `changed` to true runs the decay.
        let now_ms = Utc::now().timestamp_millis();
        let resp = self.post(
            &format!("{IDX_METADATA}/_update/last_decay_at?{}&_source=true", self.refresh_param()),
            json!({
                "scripted_upsert": true,
                "upsert": {},
                "script": {
                    "lang": "painless",
                    "source": "if (ctx._source.value == null || params.now - ctx._source.value >= 86400000L) { ctx._source.value = params.now; ctx._source.changed = true; } else { ctx._source.changed = false; }",
                    "params": {"now": now_ms}
                }
            }),
        )?;
        let changed = resp
            .get("get")
            .and_then(|g| g.get("_source"))
            .and_then(|s| s.get("changed"))
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if changed {
            self.apply_decay(0.95)?;
        }
        Ok(())
    }

    /// Auto-consolidation is not yet implemented on this backend; it is a
    /// no-op (returns `false`) so the normal store path keeps working.
    pub fn auto_consolidate(&self, _topic: &str, _threshold: usize) -> IcmResult<bool> {
        Ok(false)
    }

    /// See [`Self::auto_consolidate`].
    pub fn auto_consolidate_with_embedder(
        &self,
        _topic: &str,
        _threshold: usize,
        _embedder: Option<&dyn Embedder>,
    ) -> IcmResult<bool> {
        Ok(false)
    }
}
