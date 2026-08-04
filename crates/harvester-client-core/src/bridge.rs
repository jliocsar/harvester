//! The per-machine bridge: one background thread runs a current-thread tokio
//! runtime that owns that machine's daemon connection (this crate is tokio;
//! gpui's executor has no tokio reactor). Frontends talk to bridges over
//! tokio mpsc channels, which are reactor-independent and therefore awaitable
//! straight from gpui tasks: commands go down each bridge's own sender,
//! events from every bridge come back tagged with the machine key on one
//! shared channel. This module is deliberately gpui-free — it (plus
//! `machines.rs`) is the whole client backbone any frontend would reuse
//! (docs/SPEC.md: a mobile client needs only an SSH transport, zero protocol
//! changes).
//!
//! Every command runs as a straight-line async flow on a [`DaemonClient`] —
//! responses resolve at their await points, so there is no response-routing
//! state machine here at all.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::{HarvesterCmd, SessionInfo, SessionsData};

use crate::bootstrap::bootstrap_and_connect;
use crate::client::{DaemonClient, SessionEvent};
use crate::host::Host;
use crate::token::RequestToken;

/// UI → bridge.
///
/// A command that is answered by one event carries the [`RequestToken`] that
/// answer echoes (docs/SPEC.md §Decisions, C2). `Open`, `StartSession` and
/// `Prompt` carry none: they have no reply — what they produce is the
/// session's own event stream, which is keyed by sid and outlives the
/// request that started it.
#[derive(Debug)]
pub enum Command {
    /// Open a session: `create_session` (resume-or-create, idempotent for an
    /// already-running sid) then attach. Deliberately carries no running flag —
    /// the daemon owns that truth, and a client's cached copy goes stale the
    /// moment a session exits while it is detached. Ends in `Event::Attached`
    /// or a failure event.
    Open {
        sid: String,
        cwd: String,
    },
    Prompt {
        sid: String,
        text: String,
    },
    /// Turn a ghost chat into a real session (SPEC §Flows "Start session"):
    /// `create_session` → `attach` → first prompt, all riding pi's in-order
    /// stdin queue — no readiness probe. Ends in `Event::Attached` (with
    /// empty entries — the session is brand new) or a failure event.
    StartSession {
        sid: String,
        cwd: String,
        provider: Option<String>,
        model: Option<String>,
        name: String,
        first_prompt: String,
    },
    /// Fetch the models pi would offer in `cwd` (daemon `list_models`,
    /// uncached — costs a pi startup). Answered by `Event::ModelsListed`.
    /// For a running session use `ListSessionModels` instead.
    ListModels {
        cwd: String,
        token: RequestToken,
    },
    /// Fetch the models a running session offers (passthrough
    /// `get_available_models` — no throwaway pi). Answered by
    /// `Event::SessionModelsListed`.
    ListSessionModels {
        sid: String,
        token: RequestToken,
    },
    /// Fetch a running session's state (passthrough `get_state`) — the chat
    /// reads its current model from it. Answered by `Event::SessionState`.
    GetSessionState {
        sid: String,
        token: RequestToken,
    },
    /// Switch a running session's model (passthrough `set_model`). Answered
    /// by `Event::ModelSet`.
    SetModel {
        sid: String,
        provider: String,
        model_id: String,
        token: RequestToken,
    },
}

