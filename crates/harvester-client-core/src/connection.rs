use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harvester_proto::{
    encode_frame, lock, read_frame, HarvesterCmd, HarvesterResponse, Outgoing, PingData, Request,
    PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::time::Instant;

use crate::host::{Host, SshTarget};
use crate::{Error, Result};

pub(crate) use harvester_proto::DAEMON_SOCKET_PATH as DAEMON_SOCKET;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const HANDSHAKE_PING_ID: u64 = 0;
const FORWARD_SOCKET_PREFIX: &str = "harvester-";
const FORWARD_SOCKET_SUFFIX: &str = ".sock";

pub(crate) struct Forward {
    child: Mutex<Child>,
    local_socket: PathBuf,
}

impl Forward {
    fn exited(&self) -> std::io::Result<Option<std::process::ExitStatus>> {
        lock(&self.child, "forward child").try_wait()
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

pub(crate) type ForwardHandle = Option<Arc<Forward>>;

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
}

pub struct DaemonWriter {
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    _forward: ForwardHandle,
}

impl DaemonWriter {
    pub async fn send(&mut self, request: &Request) -> std::io::Result<()> {
        self.write_all_flushed(&encode_frame(request)?).await
    }

    async fn write_all_flushed(&mut self, frame: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(frame).await?;
        self.writer.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        self.writer.shutdown().await
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
    ) -> Self {
        let (read_half, write_half) = tokio::io::split(stream);

        Self {
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
    use harvester_proto::{read_frame, LF};

    use super::{DaemonReader, DaemonWriter};

    pub async fn next_line(reader: &mut DaemonReader) -> std::io::Result<Option<&[u8]>> {
        read_frame(&mut reader.reader, &mut reader.line_buffer).await
    }

    pub async fn send_line(writer: &mut DaemonWriter, line: &[u8]) -> std::io::Result<()> {
        let mut frame = line.to_vec();
        frame.push(LF);

        writer.write_all_flushed(&frame).await
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    Pi { sid: String, event: Value },
    SessionExit { sid: String, exit_code: Option<i32> },
    EventGap { sid: String, skipped: u64 },
}

#[derive(Clone)]
pub struct DaemonClient {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    writer: AsyncMutex<DaemonWriter>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Outgoing>>>,
    last_request_id: AtomicU64,
}

impl DaemonClient {
    pub fn start(connection: DaemonConnection) -> (Self, UnboundedReceiver<SessionEvent>) {
        let DaemonConnection { mut reader, writer } = connection;
        let (event_sender, event_receiver) = unbounded_channel();
        let client = Self {
            inner: Arc::new(ClientInner {
                writer: AsyncMutex::new(writer),
                pending: Mutex::new(HashMap::new()),
                last_request_id: AtomicU64::new(HANDSHAKE_PING_ID),
            }),
        };

        let routing_client = client.clone();

        tokio::spawn(async move {
            loop {
                match reader.next().await {
                    Ok(Some(line)) => routing_client.route_line(line, &event_sender),
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("daemon client: read failed: {error}");

                        break;
                    }
                }
            }

            routing_client.fail_all_pending();
        });

        (client, event_receiver)
    }

    pub async fn harvester(&self, cmd: HarvesterCmd) -> std::io::Result<HarvesterResponse> {
        match self.request(|id| Request::Harvester { id, cmd }).await? {
            Outgoing::HarvesterResponse { response, .. } => Ok(response),
            mismatched => Err(envelope_mismatch(&mismatched)),
        }
    }

    pub async fn pi(&self, sid: &str, cmd: Value) -> std::io::Result<Value> {
        let sid = sid.to_string();

        match self.request(|id| Request::Pi { id, sid, cmd }).await? {
            Outgoing::PiResponse { response, .. } => Ok(response),
            mismatched => Err(envelope_mismatch(&mismatched)),
        }
    }

    pub(crate) async fn close(&self) {
        if let Err(error) = self.inner.writer.lock().await.shutdown().await {
            eprintln!("daemon client: close failed: {error}");
        }
    }

    async fn request(
        &self,
        build: impl FnOnce(Option<Value>) -> Request,
    ) -> std::io::Result<Outgoing> {
        let previous = self
            .inner
            .last_request_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("daemon request id exhausted");
        let id = previous
            .checked_add(1)
            .expect("daemon request id exhausted");
        let (sender, receiver) = oneshot::channel();
        let _pending = PendingRequestGuard::register(self, id, sender);
        let request = build(Some(json!(id)));

        self.inner.writer.lock().await.send(&request).await?;

        receiver.await.map_err(|_closed| {
            std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "connection closed before the response",
            )
        })
    }

    fn route_line(&self, line: Outgoing, events: &UnboundedSender<SessionEvent>) {
        match line {
            Outgoing::Event { sid, event } => {
                drop(events.send(SessionEvent::Pi { sid, event }));
            }
            Outgoing::SessionExit { sid, exit_code } => {
                drop(events.send(SessionEvent::SessionExit { sid, exit_code }));
            }
            Outgoing::EventGap { sid, skipped } => {
                drop(events.send(SessionEvent::EventGap { sid, skipped }));
            }
            response @ (Outgoing::HarvesterResponse { .. } | Outgoing::PiResponse { .. }) => {
                self.resolve(response);
            }
        }
    }

    fn resolve(&self, response: Outgoing) {
        let id = match &response {
            Outgoing::HarvesterResponse { id, .. } | Outgoing::PiResponse { id, .. } => id.as_ref(),
            _ => None,
        };
        let Some(id) = id.and_then(Value::as_u64) else {
            eprintln!("daemon client: response without a usable id");

            return;
        };

        let Some(waiter) = lock(&self.inner.pending, "pending").remove(&id) else {
            eprintln!("daemon client: dropped response for request {id}");

            return;
        };

        drop(waiter.send(response));
    }

    fn fail_all_pending(&self) {
        lock(&self.inner.pending, "pending").clear();
    }
}

struct PendingRequestGuard {
    client: DaemonClient,
    id: u64,
}

impl PendingRequestGuard {
    fn register(client: &DaemonClient, id: u64, sender: oneshot::Sender<Outgoing>) -> Self {
        lock(&client.inner.pending, "pending").insert(id, sender);

        Self {
            client: client.clone(),
            id,
        }
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        lock(&self.client.inner.pending, "pending").remove(&self.id);
    }
}

fn envelope_mismatch(response: &Outgoing) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("response arrived in the wrong envelope: {response:?}"),
    )
}

pub(crate) async fn connect_host(host: &Host, home: &str) -> Result<(DaemonConnection, PingData)> {
    let (socket, forward) = match host {
        Host::Local => (PathBuf::from(home).join(DAEMON_SOCKET), None),
        Host::Ssh(target) => {
            let forward = start_forward(target, home)?;

            (forward.local_socket.clone(), Some(Arc::new(forward)))
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
            .as_deref()
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
        child: Mutex::new(child),
        local_socket,
    })
}

fn local_socket_path(dest: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dest.hash(&mut hasher);

    crate::host::client_socket_dir().join(forward_socket_name(std::process::id(), hasher.finish()))
}

fn forward_socket_name(client_pid: u32, destination_hash: u64) -> String {
    format!("{FORWARD_SOCKET_PREFIX}{client_pid}-{destination_hash:016x}{FORWARD_SOCKET_SUFFIX}")
}

pub(crate) async fn sweep_leaked_forward_sockets() {
    let dir = crate::host::client_socket_dir();

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

        reclaim_if_leaked(&path, name).await;
    }
}

