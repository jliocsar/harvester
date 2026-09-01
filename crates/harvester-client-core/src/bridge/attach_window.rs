use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use harvester_proto::lock;
use serde_json::Value;

pub(super) const ATTACH_EVENT_BUFFER_CAP: usize = 1024;

pub(super) enum PushOutcome {
    Deliver(Value),
    Buffered,
    Overflowed,
    Discarded,
}

pub(super) enum FlushOutcome {
    Events(Vec<Value>),
    Overflowed,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BufferKey {
    sid: String,
    generation: u64,
}

enum SessionBuffer {
    Buffering(Vec<Value>),
    Overflowed,
}

struct BufferEntry {
    owners: HashSet<u64>,
    buffer: SessionBuffer,
}

#[derive(Default)]
pub(super) struct AttachEventBuffers {
    by_attachment: HashMap<BufferKey, BufferEntry>,
    next_owner: u64,
}

impl AttachEventBuffers {
    fn register(&mut self, sid: &str, generation: u64) -> (BufferKey, u64) {
        let key = BufferKey {
            sid: sid.to_string(),
            generation,
        };
        let owner = self.next_owner;

        self.next_owner = self
            .next_owner
            .checked_add(1)
            .expect("attach registration owner exhausted");
        self.by_attachment
            .entry(key.clone())
            .or_insert_with(|| BufferEntry {
                owners: HashSet::new(),
                buffer: SessionBuffer::Buffering(Vec::new()),
            })
            .owners
            .insert(owner);

        (key, owner)
    }

    pub(super) fn push(&mut self, sid: &str, generation: u64, event: Value) -> PushOutcome {
        let key = BufferKey {
            sid: sid.to_string(),
            generation,
        };
        let Some(entry) = self.by_attachment.get_mut(&key) else {
            return PushOutcome::Deliver(event);
        };

        match &mut entry.buffer {
            SessionBuffer::Overflowed => PushOutcome::Discarded,
            SessionBuffer::Buffering(events) if events.len() < ATTACH_EVENT_BUFFER_CAP => {
                events.push(event);

                PushOutcome::Buffered
            }
            SessionBuffer::Buffering(_) => {
                entry.buffer = SessionBuffer::Overflowed;

                PushOutcome::Overflowed
            }
        }
    }

    fn flush(&mut self, key: &BufferKey, owner: u64) -> FlushOutcome {
        let owns_entry = self
            .by_attachment
            .get(key)
            .is_some_and(|entry| entry.owners.contains(&owner));

        if !owns_entry {
            return FlushOutcome::Events(Vec::new());
        }

        match self.by_attachment.remove(key).map(|entry| entry.buffer) {
            Some(SessionBuffer::Buffering(events)) => FlushOutcome::Events(events),
            Some(SessionBuffer::Overflowed) => FlushOutcome::Overflowed,
            None => FlushOutcome::Events(Vec::new()),
        }
    }

    fn drop_registration(&mut self, key: &BufferKey, owner: u64) {
        let Some(entry) = self.by_attachment.get_mut(key) else {
            return;
        };

        if !entry.owners.remove(&owner) || !entry.owners.is_empty() {
            return;
        }

        self.by_attachment.remove(key);
    }
}

pub(super) struct AttachRegistration {
    buffers: Arc<Mutex<AttachEventBuffers>>,
    key: BufferKey,
    owner: u64,
    flushed: bool,
}

impl AttachRegistration {
    pub(super) fn register(
        buffers: &Arc<Mutex<AttachEventBuffers>>,
        sid: &str,
        generation: u64,
    ) -> Self {
        let (key, owner) = lock(buffers, "attach buffer").register(sid, generation);

        Self {
            buffers: buffers.clone(),
            key,
            owner,
            flushed: false,
        }
    }