/// bridge → UI. Pi payloads stay verbatim `Value` — the client speaks Pi.
///
/// An event answering a command echoes that command's [`RequestToken`]; the
/// `sid` a session-scoped answer also carries is routing (which chat renders
/// it), not correlation. Everything else here is unsolicited — connection
/// lifecycle and the session stream — and carries no token.
#[derive(Debug)]
pub enum Event {
    Connected {
        daemon_version: String,
    },
    ConnectFailed {
        detail: String,
    },
    Sessions(Vec<SessionInfo>),
    /// Attach completed and history was replayed via passthrough
    /// `get_entries`; `entries` is pi's list, verbatim.
    Attached {
        sid: String,
        entries: Vec<Value>,
    },
    /// One live pi event for an attached session, verbatim.
    Pi {
        sid: String,
        event: Value,
    },
    SessionExit {
        sid: String,
        exit_code: Option<i32>,
    },
    /// A request failed daemon- or pi-side. Statusbar material; the
    /// connection itself is still healthy.
    RequestFailed {
        detail: String,
    },
    /// A `ListModels` answered; pi's `data` verbatim (the client speaks Pi —
    /// the workbench extracts what its picker renders). Errors render inside
    /// the ghost chat, not the statusbar.
    ModelsListed {
        token: RequestToken,
        result: Result<Value, String>,
    },
    /// A `ListSessionModels` answered; pi's `data` verbatim. Errors render
    /// in the owning chat, not the statusbar.
    SessionModelsListed {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    /// A `GetSessionState` answered; pi's `data` verbatim (`model` is the
    /// field the chat reads).
    SessionState {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    /// A `SetModel` answered; `Ok` carries pi's `data` — the full Model
    /// object now in effect.
    ModelSet {
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
    },
    Disconnected,
}

/// Handle to one machine's bridge thread. Its events arrive on the shared
/// channel passed to [`start`], tagged with the machine key.
pub struct BridgeHandle {
    pub commands: UnboundedSender<Command>,
}

/// The shared event channel's sender, bound to one machine key so bridge
/// internals stay machine-agnostic.
#[derive(Clone)]
struct EventSender {
    machine: String,
    sender: UnboundedSender<(String, Event)>,
}

impl EventSender {
    fn send(&self, event: Event) {
        // A dropped receiver just means the window closed.
        let _ = self.sender.send((self.machine.clone(), event));
    }
}

/// Everything a command flow needs; cheap to clone into its task.
#[derive(Clone)]
struct BridgeContext {
    client: DaemonClient,
    events: EventSender,
    /// The one session this bridge is attached to (single chat pane). The
    /// async mutex also *serializes* attachment changes: a second Open waits
    /// out the first flow instead of interleaving with it.
    attached: Arc<AsyncMutex<Option<String>>>,
    /// Live events held back while an attach flow is still fetching history.
    /// std::sync — every use is a lock/mutate/unlock with no await inside.
    attach_buffers: Arc<Mutex<AttachEventBuffers>>,
    /// Dialog ids already answered, per session. Shared rather than owned by
    /// the select loop because a flushed attach buffer replays through
    /// [`handle_pi_event`] from inside a flow. std::sync — never held across
    /// an await.
    answered_dialogs: Arc<Mutex<HashSet<(String, String)>>>,
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
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            events.send(Event::ConnectFailed {
                detail: format!("tokio runtime: {error}"),
            });

            return;
        }
    };

    runtime.block_on(bridge_main(host, daemon_binary, commands, events));
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
        answered_dialogs: Arc::new(Mutex::new(HashSet::new())),
    };

    // The sidebar renders from this; fetch immediately.
    spawn_flow(sessions_flow(context.clone()));

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // UI dropped the bridge; dropping the client tears down the
                // connection and any ssh forward.
                None => break,
                Some(command) => dispatch(command, &context),
            },
            event = session_events.recv() => match event {
                Some(SessionEvent::Pi { sid, event }) => {
                    handle_live_pi_event(&context, sid, event);
                }
                Some(SessionEvent::SessionExit { sid, exit_code }) => {
                    context.events.send(Event::SessionExit { sid, exit_code });
                }
                // Recover in the backbone, not the UI: refetch the history we
                // just lost and republish it as `Attached`, which every
                // frontend already knows how to render.
                Some(SessionEvent::EventGap { sid, skipped }) => {
                    eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
                    spawn_flow(resync_flow(context.clone(), sid));
                }
                Some(SessionEvent::Disconnected) | None => {
                    context.events.send(Event::Disconnected);

                    break;
                }
            },
        }
    }
}

/// Every command is its own spawned straight-line flow; the loop above never
/// waits on the daemon, so event forwarding cannot stall behind a slow command.
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
        Command::ListSessionModels { sid, token } => {
            spawn_flow(session_models_flow(context, sid, token))
        }
        Command::GetSessionState { sid, token } => {
            spawn_flow(session_state_flow(context, sid, token))
        }
        Command::SetModel {
            sid,
            provider,
            model_id,
            token,
        } => spawn_flow(set_model_flow(context, sid, provider, model_id, token)),
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
}

