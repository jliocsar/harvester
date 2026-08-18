mod attach_window;
mod flows;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::SessionInfo;

use crate::bootstrap::bootstrap_and_connect;
use crate::client::{DaemonClient, SessionEvent};
use crate::dialog::DialogAnswer;
use crate::host::Host;
use crate::token::RequestToken;
use crate::tokio_thread::run_on_tokio_thread;
use attach_window::{
    lock_attach_buffers, AttachEventBuffers, PushOutcome, ATTACH_EVENT_BUFFER_CAP,
};

#[derive(Debug)]
pub enum Command {
    Open {
        sid: String,
        cwd: String,
    },
    Prompt {
        sid: String,
        text: String,
    },
    StartSession {
        sid: String,
        cwd: String,
        provider: Option<String>,
        model: Option<String>,
        name: String,
        first_prompt: String,
    },
    ListModels {
        cwd: String,
        token: RequestToken,
    },
    ListSessionModels {
        sid: String,
        token: RequestToken,
    },
    GetSessionState {
        sid: String,
        token: RequestToken,
    },
    SetModel {
        sid: String,
        provider: String,
        model_id: String,
        token: RequestToken,
    },
    Stop {
        sid: String,
    },
    AnswerDialog {
        sid: String,
        dialog_id: String,
        answer: DialogAnswer,
    },
}

#[derive(Debug)]
pub enum Event {
    Connected {
        daemon_version: String,
    },
    ConnectFailed {
        detail: String,
    },
    Sessions(Vec<SessionInfo>),
    Attached {
        sid: String,
        entries: Vec<Value>,
    },
    AttachmentLeft {
        sid: String,
    },
    HistoryReplaced {
        sid: String,
        entries: Vec<Value>,
    },
    HistoryExtended {
        sid: String,
        entries: Vec<Value>,
    },
    Pi {
        sid: String,
        event: Value,
    },
    SessionExit {
        sid: String,
        exit_code: Option<i32>,
    },
    RequestFailed {
        detail: String,
    },
    ModelsListed {
        token: RequestToken,
        result: Result<Value, String>,
    },
    SessionModelsListed {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    SessionState {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    ModelSet {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    Disconnected,
}

pub struct BridgeHandle {
    pub commands: UnboundedSender<Command>,
}

#[derive(Clone)]
struct EventSender {
    machine: String,
    sender: UnboundedSender<(String, Event)>,
}

impl EventSender {
    fn send(&self, event: Event) {
        let _ = self.sender.send((self.machine.clone(), event));
    }
}

#[derive(Clone)]
struct BridgeContext {
    client: DaemonClient,
    events: EventSender,
    attached: Arc<AsyncMutex<Option<String>>>,
    attach_buffers: Arc<Mutex<AttachEventBuffers>>,
    resyncing: Arc<Mutex<HashSet<String>>>,
    cursor: Arc<Mutex<Option<SessionCursor>>>,
}

struct SessionCursor {
    sid: String,
    leaf_id: String,
}

pub fn start(
    machine: String,
    host: Host,
    daemon_binary: Arc<Vec<u8>>,
    events: UnboundedSender<(String, Event)>,
) -> std::io::Result<BridgeHandle> {
    let (command_sender, command_receiver) = unbounded_channel();
    let events = EventSender {
        machine,
        sender: events,
    };

    std::thread::Builder::new()
        .name("harvester-bridge".to_string())
        .spawn(move || run_bridge_thread(host, daemon_binary, command_receiver, events))?;

    Ok(BridgeHandle {
        commands: command_sender,
    })
}

fn run_bridge_thread(
    host: Host,
    daemon_binary: Arc<Vec<u8>>,
    commands: UnboundedReceiver<Command>,
    events: EventSender,
) {
    let report_events = events.clone();

    run_on_tokio_thread(
        bridge_main(host, daemon_binary, commands, events),
        |detail| report_events.send(Event::ConnectFailed { detail }),
    );
}

async fn bridge_main(
    host: Host,
    daemon_binary: Arc<Vec<u8>>,
    mut commands: UnboundedReceiver<Command>,
    events: EventSender,
) {
    let connected = match bootstrap_and_connect(&host, &daemon_binary).await {
        Ok(connected) => connected,
        Err(error) => {
            events.send(Event::ConnectFailed {
                detail: error.to_string(),
            });

            return;
        }
    };
    events.send(Event::Connected {
        daemon_version: connected.daemon_version,
    });

    let (client, mut session_events) = DaemonClient::start(connected.connection);
    let context = BridgeContext {
        client,
        events,
        attached: Arc::new(AsyncMutex::new(None)),
        attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
        resyncing: Arc::new(Mutex::new(HashSet::new())),
        cursor: Arc::new(Mutex::new(None)),
    };

    spawn_flow(flows::sessions_flow(context.clone()));

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break,
                Some(command) => dispatch(command, &context),
            },
            event = session_events.recv() => match event {
                Some(event) => handle_session_event(&context, event),
                None => {
                    context.events.send(Event::Disconnected);

                    break;
                }
            },
        }
    }
}

