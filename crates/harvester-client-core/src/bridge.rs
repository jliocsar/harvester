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

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::{HarvesterCmd, SessionInfo, SessionsData};

use crate::bootstrap::bootstrap_and_connect;
use crate::client::{DaemonClient, SessionEvent};
use crate::host::Host;
use crate::workspace::{list_directory, validate_git_workspace, RemoteDirEntry};

/// UI → bridge.
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
    },
    /// Fetch the models a running session offers (passthrough
    /// `get_available_models` — no throwaway pi). Answered by
    /// `Event::SessionModelsListed`.
    ListSessionModels {
        sid: String,
    },
    /// Fetch a running session's state (passthrough `get_state`) — the chat
    /// reads its current model from it. Answered by `Event::SessionState`.
    GetSessionState {
        sid: String,
    },
    /// Switch a running session's model (passthrough `set_model`). Answered
    /// by `Event::ModelSet`.
    SetModel {
        sid: String,
        provider: String,
        model_id: String,
    },
    /// List a directory for the folder picker (a `Host` exec, multiplexed —
    /// never the daemon). Answered by `Event::DirListed`.
    ListDir {
        path: String,
    },
    /// `git -C <path> rev-parse --git-dir` on the machine. Answered by
    /// `Event::WorkspaceValidated`.
    ValidateWorkspace {
        path: String,
    },
}

/// bridge → UI. Pi payloads stay verbatim `Value` — the client speaks Pi.
#[derive(Debug)]
pub enum Event {
    Connected {
        daemon_version: String,
        /// The machine's `$HOME` — the folder picker's start directory.
        remote_home: String,
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
    /// the workbench extracts what its picker renders). `cwd` is echoed back
    /// as the staleness guard, like `DirListed`'s `path`. Errors render
    /// inside the ghost chat, not the statusbar.
    ModelsListed {
        cwd: String,
        result: Result<Value, String>,
    },
    /// A `ListSessionModels` answered; pi's `data` verbatim. Errors render
    /// in the owning chat, not the statusbar.
    SessionModelsListed {
        sid: String,
        result: Result<Value, String>,
    },
    /// A `GetSessionState` answered; pi's `data` verbatim (`model` is the
    /// field the chat reads).
    SessionState {
        sid: String,
        result: Result<Value, String>,
    },
    /// A `SetModel` answered; `Ok` carries pi's `data` — the full Model
    /// object now in effect.
    ModelSet {
        sid: String,
        result: Result<Value, String>,
    },
    /// A `ListDir` answered; errors render inside the picker, not the
    /// statusbar.
    DirListed {
        path: String,
        result: Result<Vec<RemoteDirEntry>, String>,
    },
    /// A `ValidateWorkspace` answered; `None` means the path is a git repo.
    WorkspaceValidated {
        path: String,
        error: Option<String>,
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
    host: Host,
    /// The one session this bridge is attached to (single chat pane). The
    /// async mutex also *serializes* attachment changes: a second Open waits
    /// out the first flow instead of interleaving with it.
    attached: Arc<AsyncMutex<Option<String>>>,
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
        remote_home: connected.remote_home,
    });

    let (client, mut session_events) = DaemonClient::start(connected.connection);
    let context = BridgeContext {
        client,
        events,
        host,
        attached: Arc::new(AsyncMutex::new(None)),
    };

    // The sidebar renders from this; fetch immediately.
    spawn_flow(sessions_flow(context.clone()));