// --- the attach window -----------------------------------------------------
//
// The daemon starts streaming a session's events the instant its `attach`
// lands (docs/DAEMON.md §attach — deliberate, and not ours to reorder), but a
// frontend has nowhere to put them until `Event::Attached` publishes the
// `get_entries` snapshot one round trip later. An event that arrives in that
// window is missing from the snapshot *and* dropped by the frontend: a lost
// `tool_execution_end` leaves a tool card spinning for the life of that chat
// (streaming text self-repairs — every `message_update` carries the whole
// partial — tool ends do not). So the bridge holds that window back.

/// How many live events one session may hold back while its history is in
/// flight. A `get_entries` round trip is milliseconds and pi emits a few
/// events per second, so this is orders of magnitude of headroom; the cap only
/// exists so a wedged fetch cannot grow a `Vec` without bound.
const ATTACH_EVENT_BUFFER_CAP: usize = 1024;

/// What the select loop should do with an event it just offered the buffers.
enum PushOutcome {
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
enum FlushOutcome {
    /// Deliver these, in arrival order, before anything else.
    Events(Vec<Value>),
    /// Events were lost, so the snapshot just published is already stale.
    Overflowed,
}

enum SessionBuffer {
    Buffering(Vec<Value>),
    Overflowed,
}

/// The held-back windows, keyed by sid. Pure and synchronous on purpose: it is
/// the one piece of this fix that can be tested without a daemon.
#[derive(Default)]
struct AttachEventBuffers {
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