    pub(super) fn flush(mut self) -> FlushOutcome {
        self.flushed = true;

        lock(&self.buffers, "attach buffer").flush(&self.key, self.owner)
    }
}

impl Drop for AttachRegistration {
    fn drop(&mut self) {
        if self.flushed {
            return;
        }

        lock(&self.buffers, "attach buffer").drop_registration(&self.key, self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pi_event(kind: &str) -> Value {
        json!({"type": kind})
    }

    fn buffers() -> Arc<Mutex<AttachEventBuffers>> {
        Arc::new(Mutex::new(AttachEventBuffers::default()))
    }

    fn push(
        buffers: &Arc<Mutex<AttachEventBuffers>>,
        sid: &str,
        generation: u64,
        event: Value,
    ) -> PushOutcome {
        lock(buffers, "attach buffer").push(sid, generation, event)
    }

    #[test]
    fn an_unregistered_attachment_is_delivered_live() {
        let buffers = buffers();

        assert!(matches!(
            push(&buffers, "01H", 1, pi_event("agent_settled")),
            PushOutcome::Deliver(_)
        ));
    }

    #[test]
    fn the_window_is_flushed_in_arrival_order() {
        let buffers = buffers();
        let registration = AttachRegistration::register(&buffers, "01H", 1);

        for kind in [
            "tool_execution_start",
            "tool_execution_end",
            "agent_settled",
        ] {
            assert!(matches!(
                push(&buffers, "01H", 1, pi_event(kind)),
                PushOutcome::Buffered
            ));
        }

        let FlushOutcome::Events(events) = registration.flush() else {
            panic!("three events is under the cap");
        };

        assert_eq!(
            events,
            vec![
                pi_event("tool_execution_start"),
                pi_event("tool_execution_end"),
                pi_event("agent_settled"),
            ]
        );
    }

    #[test]
    fn a_stale_generation_neither_flushes_nor_drops_its_replacement() {
        let buffers = buffers();
        let stale = AttachRegistration::register(&buffers, "01H", 1);
        let replacement = AttachRegistration::register(&buffers, "01H", 2);

        assert!(matches!(
            push(&buffers, "01H", 2, pi_event("agent_settled")),
            PushOutcome::Buffered
        ));
        drop(stale);

        let FlushOutcome::Events(events) = replacement.flush() else {
            panic!("replacement did not overflow");
        };

        assert_eq!(events, vec![pi_event("agent_settled")]);
    }

    #[test]
    fn a_joined_registration_cannot_drop_the_other_owners_window() {
        let buffers = buffers();
        let open = AttachRegistration::register(&buffers, "01H", 3);
        let repair = AttachRegistration::register(&buffers, "01H", 3);

        drop(repair);
        assert!(matches!(
            push(&buffers, "01H", 3, pi_event("tool_execution_end")),
            PushOutcome::Buffered
        ));

        let FlushOutcome::Events(events) = open.flush() else {
            panic!("joined window did not overflow");
        };

        assert_eq!(events, vec![pi_event("tool_execution_end")]);
    }

    #[test]
    fn a_consumed_owner_cannot_remove_a_later_window_with_the_same_key() {
        let buffers = buffers();
        let first = AttachRegistration::register(&buffers, "01H", 4);
        let joined = AttachRegistration::register(&buffers, "01H", 4);

        assert!(matches!(first.flush(), FlushOutcome::Events(_)));
        let replacement = AttachRegistration::register(&buffers, "01H", 4);

        drop(joined);
        assert!(matches!(
            push(&buffers, "01H", 4, pi_event("agent_settled")),
            PushOutcome::Buffered
        ));
        assert!(matches!(replacement.flush(), FlushOutcome::Events(events) if events.len() == 1));
    }

    #[test]
    fn overflowing_the_window_discards_until_the_owner_repairs() {
        let buffers = buffers();
        let registration = AttachRegistration::register(&buffers, "01H", 1);

        for _ in 0..ATTACH_EVENT_BUFFER_CAP {
            assert!(matches!(
                push(&buffers, "01H", 1, pi_event("message_update")),
                PushOutcome::Buffered
            ));
        }

        assert!(matches!(
            push(&buffers, "01H", 1, pi_event("message_update")),
            PushOutcome::Overflowed
        ));
        assert!(matches!(
            push(&buffers, "01H", 1, pi_event("message_update")),
            PushOutcome::Discarded
        ));
        assert!(matches!(registration.flush(), FlushOutcome::Overflowed));
    }
}
