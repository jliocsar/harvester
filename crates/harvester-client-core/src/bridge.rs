mod attach_window;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::{lock, HarvesterCmd, SessionInfo, SessionsData};

use crate::bootstrap::bootstrap_and_connect;
use crate::connection::{DaemonClient, SessionEvent};
use crate::dialog::DialogAnswer;
use crate::host::Host;
use crate::run_on_tokio_thread;
use crate::token::RequestToken;
use attach_window::{
    AttachEventBuffers, AttachRegistration, FlushOutcome, PushOutcome, ATTACH_EVENT_BUFFER_CAP,
};

const UNKNOWN_PI_ERROR: &str = "unknown error";

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
    PiRequest {
        sid: String,
        cmd: Value,
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
    Connected,
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
    PromptFailed {
        sid: String,
        detail: String,
    },
    ModelsListed {
        token: RequestToken,
        result: Result<Value, String>,
    },
    PiAnswered {
        sid: String,
        token: RequestToken,
        method: String,
        result: Result<Value, String>,
    },
    Disconnected,
}

pub(crate) struct BridgeHandle {
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
    attached: Arc<AsyncMutex<Option<Attachment>>>,
    attach_buffers: Arc<Mutex<AttachEventBuffers>>,
    resyncing: Arc<Mutex<HashSet<String>>>,
}

struct Attachment {
    sid: String,
    cursor: Option<String>,
}

pub(crate) fn start(
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

    if let Err(error) = run_on_tokio_thread(bridge_main(host, daemon_binary, commands, events)) {
        report_events.send(Event::ConnectFailed {
            detail: format!("tokio runtime: {error}"),
        });
    }
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
    events.send(Event::Connected);

    let (client, mut session_events) = DaemonClient::start(connected.connection);
    let context = BridgeContext {
        client,
        events,
        attached: Arc::new(AsyncMutex::new(None)),
        attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
        resyncing: Arc::new(Mutex::new(HashSet::new())),
    };

    spawn_flow(sessions_flow(context.clone()));

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
            spawn_flow(forget_attachment(context.clone(), sid.clone()));
            context.events.send(Event::SessionExit { sid, exit_code });
        }
        SessionEvent::EventGap { sid, skipped } => {
            eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
            spawn_flow(resync_flow(context.clone(), sid));
        }
    }
}

