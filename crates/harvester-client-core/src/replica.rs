use std::sync::Arc;

use serde_json::Value;

use crate::{SubmissionId, Transcript};

#[derive(Debug, Clone)]
struct LiveEventNode {
    event: Value,
    previous: Option<Arc<Self>>,
}

#[derive(Debug, Clone, Default)]
struct LiveEventLog {
    tail: Option<Arc<LiveEventNode>>,
    len: usize,
}

impl LiveEventLog {
    fn push(&mut self, event: Value) {
        self.tail = Some(Arc::new(LiveEventNode {
            event,
            previous: self.tail.clone(),
        }));
        self.len = self.len.checked_add(1).expect("live event count exhausted");
    }

    fn iter(&self) -> impl ExactSizeIterator<Item = &Value> {
        let mut events = Vec::with_capacity(self.len);
        let mut current = self.tail.as_deref();

        while let Some(node) = current {
            events.push(&node.event);
            current = node.previous.as_deref();
        }

        events.into_iter().rev()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionReplica {
    entries: Arc<Vec<Value>>,
    live_events: LiveEventLog,
    transcript: Transcript,
    revision: u64,
}

impl SessionReplica {
    pub fn from_entries(entries: &[Value]) -> Self {
        Self {
            entries: Arc::new(entries.to_vec()),
            live_events: LiveEventLog::default(),
            transcript: Transcript::new(entries),
            revision: 0,
        }
    }

    pub fn entries(&self) -> &[Value] {
        &self.entries
    }

    pub fn live_events(&self) -> impl ExactSizeIterator<Item = &Value> {
        self.live_events.iter()
    }

    pub const fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub(crate) fn replace_history(&mut self, entries: &[Value]) {
        self.replace_history_owned(entries.to_vec());
    }

    pub(crate) fn replace_history_owned(&mut self, entries: Vec<Value>) {
        self.transcript.replace_history(&entries);
        self.entries = Arc::new(entries);
        self.live_events = LiveEventLog::default();
        self.advance_revision();
    }

    #[cfg(test)]
    pub(crate) fn extend_history(&mut self, entries: &[Value]) {
        self.extend_history_owned(entries.to_vec());
    }

    pub(crate) fn extend_history_owned(&mut self, entries: Vec<Value>) {
        self.transcript.extend_history(&entries);
        Arc::make_mut(&mut self.entries).extend(entries);
        self.live_events = LiveEventLog::default();
        self.advance_revision();
    }

    #[cfg(test)]
    pub(crate) fn apply_event(&mut self, event: &Value) -> bool {
        self.apply_event_owned(event.clone())
    }

    pub(crate) fn apply_event_owned(&mut self, event: Value) -> bool {
        let applied = self.transcript.apply(&event);

        self.live_events.push(event);
        self.advance_revision();

        applied
    }

    pub(crate) fn push_user_prompt(&mut self, submission: SubmissionId, text: String) {
        self.transcript.push_user_prompt(submission, text);
        self.advance_revision();
    }

    pub(crate) fn reconcile_streaming(&mut self, streaming: bool) {
        self.transcript.reconcile_streaming(streaming);
        self.advance_revision();
    }

    pub(crate) fn fail_prompt(
        &mut self,
        submission: SubmissionId,
        detail: String,
        turn_was_busy: bool,
    ) {
        self.transcript
            .fail_prompt(submission, detail, turn_was_busy);
        self.advance_revision();
    }

    pub(crate) fn end_turn(&mut self) {
        self.transcript.end_turn();
        self.advance_revision();
    }

    const fn advance_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("session replica revision exhausted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_custom_entries_survive_history_construction_and_extension() {
        let custom = serde_json::json!({
            "type": "custom",
            "id": "custom-1",
            "parentId": null,
            "customType": "future-renderer",
            "data": {"arbitrary": [1, {"nested": true}]}
        });
        let extension = serde_json::json!({
            "type": "custom_message",
            "id": "custom-2",
            "parentId": "custom-1",
            "content": {"not": "normalized"}
        });
        let mut replica = SessionReplica::from_entries(std::slice::from_ref(&custom));

        replica.extend_history(std::slice::from_ref(&extension));

        assert_eq!(replica.entries(), &[custom, extension]);
    }

    #[test]
    fn raw_live_events_are_retained_until_authoritative_history_replaces_them() {
        let event = serde_json::json!({
            "type": "extension_future_event",
            "details": {"unknown": [1, 2, 3]}
        });
        let mut replica = SessionReplica::default();

        assert!(!replica.apply_event(&event));
        assert_eq!(replica.live_events().collect::<Vec<_>>(), vec![&event]);

        replica.replace_history(&[serde_json::json!({"type": "custom", "id": "e"})]);

        assert_eq!(replica.live_events().len(), 0);
    }

    #[test]
    fn cumulative_stream_updates_remain_lossless_in_the_raw_log() {
        let mut replica = SessionReplica::default();

        for text in ["a", "ab", "abc"] {
            replica.apply_event(&serde_json::json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "partial": {"role": "assistant", "content": text}
                }
            }));
        }

        let retained = replica.live_events().collect::<Vec<_>>();

        assert_eq!(retained.len(), 3);
        assert_eq!(
            retained[2]["assistantMessageEvent"]["partial"]["content"],
            "abc"
        );
    }

    #[test]
    fn raw_live_retention_does_not_drop_older_unknown_events() {
        let mut replica = SessionReplica::default();

        for sequence in 0..300 {
            replica.apply_event(&serde_json::json!({
                "type": "future_event",
                "sequence": sequence
            }));
        }

        assert_eq!(replica.live_events().len(), 300);
        assert_eq!(
            replica
                .live_events()
                .next()
                .and_then(|event| event["sequence"].as_u64()),
            Some(0)
        );
        assert_eq!(replica.revision(), 300);
    }
}
