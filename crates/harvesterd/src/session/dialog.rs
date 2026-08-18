use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde_json::json;

use super::{LineProbe, Session};

const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

const TIMEOUT_EVICTION_SLACK: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct DialogRoom {
    attached: usize,
    pending: HashMap<String, Arc<[u8]>>,
}

impl DialogRoom {
    fn attach(&mut self) -> Vec<Arc<[u8]>> {
        self.attached += 1;

        self.pending.values().cloned().collect()
    }

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

    fn remove_pending(&mut self, dialog_id: &str) -> bool {
        self.pending.remove(dialog_id).is_some()
    }

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

pub(super) fn apply_policy(
    session: &Arc<Session>,
    probe: &LineProbe<'_>,
    event: &Arc<[u8]>,
) -> bool {
    let Some(dialog_id) = id_awaiting_answer(probe) else {
        return true;
    };

    if !session.remember_dialog(dialog_id, event.clone()) {
        cancel(session, dialog_id);

        return false;
    }

    if let Some(timeout_ms) = probe.timeout.filter(|milliseconds| *milliseconds > 0) {
        spawn_eviction(session, dialog_id, timeout_ms);
    }

    true
}

pub fn cancel(session: &Session, dialog_id: &str) {
    let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});

    if let Err(error) = session.write_line(&cancel) {
        eprintln!(
            "session {}: dialog {dialog_id} auto-cancel write failed: {error}",
            session.sid
        );
    }
}

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

fn id_awaiting_answer<'probe>(probe: &'probe LineProbe<'_>) -> Option<&'probe str> {
    if probe.kind.as_deref() != Some("extension_ui_request") {
        return None;
    }

    if !DIALOG_METHODS.contains(&probe.method.as_deref()?) {
        return None;
    }

    probe.id.as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

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