fn dispatch(command: Command, context: &BridgeContext) {
    let context = context.clone();

    match command {
        Command::Open { sid, cwd } => spawn_flow(open_flow(context, sid, cwd)),
        Command::StartSession {
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        } => spawn_flow(start_session_flow(
            context,
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        )),
        Command::Prompt { sid, text } => spawn_flow(prompt_flow(context, sid, text)),
        Command::ListModels { cwd, token } => spawn_flow(list_models_flow(context, cwd, token)),
        Command::PiRequest { sid, cmd, token } => {
            spawn_flow(pi_request_flow(context, sid, cmd, token))
        }
        Command::Stop { sid } => spawn_flow(stop_flow(context, sid)),
        Command::AnswerDialog {
            sid,
            dialog_id,
            answer,
        } => spawn_flow(answer_dialog_flow(context, sid, dialog_id, answer)),
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
}

fn handle_live_pi_event(context: &BridgeContext, sid: String, event: Value) {
    let outcome = lock(&context.attach_buffers, "attach buffer").push(&sid, event);

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

async fn sessions_flow(context: BridgeContext) {
    let sessions = flatten_harvester(context.client.harvester(HarvesterCmd::ListSessions).await)
        .and_then(|data| {
            serde_json::from_value::<SessionsData>(data)
                .map_err(|error| format!("malformed list_sessions response: {error}"))
        });

    match sessions {
        Ok(data) => context.events.send(Event::Sessions(data.sessions)),
        Err(detail) => report_failure(&context, "list_sessions", detail),
    }
}

struct NewSession {
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

enum WindowFlush {
    Delivered,
    Overflowed,
}

async fn open_flow(context: BridgeContext, sid: String, cwd: String) {
    let details = NewSession {
        cwd,
        provider: None,
        model: None,
        name: None,
    };
    let Some(registration) = attach_session(&context, &sid, details).await else {
        return;
    };

    if let WindowFlush::Overflowed = replay_history(&context, sid.clone(), registration).await {
        spawn_flow(resync_flow(context, sid));
    }
}

async fn resync_flow(context: BridgeContext, sid: String) {
    let Some(_in_flight) = ResyncGuard::claim(&context, &sid) else {
        return;
    };

    loop {
        if !is_attached(&context, &sid).await {
            return;
        }

        let registration = AttachRegistration::register(&context.attach_buffers, &sid);

        if let Err(detail) = repair_history(&context, &sid).await {
            report_failure(&context, "get_entries", detail);
            deliver_buffered_events(&context, sid.clone(), registration);

            return;
        }

        match deliver_buffered_events(&context, sid.clone(), registration) {
            WindowFlush::Delivered => return,
            WindowFlush::Overflowed => continue,
        }
    }
}

async fn is_attached(context: &BridgeContext, sid: &str) -> bool {
    matches!(&*context.attached.lock().await, Some(attachment) if attachment.sid == sid)
}

async fn repair_history(context: &BridgeContext, sid: &str) -> Result<(), String> {
    if let Some(leaf_id) = cursor_for(context, sid).await {
        if let Ok((entries, next_leaf_id)) = fetch_entries(context, sid, Some(&leaf_id)).await {
            record_cursor(context, sid, next_leaf_id).await;
            context.events.send(Event::HistoryExtended {
                sid: sid.to_string(),
                entries,
            });

            return Ok(());
        }
    }

    let (entries, leaf_id) = fetch_entries(context, sid, None).await?;

    record_cursor(context, sid, leaf_id).await;
    context.events.send(Event::HistoryReplaced {
        sid: sid.to_string(),
        entries,
    });

    Ok(())
}

async fn fetch_entries(
    context: &BridgeContext,
    sid: &str,
    since: Option<&str>,
) -> Result<(Vec<Value>, Option<String>), String> {
    let command = match since {
        Some(leaf_id) => json!({"type": "get_entries", "since": leaf_id}),
        None => json!({"type": "get_entries"}),
    };
    let data = pi_flow_result(context, sid, command).await?;
    let entries = data
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let leaf_id = data
        .get("leafId")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok((entries, leaf_id))
}

async fn cursor_for(context: &BridgeContext, sid: &str) -> Option<String> {
    let attached = context.attached.lock().await;

    attached
        .as_ref()
        .filter(|attachment| attachment.sid == sid)
        .and_then(|attachment| attachment.cursor.clone())
}

async fn record_cursor(context: &BridgeContext, sid: &str, leaf_id: Option<String>) {
    let mut attached = context.attached.lock().await;

    let Some(attachment) = attached.as_mut().filter(|attachment| attachment.sid == sid) else {
        return;
    };

    attachment.cursor = leaf_id;
}

struct ResyncGuard {
    in_flight: Arc<Mutex<HashSet<String>>>,
    sid: String,
}

impl ResyncGuard {
    fn claim(context: &BridgeContext, sid: &str) -> Option<Self> {
        lock(&context.resyncing, "resync")
            .insert(sid.to_string())
            .then(|| Self {
                in_flight: context.resyncing.clone(),
                sid: sid.to_string(),
            })
    }
}

impl Drop for ResyncGuard {
    fn drop(&mut self) {
        lock(&self.in_flight, "resync").remove(&self.sid);
    }
}

async fn forget_attachment(context: BridgeContext, sid: String) {
    let mut attached = context.attached.lock().await;

    if matches!(&*attached, Some(attachment) if attachment.sid == sid) {
        *attached = None;
    }
}

async fn replay_history(
    context: &BridgeContext,
    sid: String,
    registration: AttachRegistration,
) -> WindowFlush {
    let (entries, leaf_id) = match fetch_entries(context, &sid, None).await {
        Ok(fetched) => fetched,
        Err(detail) => {
            report_failure(context, "get_entries", detail);

            return WindowFlush::Delivered;
        }
    };

    record_cursor(context, &sid, leaf_id).await;
    context.events.send(Event::Attached {
        sid: sid.clone(),
        entries,
    });

    deliver_buffered_events(context, sid, registration)
}

fn deliver_buffered_events(
    context: &BridgeContext,
    sid: String,
    registration: AttachRegistration,
) -> WindowFlush {
    match registration.flush() {
        FlushOutcome::Events(events) => {
            for event in events {
                handle_pi_event(context, sid.clone(), event);
            }

            WindowFlush::Delivered
        }
        FlushOutcome::Overflowed => {
            eprintln!("bridge: held-back events overflowed for session {sid}; resyncing");

            WindowFlush::Overflowed
        }
    }
}

async fn start_session_flow(
    context: BridgeContext,
    sid: String,
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: String,
    first_prompt: String,
) {
    let details = NewSession {
        cwd,
        provider,
        model,
        name: Some(name),
    };
    let Some(registration) = attach_session(&context, &sid, details).await else {
        return;
    };

    context.events.send(Event::Attached {
        sid: sid.clone(),
        entries: Vec::new(),
    });

    if let WindowFlush::Overflowed = deliver_buffered_events(&context, sid.clone(), registration) {
        spawn_flow(resync_flow(context.clone(), sid.clone()));
    }

    spawn_flow(prompt_flow(context, sid, first_prompt));
}

async fn attach_session(
    context: &BridgeContext,
    sid: &str,
    details: NewSession,
) -> Option<AttachRegistration> {
    let attached = context.attached.clone();
    let mut attachment = attached.lock().await;

    if !detach_previous(context, &mut attachment, sid).await {
        return None;
    }

    let create = context
        .client
        .harvester(HarvesterCmd::CreateSession {
            sid: sid.to_string(),
            cwd: details.cwd,
            provider: details.provider,
            model: details.model,
            name: details.name,
        })
        .await;

    if let Err(detail) = flatten_harvester(create) {
        report_failure(context, "create session", detail);

        return None;
    }

    let registration = AttachRegistration::register(&context.attach_buffers, sid);

    if !attach(context, &mut attachment, sid).await {
        return None;
    }

    Some(registration)
}

async fn detach_previous(
    context: &BridgeContext,
    attached: &mut Option<Attachment>,
    next_sid: &str,
) -> bool {
    let Some(previous) = attached.as_ref() else {
        return true;
    };

    if previous.sid == next_sid {
        return true;
    }

    let previous_sid = previous.sid.clone();
    let detach = context
        .client
        .harvester(HarvesterCmd::Detach {
            sid: previous_sid.clone(),
        })
        .await;

    match flatten_harvester(detach) {
        Ok(_) => {
            *attached = None;
            context
                .events
                .send(Event::AttachmentLeft { sid: previous_sid });

            true
        }
        Err(detail) => {
            report_failure(context, "detach", detail);

            false
        }
    }
}

async fn attach(context: &BridgeContext, attached: &mut Option<Attachment>, sid: &str) -> bool {
    let response = context
        .client
        .harvester(HarvesterCmd::Attach {
            sid: sid.to_string(),
        })
        .await;

    match flatten_harvester(response) {
        Ok(_data) => {
            *attached = Some(Attachment {
                sid: sid.to_string(),
                cursor: None,
            });

            true
        }
        Err(detail) => {
            report_failure(context, "attach", detail);

            false
        }
    }
}

async fn prompt_flow(context: BridgeContext, sid: String, text: String) {
    let prompt = json!({"type": "prompt", "message": text, "streamingBehavior": "steer"});

    let Err(detail) = pi_flow_result(&context, &sid, prompt).await else {
        return;
    };

    eprintln!("bridge: the prompt for session {sid} was not delivered: {detail}");
    context.events.send(Event::PromptFailed { sid, detail });
}

async fn list_models_flow(context: BridgeContext, cwd: String, token: RequestToken) {
    let result = flatten_harvester(
        context
            .client
            .harvester(HarvesterCmd::ListModels { cwd })
            .await,
    );

    context.events.send(Event::ModelsListed { token, result });
}

async fn pi_request_flow(context: BridgeContext, sid: String, cmd: Value, token: RequestToken) {
    let method = cmd
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let result = pi_flow_result(&context, &sid, cmd).await;

    context.events.send(Event::PiAnswered {
        sid,
        token,
        method,
        result,
    });
}

async fn stop_flow(context: BridgeContext, sid: String) {
    let stop = context.client.harvester(HarvesterCmd::Stop { sid }).await;

    if let Err(detail) = flatten_harvester(stop) {
        report_failure(&context, "stop session", detail);
    }
}

async fn answer_dialog_flow(
    context: BridgeContext,
    sid: String,
    dialog_id: String,
    answer: DialogAnswer,
) {
    let cmd = answer.to_response(&dialog_id);

    if let Err(error) = context.client.pi(&sid, cmd).await {
        report_failure(&context, "answer dialog", error.to_string());
    }
}

async fn pi_flow_result(context: &BridgeContext, sid: &str, cmd: Value) -> Result<Value, String> {
    match context.client.pi(sid, cmd).await {
        Ok(response) if pi_success(&response) => {
            Ok(response.get("data").cloned().unwrap_or(Value::Null))
        }
        Ok(response) => Err(pi_error(&response).unwrap_or_else(|| UNKNOWN_PI_ERROR.to_string())),
        Err(error) => Err(error.to_string()),
    }
}

fn pi_success(response: &Value) -> bool {
    response
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn pi_error(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use crate::connection::DaemonConnection;
    use crate::token::RequestTokens;

    const QUIET: Duration = Duration::from_millis(100);

    fn attached_to(sid: &str) -> Option<Attachment> {
        Some(Attachment {
            sid: sid.to_string(),
            cursor: None,
        })
    }

    struct Harness {
        context: BridgeContext,
        daemon: DaemonConnection,
        events: UnboundedReceiver<(String, Event)>,
    }

    impl Harness {
        fn new() -> Harness {
            let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
            let (client, _session_events) =
                DaemonClient::start(DaemonConnection::over(client_side, None));
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
                },
                daemon: DaemonConnection::over(daemon_side, None),
                events,
            }
        }

        async fn take_request(&mut self) -> Value {
            tokio::time::timeout(QUIET, self.daemon.reader.next_line_for_test())
                .await
                .expect("the bridge sent no request")
        }

        async fn serve_harvester_ok(&mut self) -> Value {
            let request = self.take_request().await;
            let response = json!({
                "id": request["id"],
                "type": "harvester_response",
                "response": {"success": true, "data": {}},
            });

            self.daemon.writer.send_json_for_test(response).await;

            request
        }

        async fn serve_pi_entries(&mut self) -> Value {
            let request = self.take_request().await;

            self.answer_pi(&request, json!({"entries": []})).await;

            request
        }

        async fn answer_pi(&mut self, request: &Value, data: Value) {
            let response = json!({"type": "response", "success": true, "data": data});

            self.answer_pi_response(request, response).await;
        }

        async fn answer_pi_failure(&mut self, request: &Value, detail: &str) {
            let response = json!({"type": "response", "success": false, "error": detail});

            self.answer_pi_response(request, response).await;
        }

        async fn answer_pi_response(&mut self, request: &Value, response: Value) {
            let framed = json!({
                "id": request["id"],
                "type": "pi_response",
                "sid": request["sid"],
                "response": response,
            });

            self.daemon.writer.send_json_for_test(framed).await;
        }

        async fn serve_harvester_failure(&mut self, detail: &str) -> Value {
            let request = self.take_request().await;
            let response = json!({
                "id": request["id"],
                "type": "harvester_response",
                "response": {"success": false, "error": detail},
            });

            self.daemon.writer.send_json_for_test(response).await;

            request
        }

        async fn serve_open(&mut self) -> Vec<String> {
            vec![
                command_name(&self.serve_harvester_ok().await),
                command_name(&self.serve_harvester_ok().await),
                command_name(&self.serve_pi_entries().await),
            ]
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

        *harness.context.attached.lock().await = attached_to("01H-first");
        spawn_flow(open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(command_name(&harness.serve_harvester_ok().await), "detach");

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("the successful detach was not published")
            .expect("bridge event channel closed");

        assert!(matches!(
            event,
            Event::AttachmentLeft { sid } if sid == "01H-first"
        ));
        assert_eq!(
            command_name(&harness.serve_harvester_ok().await),
            "create_session",
            "the replacement open continues after retirement is published"
        );
    }

    #[tokio::test]
    async fn a_failed_detach_reports_failure_without_announcing_room_retirement() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = attached_to("01H-first");
        spawn_flow(open_flow(
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
            harness
                .context
                .attached
                .lock()
                .await
                .as_ref()
                .map(|attachment| attachment.sid.clone()),
            Some("01H-first".to_string()),
            "the bridge must keep tracking the room it failed to leave"
        );
    }

    #[tokio::test]
    async fn opening_the_attached_session_does_not_detach_it_first() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = attached_to("01H-same");
        spawn_flow(open_flow(
            harness.context.clone(),
            "01H-same".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(
            command_name(&harness.serve_harvester_ok().await),
            "create_session",
            "an idempotent reopen keeps the existing attachment until attach confirms"
        );
    }

    #[tokio::test]
    async fn a_session_that_exited_is_not_detached_from_on_the_next_open() {
        let mut harness = Harness::new();

        spawn_flow(open_flow(
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

        spawn_flow(open_flow(
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

        spawn_flow(open_flow(
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

        spawn_flow(open_flow(
            harness.context.clone(),
            "01H-second".to_string(),
            "/work".to_string(),
        ));

        assert_eq!(command_name(&harness.serve_harvester_ok().await), "detach");
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

        spawn_flow(open_flow(
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

        assert_eq!(
            command_name(&harness.serve_pi_entries().await),
            "get_entries"
        );
        harness
            .assert_silent("one repair per session at a time")
            .await;
    }

    #[tokio::test]
    async fn a_repair_is_published_as_its_own_event() {
        let mut harness = Harness::new();

        spawn_flow(open_flow(
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
        harness.serve_pi_entries().await;

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

    #[tokio::test]
    async fn a_failed_repair_still_delivers_the_events_it_held_back() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = attached_to("01H");
        spawn_flow(resync_flow(harness.context.clone(), "01H".to_string()));

        let request = harness.take_request().await;

        assert_eq!(command_name(&request), "get_entries");

        handle_live_pi_event(
            &harness.context,
            "01H".to_string(),
            json!({"type": "tool_execution_end"}),
        );
        harness.answer_pi_failure(&request, "pi is wedged").await;

        let mut delivered = Vec::new();

        while let Ok(Some((_, event))) = tokio::time::timeout(QUIET, harness.events.recv()).await {
            if let Event::Pi { sid, event } = event {
                delivered.push((sid, event));
            }
        }

        assert_eq!(
            delivered,
            vec![("01H".to_string(), json!({"type": "tool_execution_end"}))],
            "a failed repair replaced no snapshot, so the events it held back are still owed"
        );
    }

    #[tokio::test]
    async fn a_repair_stops_when_the_attachment_moved_on_under_it() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = attached_to("01H-first");
        spawn_flow(resync_flow(
            harness.context.clone(),
            "01H-first".to_string(),
        ));

        let request = harness.take_request().await;

        assert_eq!(command_name(&request), "get_entries");

        for _ in 0..=ATTACH_EVENT_BUFFER_CAP {
            handle_live_pi_event(
                &harness.context,
                "01H-first".to_string(),
                json!({"type": "message_update"}),
            );
        }

        *harness.context.attached.lock().await = attached_to("01H-second");
        harness.answer_pi(&request, json!({"entries": []})).await;

        harness
            .assert_silent("an overflowed retry for a session nobody is attached to is wasted work")
            .await;
    }

    #[tokio::test]
    async fn a_stale_repair_cannot_overwrite_the_new_sessions_cursor() {
        let mut harness = Harness::new();

        *harness.context.attached.lock().await = attached_to("01H-first");
        spawn_flow(resync_flow(
            harness.context.clone(),
            "01H-first".to_string(),
        ));

        let request = harness.take_request().await;

        assert_eq!(command_name(&request), "get_entries");

        *harness.context.attached.lock().await = Some(Attachment {
            sid: "01H-second".to_string(),
            cursor: Some("leaf-second".to_string()),
        });
        harness
            .answer_pi(&request, json!({"entries": [], "leafId": "leaf-first"}))
            .await;

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("the stale repair published nothing")
            .expect("bridge event channel closed");

        assert!(matches!(event, Event::HistoryReplaced { sid, .. } if sid == "01H-first"));

        let attached = harness.context.attached.lock().await;
        let held = attached.as_ref().expect("the new session's attachment");

        assert_eq!(
            (held.sid.as_str(), held.cursor.as_deref()),
            ("01H-second", Some("leaf-second")),
            "a repair for a replaced session must not spend the new session's cursor slot"
        );
    }

    #[tokio::test]
    async fn one_generic_round_trip_carries_every_session_scoped_pi_command() {
        let mut harness = Harness::new();

        spawn_flow(pi_request_flow(
            harness.context.clone(),
            "01H".to_string(),
            json!({"type": "set_model", "provider": "test", "modelId": "opus"}),
            RequestTokens::default().mint(),
        ));

        let request = harness.take_request().await;

        assert_eq!(command_name(&request), "set_model");

        harness.answer_pi_failure(&request, "Model not found").await;

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("the pi request was never answered")
            .expect("bridge event channel closed");

        assert!(
            matches!(
                event,
                Event::PiAnswered { sid, method, result, .. }
                    if sid == "01H"
                        && method == "set_model"
                        && result == Err("Model not found".to_string())
            ),
            "the answer echoes the pi command it was asked for so a non-asking consumer can \
             recognize it"
        );
    }

    #[tokio::test]
    async fn a_prompt_pi_refused_is_reported_against_its_own_session() {
        let mut harness = Harness::new();

        spawn_flow(prompt_flow(
            harness.context.clone(),
            "01H".to_string(),
            "fix it".to_string(),
        ));

        let request = harness.take_request().await;

        assert_eq!(command_name(&request), "prompt");

        harness.answer_pi_failure(&request, "pi is wedged").await;

        let (_, event) = tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .expect("the refused prompt was never reported")
            .expect("bridge event channel closed");

        assert!(
            matches!(
                event,
                Event::PromptFailed { sid, detail }
                    if sid == "01H" && detail == "pi is wedged"
            ),
            "a prompt failure is the sending chat's to render, not the machine's statusbar"
        );
    }
}
