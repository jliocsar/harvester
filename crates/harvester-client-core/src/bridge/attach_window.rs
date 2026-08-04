//! The attach window: the live events a session emits between its `attach`
//! landing and its history reaching the frontend.
//!
//! The daemon starts streaming a session's events the instant its `attach`
//! lands (docs/DAEMON.md §attach — deliberate, and not ours to reorder), but a
//! frontend has nowhere to put them until `Event::Attached` publishes the
//! `get_entries` snapshot one round trip later. An event that arrives in that
//! window is missing from the snapshot *and* dropped by the frontend: a lost
//! `tool_execution_end` leaves a tool card spinning for the life of that chat
//! (streaming text self-repairs — every `message_update` carries the whole
//! partial — tool ends do not). So the bridge holds that window back.
//!
//! Everything here is pure and synchronous on purpose: no daemon, no pi, no
//! runtime — it is the one piece of the fix that can be tested on its own.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// How many live events one session may hold back while its history is in
/// flight. A `get_entries` round trip is milliseconds and pi emits a few
/// events per second, so this is orders of magnitude of headroom; the cap only
/// exists so a wedged fetch cannot grow a `Vec` without bound.
pub(super) const ATTACH_EVENT_BUFFER_CAP: usize = 1024;

/// What the select loop should do with an event it just offered the buffers.
pub(super) enum PushOutcome {
    /// No flow is holding this session back — deliver it live.
    Deliver(Value),
    Buffered,
    /// The cap was just hit: the held events are gone and the session is
    /// marked for resync. Worth one line on stderr.
    Overflowed,
    /// Already overflowed; dropped on the floor because the resync that
    /// follows the flush is authoritative anyway.
    Discarded,
}

/// What an attach flow finds waiting for it once `Attached` is published.
pub(super) enum FlushOutcome {
    /// Deliver these, in arrival order, before anything else.
    Events(Vec<Value>),
    /// Events were lost, so the snapshot just published is already stale.
    Overflowed,
}

enum SessionBuffer {
    Buffering(Vec<Value>),
    Overflowed,
}

/// The held-back windows, keyed by sid.
#[derive(Default)]
pub(super) struct AttachEventBuffers {
    by_session: HashMap<String, SessionBuffer>,
}

impl AttachEventBuffers {
    /// Idempotent: a resync racing an open joins the window already in flight
    /// rather than resetting it, which would discard everything held so far.
    fn register(&mut self, sid: &str) {
        self.by_session
            .entry(sid.to_string())
            .or_insert_with(|| SessionBuffer::Buffering(Vec::new()));
    }

    pub(super) fn push(&mut self, sid: &str, event: Value) -> PushOutcome {
        let Some(buffer) = self.by_session.get_mut(sid) else {
            return PushOutcome::Deliver(event);
        };

        match buffer {
            SessionBuffer::Overflowed => PushOutcome::Discarded,
            SessionBuffer::Buffering(events) if events.len() < ATTACH_EVENT_BUFFER_CAP => {
                events.push(event);

                PushOutcome::Buffered
            }
            SessionBuffer::Buffering(_) => {
                *buffer = SessionBuffer::Overflowed;

                PushOutcome::Overflowed
            }
        }
    }

    fn flush(&mut self, sid: &str) -> FlushOutcome {
        match self.by_session.remove(sid) {
            Some(SessionBuffer::Buffering(events)) => FlushOutcome::Events(events),
            Some(SessionBuffer::Overflowed) => FlushOutcome::Overflowed,
            // Another flow for the same sid flushed first; its events are out
            // and its `Attached` seeded the chat, so there is nothing owed.
            None => FlushOutcome::Events(Vec::new()),
        }
    }

    fn drop_registration(&mut self, sid: &str) {
        self.by_session.remove(sid);
    }
}

/// Owns one session's registration for the length of an attach flow. RAII
/// because the ways out are more numerous than the way through — a failed
/// attach and a failed `get_entries` both return early, and a registration
/// that outlives its flow swallows that session's live events for the life of
/// the bridge, which is strictly worse than the gap it closes. Same shape as
/// `Pending` in `client.rs` and `ForwardGuard` in `connection.rs`.
pub(super) struct AttachRegistration {
    buffers: Arc<Mutex<AttachEventBuffers>>,
    sid: String,
    /// Set by [`AttachRegistration::flush`], which already removed the entry;
    /// without it a `Drop` landing after a same-sid re-registration would wipe
    /// somebody else's window.
    flushed: bool,
}

impl AttachRegistration {
    /// Call this *before* the `attach` request goes out — the streaming starts
    /// the moment it lands, not when its response comes back.
    pub(super) fn register(
        buffers: &Arc<Mutex<AttachEventBuffers>>,
        sid: &str,
    ) -> AttachRegistration {
        lock_attach_buffers(buffers).register(sid);

        AttachRegistration {
            buffers: buffers.clone(),
            sid: sid.to_string(),
            flushed: false,
        }
    }

    /// The held events, in arrival order. Consuming `self` makes "flushed" and
    /// "registration released" the same event.
    pub(super) fn flush(mut self) -> FlushOutcome {
        self.flushed = true;

        lock_attach_buffers(&self.buffers).flush(&self.sid)
    }
}

