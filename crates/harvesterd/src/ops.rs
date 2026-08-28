use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{
    AttachData, CreateSessionData, HarvesterCmd, HarvesterResponse, Outgoing, PingData,
    SessionsData,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::daemon::{Daemon, SpawnOutcome};
use crate::fslist;
use crate::models;
use crate::server::{answer_later, Outbound};
use crate::session::{self, Session, SessionOutput, SpawnArgs};
use crate::sid::Sid;

const STOP_KILL_BACKSTOP: Duration = Duration::from_secs(30);

pub struct Attachment {
    session: Arc<Session>,
    forwarder: JoinHandle<()>,
}

pub type AttachedMap = HashMap<Sid, Attachment>;

impl Drop for Attachment {
    fn drop(&mut self) {
        self.forwarder.abort();

        for dialog_id in self.session.detach() {
            session::cancel_dialog(&self.session, &dialog_id);
        }
    }
}

pub fn dispatch(
    daemon: &Daemon,
    id: Option<Value>,
    cmd: HarvesterCmd,
    out_tx: &mpsc::Sender<Outbound>,
    attached: &mut AttachedMap,
) -> Option<Outgoing> {
    let response = match cmd {
        HarvesterCmd::Ping { version } => ping(daemon, version),
        HarvesterCmd::ListSessions => {
            spawn_list_sessions(daemon, id, out_tx);

            return None;
        }
        HarvesterCmd::CreateSession {
            sid,
            cwd,
            provider,
            model,
            name,
        } => with_checked_sid(&sid, |sid| {
            create_session(
                daemon,
                SpawnArgs {
                    sid,
                    cwd,
                    provider,
                    model,
                    name,
                },
            )
        }),
        HarvesterCmd::ListModels { cwd } => {
            spawn_list_models(id, cwd, out_tx);

            return None;
        }
        HarvesterCmd::Attach { sid } => {
            with_checked_sid(&sid, |sid| attach(daemon, sid, out_tx, attached))
        }
        HarvesterCmd::Detach { sid } => with_checked_sid(&sid, |sid| detach(sid, attached)),
        HarvesterCmd::Stop { sid } => with_checked_sid(&sid, |sid| stop(daemon, sid)),
    };

    Some(Outgoing::HarvesterResponse { id, response })
}

fn with_checked_sid(
    raw_sid: &str,
    handle: impl FnOnce(Sid) -> HarvesterResponse,
) -> HarvesterResponse {
    match Sid::parse(raw_sid) {
        Ok(sid) => handle(sid),
        Err(rejection) => HarvesterResponse::err(rejection),
    }
}

fn ping(daemon: &Daemon, client_version: u32) -> HarvesterResponse {
    if client_version != harvester_proto::PROTOCOL_VERSION {
        eprintln!(
            "ping from a client speaking protocol v{client_version}; we speak v{}",
            harvester_proto::PROTOCOL_VERSION
        );
    }

    data_response(PingData {
        version: harvester_proto::PROTOCOL_VERSION,
        build: daemon.build_fingerprint().to_string(),
    })
}

pub fn no_such_session_message(sid: &Sid) -> String {
    format!("no running session '{sid}'")
}

fn no_such_session(sid: &Sid) -> HarvesterResponse {
    HarvesterResponse::err(no_such_session_message(sid))
}

fn data_response(data: impl serde::Serialize) -> HarvesterResponse {
    HarvesterResponse::ok(serde_json::to_value(data).expect("serializable"))
}

fn spawn_list_sessions(daemon: &Daemon, id: Option<Value>, out_tx: &mpsc::Sender<Outbound>) {
    let running = daemon.running_sessions();

    answer_later(out_tx, async move {
        let scan = tokio::task::spawn_blocking(move || fslist::list_sessions(&running));
        let response = match scan.await {
            Ok(sessions) => data_response(SessionsData { sessions }),
            Err(join_error) => HarvesterResponse::err(format!("list_sessions: {join_error}")),
        };

        Outgoing::HarvesterResponse { id, response }
    });
}

fn create_session(daemon: &Daemon, args: SpawnArgs) -> HarvesterResponse {
    let (session, already) = match daemon.spawn_session(args) {
        Ok(SpawnOutcome::Spawned(session)) => (session, false),
        Ok(SpawnOutcome::AlreadyRunning(session)) => (session, true),
        Err(error) => return HarvesterResponse::err(format!("spawn failed: {error}")),
    };

    data_response(CreateSessionData {
        sid: session.sid.to_string(),
        already,
    })
}

fn spawn_list_models(id: Option<Value>, cwd: String, out_tx: &mpsc::Sender<Outbound>) {
    answer_later(out_tx, async move {
        let response = models::list_models(cwd).await;

        Outgoing::HarvesterResponse { id, response }
    });
}

fn attach(
    daemon: &Daemon,
    sid: Sid,
    out_tx: &mpsc::Sender<Outbound>,
    attached: &mut AttachedMap,
) -> HarvesterResponse {
    let Some(session) = daemon.session(sid.as_str()) else {
        return no_such_session(&sid);
    };

    let already_attached_to_this_generation = attached
        .get(sid.as_str())
        .is_some_and(|existing| Arc::ptr_eq(&existing.session, &session));

    if already_attached_to_this_generation {
        return data_response(AttachData { already: true });
    }

    let events = session.output.subscribe();
    let pending_dialogs = session.attach();
    let forwarder = spawn_forwarder(events, pending_dialogs, sid.clone(), out_tx.clone());
    attached.insert(sid, Attachment { session, forwarder });

    data_response(AttachData { already: false })
}

fn spawn_forwarder(
    mut events: broadcast::Receiver<SessionOutput>,
    pending_dialogs: Vec<Arc<[u8]>>,
    sid: Sid,
    out_tx: mpsc::Sender<Outbound>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for dialog in pending_dialogs {
            if out_tx.send(Outbound::Raw(dialog)).await.is_err() {
                return;
            }
        }

        loop {
            let output = match events.recv().await {
                Ok(output) => output,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!("forwarder {sid}: skipped {skipped} events (slow connection)");

                    let gap = Outgoing::EventGap {
                        sid: sid.to_string(),
                        skipped,
                    };

                    if out_tx.send(Outbound::Typed(gap)).await.is_err() {
                        break;
                    }

                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            match output {
                SessionOutput::Event(line) => {
                    if out_tx.send(Outbound::Raw(line)).await.is_err() {
                        break;
                    }
                }
                SessionOutput::Exit { exit_code } => {
                    let outgoing = Outgoing::SessionExit {
                        sid: sid.to_string(),
                        exit_code,
                    };
                    let _ = out_tx.send(Outbound::Typed(outgoing)).await;

                    break;
                }
            }
        }
    })
}

