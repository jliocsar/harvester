//! One pi child per session: spawn, stdin ownership, stdout pump.

mod dialog;
mod launch;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use harvester_proto::{encode_frame, read_frame};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};

use crate::daemon::{Daemon, PASSTHROUGH_ID_PREFIX};
use crate::sid::Sid;

pub use dialog::cancel as cancel_dialog;

/// Events buffered per session for slow subscribers before they start lagging.
const SESSION_EVENT_BUFFER: usize = 1024;

/// Frames queued for pi's stdin before writers are told to back off. Only a pi
/// that stopped draining its stdin can fill this, and then failing fast beats
/// queueing without bound — the caller learns immediately instead of parking.
const STDIN_QUEUE_DEPTH: usize = 64;

/// Pump read buffer starting size; grows to the largest pi line seen.
const PUMP_LINE_BUFFER: usize = 64 * 1024;

/// Fan-out line from a session to attached connections. Events carry the
/// complete pre-serialized wire envelope: built once in the pump, shared by
/// refcount with every subscriber — never cloned or re-serialized per client.
#[derive(Debug, Clone)]
pub enum SessionOutput {
    /// `{"type":"event","sid":…,"event":…}`, ready to write (no trailing LF).
    Event(Arc<[u8]>),
    Exit {
        exit_code: Option<i32>,
    },
}

/// The only fields the pump routes on, borrowed straight out of the read
/// buffer. Pi's `message_update` deltas each carry the whole partial message,
/// so they are the largest and by far the most frequent lines — building an
/// owned `Value` for every one of them, only to read `type`, would make the
/// throwaway DOM the pump's dominant cost. The full tree is parsed in exactly
/// one arm below, where a client is actually waiting for it.
///
/// `Default` is the "none of the routing fields apply" probe: [`pump_stdout`]
/// falls back to it for a line that is valid JSON but does not fit this shape,
/// so the line is still forwarded verbatim instead of vanishing.
#[derive(Default, Deserialize)]
struct LineProbe<'line> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    id: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    method: Option<Cow<'line, str>>,
    /// Milliseconds, and only ever on a dialog request: pi puts the
    /// extension's `opts.timeout` straight on the wire for `select`, `confirm`
    /// and `input` (pi 0.83.0 `modes/rpc/rpc-mode.ts:137,142,147`, typed at
    /// `modes/rpc/rpc-types.ts:239-241,247`). `editor` never carries one
    /// (`rpc-mode.ts:253-269`). Its presence means pi will resolve the dialog
    /// on its own — see `dialog::spawn_eviction`.
    #[serde(default)]
    timeout: Option<u64>,
}

