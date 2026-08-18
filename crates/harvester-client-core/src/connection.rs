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

pub(crate) use harvester_proto::DAEMON_SOCKET_PATH as DAEMON_SOCKET;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const HANDSHAKE_PING_ID: u64 = 0;
const FORWARD_SOCKET_PREFIX: &str = "harvester-";
const FORWARD_SOCKET_SUFFIX: &str = ".sock";

pub(crate) struct Forward {
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

pub(crate) type ForwardHandle = Arc<Option<Forward>>;

pub(crate) fn no_forward() -> ForwardHandle {
    Arc::new(None)
}

pub struct DaemonReader {
    reader: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    line_buffer: Vec<u8>,
    _forward: ForwardHandle,
}

impl DaemonReader {
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

pub struct DaemonConnection {
    pub reader: DaemonReader,
    pub writer: DaemonWriter,
}

impl DaemonConnection {
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

async fn bounded_handshake(
    socket: &Path,
    forward: &ForwardHandle,
    deadline: Instant,
) -> std::io::Result<(DaemonConnection, PingData)> {
    let stream = tokio::time::timeout_at(deadline, UnixStream::connect(socket))
        .await
        .map_err(|_elapsed| timed_out())??;
    let connection = DaemonConnection::over(stream, forward.clone());

    tokio::time::timeout_at(deadline, handshake(connection))
        .await
        .map_err(|_elapsed| timed_out())?
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "ping went unanswered")
}

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

fn start_forward(target: &SshTarget, remote_home: &str) -> Result<Forward> {
    let local_socket = local_socket_path(&target.dest);

    match std::fs::remove_file(&local_socket) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::Io(error)),
    }

    let forward_spec = format!("{}:{remote_home}/{DAEMON_SOCKET}", local_socket.display());
    let mut command =
        target.command(&["-N", "-o", "ExitOnForwardFailure=yes", "-L", &forward_spec]);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;

    Ok(Forward {
        child: std::sync::Mutex::new(child),
        local_socket,
    })
}

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

    #[tokio::test]
    async fn removes_a_socket_whose_listener_is_gone() {
        let dir = std::env::temp_dir().join(format!("harvester-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");

        let dead = dir.join("harvester-1-dead.sock");
        let listener = UnixListener::bind(&dead).expect("bind");
        drop(listener);

        assert!(dead.exists(), "dropping a listener must leave the file");

        remove_if_refused(&dead).await;

        assert!(!dead.exists(), "a refusing socket must be reclaimed");

        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
    }

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
