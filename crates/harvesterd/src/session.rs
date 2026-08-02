//! One pi child per session: spawn, stdin ownership, stdout pump, dialog policy.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use harvester_proto::{CR, LF};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, Mutex};

/// Events buffered per session for slow subscribers before they start lagging.
const SESSION_EVENT_BUFFER: usize = 1024;

const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// Fan-out line from a session to attached connections.
#[derive(Debug, Clone)]
pub enum SessionOutput {
    /// A pi response line matched to a passthrough request (by injected id).
    Response {
        request_key: String,
        response: Value,
    },
    /// Any other pi stdout line — an event, forwarded verbatim.
    Event(Value),
    Exit {
        exit_code: Option<i32>,
    },
}

pub struct Session {
    pub sid: String,
    pub pid: Option<u32>,
    /// Private on purpose: every write must go through `write_line`, the only
    /// place that guarantees single-line framing + flush. `None` after
    /// `close_stdin` — the pipe fd only closes when the handle is dropped
    /// (`AsyncWriteExt::shutdown` is a mere flush for pipes; verified live:
    /// pi never saw EOF and had to be SIGKILLed).
    stdin: Mutex<Option<ChildStdin>>,
    pub output: broadcast::Sender<SessionOutput>,
    /// Dialog ids currently awaiting an `extension_ui_response` (first answer wins).
    pub pending_dialogs: Mutex<HashMap<String, ()>>,
    /// Number of attached client connections; drives the empty-room dialog policy.
    pub attached: std::sync::atomic::AtomicUsize,
}

pub struct SpawnArgs {
    pub sid: String,
    pub cwd: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
}

impl Session {
    /// The one launch line (docs/DAEMON.md). Resume-or-create via --session-id.
    pub async fn spawn(args: SpawnArgs, log_dir: &Path) -> std::io::Result<Arc<Session>> {
        let mut command = create_pi_command(&args, log_dir)?;
        let mut child = command.spawn()?;

        let pid = child.id();
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (output, _) = broadcast::channel(SESSION_EVENT_BUFFER);

        let session = Arc::new(Session {
            sid: args.sid,
            pid,
            stdin: Mutex::new(Some(stdin)),
            output,
            pending_dialogs: Mutex::new(HashMap::new()),
            attached: std::sync::atomic::AtomicUsize::new(0),
        });

        tokio::spawn(pump_stdout(session.clone(), stdout, child));

        Ok(session)
    }

    /// Write one JSONL line to pi's stdin.
    pub async fn write_line(&self, value: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(value).expect("serializable");
        let mut guard = self.stdin.lock().await;

        let Some(stdin) = guard.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session stdin already closed",
            ));
        };

        line.push(LF);
        stdin.write_all(&line).await?;
        stdin.flush().await
    }

    /// Graceful stop: close stdin (pi's documented-by-source shutdown path).
    /// Dropping the handle is what closes the pipe fd and delivers EOF.
    /// The stdout pump observes the exit and broadcasts `Exit`.
    pub async fn close_stdin(&self) {
        drop(self.stdin.lock().await.take());
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
}

/// Build the launch line: env guards, mode flags, optional overrides, stderr log.
fn create_pi_command(args: &SpawnArgs, log_dir: &Path) -> std::io::Result<Command> {
    let stderr_log = std::fs::File::create(log_dir.join(format!("{}.stderr.log", args.sid)))?;

    let mut cli_args: Vec<&str> = vec!["--mode", "rpc", "--session-id", &args.sid, "--approve"];

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

/// Read pi stdout lines forever; route responses, apply the dialog policy,
/// forward everything else. Ends by reaping the child and broadcasting Exit.
async fn pump_stdout(session: Arc<Session>, stdout: tokio::process::ChildStdout, mut child: Child) {
    // LF-only framing: read_until(LF), strip a trailing CR. Never a generic
    // line reader that splits on other Unicode separators.
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        buf.clear();

        match reader.read_until(LF, &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let mut line = buf.as_slice();

        if line.last() == Some(&LF) {
            line = &line[..line.len() - 1];
        }

        if line.last() == Some(&CR) {
            line = &line[..line.len() - 1];
        }

        // Deliberately after stripping: a bare "\n" line becomes empty here.
        if line.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            // Should never happen (pi guarantees stdout purity); don't die on it.
            Err(_) => continue,
        };

        route_line(&session, value).await;
    }

    let exit_code = child.wait().await.ok().and_then(|status| status.code());
    let _ = session.output.send(SessionOutput::Exit { exit_code });
}

async fn route_line(session: &Arc<Session>, value: Value) {
    if handle_response_line(session, &value) {
        return;
    }

    if handle_dialog_request(session, &value).await {
        return;
    }

    let _ = session.output.send(SessionOutput::Event(value));
}

/// Passthrough responses carry the id we injected (see passthrough.rs).
/// Returns true when the line was consumed as a response.
fn handle_response_line(session: &Arc<Session>, value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("response") {
        return false;
    }

    let Some(request_key) = value.get("id").and_then(Value::as_str) else {
        return false;
    };

    if !request_key.starts_with("hv-") {
        return false;
    }

    let _ = session.output.send(SessionOutput::Response {
        request_key: request_key.to_owned(),
        response: value.clone(),
    });

    true
}

/// Dialog policy (docs/DAEMON.md): dialogs get auto-cancelled when nobody is
/// attached; otherwise they are marked pending and forwarded (first client
/// answer wins). Returns true when the line was swallowed (auto-cancelled).
async fn handle_dialog_request(session: &Arc<Session>, value: &Value) -> bool {
    if value.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
        return false;
    }

    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if !DIALOG_METHODS.contains(&method) {
        // Fire-and-forget kinds are forwarded, never answered.
        return false;
    }

    let dialog_id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let nobody_attached = session.attached.load(std::sync::atomic::Ordering::SeqCst) == 0;

    if nobody_attached {
        let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});
        let _ = session.write_line(&cancel).await;

        return true;
    }

    session.pending_dialogs.lock().await.insert(dialog_id, ());

    false
}