    fn push(&mut self, sid: &str, event: Value) -> PushOutcome {
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
struct AttachRegistration {
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
    fn register(buffers: &Arc<Mutex<AttachEventBuffers>>, sid: &str) -> AttachRegistration {
        lock_attach_buffers(buffers).register(sid);

        AttachRegistration {
            buffers: buffers.clone(),
            sid: sid.to_string(),
            flushed: false,
        }
    }

    /// The held events, in arrival order. Consuming `self` makes "flushed" and
    /// "registration released" the same event.
    fn flush(mut self) -> FlushOutcome {
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

fn lock_attach_buffers(
    buffers: &Arc<Mutex<AttachEventBuffers>>,
) -> std::sync::MutexGuard<'_, AttachEventBuffers> {
    buffers.lock().expect("attach buffer mutex")
}

/// One event off the daemon stream: held back while its session's history is
/// in flight, delivered otherwise.
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

/// Release the window the instant the frontend has a chat to put it in. There
/// is no await between the `Attached` send and this, so no live event can slip
/// in front of the events it held back.
fn deliver_buffered_events(context: &BridgeContext, sid: String, registration: AttachRegistration) {
    match registration.flush() {
        FlushOutcome::Events(events) => {
            for event in events {
                handle_pi_event(context, sid.clone(), event);
            }
        }
        // Reuse the daemon's own vocabulary for this situation rather than
        // inventing a second recovery: events were lost, so the snapshot we
        // just published is stale and the cure is another `get_entries`. The
        // retry can in principle overflow again, but that needs a session
        // outrunning the cap on every attempt — the ring in front of us is the
        // narrower bound.
        FlushOutcome::Overflowed => {
            eprintln!("bridge: held-back events overflowed for session {sid}; resyncing");
            spawn_flow(resync_flow(context.clone(), sid));
        }
    }
}

// --- INTERIM: the unanswered-dialog stopgap --------------------------------
//
// Delete this whole block — the const, `handle_pi_event`'s dialog branch,
// `forwarded_dialog`, `cancel_dialog_flow` and their tests — when the
// dialogs-as-modals slice lands (docs/SPEC.md §Flows, docs/DAEMON.md §Dialogs
// and its decision C5). It exists because harvesterd's auto-cancel is an
// *empty-room* safety net only: with a client attached — the normal state — a
// dialog nobody answers blocks pi indefinitely, and no client answers today.

/// The pi `extension_ui_request` methods that block the agent until they are
/// answered. Same list as harvesterd's `DIALOG_METHODS`; the fire-and-forget
/// methods (`notify`, `setStatus`, …) are forwarded and never answered.
const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// One pi event on its way to the frontend: forwarded verbatim, and — interim
/// — auto-cancelled first when it is a dialog request. Reached both live and
/// from a flushed attach window, so it must not depend on the select loop.
fn handle_pi_event(context: &BridgeContext, sid: String, event: Value) {
    if let Some((dialog_id, method)) = forwarded_dialog(&event) {
        // The daemon replays a pending dialog to every client that attaches
        // later, so the same id arriving twice is by design (DAEMON.md
        // §Dialogs) — answer each id once. Unbounded by session lifetime, but
        // a dialog needs a user-installed pi extension to exist at all.
        let first_sighting = context
            .answered_dialogs
            .lock()
            .expect("answered dialogs mutex")
            .insert((sid.clone(), dialog_id.to_string()));

        if first_sighting {
            spawn_flow(cancel_dialog_flow(
                context.clone(),
                sid.clone(),
                dialog_id.to_string(),
                method.to_string(),
            ));
        }
    }

    context.events.send(Event::Pi { sid, event });
}

/// `(id, method)` when `event` is a dialog request Harvester owes an answer
/// for. Reads pi's payload without touching it — the client speaks Pi.
fn forwarded_dialog(event: &Value) -> Option<(&str, &str)> {
    if event.get("type").and_then(Value::as_str)? != "extension_ui_request" {
        return None;
    }

    let method = event.get("method").and_then(Value::as_str)?;

    if !DIALOG_METHODS.contains(&method) {
        return None;
    }

    let id = event.get("id").and_then(Value::as_str)?;

    Some((id, method))
}

/// Cancels one dialog and tells the user an agent asked something Harvester
/// could not show. `cancelled: true` is exactly what pi's extension host sees
/// when a human presses Escape, and it is fire-and-forget (`response: null`).
async fn cancel_dialog_flow(
    context: BridgeContext,
    sid: String,
    dialog_id: String,
    method: String,
) {
    let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});

    if let Err(error) = context.client.pi(&sid, cancel).await {
        report_failure(&context, "cancel dialog", error.to_string());

        return;
    }

    context.events.send(Event::RequestFailed {
        detail: format!(
            "an extension asked a `{method}` question; Harvester cannot show dialogs yet, \
             so it was cancelled"
        ),
    });
}

async fn sessions_flow(context: BridgeContext) {
    let sessions = context
        .client
        .harvester(HarvesterCmd::ListSessions { cwd: None })
        .await
        .map_err(|error| error.to_string())
        .and_then(|response| response.into_result())
        .and_then(|data| {
            serde_json::from_value::<SessionsData>(data)
                .map_err(|error| format!("malformed list_sessions response: {error}"))
        });

    match sessions {
        Ok(data) => context.events.send(Event::Sessions(data.sessions)),
        Err(detail) => report_failure(&context, "list_sessions", detail),
    }
}

/// `Open`: create-or-resume, then attach. Resume is the same call as create
/// (DAEMON.md), and it is idempotent for a live session, so this one chain
/// covers a running session, an exited one, and one that died while we were
/// detached — with no client-side state to go stale. Follow-up requests queue
/// on pi's stdin, so no readiness probe is needed.
async fn open_flow(context: BridgeContext, sid: String, cwd: String) {
    let attached = context.attached.clone();
    let mut attachment = attached.lock().await;

    detach_previous(&context, &mut attachment, &sid).await;

    let create = context
        .client
        .harvester(HarvesterCmd::CreateSession {
            sid: sid.clone(),
            cwd,
            provider: None,
            model: None,
            name: None,
        })
        .await;

    if let Err(detail) = flatten_harvester(create) {
        report_failure(&context, "create session", detail);

        return;
    }

    // Before the attach, not after: the daemon streams from the moment it
    // lands, and `Attached` is a whole `get_entries` away. Dropped on every
    // early return below by the guard's `Drop`.
    let registration = AttachRegistration::register(&context.attach_buffers, &sid);

    if !attach(&context, &mut attachment, &sid).await {
        return;
    }

    // The attachment transition is done, so the guard's job is done: an async
    // mutex may cover a state transition, never a remote call. Holding it
    // across `get_entries` is what let one wedged pi block every later Open
    // on this bridge for the life of the app.
    drop(attachment);

    // History replay: attach → get_entries → live events (DAEMON.md §attach).
    replay_history(&context, sid, registration).await;
}

/// A resync after events were lost: the daemon's `event_gap` and our own
/// attach-window overflow are the same situation, and `get_entries` is the
/// same cure. Owns its future for [`spawn_flow`], and registers its own
/// window — a resync that dropped the events arriving during its own fetch
/// would reintroduce exactly the hole it exists to close.
async fn resync_flow(context: BridgeContext, sid: String) {
    let registration = AttachRegistration::register(&context.attach_buffers, &sid);

    replay_history(&context, sid, registration).await;
}

/// Fetch a session's history and publish it as `Attached` — the one path that
/// (re)seeds a chat, whether the user just opened the session or the daemon
/// told us we missed events. pi's session file is the source of truth, so a
/// refetch is always the correct repair. `registration` holds back the live
/// events that race this fetch; it is released the moment `Attached` is out.
async fn replay_history(context: &BridgeContext, sid: String, registration: AttachRegistration) {
    let entries = match pi_flow_result(context, &sid, json!({"type": "get_entries"})).await {
        Ok(data) => data
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        Err(detail) => {
            report_failure(context, "get_entries", detail);

            return;
        }
    };

    context.events.send(Event::Attached {
        sid: sid.clone(),
        entries,
    });
    deliver_buffered_events(context, sid, registration);
}

/// `StartSession`: the ghost chat's create → attach → first-prompt chain as
/// one straight line. `Attached` publishes with empty entries — the session
/// file is brand new, there is no history to fetch.
async fn start_session_flow(
    context: BridgeContext,
    sid: String,
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: String,
    first_prompt: String,
) {
    let attached = context.attached.clone();
    let mut attached = attached.lock().await;

    detach_previous(&context, &mut attached, &sid).await;

    let create = context
        .client
        .harvester(HarvesterCmd::CreateSession {
            sid: sid.clone(),
            cwd,
            provider,
            model,
            name: Some(name),
        })
        .await;

    if let Err(detail) = flatten_harvester(create) {
        report_failure(&context, "create session", detail);

        return;
    }

    // No `get_entries` here, so the window is empty in practice — registered
    // anyway so that "events between attach and `Attached` are held" is a
    // property of every attach path rather than an accident of this one.
    let registration = AttachRegistration::register(&context.attach_buffers, &sid);

    if !attach(&context, &mut attached, &sid).await {
        return;
    }

    // Swap the ghost for the live chat right away; the prompt's queued-ack
    // rides its own flow (only rejection is notable).
    context.events.send(Event::Attached {
        sid: sid.clone(),
        entries: Vec::new(),
    });
    deliver_buffered_events(&context, sid.clone(), registration);
    spawn_flow(prompt_flow(context.clone(), sid, first_prompt));
}

/// One chat pane means one attachment: leaving the previous session keeps
/// the daemon from streaming events nobody renders.
async fn detach_previous(context: &BridgeContext, attached: &mut Option<String>, next_sid: &str) {
    let Some(previous_sid) = attached.take() else {
        return;
    };

    if previous_sid == next_sid {
        return;
    }

    let detach = context
        .client
        .harvester(HarvesterCmd::Detach { sid: previous_sid })
        .await;

    if let Err(detail) = flatten_harvester(detach) {
        report_failure(context, "detach", detail);
    }
}

/// The shared attach step; records the attachment on success, reports and
/// returns false otherwise.
async fn attach(context: &BridgeContext, attached: &mut Option<String>, sid: &str) -> bool {
    let response = context
        .client
        .harvester(HarvesterCmd::Attach {
            sid: sid.to_string(),
        })
        .await;

    match flatten_harvester(response) {
        Ok(_data) => {
            *attached = Some(sid.to_string());

            true
        }
        Err(detail) => {
            report_failure(context, "attach", detail);

            false
        }
    }
}

/// Accepted means queued, not completed; progress arrives as events. Only
/// rejection is notable.
///
/// `streamingBehavior` is mandatory in practice: pi rejects a prompt outright
/// while it is streaming unless one is given (agent-session.ts, pi 0.83.0), so
/// without it every prompt typed mid-turn is dropped. `steer` matches what pi's
/// own interactive mode does with a mid-turn message; pi ignores the field
/// entirely when it is idle.
async fn prompt_flow(context: BridgeContext, sid: String, text: String) {
    match context
        .client
        .pi(
            &sid,
            json!({"type": "prompt", "message": text, "streamingBehavior": "steer"}),
        )
        .await
    {
        Ok(response) if !pi_success(&response) => report_failure(
            &context,
            "prompt",
            pi_error(&response).unwrap_or_else(|| "unknown error".to_string()),
        ),
        Ok(_accepted) => {}
        Err(error) => report_failure(&context, "prompt", error.to_string()),
    }
}

async fn list_models_flow(context: BridgeContext, cwd: String, token: RequestToken) {
    let result = match context
        .client
        .harvester(HarvesterCmd::ListModels { cwd })
        .await
    {
        Ok(response) => response.into_result(),
        Err(error) => Err(error.to_string()),
    };

    context.events.send(Event::ModelsListed { token, result });
}

async fn session_models_flow(context: BridgeContext, sid: String, token: RequestToken) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_available_models"})).await;

