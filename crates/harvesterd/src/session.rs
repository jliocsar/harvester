//! One pi child per session: spawn, stdin ownership, stdout pump, dialog policy.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use harvester_proto::{encode_frame, read_frame};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::daemon::{Daemon, PASSTHROUGH_ID_PREFIX};
use crate::sid::Sid;

/// Events buffered per session for slow subscribers before they start lagging.
const SESSION_EVENT_BUFFER: usize = 1024;

/// Frames queued for pi's stdin before writers are told to back off. Only a pi
/// that stopped draining its stdin can fill this, and then failing fast beats
/// queueing without bound — the caller learns immediately instead of parking.
const STDIN_QUEUE_DEPTH: usize = 64;

/// Pump read buffer starting size; grows to the largest pi line seen.
const PUMP_LINE_BUFFER: usize = 64 * 1024;

const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

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

/// Serialize-only mirror of `Outgoing::Event` that embeds the original pi
/// line verbatim (`&RawValue`). The shared read-side `Outgoing` cannot carry
/// a borrowed `RawValue` (internally-tagged deserialization buffers through
/// an owned tree), so the daemon serializes events through this instead.
#[derive(Serialize)]
struct EventEnvelope<'line> {
    r#type: &'static str,
    sid: &'line str,
    event: &'line RawValue,
}

/// The only fields the pump routes on, borrowed straight out of the read
/// buffer. Pi's `message_update` deltas each carry the whole partial message,
/// so they are the largest and by far the most frequent lines — building an
/// owned `Value` for every one of them, only to read `type`, would make the
/// throwaway DOM the pump's dominant cost. The full tree is parsed in exactly
/// one arm below, where a client is actually waiting for it.
#[derive(Deserialize)]
struct LineProbe<'line> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    id: Option<Cow<'line, str>>,
    #[serde(borrow, default)]
    method: Option<Cow<'line, str>>,
}

pub struct Session {
    pub sid: Sid,
    pub pid: Option<u32>,
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
    /// Who is watching and what is waiting on them — one lock, because the
    /// dialog policy is a decision about both at once.
    room: Mutex<DialogRoom>,
    /// In-flight passthrough waiters by injected `hv-<n>` id. The pump
    /// resolves them; dropping the map on exit fails every waiter (pi's own
    /// reject-pending-requests-on-exit pattern).
    pending_responses: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

/// The empty-room dialog policy's whole state. Counter and pending set live
/// under one mutex on purpose: "is anybody attached, and if so remember this
/// dialog" must be one atomic decision. Split across an atomic and a separate
/// lock (as it once was), a detach landing between the load and the insert
/// strands the dialog — nobody left to answer it, and nothing left to trigger
/// the cancel.
#[derive(Default)]
struct DialogRoom {
    /// Number of attached client connections.
    attached: usize,
    /// Dialogs awaiting an `extension_ui_response`, by dialog id, each holding
    /// the pre-serialized wire envelope the pump already built. Keeping the
    /// envelope (not just the id) is what lets a client attaching *after* the
    /// dialog arrived still be shown it — otherwise a handoff between clients
    /// strands pi on a dialog nobody can see. First answer wins; all of them
    /// are cancelled when the room empties, because pi would otherwise wait
    /// on an unanswered dialog forever.
    pending: HashMap<String, Arc<[u8]>>,
}

/// The state machine itself, deliberately plain and synchronous: the locking
/// belongs to [`Session`], so the policy can be exercised without a pi child.
impl DialogRoom {
    /// Join the room; returns the dialog envelopes already waiting, which the
    /// caller's forwarder replays so a late attacher sees them too.
    fn attach(&mut self) -> Vec<Arc<[u8]>> {
        self.attached += 1;

        self.pending.values().cloned().collect()
    }

    /// Leave the room. When the last watcher goes, every still-pending dialog
    /// is returned for cancellation — nothing could answer them anymore, and
    /// pi would wait forever (docs/DAEMON.md: never leave a dialog unanswered).
    fn detach(&mut self) -> Vec<String> {
        self.attached = self.attached.saturating_sub(1);

        if self.attached > 0 {
            return Vec::new();
        }

        self.pending
            .drain()
            .map(|(dialog_id, _)| dialog_id)
            .collect()
    }

