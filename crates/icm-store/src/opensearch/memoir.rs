//! OpenSearch backend -- split out of the former monolithic opensearch.rs.
//!
//! Unsupported on this backend (issue #301) -- see mod.rs.

use super::*;

impl MemoirStore for OpenSearchStore {
    fn create_memoir(&self, _memoir: Memoir) -> IcmResult<String> {
        unsupported("memoir.create_memoir")
    }
    fn get_memoir(&self, _id: &str) -> IcmResult<Option<Memoir>> {
        unsupported("memoir.get_memoir")
    }
    fn get_memoir_by_name(&self, _name: &str) -> IcmResult<Option<Memoir>> {
        unsupported("memoir.get_memoir_by_name")
    }
    fn update_memoir(&self, _memoir: &Memoir) -> IcmResult<()> {
        unsupported("memoir.update_memoir")
    }
    fn delete_memoir(&self, _id: &str) -> IcmResult<()> {
        unsupported("memoir.delete_memoir")
    }
    fn list_memoirs(&self) -> IcmResult<Vec<Memoir>> {
        unsupported("memoir.list_memoirs")
    }
    fn add_concept(&self, _concept: Concept) -> IcmResult<String> {
        unsupported("memoir.add_concept")
    }
    fn get_concept(&self, _id: &str) -> IcmResult<Option<Concept>> {
        unsupported("memoir.get_concept")
    }
    fn get_concept_by_name(&self, _memoir_id: &str, _name: &str) -> IcmResult<Option<Concept>> {
        unsupported("memoir.get_concept_by_name")
    }
    fn update_concept(&self, _concept: &Concept) -> IcmResult<()> {
        unsupported("memoir.update_concept")
    }
    fn delete_concept(&self, _id: &str) -> IcmResult<()> {
        unsupported("memoir.delete_concept")
    }
    fn list_concepts(&self, _memoir_id: &str) -> IcmResult<Vec<Concept>> {
        unsupported("memoir.list_concepts")
    }
    fn search_concepts_fts(
        &self,
        _memoir_id: &str,
        _query: &str,
        _limit: usize,
    ) -> IcmResult<Vec<Concept>> {
        unsupported("memoir.search_concepts_fts")
    }
    fn search_concepts_by_label(
        &self,
        _memoir_id: &str,
        _label: &Label,
        _limit: usize,
    ) -> IcmResult<Vec<Concept>> {
        unsupported("memoir.search_concepts_by_label")
    }
    fn search_all_concepts_fts(&self, _query: &str, _limit: usize) -> IcmResult<Vec<Concept>> {
        unsupported("memoir.search_all_concepts_fts")
    }
    fn refine_concept(
        &self,
        _id: &str,
        _new_definition: &str,
        _new_source_ids: &[String],
    ) -> IcmResult<()> {
        unsupported("memoir.refine_concept")
    }
    fn add_link(&self, _link: ConceptLink) -> IcmResult<String> {
        unsupported("memoir.add_link")
    }
    fn get_links_from(&self, _concept_id: &str) -> IcmResult<Vec<ConceptLink>> {
        unsupported("memoir.get_links_from")
    }
    fn get_links_to(&self, _concept_id: &str) -> IcmResult<Vec<ConceptLink>> {
        unsupported("memoir.get_links_to")
    }
    fn delete_link(&self, _id: &str) -> IcmResult<()> {
        unsupported("memoir.delete_link")
    }
    fn get_neighbors(
        &self,
        _concept_id: &str,
        _relation: Option<Relation>,
    ) -> IcmResult<Vec<Concept>> {
        unsupported("memoir.get_neighbors")
    }
    fn get_neighborhood(
        &self,
        _concept_id: &str,
        _depth: usize,
    ) -> IcmResult<(Vec<Concept>, Vec<ConceptLink>)> {
        unsupported("memoir.get_neighborhood")
    }
    fn get_links_for_memoir(&self, _memoir_id: &str) -> IcmResult<Vec<ConceptLink>> {
        unsupported("memoir.get_links_for_memoir")
    }
    fn memoir_stats(&self, _memoir_id: &str) -> IcmResult<MemoirStats> {
        unsupported("memoir.memoir_stats")
    }
    fn batch_memoir_concept_counts(&self) -> IcmResult<HashMap<String, usize>> {
        unsupported("memoir.batch_memoir_concept_counts")
    }
}
