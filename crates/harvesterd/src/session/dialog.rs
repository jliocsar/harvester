//! The dialog policy (docs/DAEMON.md §Dialogs): who is watching, what is
//! waiting on them, and what happens to a dialog nobody can answer.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::json;

use super::{LineProbe, Session};

/// The `extension_ui_request` methods that block pi until they are answered.
/// Fire-and-forget kinds (`notify`, `setStatus`, …) are ordinary events.
const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// Added to a dialog's own `timeout` before the daemon forgets it. Pi's timer
/// and ours start at different moments (pi arms its before writing the request;
/// ours starts when the line is read), so the slack is what keeps the daemon
/// from forgetting a dialog that pi is still genuinely waiting on — an answer
/// arriving after eviction would never be forwarded. Generous on purpose: being
/// late costs nothing (the entry is already dead), being early costs an answer.
const TIMEOUT_EVICTION_SLACK: Duration = Duration::from_secs(5);

/// The empty-room dialog policy's whole state. Counter and pending set live
/// under one mutex on purpose: "is anybody attached, and if so remember this
/// dialog" must be one atomic decision. Split across an atomic and a separate
/// lock (as it once was), a detach landing between the load and the insert
/// strands the dialog — nobody left to answer it, and nothing left to trigger
/// the cancel.
#[derive(Default)]
pub struct DialogRoom {
    attached: usize,
    /// Dialogs awaiting an `extension_ui_response`, by dialog id, each holding
    /// the pre-serialized wire envelope the pump already built. Keeping the
    /// envelope (not just the id) is what lets a client attaching *after* the
    /// dialog arrived still be shown it — otherwise a handoff between clients
    /// strands pi on a dialog nobody can see.
    pending: HashMap<String, Arc<[u8]>>,
}

/// Deliberately plain and synchronous: the locking belongs to [`Session`], so
/// the policy can be exercised without a pi child.
impl DialogRoom {
    /// Join the room; returns the dialog envelopes already waiting, which the
    /// caller's forwarder replays so a late attacher sees them too.
    fn attach(&mut self) -> Vec<Arc<[u8]>> {
        self.attached += 1;

        self.pending.values().cloned().collect()
    }

    /// Leave the room. When the last watcher goes, every still-pending dialog
    /// is returned for cancellation — nothing could answer them anymore, and
    /// pi would wait forever (docs/DAEMON.md: never leave a dialog unanswered).
    fn detach(&mut self) -> Vec<String> {
        self.attached = self.attached.saturating_sub(1);

        if self.attached > 0 {
            return Vec::new();
        }

        self.pending
            .drain()
            .map(|(dialog_id, _)| dialog_id)
            .collect()
    }

    /// The one removal path, so "answered" and "evicted" cannot drift apart in
    /// what they do to the room — only in what the caller does about it (an
    /// answer is written to pi, an eviction never is). `true` means the dialog
    /// was still pending, which is what makes the first answer the winning one.
    fn remove_pending(&mut self, dialog_id: &str) -> bool {
        self.pending.remove(dialog_id).is_some()
    }

    /// Remember a dialog (with the envelope to replay on a later attach).
    /// `false` means the room is empty, so nobody will ever answer it and the
    /// caller must cancel instead — decided in the same critical section as the
    /// insert, which is what keeps the two outcomes mutually exclusive.
    fn remember(&mut self, dialog_id: &str, envelope: Arc<[u8]>) -> bool {
        if self.attached == 0 {
            return false;
        }

        self.pending.insert(dialog_id.to_owned(), envelope);

        true
    }
}

impl Session {
    pub fn attach(&self) -> Vec<Arc<[u8]>> {
        self.lock_room().attach()
    }

    pub fn detach(&self) -> Vec<String> {
        self.lock_room().detach()
    }

    pub fn remove_pending_dialog(&self, dialog_id: &str) -> bool {
        self.lock_room().remove_pending(dialog_id)
    }

    fn remember_dialog(&self, dialog_id: &str, envelope: Arc<[u8]>) -> bool {
        self.lock_room().remember(dialog_id, envelope)
    }

    fn lock_room(&self) -> std::sync::MutexGuard<'_, DialogRoom> {
        self.room.lock().expect("dialog room mutex")
    }
}