    /// Remove-and-return: true means the dialog was still pending and this
    /// caller's answer is the winning one (first answer wins).
    fn claim_pending(&mut self, dialog_id: &str) -> bool {
        self.pending.remove(dialog_id).is_some()
    }

    /// Remember a dialog (with the envelope to replay on a later attach).
    /// `false` means the room is empty, so nobody will ever answer it and the
    /// caller must cancel instead — decided in the same critical section as the
    /// insert, which is what keeps the two outcomes mutually exclusive.
    fn remember(&mut self, dialog_id: &str, envelope: Arc<[u8]>) -> bool {
        if self.attached == 0 {
            return false;
        }

        self.pending.insert(dialog_id.to_owned(), envelope);

        true
    }
}

pub struct SpawnArgs {
    pub sid: Sid,
    pub cwd: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
}

impl Session {
    /// The one launch line (docs/DAEMON.md). Resume-or-create via --session-id.
    /// Callers go through `Daemon::spawn_session`, which owns the pid table.
    pub(crate) fn spawn(args: SpawnArgs, daemon: &Daemon) -> std::io::Result<Arc<Session>> {
        let mut command = create_pi_command(&args, daemon)?;
        let mut child = command.spawn()?;

        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (output, _) = broadcast::channel(SESSION_EVENT_BUFFER);
        let (stdin_sender, stdin_receiver) = mpsc::channel(STDIN_QUEUE_DEPTH);

        let session = Arc::new(Session {
            sid: args.sid,
            pid,
            stdin_queue: Mutex::new(Some(stdin_sender)),
            output,
            room: Mutex::new(DialogRoom::default()),
            pending_responses: Mutex::new(HashMap::new()),
        });

        tokio::spawn(pump_stdin(session.sid.clone(), stdin, stdin_receiver));
        tokio::spawn(pump_stdout(session.clone(), daemon.clone(), stdout, child));

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

    /// Last resort after the stop grace period. Boring on purpose: shelling out
    /// beats a libc dependency for one syscall.
    pub fn kill(&self) {
        let Some(pid) = self.pid else {
            return;
        };

        let status = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();

        match status {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!("kill -9 {pid} exited with {status}"),
            Err(error) => eprintln!("kill -9 {pid} failed to run: {error}"),
        }
    }

    /// Register a waiter for the passthrough response keyed `hv-<n>`. Must be
    /// called *before* the command is written so the response cannot race it.
    pub fn register_response_waiter(&self, request_key: String) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();

        self.lock_pending_responses().insert(request_key, sender);

        receiver
    }

    /// Join the room; returns the dialog envelopes already waiting, which the
    /// caller's forwarder replays so a late attacher sees them too.
    pub fn attach(&self) -> Vec<Arc<[u8]>> {
        self.lock_room().attach()
    }

    /// Leave the room. When the last watcher goes, every still-pending dialog
    /// is returned for cancellation — nothing could answer them anymore, and
    /// pi would wait forever (docs/DAEMON.md: never leave a dialog unanswered).
    pub fn detach(&self) -> Vec<String> {
        self.lock_room().detach()
    }

    /// Remove-and-return: true means the dialog was still pending and this
    /// caller's answer is the winning one (first answer wins).
    pub fn claim_pending_dialog(&self, dialog_id: &str) -> bool {
        self.lock_room().claim_pending(dialog_id)
    }

    /// Remember a dialog (with the envelope to replay on a later attach).
    /// `false` means the room is empty, so nobody will ever answer it and the
    /// caller must cancel instead — checked under the same lock as the insert,
    /// which is what keeps the two outcomes mutually exclusive.
    fn remember_dialog(&self, dialog_id: &str, envelope: Arc<[u8]>) -> bool {
        self.lock_room().remember(dialog_id, envelope)
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

    fn lock_room(&self) -> std::sync::MutexGuard<'_, DialogRoom> {
        self.room.lock().expect("dialog room mutex")
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

/// Answer a dialog with `cancelled: true` — to the extension, identical to a
/// human pressing Escape. Used at arrival (empty room) and when the room empties.
pub fn cancel_dialog(session: &Session, dialog_id: &str) {
    let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});

