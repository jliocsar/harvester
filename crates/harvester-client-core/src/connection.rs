//! The live client ↔ harvesterd link: an `ssh -L` Unix-socket forward plus
//! LF-framed JSONL over it (docs/DAEMON.md §Protocol).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{
    encode_frame, read_frame, HarvesterCmd, Outgoing, PingData, Request, LF, PROTOCOL_VERSION,
};
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::time::Instant;

use crate::host::Host;
use crate::ssh::SshTarget;
use crate::{Error, Result};

/// The daemon's socket, relative to `$HOME` — the same path on remote
/// machines (forwarded) and the local one (connected directly). Shared with
/// the daemon through `harvester-proto`, which is the rendezvous contract.
pub(crate) use harvester_proto::DAEMON_SOCKET_PATH as DAEMON_SOCKET;
/// How long to keep retrying connect+ping while a freshly started daemon
/// binds its socket (covers slow VMs; normal case succeeds on try one).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// Request id of the handshake ping — the first request on every connection,
/// so 0 is unambiguous.
const HANDSHAKE_PING_ID: u64 = 0;
/// Forward socket file names: `harvester-<pid>-<hash>.sock`. The prefix and
/// suffix are what [`sweep_leaked_forward_sockets`] matches on, and the
/// suffix is what keeps the sweep off ssh's own control sockets
/// (`harvester-ssh-%C`, no extension) in the same directory.
const FORWARD_SOCKET_PREFIX: &str = "harvester-";
const FORWARD_SOCKET_SUFFIX: &str = ".sock";

/// Keeps the ssh forward process alive (killed on drop via `kill_on_drop`)
/// and owns the forward's local socket file. The local (no-SSH) machine
/// needs neither: `ForwardGuard::none()` — the daemon owns its socket.
struct ForwardGuard {
    child: Option<Child>,
    local_socket: Option<PathBuf>,
}

impl ForwardGuard {
    fn none() -> Self {
        ForwardGuard {
            child: None,
            local_socket: None,
        }
    }
}

impl Drop for ForwardGuard {
    fn drop(&mut self) {
        let Some(local_socket) = &self.local_socket else {
            return;
        };

        if let Err(error) = std::fs::remove_file(local_socket) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "failed to remove local socket {}: {error}",
                    local_socket.display()
                );
            }
        }
    }
}

/// Read half: daemon → client lines. Holds the forward alive.
pub struct DaemonReader {
    reader: BufReader<OwnedReadHalf>,
    line_buffer: Vec<u8>,
    _forward: Arc<ForwardGuard>,
}

impl DaemonReader {
    /// The next daemon line, `None` on clean EOF (daemon or forward went away).
    pub async fn next(&mut self) -> std::io::Result<Option<Outgoing>> {
        read_outgoing(&mut self.reader, &mut self.line_buffer).await
    }

    /// The next daemon line as raw bytes, undecoded. For tools that must see
    /// exactly what the daemon sent — a typed read turns an unrecognised line
    /// into an error and ends the stream, which is precisely the case a
    /// protocol debugger needs to observe.
    pub async fn next_raw(&mut self) -> std::io::Result<Option<&[u8]>> {
        read_frame(&mut self.reader, &mut self.line_buffer).await
    }
}

/// Write half: client → daemon requests. Holds the forward alive.
pub struct DaemonWriter {
    writer: OwnedWriteHalf,
    _forward: Arc<ForwardGuard>,
}

impl DaemonWriter {
    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        write_request(&mut self.writer, request).await
    }

    /// Send `line` verbatim as one frame, without validating it. Lets a
    /// protocol tool exercise the daemon's own error paths (a malformed frame
    /// must produce a parse-error response, and that is untestable through a
    /// typed send).
    pub async fn send_raw_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(line).await?;
        self.writer.write_all(&[LF]).await?;
        self.writer.flush().await
    }
}

/// A pinged, version-checked connection. The halves are public so callers
/// needing concurrent read/write (an event pump + a command sender) can move
/// them into separate tasks; each half keeps the ssh forward alive.
pub struct DaemonConnection {
    pub reader: DaemonReader,
    pub writer: DaemonWriter,
}

/// The one place the host kinds actually differ: a remote daemon's socket is
/// reached through an `ssh -L` forward, the local one directly. Both end in
/// the same ping handshake and return the daemon's self-reported version —
/// the mismatch policy is bootstrap's business, not ours.
pub(crate) async fn connect_host(host: &Host, home: &str) -> Result<(DaemonConnection, String)> {
    match host {
        Host::Local => connect_local(home).await,
        Host::Ssh(target) => connect(target, home).await,
    }
}