fn detach(sid: Sid, attached: &mut AttachedMap) -> HarvesterResponse {
    attached.remove(sid.as_str());

    data_response(json!({}))
}

fn stop(daemon: &Daemon, sid: Sid) -> HarvesterResponse {
    let Some(session) = daemon.session(sid.as_str()) else {
        return already_stopped();
    };

    tokio::spawn(kill_backstop(daemon.clone(), session.clone()));

    let request_key = daemon.next_passthrough_id();
    drop(session.register_response_waiter(request_key.clone()));
    let abort = json!({"id": request_key, "type": "abort"});

    if let Err(error) = session.write_line(&abort) {
        eprintln!("stop {sid}: abort write failed (continuing): {error}");
    }

    session.close_stdin();

    data_response(json!({"stopping": true}))
}

fn already_stopped() -> HarvesterResponse {
    data_response(json!({"stopping": false}))
}

async fn kill_backstop(daemon: Daemon, session: Arc<Session>) {
    tokio::time::sleep(STOP_KILL_BACKSTOP).await;

    let still_running = daemon.is_current_generation(&session);

    if still_running {
        eprintln!(
            "stop {}: pi ignored stdin EOF for {STOP_KILL_BACKSTOP:?}, killing",
            session.sid
        );
        session.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_answers_the_documented_empty_payload() {
        let sid = Sid::parse("01JABCDEF0123456789ABCDEFG").expect("a valid sid");

        let response = detach(sid, &mut AttachedMap::new());

        assert_eq!(
            serde_json::to_value(&response).expect("a response serializes"),
            serde_json::json!({"success": true, "data": {}}),
            "detach is a release, not a lookup: a sid this connection never \
             attached to still succeeds (docs/DAEMON.md)"
        );
    }

    #[test]
    fn stopping_a_session_that_is_no_longer_running_succeeds() {
        let response = already_stopped();

        assert_eq!(
            serde_json::to_value(&response).expect("a response serializes"),
            serde_json::json!({"success": true, "data": {"stopping": false}}),
            "stop is a release, not a lookup: a pi that already exited is the \
             end state the caller asked for, so it is not an error \
             (docs/DAEMON.md)"
        );
    }

    #[test]
    fn a_typed_payload_becomes_the_data_of_a_successful_response() {
        let response = data_response(CreateSessionData {
            sid: "01H".to_string(),
            already: true,
        });

        assert_eq!(
            serde_json::to_value(&response).expect("a response serializes"),
            serde_json::json!({"success": true, "data": {"sid": "01H", "already": true}})
        );
    }
}