/// Apply the policy to a stdout line that may be a dialog: while anyone is
/// attached it is remembered and forwarded (first answer wins, and a client
/// attaching later still gets shown it); with an empty room it is auto-cancelled
/// instead of broadcast into the void. `false` means "cancelled, do not forward".
pub(super) fn accept(session: &Arc<Session>, probe: &LineProbe<'_>, event: &Arc<[u8]>) -> bool {
    let Some(dialog_id) = id_awaiting_answer(probe) else {
        return true;
    };

    if !session.remember_dialog(dialog_id, event.clone()) {
        cancel(session, dialog_id);

        return false;
    }

    // Pi treats `timeout: 0` as no timeout at all (`if (opts?.timeout)` —
    // rpc-mode.ts:114), so neither do we; an eviction timer pi has no matching
    // timer for would forget a live dialog.
    if let Some(timeout_ms) = probe.timeout.filter(|milliseconds| *milliseconds > 0) {
        spawn_eviction(session, dialog_id, timeout_ms);
    }

    true
}

/// Answer a dialog with `cancelled: true` — to the extension, identical to a
/// human pressing Escape. Used at arrival (empty room) and when the room empties.
pub fn cancel(session: &Session, dialog_id: &str) {
    let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});

    if let Err(error) = session.write_line(&cancel) {
        eprintln!(
            "session {}: dialog {dialog_id} auto-cancel write failed: {error}",
            session.sid
        );
    }
}

/// Forget a `timeout`-carrying dialog once pi has resolved it internally.
///
/// Pi resolves such a dialog itself and emits **nothing** on stdout (pi 0.83.0
/// `modes/rpc/rpc-mode.ts:114-119`: the `setTimeout` callback runs `cleanup()`
/// and `resolve(defaultValue)`, with no `output(...)`), and afterwards drops any
/// response for that id silently (`:768-776`). Nothing on the wire announces it,
/// so a timer is the only way to learn about it. Evicting, never cancelling: an
/// `extension_ui_response` would answer a dialog that no longer exists. Without
/// the timer the entry is stale for the rest of the process's life — replayed to
/// every later attacher, unanswerable, cancelled for nothing at the last detach.
///
/// **Weak, never `Arc`**: a dialog may publish a timeout of hours, and the timer
/// must not keep the `Session` — and with it the broadcast channel every
/// subscriber holds — alive past the pi process it belongs to.
fn spawn_eviction(session: &Arc<Session>, dialog_id: &str, timeout_ms: u64) {
    let weak_session: Weak<Session> = Arc::downgrade(session);
    let dialog_id = dialog_id.to_owned();
    let evict_after = Duration::from_millis(timeout_ms).saturating_add(TIMEOUT_EVICTION_SLACK);

    tokio::spawn(async move {
        tokio::time::sleep(evict_after).await;

        let Some(session) = weak_session.upgrade() else {
            return;
        };

        if session.remove_pending_dialog(&dialog_id) {
            eprintln!(
                "session {}: dialog {dialog_id} resolved inside pi after {timeout_ms}ms, evicted",
                session.sid
            );
        }
    });
}

/// The dialog id when this line is an `extension_ui_request` that blocks pi
/// until it is answered.
fn id_awaiting_answer<'probe>(probe: &'probe LineProbe<'_>) -> Option<&'probe str> {
    if probe.kind.as_deref() != Some("extension_ui_request") {
        return None;
    }

    if !DIALOG_METHODS.contains(&probe.method.as_deref()?) {
        return None;
    }

    probe.id.as_deref()
}