    if let Err(error) = session.write_line(&cancel) {
        eprintln!(
            "session {}: dialog {dialog_id} auto-cancel write failed: {error}",
            session.sid
        );
    }
}

/// Open a session's stderr log for the run that is about to start. **Append,
/// never truncate**: spawn is also resume, so truncating would destroy the
/// record of why the previous run died — the very thing the user is looking at
/// when they click resume, and the sole exit-diagnosis channel the contract
/// gives clients (docs/DAEMON.md §Events). The banner is what makes appended
/// runs separable in a `tail`.
fn open_stderr_log(log_dir: &std::path::Path, sid: &Sid) -> std::io::Result<std::fs::File> {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join(format!("{sid}.stderr.log")))?;
    let started_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs())
        .unwrap_or_default();

    writeln!(
        log,
        "--- harvesterd: spawning pi for {sid} at {started_at_unix_seconds} (unix seconds) ---"
    )?;

    Ok(log)
}

/// Build the launch line: env guards, mode flags, optional overrides, stderr log.
fn create_pi_command(args: &SpawnArgs, daemon: &Daemon) -> std::io::Result<Command> {
    let log_dir = &daemon.inner().log_dir;
    let stderr_log = open_stderr_log(log_dir, &args.sid)?;

    let mut cli_args: Vec<&str> = vec![
        "--mode",
        "rpc",
        "--session-id",
        args.sid.as_str(),
        "--approve",
    ];

    if let Some(provider) = &args.provider {
        cli_args.extend(["--provider", provider]);
    }

    if let Some(model) = &args.model {
        cli_args.extend(["--model", model]);
    }

    if let Some(name) = &args.name {
        cli_args.extend(["--name", name]);
    }

    let mut command = Command::new("pi");
    command
        .current_dir(&args.cwd)
        .env("PI_OFFLINE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=5",
        )
        .args(cli_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log));

    Ok(command)
}

/// Read pi stdout lines forever; resolve responses, apply the dialog policy,
/// forward everything else. Ends by reaping the child, vacating the pid table,
/// failing in-flight passthroughs, and broadcasting Exit — in that order, so a
/// client reacting to `session_exit` with an immediate resume never races the
/// table ("already running" is structurally impossible).
async fn pump_stdout(
    session: Arc<Session>,
    daemon: Daemon,
    stdout: tokio::process::ChildStdout,
    mut child: Child,
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

        let probe: LineProbe = match serde_json::from_slice(line) {
            Ok(probe) => probe,
            // Should never happen (pi guarantees stdout purity); don't die on it.
            Err(_) => continue,
        };

        route_line(&session, line, probe);
    }

    let exit_code = child.wait().await.ok().and_then(|status| status.code());

    daemon.remove_session_if_same(&session);
    session.fail_pending_responses();
    let _ = session.output.send(SessionOutput::Exit { exit_code });
}

/// Dialog policy (docs/DAEMON.md): a dialog that expects an answer is
/// remembered and forwarded while anyone is attached (first client answer
/// wins, and a client attaching later still gets shown it); with an empty room
/// it is auto-cancelled instead of broadcast into the void.
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

    let event = encode_event(session, line);

    if let Some(dialog_id) = dialog_id_awaiting_answer(&probe) {
        if !session.remember_dialog(dialog_id, event.clone()) {
            cancel_dialog(session, dialog_id);

            return;
        }
    }

    let _ = session.output.send(SessionOutput::Event(event));
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

/// Build the wire envelope exactly once; every subscriber shares the bytes,
/// and a pending dialog keeps the same `Arc` for its later replay.
fn encode_event(session: &Session, line: &[u8]) -> Arc<[u8]> {
    // The line parsed as JSON above, so it is valid UTF-8 JSON text.
    let raw_line = std::str::from_utf8(line).expect("parsed as JSON above");
    let event: &RawValue = serde_json::from_str(raw_line).expect("parsed as JSON above");
    let envelope = EventEnvelope {
        r#type: "event",
        sid: session.sid.as_str(),
        event,
    };

    Arc::from(serde_json::to_vec(&envelope).expect("serializable"))
}

