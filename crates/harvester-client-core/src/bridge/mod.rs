//! The per-machine bridge: one background thread runs a current-thread tokio
//! runtime that owns that machine's daemon connection (this crate is tokio;
//! gpui's executor has no tokio reactor). Frontends talk to bridges over
//! tokio mpsc channels, which are reactor-independent and therefore awaitable
//! straight from gpui tasks: commands go down each bridge's own sender,
//! events from every bridge come back tagged with the machine key on one
//! shared channel. This module is deliberately gpui-free.
//!
//! This file is the whole of the bridge's *shape*: the [`Command`]/[`Event`]
//! contract a frontend codes against, the thread and its select loop, and the
//! router that turns one command into one spawned flow. The three things it
//! deliberately does not hold:
//!
//! - `flows` — one straight-line async flow per command. Responses resolve at
//!   their await points, so there is no response-routing state machine here.
//! - `attach_window` — the live events held back between `attach` and
//!   `Attached`; pure, synchronous and tested on its own.
//! - `dialog` — INTERIM, and a whole file so its removal is a `git rm`.

mod attach_window;
mod dialog;
mod flows;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex as AsyncMutex;

use harvester_proto::SessionInfo;

use crate::bootstrap::bootstrap_and_connect;
use crate::client::{DaemonClient, SessionEvent};
use crate::host::Host;
use crate::token::RequestToken;
use attach_window::{
    lock_attach_buffers, AttachEventBuffers, PushOutcome, ATTACH_EVENT_BUFFER_CAP,
};
use dialog::AnsweredDialogs;

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
    /// A session was opened: attach completed and history was replayed via
    /// passthrough `get_entries`; `entries` is pi's list, verbatim. The
    /// frontend's cue to *create* the chat for `sid`.
    Attached {
        sid: String,
        entries: Vec<Value>,
    },
    /// The same `get_entries` snapshot, but as a repair after events were lost
    /// (a daemon `event_gap`, or our own attach window overflowing) for a
    /// session the frontend already has a chat for.
    ///
    /// Split from `Attached` because the two mean different things to a
    /// frontend and one of them was wrong: rebuilding the chat entity on a
    /// repair is what let a late resync clobber the chat on screen. Replace the
    /// existing chat's entries; if there is no chat for `sid`, ignore it — the
    /// bridge only ever repairs the session it is attached to.
    HistoryReplaced {
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
    attach_buffers: Arc<Mutex<AttachEventBuffers>>,
    /// Sessions with a repair already in flight, so a burst of gaps costs one
    /// `get_entries` rather than one per gap.
    resyncing: Arc<Mutex<HashSet<String>>>,
    /// INTERIM, with `dialog`: the dialog ids this bridge already cancelled.
    answered_dialogs: Arc<AnsweredDialogs>,
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
        resyncing: Arc::new(Mutex::new(HashSet::new())),
        answered_dialogs: Arc::new(AnsweredDialogs::default()),
    };

    // The sidebar renders from this; fetch immediately.
    spawn_flow(flows::sessions_flow(context.clone()));

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // UI dropped the bridge; dropping the client tears down the
                // connection and any ssh forward.
                None => break,
                Some(command) => dispatch(command, &context),
            },
            event = session_events.recv() => {
                // A closed channel is the same news as `Disconnected`: the
                // client stopped, so this bridge is done.
                let still_connected = match event {
                    Some(event) => handle_session_event(&context, event),
                    None => false,
                };

                if !still_connected {
                    context.events.send(Event::Disconnected);

                    break;
                }
            },
        }
    }
}

/// One event off the [`DaemonClient`]; `false` when the connection is gone and
/// the caller should publish `Disconnected` and stop.
fn handle_session_event(context: &BridgeContext, event: SessionEvent) -> bool {
    match event {
        SessionEvent::Pi { sid, event } => handle_live_pi_event(context, sid, event),
        SessionEvent::SessionExit { sid, exit_code } => {
            spawn_flow(flows::forget_attachment(context.clone(), sid.clone()));
            context.events.send(Event::SessionExit { sid, exit_code });
        }
        // Recover in the backbone, not the UI: refetch the history we just lost
        // and republish it, which every frontend already knows how to render.
        SessionEvent::EventGap { sid, skipped } => {
            eprintln!("bridge: lost {skipped} events for session {sid}; resyncing");
            spawn_flow(flows::resync_flow(context.clone(), sid));
        }
        // The whole bridge stops here, so there is no attachment left to
        // forget: this context and its daemon client die with the loop.
        SessionEvent::Disconnected => return false,
    }

    true
}

/// Every command is its own spawned straight-line flow; the loop above never
/// waits on the daemon, so event forwarding cannot stall behind a slow command.
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
    }
}

fn spawn_flow(flow: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(flow);
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

/// One pi event on its way to the frontend: forwarded verbatim, and — interim
/// — auto-cancelled first when it is a dialog request. Reached both live and
/// from a flushed attach window, so it must not depend on the select loop.
fn handle_pi_event(context: &BridgeContext, sid: String, event: Value) {
    dialog::auto_cancel_if_dialog(context, &sid, &event);
    context.events.send(Event::Pi { sid, event });
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use serde_json::json;

    use crate::connection::{no_forward, DaemonConnection};

    /// How long to wait before concluding the bridge sent nothing. Only ever
    /// reached by the assertions that *want* silence.
    const QUIET: Duration = Duration::from_millis(100);

    /// A bridge context wired to a fake daemon over an in-memory duplex pair —
    /// the whole reason `DaemonConnection` stopped being a `UnixStream`.
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
                    answered_dialogs: Arc::new(AnsweredDialogs::default()),
                },
                daemon: DaemonConnection::over(daemon_side, no_forward()),
                events,
            }
        }

        /// The next request the bridge sent, answered with a bland success so
        /// the flow that sent it proceeds. Panics if nothing arrives.
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

        /// `create_session` → `attach` → `get_entries`, the whole `Open` chain.
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

    /// Both envelopes name their command in the same place.
    fn command_name(request: &Value) -> String {
        request["cmd"]["type"].as_str().unwrap_or("?").to_string()
    }

    #[tokio::test]
    async fn a_session_that_exited_is_not_detached_from_on_the_next_open() {
        // The bridge used to remember an attachment forever: `attached` was
        // cleared only by `detach_previous`. So opening a second session after
        // the first one's pi exited sent a `Detach` for a dead sid, and the
        // daemon's refusal reached the user as a statusbar error for an action
        // they never took.
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
        // The other half: forgetting the attachment must be scoped to the
        // session that actually exited, or one chat pane silently becomes two
        // live attachments.
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
        // Every `event_gap` used to spawn a full `get_entries`, `attached`
        // never consulted — so a session the user is not looking at still paid
        // for a history refetch.
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
        // N gaps used to mean N concurrent refetches of the same history.
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
        // `Attached` means "create the chat"; a repair means "replace its
        // entries". A frontend that rebuilt on both clobbered the chat on
        // screen whenever a resync landed late.
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