/// Forwards the daemon socket and completes the ping handshake, retrying
/// until `CONNECT_TIMEOUT` (a just-started daemon needs a beat to bind).
async fn connect(target: &SshTarget, remote_home: &str) -> Result<(DaemonConnection, String)> {
    let local_socket = local_socket_path(&target.dest);

    // A stale file from a previous run makes ssh refuse the bind.
    match std::fs::remove_file(&local_socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }

    let child = create_forward_command(target, &local_socket, remote_home).spawn()?;
    // Guarded from the spawn, not from the success path: every error return
    // below leaves through this scope, and the socket file is bound the
    // moment ssh comes up. Building it later orphaned one socket file per
    // expired connect attempt.
    let mut guard = ForwardGuard {
        child: Some(child),
        local_socket: Some(local_socket.clone()),
    };
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        if let Some(status) = guard.child.as_mut().expect("forward child").try_wait()? {
            return Err(Error::DaemonConnect(format!("ssh exited: {status}")));
        }

        match bounded_handshake(&local_socket, deadline).await {
            Ok((reader, writer, daemon_version)) => {
                return Ok((assemble(reader, writer, guard), daemon_version));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(Error::DaemonConnect(format!(
                        "daemon unreachable after {CONNECT_TIMEOUT:?}: {error}"
                    )));
                }

                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
            }
        }
    }
}

/// One handshake attempt, capped at the connect deadline. Without the cap a
/// daemon that accepts but never answers the ping would hang the bridge
/// forever — the retry loop only checks the deadline on the error path.
async fn bounded_handshake(
    socket: &Path,
    deadline: Instant,
) -> std::io::Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf, String)> {
    let remaining = deadline.duration_since(Instant::now());

    match tokio::time::timeout(remaining, handshake(socket)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "ping went unanswered",
        )),
    }
}

/// The local-machine variant: the daemon's socket is on this filesystem, so
/// connect directly — no forward process, no socket-file ownership. Same
/// retry window (a just-started daemon needs a beat to bind).
async fn connect_local(home: &str) -> Result<(DaemonConnection, String)> {
    let socket = PathBuf::from(home).join(DAEMON_SOCKET);
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        match bounded_handshake(&socket, deadline).await {
            Ok((reader, writer, daemon_version)) => {
                return Ok((
                    assemble(reader, writer, ForwardGuard::none()),
                    daemon_version,
                ));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(Error::DaemonConnect(format!(
                        "local daemon unreachable after {CONNECT_TIMEOUT:?}: {error}"
                    )));
                }

                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
            }
        }
    }
}

fn assemble(
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    guard: ForwardGuard,
) -> DaemonConnection {
    let guard = Arc::new(guard);

    DaemonConnection {
        reader: DaemonReader {
            reader,
            line_buffer: Vec::new(),
            _forward: guard.clone(),
        },
        writer: DaemonWriter {
            writer,
            _forward: guard,
        },
    }
}

/// The long-lived `ssh -N -L` process carrying the socket forward.
fn create_forward_command(
    target: &SshTarget,
    local_socket: &Path,
    remote_home: &str,
) -> tokio::process::Command {
    let forward_spec = format!("{}:{remote_home}/{DAEMON_SOCKET}", local_socket.display());
    // Keepalives and StreamLocalBindUnlink live in SSH_BASE_ARGS: the master
    // owns the connection and the forward's listener, so passing them here
    // (as a mux client usually is) would have no effect.
    let mut command =
        target.command(&["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward_spec]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // ssh's own diagnostics (auth failures, forward errors) go to our
        // stderr — that is the log, not noise.
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    command
}

/// One connection attempt: connect the forwarded socket, send `ping`, read
/// the response. Fails (for retry) while the forward or daemon is not up yet.
async fn handshake(
    local_socket: &Path,
) -> std::io::Result<(BufReader<OwnedReadHalf>, OwnedWriteHalf, String)> {
    let stream = UnixStream::connect(local_socket).await?;
    let (read_half, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let ping = Request::Harvester {
        id: Some(json!(HANDSHAKE_PING_ID)),
        cmd: HarvesterCmd::Ping {
            version: PROTOCOL_VERSION.to_string(),
        },
    };
    write_request(&mut writer, &ping).await?;

    let mut line_buffer = Vec::new();
    let Some(outgoing) = read_outgoing(&mut reader, &mut line_buffer).await? else {
        // The forward accepted locally but the remote connect failed
        // (daemon not bound yet): ssh closes our stream immediately.
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed before ping response",
        ));
    };

    let Outgoing::HarvesterResponse { response, .. } = outgoing else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ping answered with something other than a harvester response",
        ));
    };
    let ping: PingData = response
        .into_result()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        .and_then(|data| {
            serde_json::from_value(data).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("malformed ping response: {error}"),
                )
            })
        })?;

    Ok((reader, writer, ping.version))
}