impl Drop for AttachRegistration {
    fn drop(&mut self) {
        if self.flushed {
            return;
        }

        lock_attach_buffers(&self.buffers).drop_registration(&self.sid);
    }
}

/// std::sync — every use is a lock/mutate/unlock with no await inside.
pub(super) fn lock_attach_buffers(
    buffers: &Arc<Mutex<AttachEventBuffers>>,
) -> std::sync::MutexGuard<'_, AttachEventBuffers> {
    buffers.lock().expect("attach buffer mutex")
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

    fn push(buffers: &Arc<Mutex<AttachEventBuffers>>, sid: &str, event: Value) -> PushOutcome {
        lock_attach_buffers(buffers).push(sid, event)
    }

    #[test]
    fn an_unregistered_session_is_delivered_live() {
        let buffers = buffers();

        assert!(
            matches!(
                push(&buffers, "01H", pi_event("agent_settled")),
                PushOutcome::Deliver(_)
            ),
            "buffering is only for the attach window; everything else streams"
        );
    }

    #[test]
    fn the_window_is_flushed_in_arrival_order() {
        let buffers = buffers();
        let registration = AttachRegistration::register(&buffers, "01H");

        for kind in [
            "tool_execution_start",
            "tool_execution_end",
            "agent_settled",
        ] {
            assert!(matches!(
                push(&buffers, "01H", pi_event(kind)),
                PushOutcome::Buffered
            ));
        }

        let FlushOutcome::Events(events) = registration.flush() else {
            panic!("three events is nowhere near the cap");
        };

        assert_eq!(
            events,
            vec![
                pi_event("tool_execution_start"),
                pi_event("tool_execution_end"),
                pi_event("agent_settled"),
            ],
            "a `tool_execution_end` only mutates an existing block, so order is load-bearing"
        );
    }

    #[test]
    fn another_sessions_events_are_never_held() {
        let buffers = buffers();
        let _registration = AttachRegistration::register(&buffers, "01H");

        assert!(matches!(
            push(&buffers, "02H", pi_event("agent_settled")),
            PushOutcome::Deliver(_)
        ));
    }

    #[test]
    fn a_dropped_registration_reopens_the_live_stream() {
        // The failure mode that matters: a failed attach or a failed
        // `get_entries` returns early, and a registration that outlived it
        // would swallow that session's events for the life of the bridge.
        let buffers = buffers();

        {
            let _registration = AttachRegistration::register(&buffers, "01H");

            assert!(matches!(
                push(&buffers, "01H", pi_event("agent_settled")),
                PushOutcome::Buffered
            ));
        }

        assert!(matches!(
            push(&buffers, "01H", pi_event("agent_settled")),
            PushOutcome::Deliver(_)
        ));
    }

    #[test]
    fn overflowing_the_window_discards_it_and_asks_for_a_resync() {
        let buffers = buffers();
        let registration = AttachRegistration::register(&buffers, "01H");

        for _ in 0..ATTACH_EVENT_BUFFER_CAP {
            assert!(matches!(
                push(&buffers, "01H", pi_event("message_update")),
                PushOutcome::Buffered
            ));
        }

        assert!(
            matches!(
                push(&buffers, "01H", pi_event("message_update")),
                PushOutcome::Overflowed
            ),
            "the event past the cap trips it, and only that one logs"
        );
        assert!(
            matches!(
                push(&buffers, "01H", pi_event("message_update")),
                PushOutcome::Discarded
            ),
            "everything after is dropped — the resync is authoritative"
        );
        assert!(
            matches!(registration.flush(), FlushOutcome::Overflowed),
            "the snapshot just published is stale; mirror the daemon's event_gap"
        );
    }

    #[test]
    fn a_flushed_registration_does_not_wipe_the_next_one() {
        let buffers = buffers();
        let first = AttachRegistration::register(&buffers, "01H");

        assert!(matches!(first.flush(), FlushOutcome::Events(_)));

        let _second = AttachRegistration::register(&buffers, "01H");

        assert!(
            matches!(
                push(&buffers, "01H", pi_event("agent_settled")),
                PushOutcome::Buffered
            ),
            "the consumed guard's Drop must not release a window it no longer owns"
        );
    }

    #[test]
    fn a_second_registration_joins_the_window_already_in_flight() {
        // An `event_gap` resync racing an open: resetting the buffer here
        // would drop everything the open flow is already holding.
        let buffers = buffers();
        let open = AttachRegistration::register(&buffers, "01H");

        assert!(matches!(
            push(&buffers, "01H", pi_event("tool_execution_end")),
            PushOutcome::Buffered
        ));

        let resync = AttachRegistration::register(&buffers, "01H");
        let FlushOutcome::Events(events) = open.flush() else {
            panic!("one event is under the cap");
        };

        assert_eq!(events, vec![pi_event("tool_execution_end")]);
        assert!(
            matches!(resync.flush(), FlushOutcome::Events(events) if events.is_empty()),
            "the loser of the race owes nothing — the winner already delivered"
        );
    }
}
