//! Wire types for the client ↔ harvesterd protocol (docs/DAEMON.md, protocol v1).
//!
//! JSONL framing, pi's rules: LF is the only record delimiter, strip a trailing
//! CR, never use a reader that splits on other Unicode separators.
//!
//! Pi payloads are deliberately `serde_json::Value` — the client speaks Pi and
//! harvesterd passes them through verbatim. Only the envelope is typed.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const PROTOCOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The daemon's socket, relative to `$HOME`. The rendezvous point between the
/// two binaries, so it is spelled once here rather than in each of them: drift
/// would surface only as an unexplained connect timeout.
pub const DAEMON_SOCKET_PATH: &str = ".harvester/harvesterd.sock";

/// The only record delimiter in the protocol (and in pi's RPC framing).
pub const LF: u8 = b'\n';
/// Tolerated-and-stripped before `LF`; never emitted.
pub const CR: u8 = b'\r';

/// Read one frame into `buf` (reused across calls): split on `LF` only, strip
/// it plus at most one trailing `CR`. `None` on EOF; a final unterminated line
/// is still delivered (pi flushes one on shutdown). This is the only correct
/// way to read the protocol — never a generic line reader, which would split
/// on Unicode separators that are valid inside JSON strings.
pub async fn read_frame<'buf, R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &'buf mut Vec<u8>,
) -> std::io::Result<Option<&'buf [u8]>> {
    buf.clear();

    let bytes_read = reader.read_until(LF, buf).await?;

    if bytes_read == 0 {
        return Ok(None);
    }

    let mut frame = buf.as_slice();

    if frame.last() == Some(&LF) {
        frame = &frame[..frame.len() - 1];
    }

    if frame.last() == Some(&CR) {
        frame = &frame[..frame.len() - 1];
    }

    Ok(Some(frame))
}

/// Serialize one value as a complete `LF`-terminated wire line.
pub fn encode_frame<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(value)?;
    frame.push(LF);

    Ok(frame)
}

/// A client → daemon request line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Daemon-native operation.
    Harvester {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        cmd: HarvesterCmd,
    },
    /// Verbatim pi RPC passthrough for one session.
    Pi {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        sid: String,
        cmd: Value,
    },
}

/// Daemon-native commands — only what pi cannot do itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarvesterCmd {
    /// First command on every connection; version handshake.
    Ping { version: String },
    /// List sessions from pi's session dirs + the daemon pid table.
    ListSessions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Spawn (or resume — same thing) a pi process for `sid` in `cwd`.
    CreateSession {
        sid: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Throwaway `pi --no-session` in `cwd` answering `get_available_models`.
    ListModels { cwd: String },
    /// Subscribe this connection to a session's event stream.
    Attach { sid: String },
    /// Unsubscribe.
    Detach { sid: String },
    /// Close pi's stdin; 30s grace, then SIGKILL. Session stays resumable.
    Stop { sid: String },
}

/// A daemon → client line: response to a request, or an unsolicited event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    HarvesterResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        response: HarvesterResponse,
    },
    /// Pi's response object, verbatim. `null` for fire-and-forget commands.
    PiResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        sid: String,
        response: Value,
    },
    /// One pi stdout line, verbatim.
    Event { sid: String, event: Value },
    /// The daemon dropped `skipped` events for this connection because it was
    /// reading too slowly. Daemon-originated, never a fabricated pi event: the
    /// client has an authoritative recovery (passthrough `get_entries` from its
    /// last seen entry), but only if it is told the gap happened.
    EventGap { sid: String, skipped: u64 },
    /// The pi process for `sid` exited. Cause lives in its stderr log (SSH tail).
    SessionExit { sid: String, exit_code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvesterResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HarvesterResponse {
    pub fn ok(data: Value) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message.into()),
        }
    }

    /// `data` on success (`Null` when absent), the error string otherwise.
    pub fn into_result(self) -> Result<Value, String> {
        if self.success {
            Ok(self.data.unwrap_or(Value::Null))
        } else {
            Err(self.error.unwrap_or_else(|| "unknown error".to_string()))
        }
    }
}

/// `ping`'s `data`. Typing the daemon-native payloads (not pi's — those stay
/// verbatim `Value`) means each wire field is modelled once instead of being
/// hand-built with `json!` on one side and hand-dug with `.get()` on the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingData {
    pub version: String,
}

/// `list_sessions`' `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsData {
    pub sessions: Vec<SessionInfo>,
}

/// One entry in `list_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub sid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    pub file: String,
    /// Milliseconds since epoch of the session file's last modification.
    pub mtime_ms: u64,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Exited,
}