/// The dialog room in isolation — no pi child, no tokio, no sockets. Every case
/// here is a clause of the dialog policy in docs/DAEMON.md.
#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the wire envelope the pump would have built; the room only
    /// ever stores and hands back these bytes, never inspects them.
    fn envelope(dialog_id: &str) -> Arc<[u8]> {
        Arc::from(format!(r#"{{"type":"event","id":"{dialog_id}"}}"#).into_bytes())
    }

    fn sorted(mut dialog_ids: Vec<String>) -> Vec<String> {
        dialog_ids.sort();

        dialog_ids
    }

    #[test]
    fn attach_on_empty_room_replays_nothing() {
        let mut room = DialogRoom::default();

        assert!(room.attach().is_empty());
        assert_eq!(room.attached, 1);
    }

    #[test]
    fn remember_is_refused_when_the_room_is_empty() {
        let mut room = DialogRoom::default();

        assert!(!room.remember("d1", envelope("d1")));
        assert!(room.pending.is_empty());
    }

    #[test]
    fn remember_is_accepted_while_a_watcher_is_attached() {
        let mut room = DialogRoom::default();
        room.attach();

        assert!(room.remember("d1", envelope("d1")));
        assert_eq!(room.pending.len(), 1);
    }

    #[test]
    fn a_pending_dialog_is_replayed_to_a_later_attacher() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));

        let replayed = room.attach();

        assert_eq!(replayed, vec![envelope("d1")]);
    }

    #[test]
    fn detach_with_another_watcher_left_cancels_nothing() {
        let mut room = DialogRoom::default();
        room.attach();
        room.attach();
        room.remember("d1", envelope("d1"));

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 1);
        assert_eq!(room.pending.len(), 1);
    }

    #[test]
    fn the_last_detach_drains_every_pending_dialog() {
        let mut room = DialogRoom::default();
        room.attach();
        room.attach();
        room.remember("d1", envelope("d1"));
        room.remember("d2", envelope("d2"));
        room.detach();

        let to_cancel = room.detach();

        assert_eq!(sorted(to_cancel), vec!["d1".to_owned(), "d2".to_owned()]);
        assert_eq!(room.attached, 0);
        assert!(room.pending.is_empty());
    }

    #[test]
    fn only_the_first_answer_wins() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));

        assert!(room.remove_pending("d1"));
        assert!(!room.remove_pending("d1"));
    }

    #[test]
    fn removing_an_unknown_dialog_is_refused() {
        let mut room = DialogRoom::default();
        room.attach();

        assert!(!room.remove_pending("nope"));
    }

    #[test]
    fn a_removed_dialog_is_no_longer_replayed_on_attach() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));
        room.remove_pending("d1");

        assert!(room.attach().is_empty());
    }

    #[test]
    fn detaching_more_often_than_attaching_saturates_at_zero() {
        let mut room = DialogRoom::default();
        room.attach();
        room.detach();

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 0);

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 0);
    }

    /// The whole point of evicting rather than cancelling: pi already resolved
    /// the timed-out dialog, so a cancel would answer something gone — the
    /// entry must simply not be in the last detach's cancel list.
    #[test]
    fn an_evicted_dialog_is_not_cancelled_when_the_room_empties() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("timed_out", envelope("timed_out"));
        room.remember("still_open", envelope("still_open"));
        room.remove_pending("timed_out");

        let to_cancel = room.detach();

        assert_eq!(to_cancel, vec!["still_open".to_owned()]);
    }

    /// The wire field the eviction timer keys on, straight off pi's own
    /// `extension_ui_request` shape (`rpc-mode.ts:137,142,147`).
    fn probe(line: &str) -> LineProbe<'_> {
        serde_json::from_str(line).expect("fixture line must fit the probe")
    }

    #[test]
    fn a_dialog_timeout_is_read_off_the_wire() {
        let parsed = probe(
            r#"{"type":"extension_ui_request","id":"d1","method":"confirm",
                "title":"Deploy?","message":"to prod","timeout":30000}"#,
        );

        assert_eq!(parsed.timeout, Some(30_000));
        assert_eq!(id_awaiting_answer(&parsed), Some("d1"));
    }

    #[test]
    fn a_dialog_without_a_timeout_probes_as_none() {
        // `editor` never carries one (rpc-mode.ts:253-269), and neither does a
        // dialog whose extension passed no `opts.timeout`.
        let parsed =
            probe(r#"{"type":"extension_ui_request","id":"d1","method":"editor","title":"t"}"#);

        assert_eq!(parsed.timeout, None);
    }

    #[test]
    fn a_fire_and_forget_request_is_not_a_dialog() {
        let parsed = probe(r#"{"type":"extension_ui_request","id":"n1","method":"notify"}"#);

        assert_eq!(id_awaiting_answer(&parsed), None);
    }
}
