//! SQLite backend — split out of the former monolithic `store.rs`.
//!
//! `SqliteStore` and the shared row/parse helpers live in `super`
//! (`store/mod.rs`); each submodule here holds one trait impl (or a
//! coherent group of inherent methods) on that type.

use super::*;

/// In-process LRU cache size for hot memories. Each entry is one
/// fully-hydrated `Memory` (incl. optional 384×f32 embedding ≈ 1.5KB),
/// so 256 entries cap RAM at ~400KB worst case. Helps long-running
/// processes (`icm serve`, TUI) where the same memories are read
/// repeatedly; zero benefit in one-shot CLI invocations beyond the
/// single recall flow.
pub(crate) const MEMORY_CACHE_CAP: usize = 256;

impl SqliteStore {
    pub(super) fn cache_get(&self, id: &str) -> Option<Memory> {
        self.cache.lock().ok().and_then(|mut c| c.get(id).cloned())
    }

    pub(super) fn cache_put(&self, m: &Memory) {
        if let Ok(mut c) = self.cache.lock() {
            c.put(m.id.clone(), m.clone());
        }
    }

    pub(super) fn cache_invalidate(&self, id: &str) {
        if let Ok(mut c) = self.cache.lock() {
            c.pop(id);
        }
    }

    pub(super) fn cache_invalidate_many(&self, ids: &[&str]) {
        if let Ok(mut c) = self.cache.lock() {
            for id in ids {
                c.pop(*id);
            }
        }
    }

    pub(super) fn cache_clear(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.clear();
        }
    }
}

pub(crate) fn new_cache() -> LruCache<String, Memory> {
    let cap = NonZeroUsize::new(MEMORY_CACHE_CAP)
        .expect("MEMORY_CACHE_CAP must be non-zero — see store.rs");
    LruCache::new(cap)
}