    let mut answered_dialogs = HashSet::new();

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
                    handle_pi_event(&context, &mut answered_dialogs, sid, event);
                }
                Some(SessionEvent::SessionExit { sid, exit_code }) => {
                    context.events.send(Event::SessionExit { sid, exit_code });
                }
                // Recover in the backbone, not the UI: refetch the history we
                // just lost and republish it as `Attached`, which every
                // frontend already knows how to render.
                Some(SessionEvent::EventGap { sid, skipped }) => {
                    eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
                    spawn_flow(replay_history_owned(context.clone(), sid));
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
        Command::ListModels { cwd } => spawn_flow(list_models_flow(context, cwd)),
        Command::ListSessionModels { sid } => spawn_flow(session_models_flow(context, sid)),
        Command::GetSessionState { sid } => spawn_flow(session_state_flow(context, sid)),
        Command::SetModel {
            sid,
            provider,
            model_id,
        } => spawn_flow(set_model_flow(context, sid, provider, model_id)),
        Command::ListDir { path } => spawn_flow(list_dir_flow(context, path)),
        Command::ValidateWorkspace { path } => spawn_flow(validate_workspace_flow(context, path)),
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
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

/// One live pi event: forwarded verbatim, and — interim — auto-cancelled first
/// when it is a dialog request.
fn handle_pi_event(
    context: &BridgeContext,
    answered_dialogs: &mut HashSet<(String, String)>,
    sid: String,
    event: Value,
) {
    if let Some((dialog_id, method)) = forwarded_dialog(&event) {
        // The daemon replays a pending dialog to every client that attaches
        // later, so the same id arriving twice is by design (DAEMON.md
        // §Dialogs) — answer each id once. Unbounded by session lifetime, but
        // a dialog needs a user-installed pi extension to exist at all.
        let first_sighting = answered_dialogs.insert((sid.clone(), dialog_id.to_string()));

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

    if !attach(&context, &mut attachment, &sid).await {
        return;
    }

    // The attachment transition is done, so the guard's job is done: an async
    // mutex may cover a state transition, never a remote call. Holding it
    // across `get_entries` is what let one wedged pi block every later Open
    // on this bridge for the life of the app.
    drop(attachment);

    // History replay: attach → get_entries → live events (DAEMON.md §attach).
    replay_history(&context, sid).await;
}

/// Fetch a session's history and publish it as `Attached` — the one path that
/// (re)seeds a chat, whether the user just opened the session or the daemon
/// told us we missed events. pi's session file is the source of truth, so a
/// refetch is always the correct repair.
/// `replay_history` for [`spawn_flow`], which needs an owning future.
async fn replay_history_owned(context: BridgeContext, sid: String) {
    replay_history(&context, sid).await;
}

async fn replay_history(context: &BridgeContext, sid: String) {
    match pi_flow_result(context, &sid, json!({"type": "get_entries"})).await {
        Ok(data) => {
            let entries = data
                .get("entries")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            context.events.send(Event::Attached { sid, entries });
        }
        Err(detail) => report_failure(context, "get_entries", detail),
    }
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

    if !attach(&context, &mut attached, &sid).await {
        return;
    }

    // Swap the ghost for the live chat right away; the prompt's queued-ack
    // rides its own flow (only rejection is notable).
    context.events.send(Event::Attached {
        sid: sid.clone(),
        entries: Vec::new(),
    });
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

async fn list_models_flow(context: BridgeContext, cwd: String) {
    let result = match context
        .client
        .harvester(HarvesterCmd::ListModels { cwd: cwd.clone() })
        .await
    {
        Ok(response) => response.into_result(),
        Err(error) => Err(error.to_string()),
    };

    context.events.send(Event::ModelsListed { cwd, result });
}

async fn session_models_flow(context: BridgeContext, sid: String) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_available_models"})).await;

    context
        .events
        .send(Event::SessionModelsListed { sid, result });
}

async fn session_state_flow(context: BridgeContext, sid: String) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_state"})).await;

    context.events.send(Event::SessionState { sid, result });
}

async fn set_model_flow(context: BridgeContext, sid: String, provider: String, model_id: String) {
    let cmd = json!({"type": "set_model", "provider": provider, "modelId": model_id});
    let result = pi_flow_result(&context, &sid, cmd).await;

    context.events.send(Event::ModelSet { sid, result });
}

/// Path execs take tens of ms even multiplexed; they ride their own flow like
/// everything else, so pi events keep pumping meanwhile.
async fn list_dir_flow(context: BridgeContext, path: String) {
    let result = list_directory(&context.host, &path)
        .await
        .map_err(|error| error.to_string());

    context.events.send(Event::DirListed { path, result });
}

async fn validate_workspace_flow(context: BridgeContext, path: String) {
    let error = validate_git_workspace(&context.host, &path)
        .await
        .err()
        .map(|error| error.to_string());

    context
        .events
        .send(Event::WorkspaceValidated { path, error });
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

/// The interim dialog stopgap in isolation — no daemon, no pi. Delete with the
/// block it covers when dialogs become modals.
#[cfg(test)]
mod tests {
    use super::*;

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
