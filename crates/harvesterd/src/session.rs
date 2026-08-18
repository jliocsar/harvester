mod dialog;
mod launch;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harvester_proto::{encode_frame, read_frame, FRAME_BUFFER_START_BYTES};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

use crate::daemon::{Daemon, PASSTHROUGH_ID_PREFIX};
use crate::sid::Sid;

pub use dialog::cancel as cancel_dialog;

const SESSION_EVENT_BUFFER: usize = 1024;

const STDIN_QUEUE_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub enum SessionOutput {
    Event(Arc<[u8]>),
    Exit { exit_code: Option<i32> },
}

#[derive(Default, Deserialize)]
struct LineProbe<'line> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    id: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    method: Option<Cow<'line, str>>,
    #[serde(default)]
    timeout: Option<u64>,
}

pub struct Session {
    pub sid: Sid,
    pub cwd: String,
    pub spawned_at_ms: u64,
    event_prefix: String,
    stdin_queue: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pub output: broadcast::Sender<SessionOutput>,
    kill_switch: Notify,
    room: Mutex<dialog::DialogRoom>,
    pending_responses: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

pub struct SpawnArgs {
    pub sid: Sid,
    pub cwd: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
}

impl Session {
    pub(crate) fn spawn(args: SpawnArgs, daemon: &Daemon) -> std::io::Result<Arc<Session>> {
        let mut command = launch::create_pi_command(&args)?;
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (output, _) = broadcast::channel(SESSION_EVENT_BUFFER);
        let (stdin_sender, stdin_receiver) = mpsc::channel(STDIN_QUEUE_DEPTH);
        let event_prefix = create_event_prefix(args.sid.as_str());

        let session = Arc::new(Session {
            sid: args.sid,
            cwd: args.cwd,
            spawned_at_ms: unix_millis_now(),
            event_prefix,
            stdin_queue: Mutex::new(Some(stdin_sender)),
            output,
            kill_switch: Notify::new(),
            room: Mutex::new(dialog::DialogRoom::default()),
            pending_responses: Mutex::new(HashMap::new()),
        });
        let reaper = tokio::spawn(reap_child(session.clone(), child));

        tokio::spawn(pump_stdin(session.sid.clone(), stdin, stdin_receiver));
        tokio::spawn(pump_stdout(session.clone(), daemon.clone(), stdout, reaper));

        Ok(session)
    }

    pub fn write_line(&self, value: &Value) -> std::io::Result<()> {
        let line = encode_frame(value).expect("serializable");
        let guard = self.lock_stdin_queue();

        let Some(sender) = guard.as_ref() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session stdin already closed",
            ));
        };

        sender.try_send(line).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "session stdin queue is full — pi is not reading",
            ),
            mpsc::error::TrySendError::Closed(_) => std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session stdin writer stopped",
            ),
        })
    }

    pub fn close_stdin(&self) {
        drop(self.lock_stdin_queue().take());
    }

    pub fn kill(&self) {
        self.kill_switch.notify_one();
    }

    pub fn register_response_waiter(&self, request_key: String) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();

        self.lock_pending_responses().insert(request_key, sender);

        receiver
    }

    fn resolve_response(&self, request_key: &str, response: Value) {
        let Some(waiter) = self.lock_pending_responses().remove(request_key) else {
            eprintln!("session {}: dropped response for {request_key}", self.sid);
            return;
        };

        let _ = waiter.send(response);
    }

    fn fail_pending_responses(&self) {
        self.lock_pending_responses().clear();
    }

    fn lock_stdin_queue(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<Vec<u8>>>> {
        self.stdin_queue.lock().expect("stdin_queue mutex")
    }

    fn lock_pending_responses(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Value>>> {
        self.pending_responses
            .lock()
            .expect("pending_responses mutex")
    }
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as u64)
        .unwrap_or(0)
}

async fn pump_stdin(sid: Sid, mut stdin: ChildStdin, mut frames: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = frames.recv().await {
        if let Err(error) = stdin.write_all(&frame).await {
            eprintln!("session {sid}: stdin write failed: {error}");

            return;
        }

        if let Err(error) = stdin.flush().await {
            eprintln!("session {sid}: stdin flush failed: {error}");

            return;
        }
    }
}

async fn reap_child(session: Arc<Session>, mut child: Child) -> Option<i32> {
    tokio::select! {
        status = child.wait() => return status.ok().and_then(|status| status.code()),
        _ = session.kill_switch.notified() => {}
    }

    let _ = child.start_kill();

    child.wait().await.ok().and_then(|status| status.code())
}