fn handle_session_event(context: &BridgeContext, event: SessionEvent) {
    match event {
        SessionEvent::Pi { sid, event } => handle_live_pi_event(context, sid, event),
        SessionEvent::SessionExit { sid, exit_code } => {
            spawn_flow(flows::forget_attachment(context.clone(), sid.clone()));
            context.events.send(Event::SessionExit { sid, exit_code });
        }
        SessionEvent::EventGap { sid, skipped } => {
            eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
            spawn_flow(flows::resync_flow(context.clone(), sid));
        }
    }
}

fn dispatch(command: Command, context: &BridgeContext) {
    let context = context.clone();

    match command {
        Command::Open { sid, cwd } => spawn_flow(flows::open_flow(context, sid, cwd)),
        Command::StartSession {
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        } => spawn_flow(flows::start_session_flow(
            context,
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        )),
        Command::Prompt { sid, text } => spawn_flow(flows::prompt_flow(context, sid, text)),
        Command::ListModels { cwd, token } => {
            spawn_flow(flows::list_models_flow(context, cwd, token))
        }
        Command::ListSessionModels { sid, token } => {
            spawn_flow(flows::session_models_flow(context, sid, token))
        }
        Command::GetSessionState { sid, token } => {
            spawn_flow(flows::session_state_flow(context, sid, token))
        }
        Command::SetModel {
            sid,
            provider,
            model_id,
            token,
        } => spawn_flow(flows::set_model_flow(
            context, sid, provider, model_id, token,
        )),
        Command::Stop { sid } => spawn_flow(flows::stop_flow(context, sid)),
        Command::AnswerDialog {
            sid,
            dialog_id,
            answer,
        } => spawn_flow(flows::answer_dialog_flow(context, sid, dialog_id, answer)),
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
}

fn handle_live_pi_event(context: &BridgeContext, sid: String, event: Value) {
    let outcome = lock_attach_buffers(&context.attach_buffers).push(&sid, event);

    match outcome {
        PushOutcome::Deliver(event) => handle_pi_event(context, sid, event),
        PushOutcome::Buffered | PushOutcome::Discarded => {}
        PushOutcome::Overflowed => eprintln!(
            "bridge: session {sid} exceeded {ATTACH_EVENT_BUFFER_CAP} events while its history \
             was in flight; resyncing once it lands"
        ),
    }
}

fn handle_pi_event(context: &BridgeContext, sid: String, event: Value) {
    context.events.send(Event::Pi { sid, event });
}

fn flatten_harvester(
    response: std::io::Result<harvester_proto::HarvesterResponse>,
) -> Result<Value, String> {
    match response {
        Ok(response) => response.into_result(),
        Err(error) => Err(error.to_string()),
    }
}

fn report_failure(context: &BridgeContext, operation: &str, detail: String) {
    context.events.send(Event::RequestFailed {
        detail: format!("{operation}: {detail}"),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use serde_json::json;

    use crate::connection::{no_forward, DaemonConnection};

    const QUIET: Duration = Duration::from_millis(100);

    struct Harness {
        context: BridgeContext,
        daemon: DaemonConnection,
        events: UnboundedReceiver<(String, Event)>,
    }

    impl Harness {
        fn new() -> Harness {
            let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
            let (client, _session_events) =
                DaemonClient::start(DaemonConnection::over(client_side, no_forward()));
            let (event_sender, events) = unbounded_channel();

            Harness {
                context: BridgeContext {
                    client,
                    events: EventSender {
                        machine: "vm".to_string(),
                        sender: event_sender,
                    },
                    attached: Arc::new(AsyncMutex::new(None)),
                    attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
                    resyncing: Arc::new(Mutex::new(HashSet::new())),
                    cursor: Arc::new(Mutex::new(None)),
                },
                daemon: DaemonConnection::over(daemon_side, no_forward()),
                events,
            }
        }

        async fn serve_one(&mut self) -> Value {
            let request = tokio::time::timeout(QUIET, self.daemon.reader.next_line_for_test())
                .await
                .expect("the bridge sent no request");
            let response = if request["type"] == "pi" {
                json!({
                    "id": request["id"],
                    "type": "pi_response",
                    "sid": request["sid"],
                    "response": {"type": "response", "success": true, "data": {"entries": []}},
                })
            } else {
                json!({
                    "id": request["id"],
                    "type": "harvester_response",
                    "response": {"success": true, "data": {}},
                })
            };

            self.daemon.writer.send_json_for_test(response).await;

            request
        }

        async fn serve_harvester_failure(&mut self, detail: &str) -> Value {
            let request = tokio::time::timeout(QUIET, self.daemon.reader.next_line_for_test())
                .await
                .expect("the bridge sent no request");
            let response = json!({
                "id": request["id"],
                "type": "harvester_response",
                "response": {"success": false, "error": detail},
            });

            self.daemon.writer.send_json_for_test(response).await;

            request
        }

        async fn serve_open(&mut self) -> Vec<String> {
            let mut commands = Vec::new();

            for _ in 0..3 {
                commands.push(command_name(&self.serve_one().await));
            }

            commands
        }

        async fn assert_silent(&mut self, why: &str) {
            let quiet = tokio::time::timeout(QUIET, self.daemon.reader.next_line_for_test()).await;

            assert!(
                quiet.is_err(),
                "{why}: got {:?}",
                quiet.map(|q| q.to_string())
            );
        }
    }

    fn command_name(request: &Value) -> String {
        request["cmd"]["type"].as_str().unwrap_or("?").to_string()
    }

    #[tokio::test]
    async fn a_successful_detach_announces_the_exact_room_before_open_continues() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = Some("01H-first".to_string());
        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(command_name(&harness.serve_one().await), "detach");

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("the successful detach was not published")
            .expect("bridge event channel closed");

        assert!(matches!(
            event,
            Event::AttachmentLeft { sid } if sid == "01H-first"
        ));
        assert_eq!(
            command_name(&harness.serve_one().await),
            "create_session",
            "the replacement open continues after retirement is published"
        );
    }

    #[tokio::test]
    async fn a_failed_detach_reports_failure_without_announcing_room_retirement() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = Some("01H-first".to_string());
        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(
            command_name(&harness.serve_harvester_failure("refused").await),
            "detach"
        );

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("detach failure was not reported")
            .expect("bridge event channel closed");

        assert!(matches!(
            event,
            Event::RequestFailed { detail } if detail == "detach: refused"
        ));
        assert!(
            tokio::time::timeout(QUIET, harness.events.recv())
                .await
                .is_err(),
            "a failed detach must not retire its attachment room"
        );
        harness
            .assert_silent("a failed detach must abort the replacement open")
            .await;
        assert_eq!(
            harness.context.attached.lock().await.as_deref(),
            Some("01H-first"),
            "the bridge must keep tracking the room it failed to leave"
        );
    }

    #[tokio::test]
    async fn opening_the_attached_session_does_not_detach_it_first() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = Some("01H-same".to_string());
        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-same".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(
            command_name(&harness.serve_one().await),
            "create_session",
            "an idempotent reopen keeps the existing attachment until attach confirms"
        );
    }

    #[tokio::test]
    async fn a_session_that_exited_is_not_detached_from_on_the_next_open() {
        let mut harness = Harness::new();

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-first".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(
            harness.serve_open().await,
            ["create_session", "attach", "get_entries"]
        );

        handle_session_event(
            &harness.context,
            SessionEvent::SessionExit {
                sid: "01H-first".to_string(),
                exit_code: Some(0),
            },
        );

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(
            harness.serve_open().await,
            ["create_session", "attach", "get_entries"],
            "the exited session must not be detached from"
        );
    }

    #[tokio::test]
    async fn a_still_running_session_is_detached_from_on_the_next_open() {
        let mut harness = Harness::new();

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-first".to_string(),
            "/work".to_string(),
        ));
        harness.serve_open().await;

        handle_session_event(
            &harness.context,
            SessionEvent::SessionExit {
                sid: "01H-somebody-else".to_string(),
                exit_code: Some(0),
            },
        );

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(command_name(&harness.serve_one().await), "detach");
    }

    #[tokio::test]
    async fn a_gap_for_a_session_nobody_watches_costs_nothing() {
        let mut harness = Harness::new();

        handle_session_event(
            &harness.context,
            SessionEvent::EventGap {
                sid: "01H-unwatched".to_string(),
                skipped: 12,
            },
        );

        harness
            .assert_silent("only the attached session is worth repairing")
            .await;
    }

    #[tokio::test]
    async fn a_burst_of_gaps_costs_one_refetch() {
        let mut harness = Harness::new();

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H".to_string(),
            "/work".to_string(),
        ));
        harness.serve_open().await;

        for skipped in 1..=5 {
            handle_session_event(
                &harness.context,
                SessionEvent::EventGap {
                    sid: "01H".to_string(),
                    skipped,
                },
            );
        }

        assert_eq!(command_name(&harness.serve_one().await), "get_entries");
        harness
            .assert_silent("one repair per session at a time")
            .await;
    }

    #[tokio::test]
    async fn a_repair_is_published_as_its_own_event() {
        let mut harness = Harness::new();

        spawn_flow(flows::open_flow(
            harness.context.clone(),
            "01H".to_string(),
            "/work".to_string(),
        ));
        harness.serve_open().await;

        handle_session_event(
            &harness.context,
            SessionEvent::EventGap {
                sid: "01H".to_string(),
                skipped: 1,
            },
        );
        harness.serve_one().await;

        let mut published = Vec::new();

        while let Ok(Some((_, event))) = tokio::time::timeout(QUIET, harness.events.recv()).await {
            published.push(match event {
                Event::Attached { .. } => "attached",
                Event::HistoryReplaced { .. } => "history_replaced",
                _ => continue,
            });
        }

        assert_eq!(published, ["attached", "history_replaced"]);
    }
}
