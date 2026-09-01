mod attach_window;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::{lock, HarvesterCmd, SessionInfo, SessionsData};

use crate::bootstrap::bootstrap_and_connect;
use crate::connection::{DaemonClient, SessionEvent};
use crate::dialog::DialogAnswer;
use crate::host::Host;
use crate::token::RequestToken;
use crate::{SubmissionId, PI_AGENT_SETTLED};
use attach_window::{
    AttachEventBuffers, AttachRegistration, FlushOutcome, PushOutcome, ATTACH_EVENT_BUFFER_CAP,
};

const UNKNOWN_PI_ERROR: &str = "unknown error";
const DISCONNECTED_PROMPT_DETAIL: &str = "bridge disconnected before prompt delivery completed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    ListSessions,
    CreateSession,
    Attach,
    OpenHistory,
    RepairHistory,
    Detach,
    Stop,
}

impl Operation {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ListSessions => "list_sessions",
            Self::CreateSession => "create_session",
            Self::Attach => "attach",
            Self::OpenHistory => "get_entries while opening",
            Self::RepairHistory => "repair history",
            Self::Detach => "detach",
            Self::Stop => "stop",
        }
    }
}

#[derive(Debug)]
pub(crate) enum Command {
    Open {
        sid: String,
        cwd: String,
        generation: u64,
    },
    Prompt {
        sid: String,
        text: String,
        submission: SubmissionId,
        generation: u64,
    },
    StartSession {
        sid: String,
        cwd: String,
        provider: Option<String>,
        model: Option<String>,
        name: String,
        first_prompt: String,
        submission: SubmissionId,
        generation: u64,
    },
    ListModels {
        cwd: String,
        token: RequestToken,
    },
    PiRequest {
        sid: String,
        cmd: Value,
        token: RequestToken,
        generation: u64,
    },
    Stop {
        sid: String,
        generation: u64,
    },
    AnswerDialog {
        sid: String,
        dialog_id: String,
        answer: DialogAnswer,
        generation: u64,
    },
}

#[derive(Debug)]
pub(crate) enum Event {
    Connected,
    ConnectFailed {
        detail: String,
    },
    Sessions(Vec<SessionInfo>),
    RoomJoined {
        sid: String,
        generation: u64,
    },
    RoomLeft {
        sid: String,
        generation: u64,
    },
    Attached {
        sid: String,
        generation: u64,
        entries: Vec<Value>,
        leaf_id: Option<String>,
        history_available: bool,
    },
    Detached {
        sid: String,
        generation: u64,
    },
    Repairing {
        sid: String,
        generation: u64,
    },
    HistoryReplaced {
        sid: String,
        generation: u64,
        entries: Vec<Value>,
        leaf_id: Option<String>,
    },
    HistoryExtended {
        sid: String,
        generation: u64,
        entries: Vec<Value>,
        leaf_id: Option<String>,
    },
    Pi {
        sid: String,
        generation: u64,
        event: Value,
    },
    SessionExit {
        sid: String,
        generation: u64,
        exit_code: Option<i32>,
    },
    OperationFailed {
        sid: Option<String>,
        generation: Option<u64>,
        operation: Operation,
        detail: String,
    },
    PromptDelivered {
        sid: String,
        generation: u64,
        submission: SubmissionId,
    },
    DialogAnswered {
        sid: String,
        generation: u64,
        dialog_id: String,
        result: Result<(), String>,
    },
    PromptFailed {
        sid: String,
        generation: u64,
        submission: SubmissionId,
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
    pub(crate) commands: UnboundedSender<Command>,
}

#[derive(Clone)]
struct EventSender {
    machine: String,
    generation: u64,
    sender: UnboundedSender<(String, u64, Event)>,
}

impl EventSender {
    fn send(&self, event: Event) {
        drop(
            self.sender
                .send((self.machine.clone(), self.generation, event)),
        );
    }
}

#[derive(Clone)]
struct BridgeContext {
    client: DaemonClient,
    events: EventSender,
    attachments: Arc<Mutex<HashMap<String, Attachment>>>,
    attach_buffers: Arc<Mutex<AttachEventBuffers>>,
    resyncing: Arc<Mutex<HashSet<(String, u64)>>>,
    attachment_changes: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
    pending_prompts: Arc<Mutex<HashMap<SubmissionId, (String, u64)>>>,
}

#[derive(Clone)]
struct Attachment {
    generation: u64,
    cursor: Option<String>,
}

pub(crate) fn start(
    machine: String,
    generation: u64,
    host: Host,
    daemon_binary: Arc<Vec<u8>>,
    events: UnboundedSender<(String, u64, Event)>,
) -> BridgeHandle {
    let (command_sender, command_receiver) = unbounded_channel();
    let events = EventSender {
        machine,
        generation,
        sender: events,
    };

    tokio::spawn(bridge_main(host, daemon_binary, command_receiver, events));

    BridgeHandle {
        commands: command_sender,
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
        attachments: Arc::new(Mutex::new(HashMap::new())),
        attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
        resyncing: Arc::new(Mutex::new(HashSet::new())),
        attachment_changes: Arc::new(Mutex::new(HashMap::new())),
        pending_prompts: Arc::new(Mutex::new(HashMap::new())),
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
                    fail_pending_prompts(&context);
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
            let Some(generation) = remove_attachment(context, &sid) else {
                return;
            };

            context.events.send(Event::SessionExit {
                sid,
                generation,
                exit_code,
            });
        }
        SessionEvent::EventGap { sid, skipped } => handle_event_gap(context, sid, skipped),
    }
}