async fn write_request(writer: &mut OwnedWriteHalf, request: &Request) -> std::io::Result<()> {
    let line = encode_frame(request)?;
    writer.write_all(&line).await?;
    writer.flush().await
}

async fn read_outgoing(
    reader: &mut BufReader<OwnedReadHalf>,
    line_buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Outgoing>> {
    let Some(line) = read_frame(reader, line_buffer).await? else {
        return Ok(None);
    };

    Ok(Some(serde_json::from_slice(line)?))
}

/// Per-process, per-destination, collision-free: a hash of the *full* dest
/// (a readable sanitized tag proved collision-prone — `web_1` and `web.1`
/// sanitized identically, and the second bridge would unlink and rebind over
/// the first one's live forward, cross-wiring machines). Short by
/// construction: Unix socket paths max out around 108 bytes (`sun_path`).
fn local_socket_path(dest: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dest.hash(&mut hasher);

    crate::ssh::client_socket_dir().join(format!(
        "{FORWARD_SOCKET_PREFIX}{}-{:016x}{FORWARD_SOCKET_SUFFIX}",
        std::process::id(),
        hasher.finish()
    ))
}

/// Removes forward socket files no daemon connection can be reached through
/// any more — the ones a previous client left behind when it died without
/// running [`ForwardGuard`]'s `Drop` (SIGKILL, OOM, a panic-abort). The
/// same-pid `remove_file` preamble in [`connect`] never reclaims those: the
/// next process has a different pid, so without this they accumulate in
/// `~/.harvester/run` forever, one per destination per crash.
///
/// The liveness test is a connect, not a pid check: a bound socket whose
/// listener is gone refuses with `ECONNREFUSED`, while a live forward — this
/// process's own included — accepts. That is pid-reuse-proof, which a
/// filename-pid check would not be.
pub(crate) async fn sweep_leaked_forward_sockets() {
    let dir = crate::ssh::client_socket_dir();

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("harvester: reading {} failed: {error}", dir.display());

            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if !name.starts_with(FORWARD_SOCKET_PREFIX) || !name.ends_with(FORWARD_SOCKET_SUFFIX) {
            continue;
        }

        remove_if_refused(&path).await;
    }
}

/// One socket's liveness test. Only `ECONNREFUSED` proves the listener is
/// gone; every other error (permissions, a path that stopped being a socket)
/// is a reason to leave the file alone.
async fn remove_if_refused(path: &Path) {
    let Err(error) = UnixStream::connect(path).await else {
        return;
    };

    if error.kind() != std::io::ErrorKind::ConnectionRefused {
        return;
    }

    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!(
                "harvester: removing leaked forward socket {} failed: {error}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::UnixListener;

    /// A socket file whose listener is gone is exactly what a SIGKILLed
    /// client leaves behind: still bound on disk, refusing every connect.
    #[tokio::test]
    async fn removes_a_socket_whose_listener_is_gone() {
        let dir = std::env::temp_dir().join(format!("harvester-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");

        let dead = dir.join("harvester-1-dead.sock");
        let listener = UnixListener::bind(&dead).expect("bind");
        // Dropping the listener does not unlink the file — that is the leak.
        drop(listener);

        assert!(dead.exists(), "dropping a listener must leave the file");

        remove_if_refused(&dead).await;

        assert!(!dead.exists(), "a refusing socket must be reclaimed");

        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

    /// The other half of the invariant: a forward this or another live client
    /// still owns must survive the sweep.
    #[tokio::test]
    async fn keeps_a_socket_with_a_live_listener() {
        let dir = std::env::temp_dir().join(format!("harvester-sweep-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");

        let live = dir.join("harvester-1-live.sock");
        let _listener = UnixListener::bind(&live).expect("bind");

        remove_if_refused(&live).await;

        assert!(live.exists(), "a live forward must not be reclaimed");

        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }
}