async fn reclaim_if_leaked(path: &Path, name: &str) {
    let Some(owner_pid) = forward_socket_owner_pid(name) else {
        eprintln!("harvester: leaving {name} alone: it names no owning client");

        return;
    };

    if process_is_alive(owner_pid).await {
        return;
    }

    remove_if_refused(path).await;
}

fn forward_socket_owner_pid(name: &str) -> Option<u32> {
    let owned = name
        .strip_prefix(FORWARD_SOCKET_PREFIX)?
        .strip_suffix(FORWARD_SOCKET_SUFFIX)?;
    let (client_pid, _destination_hash) = owned.split_once('-')?;

    client_pid.parse().ok()
}

async fn process_is_alive(pid: u32) -> bool {
    let status = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(pid_liveness_script(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(status) => status.success(),
        Err(error) => {
            eprintln!("harvester: probing pid {pid} for liveness failed: {error}");

            true
        }
    }
}

fn pid_liveness_script(pid: u32) -> String {
    format!("kill -0 {pid}")
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
    pub(crate) async fn next_line_for_test(&mut self) -> Value {
        let line = read_frame(&mut self.reader, &mut self.line_buffer)
            .await
            .expect("read")
            .expect("a frame, not EOF");

        serde_json::from_slice(line).expect("valid json")
    }
}

#[cfg(test)]
impl DaemonWriter {
    pub(crate) async fn send_json_for_test(&mut self, value: Value) {
        let frame = encode_frame(&value).expect("serializes");

        self.write_all_flushed(&frame).await.expect("write");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::UnixListener;

    use crate::scratch::ScratchDir;

    #[tokio::test]
    async fn removes_a_socket_whose_listener_is_gone() {
        let dir = ScratchDir::create("sweep");
        let dead = dir.join("harvester-1-dead.sock");
        let listener = UnixListener::bind(&dead).expect("bind");

        drop(listener);

        assert!(dead.exists(), "dropping a listener must leave the file");

        remove_if_refused(&dead).await;

        assert!(!dead.exists(), "a refusing socket must be reclaimed");
    }

    #[test]
    fn a_forward_socket_name_says_which_client_owns_it() {
        let name = forward_socket_name(4242, 0x0123_4567_89ab_cdef);

        assert_eq!(forward_socket_owner_pid(&name), Some(4242));
    }

    #[test]
    fn a_socket_that_names_no_client_is_left_alone() {
        assert_eq!(forward_socket_owner_pid("harvester-ssh-9f3c.sock"), None);
        assert_eq!(forward_socket_owner_pid("harvester-.sock"), None);
    }

    #[tokio::test]
    async fn a_running_client_is_seen_as_alive() {
        assert!(
            process_is_alive(std::process::id()).await,
            "the sweep must never poke a forward whose client is still running"
        );
    }

    #[tokio::test]
    async fn keeps_a_socket_with_a_live_listener() {
        let dir = ScratchDir::create("sweep-live");
        let live = dir.join("harvester-1-live.sock");
        let _listener = UnixListener::bind(&live).expect("bind");

        remove_if_refused(&live).await;

        assert!(live.exists(), "a live forward must not be reclaimed");
    }

    #[tokio::test]
    async fn a_connection_frames_over_any_duplex_stream() {
        let (client_side, mut daemon_side) = tokio::io::duplex(4096);
        let mut connection = DaemonConnection::over(client_side, None);

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
            serde_json::from_slice::<Value>(request).expect("json"),
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
    async fn a_client_request_resolves_when_the_daemon_echoes_its_id() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);
        let (client, _session_events) =
            DaemonClient::start(DaemonConnection::over(client_side, None));
        let mut daemon = DaemonConnection::over(daemon_side, None);

        let request_task =
            tokio::spawn(async move { client.harvester(HarvesterCmd::ListSessions).await });
        let request = daemon.reader.next_line_for_test().await;

        assert_eq!(
            request["id"],
            json!(HANDSHAKE_PING_ID + 1),
            "request ids continue the handshake's own sequence"
        );

        daemon
            .writer
            .send_json_for_test(json!({
                "id": request["id"],
                "type": "harvester_response",
                "response": {"success": true, "data": {"sessions": []}},
            }))
            .await;

        let response = request_task
            .await
            .expect("request task")
            .expect("the response resolves at the await point");

        assert_eq!(response.into_result(), Ok(json!({"sessions": []})));
    }

    #[tokio::test]
    async fn the_handshake_exchanges_the_ping() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            let mut daemon = DaemonConnection::over(daemon_side, None);
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

        let (_connection, ping) = handshake(DaemonConnection::over(client_side, None))
            .await
            .expect("the handshake completes");

        assert_eq!(ping.version, PROTOCOL_VERSION);
        assert_eq!(ping.build, "fnv1a64-1-0");
    }

    #[tokio::test]
    async fn a_stream_that_closes_before_the_ping_response_is_an_error() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            DaemonConnection::over(daemon_side, None)
                .reader
                .next_line_for_test()
                .await;
        });

        let Err(error) = handshake(DaemonConnection::over(client_side, None)).await else {
            panic!("EOF before the response must fail");
        };

        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_ping_answered_in_the_wrong_envelope_is_an_error() {
        let (client_side, daemon_side) = tokio::io::duplex(4096);

        tokio::spawn(async move {
            let mut daemon = DaemonConnection::over(daemon_side, None);
            daemon.reader.next_line_for_test().await;
            daemon
                .writer
                .send_json_for_test(json!({"type": "event", "sid": "01H", "event": {}}))
                .await;
        });

        let Err(error) = handshake(DaemonConnection::over(client_side, None)).await else {
            panic!("an event is not a ping response");
        };

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
