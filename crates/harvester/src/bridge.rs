//! The tokio ↔ gpui bridge: one background thread per machine runs a
//! current-thread tokio runtime that owns that machine's daemon connection
//! (harvester-client-core is tokio; gpui's executor has no tokio reactor).
//! The UI talks to bridges over tokio mpsc channels, which are
//! reactor-independent and therefore awaitable straight from gpui tasks:
//! commands go down each bridge's own sender, events from every bridge come
//! back tagged with the machine key on one shared channel.

use std::collections::HashMap;
use std::sync::Arc;

use harvester_client_core::{
    bootstrap_and_connect, bootstrap_and_connect_local, list_directory, list_directory_local,
    validate_git_workspace, validate_git_workspace_local, DaemonConnection, DaemonWriter,
    RemoteDirEntry, SshTarget,
};
use harvester_proto::{HarvesterCmd, HarvesterResponse, Outgoing, Request, SessionInfo};
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// UI → bridge.
#[derive(Debug)]
pub enum Command {
    /// Open a session: attach if running, resume (`create_session`, same sid)
    /// then attach if exited. Ends in `Event::Attached` or a failure event.
    Open {
        sid: String,
        cwd: String,
        running: bool,
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
    /// List a remote directory for the folder picker (plain SSH exec,
    /// multiplexed — never the daemon). Answered by `Event::DirListed`.
    ListDir {
        path: String,
    },
    /// `git -C <path> rev-parse --git-dir` over SSH. Answered by
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
        /// Remote `$HOME` — the folder picker's start directory.
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

/// Where a bridge's daemon lives: the hardcoded local machine (harvesterd as
/// a detached child, Unix socket, no SSH — SPEC §Machine) or an SSH
/// destination (bootstrap + socket forward).
#[derive(Debug, Clone)]
pub enum BridgeTarget {
    Local,
    Ssh(SshTarget),
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

/// What an in-flight request id was for; the follow-up on its response
/// depends on it (create chains into attach chains into get_entries).
enum Pending {
    ListSessions,
    /// `cwd` rides along because the daemon response doesn't echo it and the
    /// UI needs it to drop stale answers.
    ListModels {
        cwd: String,
    },
    /// `first_prompt` is carried through the create → attach chain: `Some`
    /// marks a ghost-chat start (send it after attach, skip `get_entries` —
    /// the session file is brand new), `None` a plain resume.
    Create {
        sid: String,
        first_prompt: Option<String>,
    },
    Attach {
        sid: String,
        first_prompt: Option<String>,
    },
    Detach,
    Entries {
        sid: String,
    },
    Prompt,
    SessionModels {
        sid: String,
    },
    SessionState {
        sid: String,
    },
    SetModel {
        sid: String,
    },
}

pub fn start(
    machine: String,
    target: BridgeTarget,
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
        .spawn(move || run_bridge_thread(target, daemon_binary, command_receiver, events))?;

    Ok(BridgeHandle {
        commands: command_sender,
    })
}

fn run_bridge_thread(
    target: BridgeTarget,
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

    runtime.block_on(bridge_main(target, daemon_binary, commands, events));
}

async fn bridge_main(
    target: BridgeTarget,
    daemon_binary: Arc<Vec<u8>>,
    mut commands: UnboundedReceiver<Command>,
    events: EventSender,
) {
    let bootstrap_result = match &target {
        BridgeTarget::Local => bootstrap_and_connect_local(&daemon_binary).await,
        BridgeTarget::Ssh(ssh_target) => bootstrap_and_connect(ssh_target, &daemon_binary).await,
    };
    let connected = match bootstrap_result {
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

    let DaemonConnection { mut reader, writer } = connected.connection;
    let mut link = Link {
        target,
        writer,
        events,
        pending: HashMap::new(),
        last_request_id: 0, // 0 was the handshake ping; ids start at 1
        attached: None,
    };

    // Reads get their own task: `select!` cancels the losing future, and a
    // cancelled `reader.next()` would drop a partially-read line. Channel
    // recvs below are cancel-safe. EOF drops the sender, closing the channel.
    let (line_sender, mut lines) = unbounded_channel::<std::io::Result<Outgoing>>();
    tokio::spawn(async move {
        loop {
            match reader.next().await {
                Ok(Some(outgoing)) => {
                    if line_sender.send(Ok(outgoing)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = line_sender.send(Err(error));

                    break;
                }
            }
        }
    });

    // The sidebar renders from this; fetch immediately.
    link.send_harvester(
        HarvesterCmd::ListSessions { cwd: None },
        Pending::ListSessions,
    )
    .await;

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // UI dropped the bridge; dropping the connection tears down
                // the ssh forward.
                None => break,
                Some(command) => handle_ui_command(command, &mut link).await,
            },
            line = lines.recv() => match line {
                Some(Ok(outgoing)) => handle_daemon_line(outgoing, &mut link).await,
                Some(Err(error)) => {
                    eprintln!("harvester: read from daemon: {error}");
                    link.events.send(Event::Disconnected);

                    break;
                }
                None => {
                    link.events.send(Event::Disconnected);

                    break;
                }
            },
        }
    }
}

async fn handle_ui_command(command: Command, link: &mut Link) {
    match command {
        Command::Open { sid, cwd, running } => handle_open_command(sid, cwd, running, link).await,
        Command::StartSession {
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        } => {
            handle_start_session_command(sid, cwd, provider, model, name, first_prompt, link).await
        }
        Command::ListModels { cwd } => {
            link.send_harvester(
                HarvesterCmd::ListModels { cwd: cwd.clone() },
                Pending::ListModels { cwd },
            )
            .await;
        }
        Command::Prompt { sid, text } => {
            link.send_pi(
                &sid,
                json!({"type": "prompt", "message": text}),
                Pending::Prompt,
            )
            .await;
        }
        Command::ListSessionModels { sid } => {
            link.send_pi(
                &sid.clone(),
                json!({"type": "get_available_models"}),
                Pending::SessionModels { sid },
            )
            .await;
        }
        Command::GetSessionState { sid } => {
            link.send_pi(
                &sid.clone(),
                json!({"type": "get_state"}),
                Pending::SessionState { sid },
            )
            .await;
        }
        Command::SetModel {
            sid,
            provider,
            model_id,
        } => {
            link.send_pi(
                &sid.clone(),
                json!({"type": "set_model", "provider": provider, "modelId": model_id}),
                Pending::SetModel { sid },
            )
            .await;
        }
        Command::ListDir { path } => handle_list_dir(path, link),
        Command::ValidateWorkspace { path } => handle_validate_workspace(path, link),
    }
}

/// `Open`: attach if running, resume then attach if exited. Resume = create
/// with the same sid (DAEMON.md); follow-up requests queue on pi's stdin, so
/// no readiness probe is needed.
async fn handle_open_command(sid: String, cwd: String, running: bool, link: &mut Link) {
    detach_previous(link, &sid).await;

    if running {
        link.send_harvester(
            HarvesterCmd::Attach { sid: sid.clone() },
            Pending::Attach {
                sid,
                first_prompt: None,
            },
        )
        .await;
    } else {
        link.send_harvester(
            HarvesterCmd::CreateSession {
                sid: sid.clone(),
                cwd,
                provider: None,
                model: None,
                name: None,
            },
            Pending::Create {
                sid,
                first_prompt: None,
            },
        )
        .await;
    }
}

/// `StartSession`: the ghost chat's create → attach → prompt chain, entered
/// here; the first prompt rides `Pending::Create` through the follow-ups.
async fn handle_start_session_command(
    sid: String,
    cwd: String,
    provider: Option<String>,
    model: Option<String>,
    name: String,
    first_prompt: String,
    link: &mut Link,
) {
    detach_previous(link, &sid).await;
    link.send_harvester(
        HarvesterCmd::CreateSession {
            sid: sid.clone(),
            cwd,
            provider,
            model,
            name: Some(name),
        },
        Pending::Create {
            sid,
            first_prompt: Some(first_prompt),
        },
    )
    .await;
}

/// One chat pane means one attachment: leaving the previous session keeps
/// the daemon from streaming events nobody renders.
async fn detach_previous(link: &mut Link, next_sid: &str) {
    if let Some(previous_sid) = link.attached.take() {
        if previous_sid != next_sid {
            link.send_harvester(HarvesterCmd::Detach { sid: previous_sid }, Pending::Detach)
                .await;
        }
    }
}

/// Path execs (SSH or local) ride their own task: even multiplexed they take
/// tens of ms, and the select loop must keep pumping pi events meanwhile.
fn handle_list_dir(path: String, link: &Link) {
    let target = link.target.clone();
    let events = link.events.clone();

    tokio::spawn(async move {
        let result = match &target {
            BridgeTarget::Local => list_directory_local(&path).await,
            BridgeTarget::Ssh(ssh_target) => list_directory(ssh_target, &path).await,
        }
        .map_err(|error| error.to_string());

        events.send(Event::DirListed { path, result });
    });
}

fn handle_validate_workspace(path: String, link: &Link) {
    let target = link.target.clone();
    let events = link.events.clone();

    tokio::spawn(async move {
        let error = match &target {
            BridgeTarget::Local => validate_git_workspace_local(&path).await,
            BridgeTarget::Ssh(ssh_target) => validate_git_workspace(ssh_target, &path).await,
        }
        .err()
        .map(|error| error.to_string());

        events.send(Event::WorkspaceValidated { path, error });
    });
}

async fn handle_daemon_line(outgoing: Outgoing, link: &mut Link) {
    match outgoing {
        Outgoing::Event { sid, event } => {
            link.events.send(Event::Pi { sid, event });
        }
        Outgoing::SessionExit { sid, exit_code } => {
            link.events.send(Event::SessionExit { sid, exit_code });
        }
        Outgoing::HarvesterResponse { id, response } => {
            let Some(purpose) = link.take_pending(id.as_ref()) else {
                return;
            };

            match purpose {
                Pending::ListSessions => handle_list_sessions_response(link, response),
                Pending::ListModels { cwd } => handle_list_models_response(link, cwd, response),
                Pending::Create { sid, first_prompt } => {
                    handle_create_response(link, sid, first_prompt, response).await
                }
                Pending::Attach { sid, first_prompt } => {
                    handle_attach_response(link, sid, first_prompt, response).await
                }
                Pending::Detach => handle_detach_response(link, response),
                Pending::Entries { .. }
                | Pending::Prompt
                | Pending::SessionModels { .. }
                | Pending::SessionState { .. }
                | Pending::SetModel { .. } => {
                    eprintln!("harvester: pi-bound request answered by daemon envelope");
                }
            }
        }
        Outgoing::PiResponse { id, response, .. } => {
            let Some(purpose) = link.take_pending(id.as_ref()) else {
                return;
            };

            match purpose {
                Pending::Entries { sid } => handle_entries_response(link, sid, &response),
                Pending::Prompt => handle_prompt_response(link, &response),
                Pending::SessionModels { sid } => {
                    link.events.send(Event::SessionModelsListed {
                        sid,
                        result: pi_result(&response),
                    });
                }
                Pending::SessionState { sid } => {
                    link.events.send(Event::SessionState {
                        sid,
                        result: pi_result(&response),
                    });
                }
                Pending::SetModel { sid } => {
                    link.events.send(Event::ModelSet {
                        sid,
                        result: pi_result(&response),
                    });
                }
                Pending::ListSessions
                | Pending::ListModels { .. }
                | Pending::Create { .. }
                | Pending::Attach { .. }
                | Pending::Detach => {
                    eprintln!("harvester: daemon-bound request answered by pi envelope");
                }
            }
        }
    }
}

fn handle_list_sessions_response(link: &mut Link, response: HarvesterResponse) {
    if response.success {
        link.publish_sessions(response.data.as_ref());
    } else {
        link.report_failure("list_sessions", response.error);
    }
}

fn handle_list_models_response(link: &mut Link, cwd: String, response: HarvesterResponse) {
    let result = if response.success {
        Ok(response.data.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "unknown error".to_string()))
    };
    link.events.send(Event::ModelsListed { cwd, result });
}

/// A `create_session` answered (resume or ghost-chat start); chain into attach.
async fn handle_create_response(
    link: &mut Link,
    sid: String,
    first_prompt: Option<String>,
    response: HarvesterResponse,
) {
    if response.success {
        link.send_harvester(
            HarvesterCmd::Attach { sid: sid.clone() },
            Pending::Attach { sid, first_prompt },
        )
        .await;
    } else {
        link.report_failure("create session", response.error);
    }
}

/// Attach answered; record the attachment. A resume chains into history
/// replay: attach → get_entries → live events (DAEMON.md §attach, no
/// `since`: full history). A ghost-chat start sends the first prompt instead
/// — the session file is brand new, there is no history to fetch — and
/// publishes `Attached` right away so the UI can swap the ghost for the chat.
async fn handle_attach_response(
    link: &mut Link,
    sid: String,
    first_prompt: Option<String>,
    response: HarvesterResponse,
) {
    if response.success {
        link.attached = Some(sid.clone());

        match first_prompt {
            Some(text) => {
                link.send_pi(
                    &sid,
                    json!({"type": "prompt", "message": text}),
                    Pending::Prompt,
                )
                .await;
                link.events.send(Event::Attached {
                    sid,
                    entries: Vec::new(),
                });
            }
            None => {
                link.send_pi(
                    &sid.clone(),
                    json!({"type": "get_entries"}),
                    Pending::Entries { sid },
                )
                .await;
            }
        }
    } else {
        link.report_failure("attach", response.error);
    }
}

fn handle_detach_response(link: &mut Link, response: HarvesterResponse) {
    if !response.success {
        link.report_failure("detach", response.error);
    }
}

/// `get_entries` answered (pi envelope); publish the attached session with
/// its history.
fn handle_entries_response(link: &mut Link, sid: String, response: &Value) {
    if pi_success(response) {
        let entries = response
            .get("data")
            .and_then(|data| data.get("entries"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        link.events.send(Event::Attached { sid, entries });
    } else {
        link.report_failure("get_entries", pi_error(response));
    }
}

/// Accepted means queued, not completed; progress arrives as events. Only
/// rejection is notable.
fn handle_prompt_response(link: &mut Link, response: &Value) {
    if !pi_success(response) {
        link.report_failure("prompt", pi_error(response));
    }
}

/// A pi response → the `Result` the session-model events carry: `data`
/// verbatim on success, pi's error string otherwise.
fn pi_result(response: &Value) -> Result<Value, String> {
    if pi_success(response) {
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(pi_error(response).unwrap_or_else(|| "unknown error".to_string()))
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

struct Link {
    /// The machine this bridge serves; cloned into spawned path-exec tasks.
    target: BridgeTarget,
    writer: DaemonWriter,
    events: EventSender,
    pending: HashMap<u64, Pending>,
    last_request_id: u64,
    /// The one session this client is attached to (single chat pane); the
    /// previous attachment is detached when a new one opens.
    attached: Option<String>,
}

impl Link {
    async fn send_harvester(&mut self, cmd: HarvesterCmd, purpose: Pending) {
        let id = self.next_request_id();
        self.pending.insert(id, purpose);
        let request = Request::Harvester {
            id: Some(json!(id)),
            cmd,
        };
        self.write(&request, id).await;
    }

    async fn send_pi(&mut self, sid: &str, cmd: Value, purpose: Pending) {
        let id = self.next_request_id();
        self.pending.insert(id, purpose);
        let request = Request::Pi {
            id: Some(json!(id)),
            sid: sid.to_string(),
            cmd,
        };
        self.write(&request, id).await;
    }

    async fn write(&mut self, request: &Request, id: u64) {
        if let Err(error) = self.writer.send(request).await {
            self.pending.remove(&id);
            self.events.send(Event::RequestFailed {
                detail: format!("send to daemon: {error}"),
            });
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.last_request_id += 1;

        self.last_request_id
    }

    fn take_pending(&mut self, id: Option<&Value>) -> Option<Pending> {
        self.pending.remove(&id?.as_u64()?)
    }

    fn publish_sessions(&self, data: Option<&Value>) {
        let sessions = data
            .and_then(|data| data.get("sessions"))
            .and_then(|sessions| serde_json::from_value::<Vec<SessionInfo>>(sessions.clone()).ok());

        match sessions {
            Some(sessions) => {
                self.events.send(Event::Sessions(sessions));
            }
            None => {
                self.events.send(Event::RequestFailed {
                    detail: "malformed list_sessions response".to_string(),
                });
            }
        }
    }

    fn report_failure(&self, context: &str, error: Option<String>) {
        let detail = format!(
            "{context}: {}",
            error.unwrap_or_else(|| "unknown error".to_string())
        );
        self.events.send(Event::RequestFailed { detail });
    }
}
