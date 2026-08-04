//! The live client ↔ harvesterd link: an `ssh -L` Unix-socket forward plus
//! LF-framed JSONL over it (docs/DAEMON.md §Protocol).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{HarvesterCmd, Outgoing, Request, CR, LF, PROTOCOL_VERSION};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::time::Instant;

use crate::ssh::SshTarget;
use crate::{Error, Result};

/// The daemon's socket, relative to `$HOME` — the same path on remote
/// machines (forwarded) and the local one (connected directly).
pub(crate) const DAEMON_SOCKET: &str = ".harvester/harvesterd.sock";
/// How long to keep retrying connect+ping while a freshly started daemon
/// binds its socket (covers slow VMs; normal case succeeds on try one).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// Request id of the handshake ping — the first request on every connection,
/// so 0 is unambiguous.
const HANDSHAKE_PING_ID: u64 = 0;

/// Keeps the ssh forward process alive (killed on drop via `kill_on_drop`)
/// and owns the forward's local socket file. The local (no-SSH) machine
/// needs neither: `ForwardGuard::none()` — the daemon owns its socket.
struct ForwardGuard {
    _child: Option<Child>,
    local_socket: Option<PathBuf>,
}

impl ForwardGuard {
    fn none() -> Self {
        ForwardGuard {
            _child: None,
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
}

/// A pinged, version-checked connection. The halves are public so callers
/// needing concurrent read/write (an event pump + a command sender) can move
/// them into separate tasks; each half keeps the ssh forward alive.
pub struct DaemonConnection {
    pub reader: DaemonReader,
    pub writer: DaemonWriter,
}

impl DaemonConnection {
    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        self.writer.send(request).await
    }

    pub async fn next(&mut self) -> std::io::Result<Option<Outgoing>> {
        self.reader.next().await
    }
}

/// Forwards the daemon socket and completes the ping handshake, retrying
/// until `CONNECT_TIMEOUT` (a just-started daemon needs a beat to bind).
/// Returns the connection plus the daemon's self-reported version — the
/// mismatch policy is bootstrap's business, not ours.
pub(crate) async fn connect(
    target: &SshTarget,
    remote_home: &str,
) -> Result<(DaemonConnection, String)> {
    let local_socket = local_socket_path(&target.dest);

    // A stale file from a previous run makes ssh refuse the bind.
    match std::fs::remove_file(&local_socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }

    let mut child = create_forward_command(target, &local_socket, remote_home).spawn()?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        if let Some(status) = child.try_wait()? {
            return Err(Error::Forward(format!("ssh exited: {status}")));
        }

        match handshake(&local_socket).await {
            Ok((reader, writer, daemon_version)) => {
                let guard = ForwardGuard {
                    _child: Some(child),
                    local_socket: Some(local_socket),
                };

                return Ok((assemble(reader, writer, guard), daemon_version));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(Error::Forward(format!(
                        "daemon unreachable after {CONNECT_TIMEOUT:?}: {error}"
                    )));
                }

                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
            }
        }
    }
}

/// The local-machine variant: the daemon's socket is on this filesystem, so
/// connect directly — no forward process, no socket-file ownership. Same
/// retry window (a just-started daemon needs a beat to bind).
pub(crate) async fn connect_local(home: &str) -> Result<(DaemonConnection, String)> {
    let socket = PathBuf::from(home).join(DAEMON_SOCKET);
    let deadline = Instant::now() + CONNECT_TIMEOUT;

    loop {
        match handshake(&socket).await {
            Ok((reader, writer, daemon_version)) => {
                return Ok((
                    assemble(reader, writer, ForwardGuard::none()),
                    daemon_version,
                ));
            }
            Err(error) => {
                if Instant::now() >= deadline {
                    return Err(Error::Forward(format!(
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
    // StreamLocalBindUnlink makes ssh unlink an existing socket file before
    // binding — a dying forward's leftover (respawn races its teardown) can
    // otherwise fail the bind with "Address already in use".
    let mut command = target.command(&[
        "-N",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StreamLocalBindUnlink=yes",
        "-L",
        &forward_spec,
    ]);
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

    let daemon_version = match outgoing {
        Outgoing::HarvesterResponse { response, .. } if response.success => response
            .data
            .as_ref()
            .and_then(|data| data.get("version"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    };
    let Some(daemon_version) = daemon_version else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed ping response",
        ));
    };

    Ok((reader, writer, daemon_version))
}

async fn write_request(writer: &mut OwnedWriteHalf, request: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(request)?;
    line.push(LF);
    writer.write_all(&line).await?;
    writer.flush().await
}

/// LF-only framing (pi's rule, ours too): split on LF, strip one trailing CR.
async fn read_outgoing(
    reader: &mut BufReader<OwnedReadHalf>,
    line_buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Outgoing>> {
    line_buffer.clear();

    let bytes_read = reader.read_until(LF, line_buffer).await?;
    if bytes_read == 0 {
        return Ok(None);
    }

    let mut line = line_buffer.as_slice();
    if line.last() == Some(&LF) {
        line = &line[..line.len() - 1];
    }
    if line.last() == Some(&CR) {
        line = &line[..line.len() - 1];
    }

    Ok(Some(serde_json::from_slice(line)?))
}

/// Cap on the sanitized-destination tag inside the local socket filename:
/// Unix socket paths max out around 108 bytes (`sun_path`), and tempdir +
/// prefix + pid must always fit alongside the tag.
const LOCAL_SOCKET_DEST_TAG_MAX_CHARS: usize = 32;

/// Short and per-process-unique: see `LOCAL_SOCKET_DEST_TAG_MAX_CHARS`.
fn local_socket_path(dest: &str) -> PathBuf {
    let sanitized_dest: String = dest
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(LOCAL_SOCKET_DEST_TAG_MAX_CHARS)
        .collect();

    std::env::temp_dir().join(format!(
        "harvester-{}-{sanitized_dest}.sock",
        std::process::id()
    ))
}
