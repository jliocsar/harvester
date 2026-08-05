//! The live client ↔ harvesterd link: a Unix socket (forwarded over `ssh -L`
//! for a remote machine, direct for the local one) plus LF-framed JSONL over
//! it (docs/DAEMON.md §Protocol).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{
    encode_frame, read_frame, HarvesterCmd, Outgoing, PingData, Request, PROTOCOL_VERSION,
};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
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
/// so 0 is unambiguous. `client.rs` starts its own ids above it.
pub(crate) const HANDSHAKE_PING_ID: u64 = 0;
/// Forward socket file names: `harvester-<pid>-<hash>.sock`. The prefix and
/// suffix are what [`sweep_leaked_forward_sockets`] matches on, and the
/// suffix is what keeps the sweep off ssh's own control sockets
/// (`harvester-ssh-%C`, no extension) in the same directory.
const FORWARD_SOCKET_PREFIX: &str = "harvester-";
const FORWARD_SOCKET_SUFFIX: &str = ".sock";

/// A live `ssh -N -L` process and the local socket file it bound. Dropping it
/// kills the process (`kill_on_drop`) and unlinks the file. The local machine
/// has none — the daemon owns its socket there.
pub(crate) struct Forward {
    /// Behind a mutex only so liveness can be checked through the shared
    /// handle both connection halves hold: `try_wait` needs `&mut Child`, and
    /// the alternative — dropping the forward on every failed handshake —
    /// would kill ssh on the retry path this loop exists to walk.
    child: std::sync::Mutex<Child>,
    local_socket: PathBuf,
}

impl Forward {
    fn exited(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.lock().expect("forward child mutex").try_wait()
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.local_socket) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "failed to remove local socket {}: {error}",
                    self.local_socket.display()
                );
            }
        }
    }
}

/// The forward both halves keep alive, and every failed connect attempt shares.
/// `None` for the local machine — the daemon owns its socket there.
pub(crate) type ForwardHandle = Arc<Option<Forward>>;

/// The local machine, and every test that speaks over a duplex pair.
pub(crate) fn no_forward() -> ForwardHandle {
    Arc::new(None)
}

/// Read half: daemon → client lines. Holds the forward alive.
pub struct DaemonReader {
    reader: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    line_buffer: Vec<u8>,
    _forward: ForwardHandle,
}

impl DaemonReader {
    /// The next daemon line, `None` on clean EOF (daemon or forward went away).
    pub async fn next(&mut self) -> std::io::Result<Option<Outgoing>> {
        let Some(line) = read_frame(&mut self.reader, &mut self.line_buffer).await? else {
            return Ok(None);
        };

        Ok(Some(serde_json::from_slice(line)?))
    }

    #[cfg(feature = "raw-protocol")]
    async fn next_raw(&mut self) -> std::io::Result<Option<&[u8]>> {
        read_frame(&mut self.reader, &mut self.line_buffer).await
    }
}

/// Write half: client → daemon requests. Holds the forward alive.
pub struct DaemonWriter {
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    _forward: ForwardHandle,
}

impl DaemonWriter {
    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        self.write_all_flushed(&encode_frame(request)?).await
    }

    #[cfg(feature = "raw-protocol")]
    async fn send_raw_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        let mut frame = line.to_vec();
        frame.push(harvester_proto::LF);

        self.write_all_flushed(&frame).await
    }

    async fn write_all_flushed(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(frame).await?;
        self.writer.flush().await
    }
}

/// A connection that has completed the ping handshake. The halves are public so
/// callers needing concurrent read/write (an event pump + a command sender) can
/// move them into separate tasks; each half keeps the ssh forward alive.
pub struct DaemonConnection {
    pub reader: DaemonReader,
    pub writer: DaemonWriter,
}

impl DaemonConnection {
    /// Frame the protocol over any duplex stream. Boxed rather than generic
    /// because the stream kind is an implementation detail of *connecting*:
    /// type parameters here would propagate through `DaemonClient`, every
    /// bridge flow and `Fleet` for a per-line cost that is lost in the noise of
    /// an SSH round trip. In exchange, a `tokio::io::duplex` pair stands in for
    /// a daemon in tests, which is what makes everything above this layer
    /// testable without a VM.
    pub(crate) fn over(
        stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        forward: ForwardHandle,
    ) -> DaemonConnection {
        let (read_half, write_half) = tokio::io::split(stream);

        DaemonConnection {
            reader: DaemonReader {
                reader: BufReader::new(Box::new(read_half)),
                line_buffer: Vec::new(),
                _forward: forward.clone(),
            },
            writer: DaemonWriter {
                writer: Box::new(write_half),
                _forward: forward,
            },
        }
    }
}