/// The dialog id when this line is an `extension_ui_request` that blocks pi
/// until it is answered. Fire-and-forget kinds (`notify`, `setStatus`) are
/// forwarded like any other event, so they are not dialogs for policy purposes.
fn dialog_id_awaiting_answer<'probe>(probe: &'probe LineProbe<'_>) -> Option<&'probe str> {
    if probe.kind.as_deref() != Some("extension_ui_request") {
        return None;
    }

    if !DIALOG_METHODS.contains(&probe.method.as_deref()?) {
        return None;
    }

    probe.id.as_deref()
}

/// The dialog room in isolation — no pi child, no tokio, no sockets. Every case
/// here is a clause of the dialog policy in docs/DAEMON.md.
#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the wire envelope the pump would have built; the room only
    /// ever stores and hands back these bytes, never inspects them.
    fn envelope(dialog_id: &str) -> Arc<[u8]> {
        Arc::from(format!(r#"{{"type":"event","id":"{dialog_id}"}}"#).into_bytes())
    }

    fn sorted(mut dialog_ids: Vec<String>) -> Vec<String> {
        dialog_ids.sort();

        dialog_ids
    }

    #[test]
    fn attach_on_empty_room_replays_nothing() {
        let mut room = DialogRoom::default();

        assert!(room.attach().is_empty());
        assert_eq!(room.attached, 1);
    }

    #[test]
    fn remember_is_refused_when_the_room_is_empty() {
        let mut room = DialogRoom::default();

        assert!(!room.remember("d1", envelope("d1")));
        assert!(room.pending.is_empty());
    }

    #[test]
    fn remember_is_accepted_while_a_watcher_is_attached() {
        let mut room = DialogRoom::default();
        room.attach();

        assert!(room.remember("d1", envelope("d1")));
        assert_eq!(room.pending.len(), 1);
    }

    #[test]
    fn a_pending_dialog_is_replayed_to_a_later_attacher() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));

        let replayed = room.attach();

        assert_eq!(replayed, vec![envelope("d1")]);
    }

    #[test]
    fn detach_with_another_watcher_left_cancels_nothing() {
        let mut room = DialogRoom::default();
        room.attach();
        room.attach();
        room.remember("d1", envelope("d1"));

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 1);
        assert_eq!(room.pending.len(), 1);
    }

    #[test]
    fn the_last_detach_drains_every_pending_dialog() {
        let mut room = DialogRoom::default();
        room.attach();
        room.attach();
        room.remember("d1", envelope("d1"));
        room.remember("d2", envelope("d2"));
        room.detach();

        let to_cancel = room.detach();

        assert_eq!(sorted(to_cancel), vec!["d1".to_owned(), "d2".to_owned()]);
        assert_eq!(room.attached, 0);
        assert!(room.pending.is_empty());
    }

    #[test]
    fn claim_pending_lets_only_the_first_answer_win() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));

        assert!(room.claim_pending("d1"));
        assert!(!room.claim_pending("d1"));
    }

    #[test]
    fn claiming_an_unknown_dialog_is_refused() {
        let mut room = DialogRoom::default();
        room.attach();

        assert!(!room.claim_pending("nope"));
    }

    #[test]
    fn a_claimed_dialog_is_no_longer_replayed_on_attach() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("d1", envelope("d1"));
        room.claim_pending("d1");

        assert!(room.attach().is_empty());
    }

    #[test]
    fn detaching_more_often_than_attaching_saturates_at_zero() {
        let mut room = DialogRoom::default();
        room.attach();
        room.detach();

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 0);

        assert!(room.detach().is_empty());
        assert_eq!(room.attached, 0);
    }

    #[test]
    fn only_the_unanswered_dialog_comes_back_for_cancellation() {
        let mut room = DialogRoom::default();
        room.attach();
        room.remember("answered", envelope("answered"));
        room.remember("stranded", envelope("stranded"));

        assert!(room.claim_pending("answered"));

        let to_cancel = room.detach();

        assert_eq!(to_cancel, vec!["stranded".to_owned()]);
    }
}