    context
        .events
        .send(Event::SessionModelsListed { sid, token, result });
}

async fn session_state_flow(context: BridgeContext, sid: String, token: RequestToken) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_state"})).await;

    context
        .events
        .send(Event::SessionState { sid, token, result });
}

async fn set_model_flow(
    context: BridgeContext,
    sid: String,
    provider: String,
    model_id: String,
    token: RequestToken,
) {
    let cmd = json!({"type": "set_model", "provider": provider, "modelId": model_id});
    let result = pi_flow_result(&context, &sid, cmd).await;

    context.events.send(Event::ModelSet { sid, token, result });
}

/// A pi command → the `Result` the picker/statusbar events carry: `data`
/// verbatim on success, pi's error string otherwise.
async fn pi_flow_result(context: &BridgeContext, sid: &str, cmd: Value) -> Result<Value, String> {
    match context.client.pi(sid, cmd).await {
        Ok(response) if pi_success(&response) => {
            Ok(response.get("data").cloned().unwrap_or(Value::Null))
        }
        Ok(response) => Err(pi_error(&response).unwrap_or_else(|| "unknown error".to_string())),
        Err(error) => Err(error.to_string()),
    }
}

/// Transport error or daemon-reported failure → one error string.
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

/// The attach window and the interim dialog stopgap in isolation — no daemon,
/// no pi. Delete the dialog half with the block it covers when dialogs become
/// modals.
#[cfg(test)]
mod tests {
    use super::*;

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