/// Byte access to a live connection, for `harvester-cli`'s protocol debugger
/// and nothing else. **Not** a framing bypass: reads still go through
/// [`read_frame`] and writes still terminate with exactly one `LF`. What these
/// two skip is *typing* — a line the client cannot parse must be printable
/// rather than end the stream, and a malformed frame must reach the daemon so
/// its documented parse-error response can be observed. Neither is expressible
/// through the typed calls.
///
/// A module rather than two more methods on [`DaemonReader`] / [`DaemonWriter`]
/// because a cargo feature is not a boundary: features unify across a workspace
/// build, so gating the methods still left them on the production types for the
/// gpui client to find. Reaching them now means naming this module, which the
/// shipped client has no reason to do and no way to do by accident.
#[cfg(feature = "raw-protocol")]
pub mod raw {
    use super::{DaemonReader, DaemonWriter};

    pub async fn next_line(reader: &mut DaemonReader) -> std::io::Result<Option<&[u8]>> {
        reader.next_raw().await
    }

    pub async fn send_line(writer: &mut DaemonWriter, line: &[u8]) -> std::io::Result<()> {
        writer.send_raw_line(line).await
    }
}

/// The one place the host kinds actually differ: a remote daemon's socket is
/// reached through an `ssh -L` forward, the local one directly. Both end in the
/// same retry-until-the-ping-lands loop and hand back what the daemon said
/// about itself (protocol version and build fingerprint) — judging it is
/// bootstrap's business, not ours.
pub(crate) async fn connect_host(host: &Host, home: &str) -> Result<(DaemonConnection, PingData)> {
    let (socket, forward) = match host {
        Host::Local => (PathBuf::from(home).join(DAEMON_SOCKET), no_forward()),
        Host::Ssh(target) => {
            let forward = start_forward(target, home)?;

            (forward.local_socket.clone(), Arc::new(Some(forward)))
        }
    };

    connect_socket(&socket, forward).await
}

/// Retries connect+ping until `CONNECT_TIMEOUT` (a just-started daemon needs a
/// beat to bind), giving up early if the forward process dies.
async fn connect_socket(
    socket: &Path,
    forward: ForwardHandle,
) -> Result<(DaemonConnection, PingData)> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        if let Some(status) = forward
            .as_ref()
            .as_ref()
            .map(Forward::exited)
            .transpose()?
            .flatten()
        {
            return Err(Error::DaemonConnect(format!("ssh exited: {status}")));
        }

        match bounded_handshake(socket, &forward, deadline).await {
            Ok(connected) => return Ok(connected),
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
/// forever — the retry loop only checks the deadline on the error path. The
/// forward outlives every attempt — the common retry is a socket ssh accepted
/// locally and closed immediately because the daemon has not bound yet.
async fn bounded_handshake(
    socket: &Path,
    forward: &ForwardHandle,
    deadline: Instant,
) -> std::io::Result<(DaemonConnection, PingData)> {
    let remaining = deadline.duration_since(Instant::now());
    let stream = tokio::time::timeout(remaining, UnixStream::connect(socket))
        .await
        .map_err(|_elapsed| timed_out())??;
    let connection = DaemonConnection::over(stream, forward.clone());

    tokio::time::timeout(
        deadline.duration_since(Instant::now()),
        handshake(connection),
    )
    .await
    .map_err(|_elapsed| timed_out())?
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "ping went unanswered")
}

/// The first request on every connection (docs/DAEMON.md §ping): our protocol
/// version out, the daemon's version and build fingerprint back.
async fn handshake(
    mut connection: DaemonConnection,
) -> std::io::Result<(DaemonConnection, PingData)> {
    connection
        .writer
        .send(&Request::Harvester {
            id: Some(json!(HANDSHAKE_PING_ID)),
            cmd: HarvesterCmd::Ping {
                version: PROTOCOL_VERSION,
            },
        })
        .await?;

    let ping = match connection.reader.next().await? {
        Some(Outgoing::HarvesterResponse { response, .. }) => response
            .into_result()
            .and_then(|data| {
                serde_json::from_value::<PingData>(data)
                    .map_err(|error| format!("malformed ping response: {error}"))
            })
            .map_err(invalid_data)?,
        // The forward accepted locally but the remote connect failed (daemon
        // not bound yet): ssh closes our stream immediately.
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before ping response",
            ))
        }
        Some(_) => return Err(invalid_data("ping answered in the wrong envelope")),
    };

    Ok((connection, ping))
}

fn invalid_data(detail: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, detail)
}