fn handle_event_gap(context: &BridgeContext, sid: String, skipped: u64) {
    let Some(generation) = attachment_generation(context, &sid) else {
        return;
    };

    eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
    context.events.send(Event::Repairing {
        sid: sid.clone(),
        generation,
    });
    spawn_flow(repair_attachment_flow(context.clone(), sid, generation));
}

fn dispatch(command: Command, context: &BridgeContext) {
    let context = context.clone();

    match command {
        Command::Open {
            sid,
            cwd,
            generation,
        } => spawn_flow(open_flow(context, sid, cwd, generation)),
        Command::StartSession {
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
            submission,
            generation,
        } => spawn_flow(start_session_flow(
            context,
            StartSessionRequest {
                sid,
                cwd,
                provider,
                model,
                name,
                first_prompt,
                submission,
                generation,
            },
        )),
        Command::Prompt {
            sid,
            text,
            submission,
            generation,
        } => spawn_prompt_flow(context, sid, text, submission, generation),
        Command::ListModels { cwd, token } => spawn_flow(list_models_flow(context, cwd, token)),
        Command::PiRequest {
            sid,
            cmd,
            token,
            generation,
        } => spawn_flow(pi_request_flow(context, sid, cmd, token, generation)),
        Command::Stop { sid, generation } => spawn_flow(stop_flow(context, sid, generation)),
        Command::AnswerDialog {
            sid,
            dialog_id,
            answer,
            generation,
        } => spawn_flow(answer_dialog_flow(
            context, sid, dialog_id, answer, generation,
        )),
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
}

fn handle_live_pi_event(context: &BridgeContext, sid: String, event: Value) {
    let Some(generation) = attachment_generation(context, &sid) else {
        return;
    };
    let outcome = lock(&context.attach_buffers, "attach buffer").push(&sid, generation, event);

    match outcome {
        PushOutcome::Deliver(event) => deliver_live_pi_event(context, sid, generation, event),
        PushOutcome::Buffered | PushOutcome::Discarded => {}
        PushOutcome::Overflowed => eprintln!(
            "bridge: session {sid} exceeded {ATTACH_EVENT_BUFFER_CAP} events while its history \
             was in flight; resyncing once it lands"
        ),
    }
}

fn deliver_live_pi_event(context: &BridgeContext, sid: String, generation: u64, event: Value) {
    let registration = pi_event_ends_turn(&event)
        .then(|| AttachRegistration::register(&context.attach_buffers, &sid, generation));

    context.events.send(Event::Pi {
        sid: sid.clone(),
        generation,
        event,
    });

    if let Some(registration) = registration {
        spawn_history_sync(context, sid, generation, registration);
    }
}

fn flatten_harvester(
    response: std::io::Result<harvester_proto::HarvesterResponse>,
) -> Result<Value, String> {
    match response {
        Ok(response) => response.into_result(),
        Err(error) => Err(error.to_string()),
    }
}

fn report_failure(
    context: &BridgeContext,
    sid: Option<&str>,
    generation: Option<u64>,
    operation: Operation,
    detail: String,
) {
    context.events.send(Event::OperationFailed {
        sid: sid.map(str::to_string),
        generation,
        operation,
        detail,
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
        Err(detail) => report_failure(&context, None, None, Operation::ListSessions, detail),
    }
}

struct NewSession {
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

struct StartSessionRequest {
    sid: String,
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: String,
    first_prompt: String,
    submission: SubmissionId,
    generation: u64,
}

enum WindowFlush {
    Delivered,
    Overflowed,
}

async fn open_flow(context: BridgeContext, sid: String, cwd: String, generation: u64) {
    let attachment_change = attachment_change_for(&context, &sid);
    let _attachment_change = attachment_change.lock().await;
    let details = NewSession {
        cwd,
        provider: None,
        model: None,
        name: None,
    };
    let Some(registration) = attach_session(&context, &sid, generation, details).await else {
        return;
    };
    let flush = replay_history(&context, sid.clone(), generation, registration).await;

    if matches!(flush, WindowFlush::Overflowed) {
        schedule_attachment_repair(&context, sid, generation);
    }
}

fn schedule_attachment_repair(context: &BridgeContext, sid: String, generation: u64) {
    context.events.send(Event::Repairing {
        sid: sid.clone(),
        generation,
    });
    spawn_flow(repair_attachment_flow(context.clone(), sid, generation));
}

fn spawn_history_sync(
    context: &BridgeContext,
    sid: String,
    generation: u64,
    registration: AttachRegistration,
) {
    spawn_flow(sync_history_flow(
        context.clone(),
        sid,
        generation,
        registration,
    ));
}

async fn sync_history_flow(
    context: BridgeContext,
    sid: String,
    generation: u64,
    registration: AttachRegistration,
) {
    let attachment_change = attachment_change_for(&context, &sid);
    let Some(in_flight) = ResyncGuard::claim(&context, &sid, generation) else {
        return;
    };
    let attachment_guard = attachment_change.lock().await;

    if !is_attached(&context, &sid, generation) {
        return;
    }

    if let Err(detail) = repair_history(&context, &sid, generation).await {
        eprintln!("bridge: history sync for session {sid} failed: {detail}");
        deliver_buffered_events(&context, sid, generation, registration);

        return;
    }

    let overflowed = matches!(
        deliver_buffered_events(&context, sid.clone(), generation, registration),
        WindowFlush::Overflowed
    );

    drop(attachment_guard);
    drop(in_flight);

    if overflowed {
        schedule_attachment_repair(&context, sid, generation);
    }
}

async fn repair_attachment_flow(context: BridgeContext, sid: String, generation: u64) {
    let attachment_change = attachment_change_for(&context, &sid);
    let Some(_in_flight) = ResyncGuard::claim(&context, &sid, generation) else {
        return;
    };
    let _attachment_change = attachment_change.lock().await;

    loop {
        if !is_attached(&context, &sid, generation) {
            return;
        }

        let cursor = cursor_for(&context, &sid, generation);

        if let Err(detail) = detach_for_repair(&context, &sid, generation).await {
            report_failure(
                &context,
                Some(&sid),
                Some(generation),
                Operation::RepairHistory,
                detail,
            );
            context.client.close().await;

            return;
        }

        let Some(registration) = reattach_for_repair(&context, &sid, generation, cursor).await
        else {
            return;
        };

        if let Err(detail) = repair_history(&context, &sid, generation).await {
            report_failure(
                &context,
                Some(&sid),
                Some(generation),
                Operation::RepairHistory,
                detail,
            );
            deliver_buffered_events(&context, sid.clone(), generation, registration);

            return;
        }

        match deliver_buffered_events(&context, sid.clone(), generation, registration) {
            WindowFlush::Delivered => return,
            WindowFlush::Overflowed => continue,
        }
    }
}

async fn detach_for_repair(
    context: &BridgeContext,
    sid: &str,
    generation: u64,
) -> Result<(), String> {
    let response = context
        .client
        .harvester(HarvesterCmd::Detach {
            sid: sid.to_string(),
        })
        .await;

    flatten_harvester(response)?;
    remove_attachment_if_generation(context, sid, generation);
    context.events.send(Event::RoomLeft {
        sid: sid.to_string(),
        generation,
    });

    Ok(())
}

async fn reattach_for_repair(
    context: &BridgeContext,
    sid: &str,
    generation: u64,
    cursor: Option<String>,
) -> Option<AttachRegistration> {
    let registration = AttachRegistration::register(&context.attach_buffers, sid, generation);

    install_attachment(context, sid, generation, cursor);
    let response = context
        .client
        .harvester(HarvesterCmd::Attach {
            sid: sid.to_string(),
        })
        .await;

    if let Err(detail) = flatten_harvester(response) {
        remove_attachment_if_generation(context, sid, generation);
        report_failure(
            context,
            Some(sid),
            Some(generation),
            Operation::Attach,
            detail,
        );

        return None;
    }

    context.events.send(Event::RoomJoined {
        sid: sid.to_string(),
        generation,
    });

    Some(registration)
}

fn is_attached(context: &BridgeContext, sid: &str, generation: u64) -> bool {
    matches!(lock(&context.attachments, "attachments").get(sid), Some(attachment) if attachment.generation == generation)
}

async fn repair_history(context: &BridgeContext, sid: &str, generation: u64) -> Result<(), String> {
    if let Some(leaf_id) = cursor_for(context, sid, generation) {
        if let Ok((entries, next_leaf_id)) = fetch_entries(context, sid, Some(&leaf_id)).await {
            record_cursor(context, sid, generation, next_leaf_id.clone());

            if is_attached(context, sid, generation) {
                context.events.send(Event::HistoryExtended {
                    sid: sid.to_string(),
                    generation,
                    entries,
                    leaf_id: next_leaf_id,
                });
            }

            return Ok(());
        }
    }

    let (entries, leaf_id) = fetch_entries(context, sid, None).await?;

    record_cursor(context, sid, generation, leaf_id.clone());

    if is_attached(context, sid, generation) {
        context.events.send(Event::HistoryReplaced {
            sid: sid.to_string(),
            generation,
            entries,
            leaf_id,
        });
    }

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
        .ok_or_else(|| "get_entries response did not contain an entries array".to_string())?;
    let leaf_id = data
        .get("leafId")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok((entries, leaf_id))
}

fn cursor_for(context: &BridgeContext, sid: &str, generation: u64) -> Option<String> {
    lock(&context.attachments, "attachments")
        .get(sid)
        .filter(|attachment| attachment.generation == generation)
        .and_then(|attachment| attachment.cursor.clone())
}

fn record_cursor(context: &BridgeContext, sid: &str, generation: u64, leaf_id: Option<String>) {
    let mut attachments = lock(&context.attachments, "attachments");
    let Some(attachment) = attachments
        .get_mut(sid)
        .filter(|attachment| attachment.generation == generation)
    else {
        return;
    };

    attachment.cursor = leaf_id;
    drop(attachments);
}

struct ResyncGuard {
    in_flight: Arc<Mutex<HashSet<(String, u64)>>>,
    key: (String, u64),
}

impl ResyncGuard {
    fn claim(context: &BridgeContext, sid: &str, generation: u64) -> Option<Self> {
        let key = (sid.to_string(), generation);

        lock(&context.resyncing, "resync")
            .insert(key.clone())
            .then(|| Self {
                in_flight: context.resyncing.clone(),
                key,
            })
    }
}

impl Drop for ResyncGuard {
    fn drop(&mut self) {
        lock(&self.in_flight, "resync").remove(&self.key);
    }
}

async fn replay_history(
    context: &BridgeContext,
    sid: String,
    generation: u64,
    registration: AttachRegistration,
) -> WindowFlush {
    let (entries, leaf_id) = match fetch_entries(context, &sid, None).await {
        Ok(fetched) => fetched,
        Err(detail) => {
            context.events.send(Event::Attached {
                sid: sid.clone(),
                generation,
                entries: Vec::new(),
                leaf_id: None,
                history_available: false,
            });
            report_failure(
                context,
                Some(&sid),
                Some(generation),
                Operation::OpenHistory,
                detail,
            );
            deliver_buffered_events(context, sid.clone(), generation, registration);
            schedule_attachment_repair(context, sid, generation);

            return WindowFlush::Delivered;
        }
    };

    record_cursor(context, &sid, generation, leaf_id.clone());

    if !is_attached(context, &sid, generation) {
        return WindowFlush::Delivered;
    }

    context.events.send(Event::Attached {
        sid: sid.clone(),
        generation,
        entries,
        leaf_id,
        history_available: true,
    });

    deliver_buffered_events(context, sid, generation, registration)
}

fn deliver_buffered_events(
    context: &BridgeContext,
    sid: String,
    generation: u64,
    registration: AttachRegistration,
) -> WindowFlush {
    match registration.flush() {
        FlushOutcome::Events(events) => {
            let mut turn_settled = false;

            for event in events {
                if !is_attached(context, &sid, generation) {
                    break;
                }

                turn_settled |= pi_event_ends_turn(&event);
                context.events.send(Event::Pi {
                    sid: sid.clone(),
                    generation,
                    event,
                });
            }

            if turn_settled && is_attached(context, &sid, generation) {
                let registration =
                    AttachRegistration::register(&context.attach_buffers, &sid, generation);

                spawn_history_sync(context, sid, generation, registration);
            }

            WindowFlush::Delivered
        }
        FlushOutcome::Overflowed => {
            eprintln!("bridge: held-back events overflowed for session {sid}; resyncing");

            WindowFlush::Overflowed
        }
    }
}

async fn start_session_flow(context: BridgeContext, request: StartSessionRequest) {
    let StartSessionRequest {
        sid,
        cwd,
        provider,
        model,
        name,
        first_prompt,
        submission,
        generation,
    } = request;
    let attachment_change = attachment_change_for(&context, &sid);
    let _attachment_change = attachment_change.lock().await;
    let details = NewSession {
        cwd,
        provider,
        model,
        name: Some(name),
    };
    let Some(registration) = attach_session(&context, &sid, generation, details).await else {
        return;
    };

    context.events.send(Event::Attached {
        sid: sid.clone(),
        generation,
        entries: Vec::new(),
        leaf_id: None,
        history_available: true,
    });

    if matches!(
        deliver_buffered_events(&context, sid.clone(), generation, registration),
        WindowFlush::Overflowed
    ) {
        schedule_attachment_repair(&context, sid.clone(), generation);
    }

    spawn_prompt_flow(context, sid, first_prompt, submission, generation);
}

async fn attach_session(
    context: &BridgeContext,
    sid: &str,
    generation: u64,
    details: NewSession,
) -> Option<AttachRegistration> {
    if let Some(previous_generation) = attachment_generation(context, sid) {
        if previous_generation > generation {
            return None;
        }

        if previous_generation < generation
            && detach_replaced_attachment(context, sid, previous_generation)
                .await
                .is_none()
        {
            return None;
        }
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
        report_failure(
            context,
            Some(sid),
            Some(generation),
            Operation::CreateSession,
            detail,
        );

        return None;
    }

    let registration = AttachRegistration::register(&context.attach_buffers, sid, generation);

    install_attachment(context, sid, generation, None);

    let response = context
        .client
        .harvester(HarvesterCmd::Attach {
            sid: sid.to_string(),
        })
        .await;

    if let Err(detail) = flatten_harvester(response) {
        remove_attachment_if_generation(context, sid, generation);
        report_failure(
            context,
            Some(sid),
            Some(generation),
            Operation::Attach,
            detail,
        );

        return None;
    }

    context.events.send(Event::RoomJoined {
        sid: sid.to_string(),
        generation,
    });

    Some(registration)
}

async fn detach_replaced_attachment(
    context: &BridgeContext,
    sid: &str,
    generation: u64,
) -> Option<()> {
    let response = context
        .client
        .harvester(HarvesterCmd::Detach {
            sid: sid.to_string(),
        })
        .await;

    if let Err(detail) = flatten_harvester(response) {
        report_failure(
            context,
            Some(sid),
            Some(generation),
            Operation::Detach,
            detail,
        );

        return None;
    }

    remove_attachment_if_generation(context, sid, generation);
    context.events.send(Event::RoomLeft {
        sid: sid.to_string(),
        generation,
    });
    context.events.send(Event::Detached {
        sid: sid.to_string(),
        generation,
    });

    Some(())
}

fn attachment_change_for(context: &BridgeContext, sid: &str) -> Arc<AsyncMutex<()>> {
    lock(&context.attachment_changes, "attachment changes")
        .entry(sid.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn install_attachment(context: &BridgeContext, sid: &str, generation: u64, cursor: Option<String>) {
    lock(&context.attachments, "attachments")
        .insert(sid.to_string(), Attachment { generation, cursor });
}

fn spawn_prompt_flow(
    context: BridgeContext,
    sid: String,
    text: String,
    submission: SubmissionId,
    generation: u64,
) {
    let previous = lock(&context.pending_prompts, "pending prompts")
        .insert(submission, (sid.clone(), generation));

    assert!(previous.is_none(), "duplicate prompt submission");
    spawn_flow(prompt_flow(context, sid, text, submission, generation));
}

fn finish_prompt(context: &BridgeContext, submission: SubmissionId, event: Event) {
    if lock(&context.pending_prompts, "pending prompts")
        .remove(&submission)
        .is_some()
    {
        context.events.send(event);
    }
}

fn fail_pending_prompts(context: &BridgeContext) {
    let pending = std::mem::take(&mut *lock(&context.pending_prompts, "pending prompts"));

    for (submission, (sid, generation)) in pending {
        context.events.send(Event::PromptFailed {
            sid,
            generation,
            submission,
            detail: DISCONNECTED_PROMPT_DETAIL.to_string(),
        });
    }
}

async fn prompt_flow(
    context: BridgeContext,
    sid: String,
    text: String,
    submission: SubmissionId,
    generation: u64,
) {
    if !is_attached(&context, &sid, generation) {
        finish_prompt(
            &context,
            submission,
            Event::PromptFailed {
                sid,
                generation,
                submission,
                detail: "session attachment is no longer current".to_string(),
            },
        );

        return;
    }

    let prompt = json!({"type": "prompt", "message": text, "streamingBehavior": "steer"});

    match pi_flow_result(&context, &sid, prompt).await {
        Ok(_) => finish_prompt(
            &context,
            submission,
            Event::PromptDelivered {
                sid,
                generation,
                submission,
            },
        ),
        Err(detail) => {
            eprintln!("bridge: the prompt for session {sid} was not delivered: {detail}");
            finish_prompt(
                &context,
                submission,
                Event::PromptFailed {
                    sid,
                    generation,
                    submission,
                    detail,
                },
            );
        }
    }
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

async fn pi_request_flow(
    context: BridgeContext,
    sid: String,
    cmd: Value,
    token: RequestToken,
    generation: u64,
) {
    let method = cmd
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if !is_attached(&context, &sid, generation) {
        context.events.send(Event::PiAnswered {
            sid,
            token,
            method,
            result: Err("session attachment is no longer current".to_string()),
        });

        return;
    }

    let result = pi_flow_result(&context, &sid, cmd).await;

    context.events.send(Event::PiAnswered {
        sid,
        token,
        method,
        result,
    });
}

async fn stop_flow(context: BridgeContext, sid: String, generation: u64) {
    if !is_attached(&context, &sid, generation) {
        report_failure(
            &context,
            Some(&sid),
            Some(generation),
            Operation::Stop,
            "session attachment is no longer current".to_string(),
        );

        return;
    }

    let stop = context
        .client
        .harvester(HarvesterCmd::Stop { sid: sid.clone() })
        .await;

    if let Err(detail) = flatten_harvester(stop) {
        report_failure(
            &context,
            Some(&sid),
            Some(generation),
            Operation::Stop,
            detail,
        );
    }
}

async fn answer_dialog_flow(
    context: BridgeContext,
    sid: String,
    dialog_id: String,
    answer: DialogAnswer,
    generation: u64,
) {
    let result = if is_attached(&context, &sid, generation) {
        pi_flow_result(&context, &sid, answer.to_response(&dialog_id))
            .await
            .map(drop)
    } else {
        Err("session attachment is no longer current".to_string())
    };

    context.events.send(Event::DialogAnswered {
        sid,
        generation,
        dialog_id,
        result,
    });
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

fn pi_event_ends_turn(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some(PI_AGENT_SETTLED)
}

fn pi_error(response: &Value) -> Option<String> {
    response
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn attachment_generation(context: &BridgeContext, sid: &str) -> Option<u64> {
    lock(&context.attachments, "attachments")
        .get(sid)
        .map(|attachment| attachment.generation)
}

fn remove_attachment(context: &BridgeContext, sid: &str) -> Option<u64> {
    lock(&context.attachments, "attachments")
        .remove(sid)
        .map(|attachment| attachment.generation)
}

fn remove_attachment_if_generation(context: &BridgeContext, sid: &str, generation: u64) {
    let mut attachments = lock(&context.attachments, "attachments");

    if attachments
        .get(sid)
        .is_some_and(|attachment| attachment.generation == generation)
    {
        attachments.remove(sid);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::connection::DaemonConnection;

    const QUIET: Duration = Duration::from_millis(100);

    struct Harness {
        context: BridgeContext,
        daemon: DaemonConnection,
        events: UnboundedReceiver<(String, u64, Event)>,
    }

    impl Harness {
        fn new() -> Self {
            let (client_side, daemon_side) = tokio::io::duplex(64 * 1024);
            let (client, _session_events) =
                DaemonClient::start(DaemonConnection::over(client_side, None));
            let (event_sender, events) = unbounded_channel();

            Self {
                context: BridgeContext {
                    client,
                    events: EventSender {
                        machine: "machine".to_string(),
                        generation: 7,
                        sender: event_sender,
                    },
                    attachments: Arc::new(Mutex::new(HashMap::new())),
                    attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
                    resyncing: Arc::new(Mutex::new(HashSet::new())),
                    attachment_changes: Arc::new(Mutex::new(HashMap::new())),
                    pending_prompts: Arc::new(Mutex::new(HashMap::new())),
                },
                daemon: DaemonConnection::over(daemon_side, None),
                events,
            }
        }

        async fn request(&mut self) -> Value {
            tokio::time::timeout(QUIET, self.daemon.reader.next_line_for_test())
                .await
                .expect("bridge request")
        }

        async fn answer_harvester(&mut self, data: Value) -> Value {
            let request = self.request().await;

            self.daemon
                .writer
                .send_json_for_test(json!({
                    "id": request["id"],
                    "type": "harvester_response",
                    "response": {"success": true, "data": data},
                }))
                .await;

            request
        }

        async fn answer_pi(&mut self, data: Value) -> Value {
            let request = self.request().await;

            self.daemon
                .writer
                .send_json_for_test(json!({
                    "id": request["id"],
                    "type": "pi_response",
                    "sid": request["sid"],
                    "response": {"type": "response", "success": true, "data": data},
                }))
                .await;

            request
        }

        async fn answer_pi_failure(&mut self, detail: &str) -> Value {
            let request = self.request().await;

            self.daemon
                .writer
                .send_json_for_test(json!({
                    "id": request["id"],
                    "type": "pi_response",
                    "sid": request["sid"],
                    "response": {"type": "response", "success": false, "error": detail},
                }))
                .await;

            request
        }

        async fn serve_open(&mut self, leaf_id: &str, generation: u64) {
            let create = self.answer_harvester(json!({})).await;
            let attach = self.answer_harvester(json!({})).await;
            let entries = self
                .answer_pi(json!({"entries": [], "leafId": leaf_id}))
                .await;

            assert_eq!(create["cmd"]["type"], "create_session");
            assert_eq!(attach["cmd"]["type"], "attach");
            assert_eq!(entries["cmd"]["type"], "get_entries");
            assert_eq!(
                attachment_generation(&self.context, entries["sid"].as_str().expect("sid")),
                Some(generation)
            );
        }

        async fn event(&mut self) -> Event {
            let (machine, bridge_generation, event) =
                tokio::time::timeout(QUIET, self.events.recv())
                    .await
                    .expect("bridge event")
                    .expect("event channel");

            assert_eq!(machine, "machine");
            assert_eq!(bridge_generation, 7);

            event
        }
    }

    #[tokio::test]
    async fn list_open_and_attach_publish_authoritative_history() {
        let mut harness = Harness::new();

        spawn_flow(sessions_flow(harness.context.clone()));
        let request = harness
            .answer_harvester(json!({"sessions": [{
                "sid": "listed",
                "cwd": "/repo",
                "mtime_ms": 9,
                "status": "exited"
            }]}))
            .await;

        assert_eq!(request["cmd"]["type"], "list_sessions");
        assert!(matches!(harness.event().await, Event::Sessions(rows) if rows.len() == 1));

        spawn_flow(open_flow(
            harness.context.clone(),
            "listed".to_string(),
            "/repo".to_string(),
            1,
        ));
        harness.serve_open("leaf-1", 1).await;

        assert!(matches!(
            harness.event().await,
            Event::RoomJoined {
                sid,
                generation: 1
            } if sid == "listed"
        ));
        assert!(matches!(
            harness.event().await,
            Event::Attached {
                sid,
                generation: 1,
                leaf_id: Some(leaf_id),
                ..
            } if sid == "listed" && leaf_id == "leaf-1"
        ));
    }

    #[tokio::test]
    async fn settled_turn_sync_replaces_the_raw_live_suffix_with_a_history_delta() {
        let mut harness = Harness::new();

        install_attachment(&harness.context, "listed", 1, Some("leaf-1".to_string()));
        handle_live_pi_event(
            &harness.context,
            "listed".to_string(),
            json!({"type": "agent_settled"}),
        );

        assert!(matches!(
            harness.event().await,
            Event::Pi {
                sid,
                generation: 1,
                event,
            } if sid == "listed" && event["type"] == "agent_settled"
        ));

        let request = harness
            .answer_pi(json!({
                "entries": [{"type": "custom", "id": "next"}],
                "leafId": "leaf-2"
            }))
            .await;

        assert_eq!(request["cmd"]["type"], "get_entries");
        assert_eq!(request["cmd"]["since"], "leaf-1");
        assert!(matches!(
            harness.event().await,
            Event::HistoryExtended {
                sid,
                generation: 1,
                leaf_id: Some(leaf_id),
                ..
            } if sid == "listed" && leaf_id == "leaf-2"
        ));
    }

    #[tokio::test]
    async fn opening_history_failure_establishes_then_retires_the_dialog_room_for_repair() {
        let mut harness = Harness::new();

        spawn_flow(open_flow(
            harness.context.clone(),
            "listed".to_string(),
            "/repo".to_string(),
            1,
        ));
        let _create = harness.answer_harvester(json!({})).await;
        let _attach = harness.answer_harvester(json!({})).await;
        let _entries = harness.answer_pi_failure("history unavailable").await;

        assert!(matches!(
            harness.event().await,
            Event::RoomJoined {
                sid,
                generation: 1
            } if sid == "listed"
        ));
        assert!(matches!(
            harness.event().await,
            Event::Attached {
                sid,
                generation: 1,
                entries,
                history_available: false,
                ..
            } if sid == "listed" && entries.is_empty()
        ));
        assert!(matches!(
            harness.event().await,
            Event::OperationFailed {
                operation: Operation::OpenHistory,
                ..
            }
        ));
        assert!(matches!(
            harness.event().await,
            Event::Repairing {
                sid,
                generation: 1
            } if sid == "listed"
        ));

        let detach = harness.answer_harvester(json!({})).await;

        assert_eq!(detach["cmd"]["type"], "detach");
        assert!(matches!(
            harness.event().await,
            Event::RoomLeft {
                sid,
                generation: 1
            } if sid == "listed"
        ));

        let reattach = harness.answer_harvester(json!({})).await;

        assert_eq!(reattach["cmd"]["type"], "attach");
        assert!(matches!(
            harness.event().await,
            Event::RoomJoined {
                sid,
                generation: 1
            } if sid == "listed"
        ));
        let repaired = harness
            .answer_pi(json!({"entries": [], "leafId": "leaf"}))
            .await;

        assert_eq!(repaired["cmd"]["type"], "get_entries");
        assert!(matches!(
            harness.event().await,
            Event::HistoryReplaced {
                sid,
                generation: 1,
                ..
            } if sid == "listed"
        ));
    }

    #[tokio::test]
    async fn attachment_transitions_serialize_per_session_not_per_machine() {
        let harness = Harness::new();
        let first = attachment_change_for(&harness.context, "first");
        let first_again = attachment_change_for(&harness.context, "first");
        let second = attachment_change_for(&harness.context, "second");
        let _first_guard = first.lock().await;

        assert!(first_again.try_lock().is_err());
        assert!(second.try_lock().is_ok());
    }

    #[tokio::test]
    async fn prompt_success_is_acknowledged_with_its_submission_and_generation() {
        let mut harness = Harness::new();
        let submission = SubmissionId(19);

        install_attachment(&harness.context, "session", 4, None);
        spawn_prompt_flow(
            harness.context.clone(),
            "session".to_string(),
            "hello".to_string(),
            submission,
            4,
        );
        let request = harness.answer_pi(json!({})).await;

        assert_eq!(request["cmd"]["type"], "prompt");
        assert_eq!(request["cmd"]["message"], "hello");
        assert!(matches!(
            harness.event().await,
            Event::PromptDelivered {
                sid,
                generation: 4,
                submission: delivered
            } if sid == "session" && delivered == submission
        ));
    }

    #[tokio::test]
    async fn disconnect_and_pi_response_race_to_one_prompt_outcome() {
        let mut harness = Harness::new();
        let submission = SubmissionId(20);

        install_attachment(&harness.context, "session", 4, None);
        spawn_prompt_flow(
            harness.context.clone(),
            "session".to_string(),
            "hello".to_string(),
            submission,
            4,
        );
        let request = harness.request().await;

        fail_pending_prompts(&harness.context);

        assert!(matches!(
            harness.event().await,
            Event::PromptFailed {
                submission: failed,
                ..
            } if failed == submission
        ));

        harness
            .daemon
            .writer
            .send_json_for_test(json!({
                "id": request["id"],
                "type": "pi_response",
                "sid": request["sid"],
                "response": {"type": "response", "success": true, "data": {}},
            }))
            .await;

        assert!(tokio::time::timeout(QUIET, harness.events.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn two_sessions_on_one_bridge_stay_attached_and_repair_independently() {
        let mut harness = Harness::new();

        for (sid, leaf) in [("first", "leaf-1"), ("second", "leaf-2")] {
            spawn_flow(open_flow(
                harness.context.clone(),
                sid.to_string(),
                "/repo".to_string(),
                1,
            ));
            harness.serve_open(leaf, 1).await;
            let _room_joined = harness.event().await;
            let _attached = harness.event().await;
        }

        assert!(is_attached(&harness.context, "first", 1));
        assert!(is_attached(&harness.context, "second", 1));

        handle_event_gap(&harness.context, "first".to_string(), 3);
        assert!(matches!(
            harness.event().await,
            Event::Repairing {
                sid,
                generation: 1
            } if sid == "first"
        ));
        let detach = harness.answer_harvester(json!({})).await;

        assert_eq!(detach["cmd"]["type"], "detach");
        assert!(matches!(
            harness.event().await,
            Event::RoomLeft {
                sid,
                generation: 1
            } if sid == "first"
        ));

        let attach = harness.answer_harvester(json!({})).await;

        assert_eq!(attach["cmd"]["type"], "attach");
        assert!(matches!(
            harness.event().await,
            Event::RoomJoined {
                sid,
                generation: 1
            } if sid == "first"
        ));

        let repair = harness
            .answer_pi(json!({"entries": [{"type": "custom", "id": "new"}], "leafId": "leaf-3"}))
            .await;

        assert_eq!(repair["cmd"]["since"], "leaf-1");
        assert!(matches!(
            harness.event().await,
            Event::HistoryExtended {
                sid,
                generation: 1,
                leaf_id: Some(leaf_id),
                ..
            } if sid == "first" && leaf_id == "leaf-3"
        ));
        assert_eq!(
            cursor_for(&harness.context, "second", 1).as_deref(),
            Some("leaf-2")
        );
    }

    #[tokio::test]
    async fn attachment_generations_are_scoped_per_session() {
        let (client_side, _daemon_side) = tokio::io::duplex(1024);
        let (client, _session_events) =
            DaemonClient::start(DaemonConnection::over(client_side, None));
        let (events, _receiver) = unbounded_channel();
        let context = BridgeContext {
            client,
            events: EventSender {
                machine: "machine".to_string(),
                generation: 1,
                sender: events,
            },
            attachments: Arc::new(Mutex::new(HashMap::from([
                (
                    "first".to_string(),
                    Attachment {
                        generation: 1,
                        cursor: None,
                    },
                ),
                (
                    "second".to_string(),
                    Attachment {
                        generation: 4,
                        cursor: None,
                    },
                ),
            ]))),
            attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
            resyncing: Arc::new(Mutex::new(HashSet::new())),
            attachment_changes: Arc::new(Mutex::new(HashMap::new())),
            pending_prompts: Arc::new(Mutex::new(HashMap::new())),
        };

        remove_attachment_if_generation(&context, "first", 0);

        assert!(is_attached(&context, "first", 1));
        assert!(is_attached(&context, "second", 4));

        remove_attachment_if_generation(&context, "first", 1);

        assert!(!is_attached(&context, "first", 1));
        assert!(is_attached(&context, "second", 4));
    }

    #[tokio::test]
    async fn stale_cursor_writes_cannot_repair_a_new_generation() {
        let (client_side, _daemon_side) = tokio::io::duplex(1024);
        let (client, _session_events) =
            DaemonClient::start(DaemonConnection::over(client_side, None));
        let (events, _receiver) = unbounded_channel();
        let context = BridgeContext {
            client,
            events: EventSender {
                machine: "machine".to_string(),
                generation: 1,
                sender: events,
            },
            attachments: Arc::new(Mutex::new(HashMap::from([(
                "sid".to_string(),
                Attachment {
                    generation: 2,
                    cursor: Some("new".to_string()),
                },
            )]))),
            attach_buffers: Arc::new(Mutex::new(AttachEventBuffers::default())),
            resyncing: Arc::new(Mutex::new(HashSet::new())),
            attachment_changes: Arc::new(Mutex::new(HashMap::new())),
            pending_prompts: Arc::new(Mutex::new(HashMap::new())),
        };

        record_cursor(&context, "sid", 1, Some("stale".to_string()));

        assert_eq!(cursor_for(&context, "sid", 2).as_deref(), Some("new"));
    }
}
