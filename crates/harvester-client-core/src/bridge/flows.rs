//! One straight-line async flow per command (plus the two the bridge starts
//! on its own: the first `list_sessions`, and a resync after events were
//! lost). Responses resolve at their await points, so there is no
//! response-routing state machine anywhere in this crate — which is the whole
//! reason `mod.rs`'s select loop can stay a router.
//!
//! Every flow ends by publishing exactly one outcome: an
//! [`Event`](super::Event) the frontend renders, or a `RequestFailed` through
//! [`report_failure`](super::report_failure). None of them return a value —
//! spawning a flow is fire-and-forget.

use serde_json::{json, Value};

use harvester_proto::{HarvesterCmd, SessionsData};

use super::attach_window::{AttachRegistration, FlushOutcome};
use super::{flatten_harvester, handle_pi_event, report_failure, spawn_flow, BridgeContext, Event};
use crate::token::RequestToken;

pub(super) async fn sessions_flow(context: BridgeContext) {
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
pub(super) async fn open_flow(context: BridgeContext, sid: String, cwd: String) {
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
pub(super) async fn resync_flow(context: BridgeContext, sid: String) {
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

/// `StartSession`: the ghost chat's create → attach → first-prompt chain as
/// one straight line. `Attached` publishes with empty entries — the session
/// file is brand new, there is no history to fetch.
pub(super) async fn start_session_flow(
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
pub(super) async fn prompt_flow(context: BridgeContext, sid: String, text: String) {
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

pub(super) async fn list_models_flow(context: BridgeContext, cwd: String, token: RequestToken) {
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

pub(super) async fn session_models_flow(context: BridgeContext, sid: String, token: RequestToken) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_available_models"})).await;

    context
        .events
        .send(Event::SessionModelsListed { sid, token, result });
}

pub(super) async fn session_state_flow(context: BridgeContext, sid: String, token: RequestToken) {
    let result = pi_flow_result(&context, &sid, json!({"type": "get_state"})).await;

    context
        .events
        .send(Event::SessionState { sid, token, result });
}

pub(super) async fn set_model_flow(
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