/// Spawns the long-lived `ssh -N -L` process carrying the socket forward. The
/// [`Forward`] is built from the spawn, not from the success path: the socket
/// file is bound the moment ssh comes up, and building the guard later orphaned
/// one socket file per expired connect attempt.
fn start_forward(target: &SshTarget, remote_home: &str) -> Result<Forward> {
    let local_socket = local_socket_path(&target.dest);

    // A stale file from a previous run makes ssh refuse the bind.
    match std::fs::remove_file(&local_socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }

    let forward_spec = format!("{}:{remote_home}/{DAEMON_SOCKET}", local_socket.display());
    // Keepalives and StreamLocalBindUnlink live in SSH_BASE_ARGS: the master
    // owns the connection and the forward's listener, so passing them here
    // (as a mux client usually is) would have no effect.
    let mut command =
        target.command(&["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward_spec]);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // ssh's own diagnostics (auth failures, forward errors) go to our
        // stderr — that is the log, not noise.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    Ok(Forward {
        child: std::sync::Mutex::new(child),
        local_socket,
    })
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
/// running [`Forward`]'s `Drop` (SIGKILL, OOM, a panic-abort). The same-pid
/// `remove_file` preamble in [`start_forward`] never reclaims those: the next
/// process has a different pid, so without this they accumulate in
/// `~/.harvester/run` forever, one per destination per crash.
///
/// The liveness test is a connect, not a pid check: a bound socket whose
/// listener is gone refuses with `ECONNREFUSED`, while a live forward — this
/// process's own included — accepts. That is pid-reuse-proof, which a
/// filename-pid check would not be.
pub(crate) async fn sweep_leaked_forward_sockets() {
    let dir = crate::ssh::client_socket_dir();

    let entries = match std::fs::read_dir(dir) {
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

/// The daemon side of a duplex pair, for tests above and in `client.rs` /
/// `bridge`: raw JSON in both directions, since a fake daemon must be able to
/// send lines the typed client would never produce.
#[cfg(test)]
impl DaemonReader {
    pub(crate) async fn next_line_for_test(&mut self) -> serde_json::Value {
        let line = read_frame(&mut self.reader, &mut self.line_buffer)
            .await
            .expect("read")
            .expect("a frame, not EOF");

        serde_json::from_slice(line).expect("valid json")
    }
}

#[cfg(test)]
impl DaemonWriter {
    pub(crate) async fn send_json_for_test(&mut self, value: serde_json::Value) {
        let frame = encode_frame(&value).expect("serializes");

        self.write_all_flushed(&frame).await.expect("write");
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

    /// The whole point of framing over a duplex stream: a plain in-memory pipe
    /// stands in for a daemon, no socket and no VM.
    #[tokio::test]
    async fn a_connection_frames_over_any_duplex_stream() {
        let (client_side, mut daemon_side) = tokio::io::duplex(4096);
        let mut connection = DaemonConnection::over(client_side, no_forward());

        connection
            .writer
            .send(&Request::Harvester {
                id: Some(json!(7)),
                cmd: HarvesterCmd::Detach {
                    sid: "01H".to_string(),
                },
            })
            .await
            .expect("send");

        let mut request_buffer = Vec::new();
        let request = read_frame(&mut BufReader::new(&mut daemon_side), &mut request_buffer)
            .await
            .expect("read")
            .expect("one frame");

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(request).expect("json"),
            json!({"id": 7, "type": "harvester", "cmd": {"type": "detach", "sid": "01H"}})
        );

        daemon_side
            .write_all(
                b"{\"type\":\"event\",\"sid\":\"01H\",\"event\":{\"type\":\"agent_settled\"}}\n",
            )
            .await
            .expect("write");

        let Some(Outgoing::Event { sid, event }) = connection.reader.next().await.expect("read")
        else {
            panic!("expected an event");
        };

        assert_eq!(sid, "01H");
        assert_eq!(event, json!({"type": "agent_settled"}));
    }

    /// The handshake is the only thing between a connected stream and a usable
    /// connection, and it now runs against a fake daemon.
    #[tokio::test]
    async fn the_handshake_exchanges_the_ping() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            let mut daemon = DaemonConnection::over(daemon_side, no_forward());
            let request = daemon.reader.next_line_for_test().await;

            assert_eq!(request["cmd"], json!({"type": "ping", "version": 1}));
            daemon
                .writer
                .send_json_for_test(json!({
                    "id": request["id"],
                    "type": "harvester_response",
                    "response": {"success": true, "data": {"version": 1, "build": "fnv1a64-1-0"}},
                }))
                .await;
        });

        let (_connection, ping) = handshake(DaemonConnection::over(client_side, no_forward()))
            .await
            .expect("the handshake completes");

        assert_eq!(ping.version, PROTOCOL_VERSION);
        assert_eq!(ping.build, "fnv1a64-1-0");
    }

    #[tokio::test]
    async fn a_stream_that_closes_before_the_ping_response_is_an_error() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            // Take the ping, then go away — exactly what an `ssh -L` forward
            // does when the remote connect fails because the daemon has not
            // bound its socket yet.
            DaemonConnection::over(daemon_side, no_forward())
                .reader
                .next_line_for_test()
                .await;
        });

        let Err(error) = handshake(DaemonConnection::over(client_side, no_forward())).await else {
            panic!("EOF before the response must fail");
        };

        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_ping_answered_in_the_wrong_envelope_is_an_error() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            let mut daemon = DaemonConnection::over(daemon_side, no_forward());
            daemon.reader.next_line_for_test().await;
            daemon
                .writer
                .send_json_for_test(json!({"type": "event", "sid": "01H", "event": {}}))
                .await;
        });

        let Err(error) = handshake(DaemonConnection::over(client_side, no_forward())).await else {
            panic!("an event is not a ping response");
        };

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
