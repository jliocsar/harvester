use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Value};

use harvester_proto::{HarvesterCmd, SessionsData};

use super::attach_window::{AttachRegistration, FlushOutcome};
use super::{
    flatten_harvester, handle_pi_event, pi_event_type, report_failure, spawn_flow, BridgeContext,
    Event, SessionCursor,
};
use crate::dialog::DialogAnswer;
use crate::token::RequestToken;

const UNKNOWN_PI_ERROR: &str = "unknown error";

pub(super) async fn sessions_flow(context: BridgeContext) {
    let sessions = context
        .client
        .harvester(HarvesterCmd::ListSessions)
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

pub(super) async fn open_flow(context: BridgeContext, sid: String, cwd: String) {
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

pub(super) async fn resync_flow(context: BridgeContext, sid: String) {
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
    context.attached.lock().await.as_deref() == Some(sid)
}

async fn repair_history(context: &BridgeContext, sid: &str) -> Result<(), String> {
    if let Some(leaf_id) = cursor_for(context, sid) {
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

fn cursor_for(context: &BridgeContext, sid: &str) -> Option<String> {
    lock_cursor(&context.cursor)
        .as_ref()
        .filter(|cursor| cursor.sid == sid)
        .map(|cursor| cursor.leaf_id.clone())
}

async fn record_cursor(context: &BridgeContext, sid: &str, leaf_id: Option<String>) {
    if !is_attached(context, sid).await {
        return;
    }

    *lock_cursor(&context.cursor) = leaf_id.map(|leaf_id| SessionCursor {
        sid: sid.to_string(),
        leaf_id,
    });
}

fn lock_cursor(cursor: &Mutex<Option<SessionCursor>>) -> MutexGuard<'_, Option<SessionCursor>> {
    cursor.lock().expect("cursor mutex")
}

struct ResyncGuard {
    in_flight: Arc<Mutex<HashSet<String>>>,
    sid: String,
}

impl ResyncGuard {
    fn claim(context: &BridgeContext, sid: &str) -> Option<ResyncGuard> {
        lock_resyncing(&context.resyncing)
            .insert(sid.to_string())
            .then(|| ResyncGuard {
                in_flight: context.resyncing.clone(),
                sid: sid.to_string(),
            })
    }
}

impl Drop for ResyncGuard {
    fn drop(&mut self) {
        lock_resyncing(&self.in_flight).remove(&self.sid);
    }
}

fn lock_resyncing(sessions: &Mutex<HashSet<String>>) -> std::sync::MutexGuard<'_, HashSet<String>> {
    sessions.lock().expect("resync mutex")
}

pub(super) async fn forget_attachment(context: BridgeContext, sid: String) {
    let mut attached = context.attached.lock().await;

    if attached.as_deref() == Some(sid.as_str()) {
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

pub(super) async fn start_session_flow(
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
    attached: &mut Option<String>,
    next_sid: &str,
) -> bool {
    let Some(previous_sid) = attached.as_ref() else {
        return true;
    };

    if previous_sid == next_sid {
        return true;
    }

    let previous_sid = previous_sid.clone();
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

pub(super) async fn prompt_flow(context: BridgeContext, sid: String, text: String) {
    let sent = context
        .client
        .pi(
            &sid,
            json!({"type": "prompt", "message": text, "streamingBehavior": "steer"}),
        )
        .await;
    let detail = match sent {
        Ok(response) if pi_success(&response) => return,
        Ok(response) => pi_error(&response).unwrap_or_else(|| UNKNOWN_PI_ERROR.to_string()),
        Err(error) => error.to_string(),
    };

    eprintln!("bridge: the prompt for session {sid} was not delivered: {detail}");
    context.events.send(Event::PromptFailed { sid, detail });
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

pub(super) async fn pi_request_flow(
    context: BridgeContext,
    sid: String,
    cmd: Value,
    token: RequestToken,
) {
    let method = pi_event_type(&cmd).unwrap_or_default().to_string();
    let result = pi_flow_result(&context, &sid, cmd).await;

    context.events.send(Event::PiAnswered {
        sid,
        token,
        method,
        result,
    });
}

pub(super) async fn stop_flow(context: BridgeContext, sid: String) {
    let stop = context.client.harvester(HarvesterCmd::Stop { sid }).await;

    if let Err(detail) = flatten_harvester(stop) {
        report_failure(&context, "stop session", detail);
    }
}

pub(super) async fn answer_dialog_flow(
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
