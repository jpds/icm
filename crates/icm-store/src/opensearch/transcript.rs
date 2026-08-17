//! OpenSearch backend -- split out of the former monolithic opensearch.rs.
//!
//! Unsupported on this backend (issue #301) -- see mod.rs.

use super::*;

impl TranscriptStore for OpenSearchStore {
    fn create_session(
        &self,
        _agent: &str,
        _project: Option<&str>,
        _metadata: Option<&str>,
    ) -> IcmResult<String> {
        unsupported("transcript.create_session")
    }
    fn ensure_session(
        &self,
        _id: &str,
        _agent: &str,
        _project: Option<&str>,
        _metadata: Option<&str>,
    ) -> IcmResult<String> {
        unsupported("transcript.ensure_session")
    }
    fn get_session(&self, _id: &str) -> IcmResult<Option<Session>> {
        unsupported("transcript.get_session")
    }
    fn list_sessions(&self, _project: Option<&str>, _limit: usize) -> IcmResult<Vec<Session>> {
        unsupported("transcript.list_sessions")
    }
    fn record_message(
        &self,
        _session_id: &str,
        _role: Role,
        _content: &str,
        _tool_name: Option<&str>,
        _tokens: Option<i64>,
        _metadata: Option<&str>,
    ) -> IcmResult<String> {
        unsupported("transcript.record_message")
    }
    fn list_session_messages(
        &self,
        _session_id: &str,
        _limit: usize,
        _offset: usize,
    ) -> IcmResult<Vec<Message>> {
        unsupported("transcript.list_session_messages")
    }
    fn search_transcripts(
        &self,
        _query: &str,
        _session_id: Option<&str>,
        _project: Option<&str>,
        _limit: usize,
    ) -> IcmResult<Vec<TranscriptHit>> {
        unsupported("transcript.search_transcripts")
    }
    fn forget_session(&self, _id: &str) -> IcmResult<()> {
        unsupported("transcript.forget_session")
    }
    fn transcript_stats(&self) -> IcmResult<TranscriptStats> {
        unsupported("transcript.transcript_stats")
    }
}