pub struct Session {
    pub sid: Sid,
    /// `{"type":"event","sid":<escaped sid>,"event":` — the constant head of
    /// every event envelope this session emits, built once at spawn so
    /// [`encode_event`] is a byte concatenation instead of a second full JSON
    /// scan. **Soundness depends on [`route_line`] only ever being handed a
    /// line that parsed as JSON**: the pi line is spliced in as raw bytes, so a
    /// non-JSON line would corrupt the client's frame. [`pump_stdout`] is what
    /// guarantees that (it re-parses as `&RawValue` before falling back to a
    /// default probe) — do not weaken it without changing this.
    event_prefix: String,
    /// Frames bound for pi's stdin. Private on purpose: every write goes
    /// through `write_line`, and [`pump_stdin`] is the only holder of the pipe
    /// handle — so no caller ever awaits pi's stdin, and writes keep their
    /// arrival order across every connection. `None` after `close_stdin`:
    /// dropping the sender lets the pump drain what is queued and *then* drop
    /// the handle, which is what actually delivers EOF (`AsyncWriteExt::
    /// shutdown` is a mere flush for pipes; verified live: pi never saw EOF
    /// and had to be SIGKILLed).
    stdin_queue: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    pub output: broadcast::Sender<SessionOutput>,
    /// Fires the stop ladder's last rung; [`reap_child`] is listening.
    kill_switch: Notify,
    /// Who is watching and what is waiting on them — one lock, because the
    /// dialog policy is a decision about both at once (see `dialog`).
    room: Mutex<dialog::DialogRoom>,
    /// In-flight passthrough waiters by injected `hv-<n>` id. The pump
    /// resolves them; dropping the map on exit fails every waiter (pi's own
    /// reject-pending-requests-on-exit pattern).
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
    /// Callers go through `Daemon::spawn_session`, which owns the pid table.
    pub(crate) fn spawn(args: SpawnArgs, daemon: &Daemon) -> std::io::Result<Arc<Session>> {
        let mut command = launch::create_pi_command(&args, daemon)?;
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (output, _) = broadcast::channel(SESSION_EVENT_BUFFER);
        let (stdin_sender, stdin_receiver) = mpsc::channel(STDIN_QUEUE_DEPTH);
        let event_prefix = create_event_prefix(args.sid.as_str());

        let session = Arc::new(Session {
            sid: args.sid,
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

    /// Queue one JSONL line for pi's stdin. Deliberately synchronous: the
    /// queue absorbs the write so no caller — least of all the connection
    /// loop — can be parked by a pi that stopped reading. A full queue means
    /// exactly that, and says so instead of blocking.
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

    /// Graceful stop: drop the queue's sender. The pump drains what is already
    /// queued (so a just-written `abort` still reaches pi), then drops the pipe
    /// handle — that drop is what delivers EOF, pi's documented-by-source
    /// shutdown path. The stdout pump observes the exit and broadcasts `Exit`.
    pub fn close_stdin(&self) {
        drop(self.lock_stdin_queue().take());
    }

    /// Last resort after the stop grace period: SIGKILL, delivered by
    /// [`reap_child`] — the only owner of the `Child`.
    pub fn kill(&self) {
        self.kill_switch.notify_one();
    }

    /// Register a waiter for the passthrough response keyed `hv-<n>`. Must be
    /// called *before* the command is written so the response cannot race it.
    pub fn register_response_waiter(&self, request_key: String) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();

        self.lock_pending_responses().insert(request_key, sender);

        receiver
    }

    fn resolve_response(&self, request_key: &str, response: Value) {
        let Some(waiter) = self.lock_pending_responses().remove(request_key) else {
            // No waiter can only mean its task died; worth a trace.
            eprintln!("session {}: dropped response for {request_key}", self.sid);
            return;
        };

        // A closed receiver means the requesting connection went away.
        let _ = waiter.send(response);
    }

    /// Fail every in-flight passthrough: dropping the senders errors the
    /// receivers, which report "session exited".
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

/// The only holder of pi's stdin: drains queued frames in order, one flush per
/// frame, so every writer's lines stay whole and in arrival order. Ends when
/// the last sender is gone (`close_stdin`) or the pipe breaks — dropping the
/// handle on the way out is what delivers EOF.
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

/// The sole owner of the `Child`, so [`Session::kill`] needs no pid and no
/// `kill(1)`: it waits for the exit and for the kill switch at once, because
/// the stdout pump is parked on a read that only a dead pi will ever end.
async fn reap_child(session: Arc<Session>, mut child: Child) -> Option<i32> {
    tokio::select! {
        status = child.wait() => return status.ok().and_then(|status| status.code()),
        _ = session.kill_switch.notified() => {}
    }

    let _ = child.start_kill();

    child.wait().await.ok().and_then(|status| status.code())
}

/// Read pi stdout lines forever; resolve responses, apply the dialog policy,
/// forward everything else. Ends by collecting the exit code, vacating the pid
/// table, failing in-flight passthroughs, and broadcasting Exit — in that
/// order, so a client reacting to `session_exit` with an immediate resume never
/// races the table ("already running" is structurally impossible).
async fn pump_stdout(
    session: Arc<Session>,
    daemon: Daemon,
    stdout: tokio::process::ChildStdout,
    reaper: tokio::task::JoinHandle<Option<i32>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(PUMP_LINE_BUFFER);

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

/// The routing probe for one pi stdout line, or `None` when the line is not
/// JSON at all and must be dropped.
///
/// A line that *is* JSON but does not fit [`LineProbe`] (a non-string `type`,
/// `id` or `method`, which pi 0.83.0 never emits but a later pi might) is not a
/// routing decision we can make — yet docs/DAEMON.md is unconditional: every
/// line pi emits on stdout is forwarded verbatim. So it is forwarded under a
/// default probe rather than dropped; a swallowed `agent_settled` would spin a
/// client's turn forever. The `&RawValue` re-parse that separates the two cases
/// runs on the error path only, so the pump's hot loop is unchanged.
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
        // The one arm that needs the whole tree: it hands pi's response object
        // to the client waiting on it.
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

/// The `hv-<n>` id when the line is a response to an injected passthrough
/// request (minted by `Daemon::next_passthrough_id`).
fn passthrough_response_key<'probe>(probe: &'probe LineProbe<'_>) -> Option<&'probe str> {
    if probe.kind.as_deref() != Some("response") {
        return None;
    }

    let request_key = probe.id.as_deref()?;

    request_key
        .starts_with(PASSTHROUGH_ID_PREFIX)
        .then_some(request_key)
}

/// The constant head of this session's event envelopes, with the sid escaped
/// exactly as serde would escape it (so the result is byte-identical to
/// serializing `Outgoing::Event`, whose field order this matches).
fn create_event_prefix(sid: &str) -> String {
    let escaped_sid = serde_json::to_string(sid).expect("a str always serializes");

    format!(r#"{{"type":"event","sid":{escaped_sid},"event":"#)
}

/// Build the wire envelope exactly once; every subscriber shares the bytes,
/// and a pending dialog keeps the same `Arc` for its later replay. The pi line
/// is spliced in verbatim — see [`Session::event_prefix`] for why that is sound
/// and what would break it.
fn encode_event(event_prefix: &str, line: &[u8]) -> Arc<[u8]> {
    let prefix = event_prefix.as_bytes();
    let mut envelope = Vec::with_capacity(prefix.len() + line.len() + 1);

    envelope.extend_from_slice(prefix);
    envelope.extend_from_slice(line);
    envelope.push(b'}');

    Arc::from(envelope)
}

/// The hand-built envelope against the shared wire enum it must stay
/// byte-compatible with.
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
