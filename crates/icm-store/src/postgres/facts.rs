//! PostgreSQL backend -- split out of the former monolithic postgres.rs.
//!
//! Unsupported on this backend (issue #301) -- see mod.rs.

use super::*;

impl FactsStore for PostgresStore {
    fn set_fact(
        &self,
        _entity: &str,
        _key: &str,
        _value: &str,
        _source: &str,
    ) -> IcmResult<String> {
        unsupported("facts.set_fact")
    }
    fn get_fact(&self, _entity: &str, _key: &str) -> IcmResult<Option<Fact>> {
        unsupported("facts.get_fact")
    }
    fn list_facts(&self, _entity: &str, _key_prefix: Option<&str>) -> IcmResult<Vec<Fact>> {
        unsupported("facts.list_facts")
    }
    fn history(&self, _entity: &str, _key: &str) -> IcmResult<Vec<Fact>> {
        unsupported("facts.history")
    }
    fn forget_fact(&self, _entity: &str, _key: &str) -> IcmResult<usize> {
        unsupported("facts.forget_fact")
    }
    fn facts_stats(&self) -> IcmResult<FactsStats> {
        unsupported("facts.facts_stats")
    }
}