async fn pump_stdout(
    session: Arc<Session>,
    daemon: Daemon,
    stdout: tokio::process::ChildStdout,
    reaper: tokio::task::JoinHandle<Option<i32>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(FRAME_BUFFER_START_BYTES);

    loop {
        let line = match read_frame(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };

        if line.is_empty() {
            continue;
        }

        let Some(probe) = probe_line(&session.sid, line) else {
            continue;
        };

        route_line(&session, line, probe);
    }

    let exit_code = reaper.await.ok().flatten();

    daemon.remove_session_if_same(&session);
    session.fail_pending_responses();
    let _ = session.output.send(SessionOutput::Exit { exit_code });
}

fn probe_line<'line>(sid: &Sid, line: &'line [u8]) -> Option<LineProbe<'line>> {
    let shape_error = match serde_json::from_slice::<LineProbe<'line>>(line) {
        Ok(probe) => return Some(probe),
        Err(error) => error,
    };

    if let Err(json_error) = serde_json::from_slice::<&RawValue>(line) {
        eprintln!("session {sid}: unparseable stdout line dropped: {json_error}");

        return None;
    }

    eprintln!("session {sid}: unexpected line shape, forwarding verbatim: {shape_error}");

    Some(LineProbe::default())
}

fn route_line(session: &Arc<Session>, line: &[u8], probe: LineProbe<'_>) {
    if let Some(request_key) = passthrough_response_key(&probe) {
        match serde_json::from_slice(line) {
            Ok(response) => session.resolve_response(request_key, response),
            Err(error) => eprintln!(
                "session {}: response {request_key} did not parse: {error}",
                session.sid
            ),
        }

        return;
    }

    let event = encode_event(&session.event_prefix, line);

    if dialog::apply_policy(session, &probe, &event) {
        let _ = session.output.send(SessionOutput::Event(event));
    }
}

fn passthrough_response_key<'probe>(probe: &'probe LineProbe<'_>) -> Option<&'probe str> {
    if probe.kind.as_deref() != Some("response") {
        return None;
    }

    let request_key = probe.id.as_deref()?;

    request_key
        .starts_with(PASSTHROUGH_ID_PREFIX)
        .then_some(request_key)
}

fn create_event_prefix(sid: &str) -> String {
    let escaped_sid = serde_json::to_string(sid).expect("a str always serializes");

    format!(r#"{{"type":"event","sid":{escaped_sid},"event":"#)
}

fn encode_event(event_prefix: &str, line: &[u8]) -> Arc<[u8]> {
    let prefix = event_prefix.as_bytes();
    let mut envelope = Vec::with_capacity(prefix.len() + line.len() + 1);

    envelope.extend_from_slice(prefix);
    envelope.extend_from_slice(line);
    envelope.push(b'}');

    Arc::from(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    use harvester_proto::Outgoing;

    const FIXTURE_SID: &str = "01JABCDEF0123456789ABCDEFG";

    fn serde_serialized_event(sid: &str, event: Value) -> Vec<u8> {
        let outgoing = Outgoing::Event {
            sid: sid.to_owned(),
            event,
        };

        serde_json::to_vec(&outgoing).expect("wire enum must serialize")
    }

    fn encoded_event(sid: &str, line: &str) -> Vec<u8> {
        encode_event(&create_event_prefix(sid), line.as_bytes()).to_vec()
    }

    #[test]
    fn the_encoded_envelope_is_valid_json() {
        let encoded = encoded_event(FIXTURE_SID, r#"{"type":"agent_settled"}"#);

        let parsed: Value = serde_json::from_slice(&encoded).expect("envelope must be valid JSON");

        assert_eq!(parsed["type"], "event");
        assert_eq!(parsed["sid"], FIXTURE_SID);
        assert_eq!(parsed["event"]["type"], "agent_settled");
    }

    #[test]
    fn the_encoded_envelope_matches_the_serde_serialized_wire_enum() {
        let line = r#"{"type":"message_update","text":"héllo \"quoted\"","n":[1,2,3]}"#;
        let event: Value = serde_json::from_str(line).expect("fixture line must parse");

        let encoded = encoded_event(FIXTURE_SID, line);
        let from_serde = serde_serialized_event(FIXTURE_SID, event);

        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).expect("envelope must be valid JSON"),
            serde_json::from_slice::<Value>(&from_serde).expect("wire enum must be valid JSON"),
        );
    }

    #[test]
    fn a_sid_needing_json_escaping_is_escaped_in_the_prefix() {
        let awkward_sid = r#"we"ird\sid"#;

        let encoded = encoded_event(awkward_sid, r#"{"type":"noop"}"#);
        let parsed: Value = serde_json::from_slice(&encoded).expect("envelope must be valid JSON");

        assert_eq!(parsed["sid"], awkward_sid);
        assert_eq!(
            encoded,
            serde_serialized_event(awkward_sid, serde_json::json!({"type": "noop"})),
            "the hand-built prefix must escape exactly as serde does",
        );
    }
}