    fn dialog_event(method: &str) -> Value {
        json!({
            "type": "extension_ui_request",
            "id": "dlg-1",
            "method": method,
            "params": {"message": "pick one", "options": ["a", "b"]},
        })
    }

    #[test]
    fn every_blocking_dialog_method_is_recognised() {
        for method in DIALOG_METHODS {
            assert_eq!(
                forwarded_dialog(&dialog_event(method)),
                Some(("dlg-1", method)),
                "{method} blocks pi until answered, so it must be cancelled"
            );
        }
    }

    #[test]
    fn fire_and_forget_ui_methods_are_left_alone() {
        // docs/DAEMON.md §Dialogs: forwarded, never answered — cancelling one
        // would send pi a response for an id it is not waiting on.
        for method in ["notify", "setStatus", "setWidget", "setTitle"] {
            assert_eq!(forwarded_dialog(&dialog_event(method)), None);
        }
    }

    #[test]
    fn ordinary_pi_events_are_not_dialogs() {
        assert_eq!(forwarded_dialog(&json!({"type": "agent_settled"})), None);
        assert_eq!(
            forwarded_dialog(&json!({"type": "message_update", "method": "select"})),
            None,
            "the type is what makes it a dialog request, not the method"
        );
    }

    #[test]
    fn a_dialog_without_a_string_id_is_not_answerable() {
        let mut event = dialog_event("confirm");
        event["id"] = json!(7);

        assert_eq!(forwarded_dialog(&event), None);
    }

    #[test]
    fn the_replayed_copy_of_a_dialog_is_answered_only_once() {
        // The daemon replays every pending dialog to a client that attaches
        // later (docs/DAEMON.md §Dialogs), so the same id arrives twice by
        // design; the second cancel must be dropped, not sent.
        let mut answered = HashSet::new();
        let dialog = ("01H".to_string(), "dlg-1".to_string());

        assert!(answered.insert(dialog.clone()), "first sighting cancels");
        assert!(!answered.insert(dialog), "the replay must not cancel again");
    }

    #[test]
    fn the_same_dialog_id_in_another_session_is_a_different_dialog() {
        let mut answered = HashSet::new();

        assert!(answered.insert(("01H".to_string(), "dlg-1".to_string())));
        assert!(answered.insert(("02H".to_string(), "dlg-1".to_string())));
    }
}
