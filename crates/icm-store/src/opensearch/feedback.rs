//! OpenSearch backend -- split out of the former monolithic opensearch.rs.
//!
//! Unsupported on this backend (issue #301) -- see mod.rs.

use super::*;

impl FeedbackStore for OpenSearchStore {
    fn store_feedback(&self, _feedback: Feedback) -> IcmResult<String> {
        unsupported("feedback.store_feedback")
    }
    fn search_feedback(
        &self,
        _query: &str,
        _query_embedding: Option<&[f32]>,
        _topic: Option<&str>,
        _limit: usize,
    ) -> IcmResult<Vec<Feedback>> {
        unsupported("feedback.search_feedback")
    }
    fn list_feedback(&self, _topic: Option<&str>, _limit: usize) -> IcmResult<Vec<Feedback>> {
        unsupported("feedback.list_feedback")
    }
    fn increment_applied(&self, _id: &str) -> IcmResult<()> {
        unsupported("feedback.increment_applied")
    }
    fn delete_feedback(&self, _id: &str) -> IcmResult<()> {
        unsupported("feedback.delete_feedback")
    }
    fn feedback_stats(&self) -> IcmResult<FeedbackStats> {
        unsupported("feedback.feedback_stats")
    }
}
