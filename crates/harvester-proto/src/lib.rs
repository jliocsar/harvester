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

/// Envelope compatibility, and nothing else: bumped **only** when the wire
/// shape changes in a way that makes an older peer wrong. It is what `ping`
/// exchanges and what a version-mismatch check tests.
///
/// Deliberately *not* the crate version (which it was, via
/// `env!("CARGO_PKG_VERSION")`, with one workspace version across all five
/// crates). That conflated two unrelated questions and failed in both
/// directions: rebuilding harvesterd without bumping the workspace left every
/// bootstrapped machine on the stale binary forever (the probe saw a match and
/// skipped the upload), and a UI-only bump force-upgraded every daemon —
/// killing every running pi child — for a change the wire never saw.
///
/// "Is the daemon out there the binary I ship?" is the *other* question, and it
/// is answered by the build fingerprint in [`PingData::build`], not here.
pub const PROTOCOL_VERSION: u32 = 1;

/// The daemon's socket, relative to `$HOME`. The rendezvous point between the
/// two binaries, so it is spelled once here rather than in each of them: drift
/// would surface only as an unexplained connect timeout.
pub const DAEMON_SOCKET_PATH: &str = ".harvester/harvesterd.sock";

/// The only record delimiter in the protocol (and in pi's RPC framing).
pub const LF: u8 = b'\n';
/// Tolerated-and-stripped before `LF`; never emitted.
pub const CR: u8 = b'\r';

/// Starting size of a reused frame buffer — every read and write side of this
/// framing: a connection's writer, a session's stdout pump, the throwaway
/// `list_models` pi. The buffer grows to the largest line it ever sees, so this
/// only decides how often it has to; 64 KiB clears pi's `message_update`
/// deltas, which re-send a whole partial message and are the largest routine
/// line on the wire.
pub const FRAME_BUFFER_START_BYTES: usize = 64 * 1024;

/// Bytes hashed per read by [`fingerprint_reader`]. A tunable, so it gets a
/// name; it cannot change the result (see the chunk-independence test).
const FINGERPRINT_CHUNK_BYTES: usize = 64 * 1024;

/// FNV-1a, 64-bit — the reference constants.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The build fingerprint of a harvesterd binary: `fnv1a64-<byte count>-<hash>`,
/// FNV-1a-64 over the file's bytes with the hash in lowercase 16-digit hex.
///
/// **This lives in the protocol crate because the string is wire-visible.** It
/// travels in [`PingData::build`], and the client computes the identical value
/// over the binary it ships in order to decide "install iff mine differs from
/// yours" (docs/DAEMON.md §Bootstrap). Two implementations of a value that must
/// compare equal across a network is a drift bug with a vicious failure mode:
/// every machine either never upgrades or upgrades on every connect. So there
/// is exactly one implementation, here, next to the framing it is peer to.
///
/// The byte count is free to carry and makes two deploys comparable by eye; the
/// `fnv1a64` tag names the algorithm so a future change to it is visible in the
/// value instead of silent.
///
/// **Not a security boundary.** Non-cryptographic on purpose — a static musl
/// binary shipped over SSH on every deploy earns no crypto dependency for a
/// staleness check, and anything that can answer on the socket at all can
/// answer with any string it likes. It detects the deploy failure that actually
/// happens (a rebuild that never reached the machine), not a hostile one.
pub fn fingerprint_reader<R: std::io::Read>(reader: &mut R) -> std::io::Result<String> {
    let mut buf = vec![0_u8; FINGERPRINT_CHUNK_BYTES];
    let mut hash = FNV_OFFSET_BASIS;
    let mut byte_count: u64 = 0;

    loop {
        let filled = reader.read(&mut buf)?;

        if filled == 0 {
            break;
        }

        byte_count += filled as u64;

        for byte in &buf[..filled] {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME);
        }
    }

    Ok(format!("fnv1a64-{byte_count}-{hash:016x}"))
}

/// [`fingerprint_reader`] for a binary already in memory — which is how the
/// client holds the daemon it ships (`bootstrap_and_connect`'s `daemon_binary`).
/// Infallible: reading from a slice cannot fail.
pub fn fingerprint_bytes(bytes: &[u8]) -> String {
    fingerprint_reader(&mut &bytes[..]).expect("reading from a slice cannot fail")
}

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
    /// First command on every connection; protocol-version handshake. The
    /// client sends its [`PROTOCOL_VERSION`]; the daemon answers with its own.
    Ping { version: u32 },
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    HarvesterResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        response: HarvesterResponse,
    },
    /// Pi's response object, verbatim. `null` for fire-and-forget commands —
    /// and `default` because serde cannot tell an explicit null from an absent
    /// key, while both mean the same thing here.
    PiResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        sid: String,
        #[serde(default)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// The daemon's [`PROTOCOL_VERSION`]. Compare for envelope compatibility.
    pub version: u32,
    /// A fingerprint of the **running daemon's own executable image**, computed
    /// by harvesterd at startup. This is the answer to "is the daemon running
    /// over there the binary I ship?", which the protocol version cannot give:
    /// two builds of the same protocol are interchangeable on the wire and
    /// completely different code.
    ///
    /// Opaque to everyone but the producer: a client compares it for equality
    /// against the fingerprint of the binary it is about to install (obtained
    /// from the same binary via `harvesterd build`) and installs iff they
    /// differ. It is a staleness check, **not a security boundary** — the hash
    /// is non-cryptographic and trivially forgeable by anything that can answer
    /// on the socket at all.
    pub build: String,
}

/// `create_session`'s `data` (docs/DAEMON.md §create_session).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionData {
    pub sid: String,
    /// The sid was already in the pid table, so no second pi was spawned —
    /// which is what keeps "one pi process = one session file" true. Not an
    /// error: it is what lets a client open any session with an unconditional
    /// `create_session` → `attach` chain.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
}

/// `attach`'s `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachData {
    /// This connection was already attached to *this generation* of the session
    /// object. A session that exited and respawned is a new generation, so the
    /// attach is real even for a sid the client believes it holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
}

/// `detach`'s `data` — deliberately empty: the acknowledgement is the whole
/// payload. A type rather than a bare `{}` so the operation has somewhere to
/// grow, and so no call site hand-builds an object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachData {}

/// `stop`'s `data`. Always true, and it means "shutdown initiated" — never
/// "done": `session_exit` is the authoritative completion signal and may arrive
/// before or after this response (docs/DAEMON.md §stop).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopData {
    pub stopping: bool,
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
    /// The session file on the machine, absent while pi has not written one.
    /// Pi creates it lazily on the first entry, so a session spawned but never
    /// prompted is genuinely fileless — and a client that fabricates a blank
    /// path here is claiming a file that does not exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Milliseconds since epoch of the session file's last modification, or of
    /// the spawn for a session that has no file yet. Either way: last activity.
    pub mtime_ms: u64,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Exited,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    /// `read_frame` borrows from the caller's buffer, so tests that need to keep
    /// a frame around (or read the next one) copy it out first.
    async fn read_owned_frame<R: AsyncBufRead + Unpin>(
        reader: &mut R,
        buf: &mut Vec<u8>,
    ) -> Option<Vec<u8>> {
        read_frame(reader, buf)
            .await
            .expect("reading from an in-memory slice cannot fail")
            .map(<[u8]>::to_vec)
    }

    /// Read every frame a byte slice yields, to EOF.
    async fn read_all_frames(input: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = BufReader::new(input);
        let mut buf = Vec::new();
        let mut frames = Vec::new();

        while let Some(frame) = read_owned_frame(&mut reader, &mut buf).await {
            frames.push(frame);
        }

        frames
    }

    /// Encode a value, read it back as one frame, and hand back the parsed JSON
    /// plus the raw wire bytes — the shape every envelope round-trip asserts on.
    async fn round_trip<T: Serialize>(value: &T) -> (Value, Vec<u8>) {
        let encoded = encode_frame(value).expect("envelope types are always serializable");
        let mut reader = BufReader::new(encoded.as_slice());
        let mut buf = Vec::new();

        let frame = read_owned_frame(&mut reader, &mut buf)
            .await
            .expect("an encoded frame is never EOF");
        let parsed = serde_json::from_slice(&frame).expect("a frame round-trips as JSON");

        (parsed, encoded)
    }

    // --- read_frame: the framing invariant ---------------------------------

    #[tokio::test]
    async fn read_frame_strips_the_lf_terminator() {
        let frames = read_all_frames(b"hello\n").await;

        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[tokio::test]
    async fn read_frame_strips_exactly_one_cr_before_the_lf() {
        let frames = read_all_frames(b"hello\r\n").await;

        assert_eq!(frames, vec![b"hello".to_vec()]);
    }

    #[tokio::test]
    async fn read_frame_keeps_a_second_cr_as_payload_data() {
        let frames = read_all_frames(b"hello\r\r\n").await;

        assert_eq!(frames, vec![b"hello\r".to_vec()]);
    }

    #[tokio::test]
    async fn read_frame_delivers_a_final_unterminated_line() {
        // Pi flushes one of these on shutdown; dropping it would lose the last
        // response of every session.
        let frames = read_all_frames(b"first\nlast-without-lf").await;

        assert_eq!(frames, vec![b"first".to_vec(), b"last-without-lf".to_vec()]);
    }

    #[tokio::test]
    async fn read_frame_returns_none_at_eof() {
        let mut reader = BufReader::new(b"".as_slice());
        let mut buf = Vec::new();

        let frame = read_frame(&mut reader, &mut buf)
            .await
            .expect("no io error");

        assert!(frame.is_none());
    }

    #[tokio::test]
    async fn read_frame_distinguishes_an_empty_line_from_eof() {
        let mut reader = BufReader::new(b"\n".as_slice());
        let mut buf = Vec::new();

        let first = read_owned_frame(&mut reader, &mut buf).await;
        let second = read_owned_frame(&mut reader, &mut buf).await;

        assert_eq!(first, Some(Vec::new()), "an empty line is an empty frame");
        assert_eq!(second, None, "only EOF is None");
    }

    #[tokio::test]
    async fn read_frame_reads_many_frames_through_one_reused_buffer() {
        // The buffer-reuse contract from the doc comment: `buf` is cleared per
        // call, so frame N never leaks bytes into frame N+1.
        let frames = read_all_frames(b"aaa\nbb\r\nc\n\nlonger-than-the-rest\n").await;

        assert_eq!(
            frames,
            vec![
                b"aaa".to_vec(),
                b"bb".to_vec(),
                b"c".to_vec(),
                Vec::new(),
                b"longer-than-the-rest".to_vec(),
            ]
        );
    }

    #[tokio::test]
    async fn read_frame_never_splits_on_unicode_line_separators() {
        // The exact bug the doc comment warns about: U+2028 / U+2029 are line
        // breaks to a generic Unicode line reader but ordinary characters
        // inside a JSON string, which serde_json emits unescaped.
        let payload = json!({
            "id": 1,
            "type": "pi",
            "sid": "01H",
            "cmd": {"type": "prompt", "message": "line one\u{2028}line two\u{2029}line three"},
        });
        let encoded = encode_frame(&payload).expect("json value serializes");

        assert!(
            encoded.windows(3).any(|w| w == [0xE2, 0x80, 0xA8]),
            "the separator must reach the wire unescaped or the test proves nothing"
        );
        assert!(
            encoded.windows(3).any(|w| w == [0xE2, 0x80, 0xA9]),
            "same for U+2029"
        );

        let frames = read_all_frames(&encoded).await;

        assert_eq!(frames.len(), 1, "the payload must arrive as ONE frame");
        assert_eq!(
            serde_json::from_slice::<Value>(&frames[0]).expect("frame is valid json"),
            payload
        );
    }

    // --- encode_frame: line discipline -------------------------------------

    #[tokio::test]
    async fn encode_frame_terminates_with_exactly_one_lf_and_no_interior_lf() {
        let encoded = encode_frame(&json!({
            "type": "event",
            "sid": "01H",
            "event": {"text": "an embedded\nnewline and a \r carriage return"},
        }))
        .expect("json value serializes");

        assert_eq!(encoded.last(), Some(&LF));
        assert_eq!(
            encoded.iter().filter(|byte| **byte == LF).count(),
            1,
            "serde_json escapes payload newlines, so the terminator is the only LF"
        );
    }

    // --- envelopes: wire shape against docs/DAEMON.md ----------------------

    #[tokio::test]
    async fn request_harvester_matches_the_documented_envelope() {
        // docs/DAEMON.md §Envelope:
        // {"id": 1, "type": "harvester", "cmd": {"type": "list_sessions"}}
        let request = Request::Harvester {
            id: Some(json!(1)),
            cmd: HarvesterCmd::ListSessions { cwd: None },
        };

        let (parsed, _) = round_trip(&request).await;

        assert_eq!(
            parsed,
            json!({"id": 1, "type": "harvester", "cmd": {"type": "list_sessions"}})
        );
    }

    #[tokio::test]
    async fn request_pi_matches_the_documented_envelope() {
        // docs/DAEMON.md §Envelope:
        // {"id": 2, "type": "pi", "sid": "01H...", "cmd": {"type": "prompt", ...}}
        let request = Request::Pi {
            id: Some(json!(2)),
            sid: "01H".to_string(),
            cmd: json!({"type": "prompt", "message": "fix the tests"}),
        };

        let (parsed, _) = round_trip(&request).await;

        assert_eq!(
            parsed,
            json!({
                "id": 2,
                "type": "pi",
                "sid": "01H",
                "cmd": {"type": "prompt", "message": "fix the tests"},
            })
        );
    }

    #[tokio::test]
    async fn request_omits_a_none_id_entirely() {
        let request = Request::Harvester {
            id: None,
            cmd: HarvesterCmd::Detach {
                sid: "01H".to_string(),
            },
        };

        let (parsed, encoded) = round_trip(&request).await;

        assert!(
            parsed.get("id").is_none(),
            "skip_serializing_if must elide the key, not emit null"
        );
        assert!(!String::from_utf8(encoded).expect("utf8").contains("\"id\""));
    }

    #[tokio::test]
    async fn request_round_trips_back_into_the_typed_enum() {
        let request = Request::Harvester {
            id: Some(json!("abc")),
            cmd: HarvesterCmd::CreateSession {
                sid: "01H".to_string(),
                cwd: "/work/repo".to_string(),
                provider: Some("anthropic".to_string()),
                model: Some("sonnet".to_string()),
                name: Some("wip-a3f2".to_string()),
            },
        };
        let encoded = encode_frame(&request).expect("serializes");

        let mut reader = BufReader::new(encoded.as_slice());
        let mut buf = Vec::new();
        let frame = read_owned_frame(&mut reader, &mut buf)
            .await
            .expect("one frame");
        let decoded: Request = serde_json::from_slice(&frame).expect("decodes as a Request");

        match decoded {
            Request::Harvester { id, cmd } => {
                assert_eq!(id, Some(json!("abc")));

                match cmd {
                    HarvesterCmd::CreateSession {
                        sid,
                        cwd,
                        provider,
                        model,
                        name,
                    } => {
                        assert_eq!(sid, "01H");
                        assert_eq!(cwd, "/work/repo");
                        assert_eq!(provider.as_deref(), Some("anthropic"));
                        assert_eq!(model.as_deref(), Some("sonnet"));
                        assert_eq!(name.as_deref(), Some("wip-a3f2"));
                    }
                    other => panic!("expected create_session, got {other:?}"),
                }
            }
            other => panic!("expected a harvester request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn harvester_cmd_tags_are_snake_case() {
        // docs/DAEMON.md §Operations names every command; the tag is the name.
        let cases = vec![
            (
                HarvesterCmd::Ping { version: 1 },
                json!({"type": "ping", "version": 1}),
            ),
            (
                HarvesterCmd::ListSessions {
                    cwd: Some("/work/repo".to_string()),
                },
                json!({"type": "list_sessions", "cwd": "/work/repo"}),
            ),
            (
                HarvesterCmd::CreateSession {
                    sid: "01H".to_string(),
                    cwd: "/work/repo".to_string(),
                    provider: None,
                    model: None,
                    name: None,
                },
                json!({"type": "create_session", "sid": "01H", "cwd": "/work/repo"}),
            ),
            (
                HarvesterCmd::ListModels {
                    cwd: "/work/repo".to_string(),
                },
                json!({"type": "list_models", "cwd": "/work/repo"}),
            ),
            (
                HarvesterCmd::Attach {
                    sid: "01H".to_string(),
                },
                json!({"type": "attach", "sid": "01H"}),
            ),
            (
                HarvesterCmd::Detach {
                    sid: "01H".to_string(),
                },
                json!({"type": "detach", "sid": "01H"}),
            ),
            (
                HarvesterCmd::Stop {
                    sid: "01H".to_string(),
                },
                json!({"type": "stop", "sid": "01H"}),
            ),
        ];

        for (cmd, expected) in cases {
            let (parsed, _) = round_trip(&cmd).await;

            assert_eq!(parsed, expected);
        }
    }

    #[tokio::test]
    async fn outgoing_harvester_response_matches_the_documented_envelope() {
        // docs/DAEMON.md §Envelope:
        // {"id": 1, "type": "harvester_response", "response": {"success": true, "data": {...}}}
        let outgoing = Outgoing::HarvesterResponse {
            id: Some(json!(1)),
            response: HarvesterResponse::ok(json!({"sessions": []})),
        };

        let (parsed, _) = round_trip(&outgoing).await;

        assert_eq!(
            parsed,
            json!({
                "id": 1,
                "type": "harvester_response",
                "response": {"success": true, "data": {"sessions": []}},
            })
        );
    }

    #[tokio::test]
    async fn outgoing_pi_response_matches_the_documented_envelope() {
        // docs/DAEMON.md §Envelope:
        // {"id": 2, "type": "pi_response", "sid": "01H...", "response": {...}}
        let outgoing = Outgoing::PiResponse {
            id: Some(json!(2)),
            sid: "01H".to_string(),
            response: json!({"type": "response", "command": "prompt", "success": true}),
        };

        let (parsed, _) = round_trip(&outgoing).await;

        assert_eq!(
            parsed,
            json!({
                "id": 2,
                "type": "pi_response",
                "sid": "01H",
                "response": {"type": "response", "command": "prompt", "success": true},
            })
        );
    }

    #[tokio::test]
    async fn outgoing_pi_response_carries_null_for_fire_and_forget_commands() {
        // docs/DAEMON.md §The pi passthrough: `extension_ui_response` answers
        // with `response: null`, which must survive as an explicit null.
        let outgoing = Outgoing::PiResponse {
            id: None,
            sid: "01H".to_string(),
            response: Value::Null,
        };

        let (parsed, _) = round_trip(&outgoing).await;

        assert_eq!(
            parsed,
            json!({"type": "pi_response", "sid": "01H", "response": null})
        );
    }

    #[tokio::test]
    async fn outgoing_events_match_the_documented_envelopes() {
        // docs/DAEMON.md §Events.
        let cases = vec![
            (
                Outgoing::Event {
                    sid: "01H".to_string(),
                    event: json!({"type": "agent_settled"}),
                },
                json!({"type": "event", "sid": "01H", "event": {"type": "agent_settled"}}),
            ),
            (
                Outgoing::SessionExit {
                    sid: "01H".to_string(),
                    exit_code: Some(0),
                },
                json!({"type": "session_exit", "sid": "01H", "exit_code": 0}),
            ),
            (
                Outgoing::EventGap {
                    sid: "01H".to_string(),
                    skipped: 12,
                },
                json!({"type": "event_gap", "sid": "01H", "skipped": 12}),
            ),
        ];

        for (outgoing, expected) in cases {
            let (parsed, _) = round_trip(&outgoing).await;

            assert_eq!(parsed, expected);
        }
    }

    #[tokio::test]
    async fn session_exit_keeps_a_null_exit_code_for_signal_kills() {
        // docs/DAEMON.md §Events: `exit_code` is null when pi was killed by a
        // signal. The key must be present-and-null, not elided.
        let outgoing = Outgoing::SessionExit {
            sid: "01H".to_string(),
            exit_code: None,
        };

        let (parsed, _) = round_trip(&outgoing).await;

        assert_eq!(
            parsed,
            json!({"type": "session_exit", "sid": "01H", "exit_code": null})
        );
    }

    #[tokio::test]
    async fn ping_data_matches_the_documented_response() {
        // docs/DAEMON.md §ping:
        // {"id": 1, "type": "harvester_response",
        //  "response": {"success": true, "data": {"version": 1, "build": "fnv1a64-…"}}}
        let outgoing = Outgoing::HarvesterResponse {
            id: Some(json!(1)),
            response: HarvesterResponse::ok(
                serde_json::to_value(PingData {
                    version: PROTOCOL_VERSION,
                    build: "fnv1a64-852768-1f0c3a7b9d2e4c55".to_string(),
                })
                .expect("PingData serializes"),
            ),
        };

        let (parsed, _) = round_trip(&outgoing).await;

        assert_eq!(
            parsed,
            json!({
                "id": 1,
                "type": "harvester_response",
                "response": {"success": true, "data": {
                    "version": 1,
                    "build": "fnv1a64-852768-1f0c3a7b9d2e4c55",
                }},
            })
        );
    }

    #[tokio::test]
    async fn ping_data_round_trips_back_into_the_typed_struct() {
        // The version is a *number* on the wire, not a version string: a client
        // that still sends `"1"` must fail loudly here rather than compare
        // strings that happen to look right.
        let decoded: PingData =
            serde_json::from_value(json!({"version": 2, "build": "fnv1a64-1-0"}))
                .expect("PingData decodes");

        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.build, "fnv1a64-1-0");

        assert!(
            serde_json::from_value::<PingData>(json!({"version": "1", "build": "x"})).is_err(),
            "a string version must not decode as a protocol version"
        );
    }

    /// These four replace hand-built `json!` objects on the daemon side, so
    /// the bytes they emit must not move: a client that reads `already` or
    /// `stopping` is talking to a daemon that may be older or newer than it.
    #[tokio::test]
    async fn the_operation_payloads_match_the_bytes_they_replace() {
        let cases = vec![
            (
                serde_json::to_value(CreateSessionData {
                    sid: "01H".to_string(),
                    already: false,
                })
                .expect("serializes"),
                json!({"sid": "01H"}),
            ),
            (
                serde_json::to_value(CreateSessionData {
                    sid: "01H".to_string(),
                    already: true,
                })
                .expect("serializes"),
                json!({"sid": "01H", "already": true}),
            ),
            (
                serde_json::to_value(AttachData { already: false }).expect("serializes"),
                json!({}),
            ),
            (
                serde_json::to_value(AttachData { already: true }).expect("serializes"),
                json!({"already": true}),
            ),
            (
                serde_json::to_value(DetachData {}).expect("serializes"),
                json!({}),
            ),
            (
                serde_json::to_value(StopData { stopping: true }).expect("serializes"),
                json!({"stopping": true}),
            ),
        ];

        for (encoded, expected) in cases {
            assert_eq!(encoded, expected);
        }
    }

    #[tokio::test]
    async fn an_absent_already_decodes_as_false() {
        // The elided key and an explicit `false` must mean the same thing, or a
        // client reading a field the daemon skipped would fail the whole
        // response instead of seeing "no, this was a fresh spawn".
        let created: CreateSessionData =
            serde_json::from_value(json!({"sid": "01H"})).expect("decodes");
        let attached: AttachData = serde_json::from_value(json!({})).expect("decodes");

        assert_eq!(created.sid, "01H");
        assert!(!created.already);
        assert!(!attached.already);
    }

    #[tokio::test]
    async fn sessions_data_matches_the_documented_response() {
        // docs/DAEMON.md §list_sessions, literal example.
        let sessions = SessionsData {
            sessions: vec![SessionInfo {
                sid: "01H".to_string(),
                name: Some("Refactor auth".to_string()),
                cwd: "/work/repo".to_string(),
                file: Some(
                    "/home/u/.pi/agent/sessions/--work-repo--/2026-..._01H....jsonl".to_string(),
                ),
                mtime_ms: 1754049600000,
                status: SessionStatus::Running,
            }],
        };

        let (parsed, _) = round_trip(&sessions).await;

        assert_eq!(
            parsed,
            json!({"sessions": [{
                "sid": "01H",
                "name": "Refactor auth",
                "cwd": "/work/repo",
                "file": "/home/u/.pi/agent/sessions/--work-repo--/2026-..._01H....jsonl",
                "mtime_ms": 1754049600000u64,
                "status": "running",
            }]})
        );
    }

    #[tokio::test]
    async fn session_info_omits_an_absent_name() {
        let info = SessionInfo {
            sid: "01H".to_string(),
            name: None,
            cwd: "/work/repo".to_string(),
            file: Some("/tmp/s.jsonl".to_string()),
            mtime_ms: 0,
            status: SessionStatus::Exited,
        };

        let (parsed, _) = round_trip(&info).await;

        assert!(parsed.get("name").is_none());
        assert_eq!(parsed["status"], json!("exited"));
    }

    #[tokio::test]
    async fn session_status_round_trips_as_snake_case() {
        for (status, tag) in [
            (SessionStatus::Running, "running"),
            (SessionStatus::Exited, "exited"),
        ] {
            let (parsed, _) = round_trip(&status).await;

            assert_eq!(parsed, json!(tag));
            assert_eq!(
                serde_json::from_value::<SessionStatus>(parsed).expect("decodes"),
                status
            );
        }
    }

    #[tokio::test]
    async fn harvester_response_error_omits_data() {
        let response = HarvesterResponse::err("no such session");

        let (parsed, _) = round_trip(&response).await;

        assert_eq!(
            parsed,
            json!({"success": false, "error": "no such session"})
        );
    }

    // --- the build fingerprint: a value two crates must agree on -----------

    /// The published FNV-1a-64 test vectors. These pin the exact output of a
    /// function the daemon and the client compute independently and compare for
    /// equality — an unpinned rewrite would silently make every machine either
    /// never upgrade or upgrade on every connect.
    #[test]
    fn the_fingerprint_matches_the_reference_fnv1a64_vectors() {
        for (input, expected_hash) in [
            ("", 0xcbf2_9ce4_8422_2325_u64),
            ("a", 0xaf63_dc4c_8601_ec8c),
            ("foobar", 0x8594_4171_f739_67e8),
        ] {
            assert_eq!(
                fingerprint_bytes(input.as_bytes()),
                format!("fnv1a64-{}-{expected_hash:016x}", input.len()),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn the_fingerprint_is_stable_across_calls() {
        let bytes = vec![7_u8; FINGERPRINT_CHUNK_BYTES * 3 + 11];

        assert_eq!(fingerprint_bytes(&bytes), fingerprint_bytes(&bytes));
    }

    #[test]
    fn chunking_does_not_change_the_fingerprint() {
        // The chunk loop is the only place a rewrite could corrupt state; an
        // independent single-pass fold over the same bytes is the check. It
        // also proves the reader and slice entry points agree.
        let bytes: Vec<u8> = (0..FINGERPRINT_CHUNK_BYTES * 2 + 3)
            .map(|index| (index % 251) as u8)
            .collect();
        let single_pass = bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
        });

        let from_reader = fingerprint_reader(&mut bytes.as_slice()).expect("no io error");

        assert_eq!(
            from_reader,
            format!("fnv1a64-{}-{single_pass:016x}", bytes.len())
        );
        assert_eq!(from_reader, fingerprint_bytes(&bytes));
    }

    #[test]
    fn one_flipped_byte_changes_the_fingerprint() {
        // The whole job: a rebuilt binary of the same size must not compare
        // equal to the old one.
        let mut bytes = vec![0_u8; FINGERPRINT_CHUNK_BYTES + 64];
        let unchanged = fingerprint_bytes(&bytes);
        bytes[FINGERPRINT_CHUNK_BYTES] = 1;

        assert_ne!(unchanged, fingerprint_bytes(&bytes));
    }

    #[test]
    fn a_truncated_binary_does_not_match_the_whole_one() {
        // The failure mode the client's install guard exists for.
        let bytes = vec![3_u8; 4096];

        assert_ne!(fingerprint_bytes(&bytes), fingerprint_bytes(&bytes[..2048]));
    }

    // --- decoding a daemon line --------------------------------------------

    fn decode(line: &[u8]) -> serde_json::Result<Outgoing> {
        serde_json::from_slice(line)
    }

    #[tokio::test]
    async fn every_outgoing_variant_round_trips() {
        for sample in [
            Outgoing::HarvesterResponse {
                id: Some(json!(1)),
                response: HarvesterResponse::ok(json!({"sessions": []})),
            },
            Outgoing::PiResponse {
                id: Some(json!(2)),
                sid: "01H".to_string(),
                response: json!({"type": "response", "command": "prompt", "success": true}),
            },
            Outgoing::PiResponse {
                // Fire-and-forget pi commands answer with an explicit null,
                // which the client distinguishes from an error.
                id: None,
                sid: "01H".to_string(),
                response: Value::Null,
            },
            Outgoing::Event {
                sid: "01H".to_string(),
                event: json!({"type": "message_update", "message": {"role": "assistant"}}),
            },
            Outgoing::EventGap {
                sid: "01H".to_string(),
                skipped: 12,
            },
            Outgoing::SessionExit {
                sid: "01H".to_string(),
                exit_code: Some(0),
            },
            // Signal-killed: present-and-null, not elided.
            Outgoing::SessionExit {
                sid: "01H".to_string(),
                exit_code: None,
            },
        ] {
            let (_, encoded) = round_trip(&sample).await;

            assert_eq!(
                decode(&encoded[..encoded.len() - 1]).expect("a line serde produced must decode"),
                sample
            );
        }
    }

    #[test]
    fn a_pi_payload_reaches_the_client_verbatim() {
        // The invariant the envelope exists to serve: pi's event is handed
        // over untouched, not normalized on the way through.
        let event = json!({
            "type": "tool_execution_end",
            "toolCallId": "call_1",
            "details": {"diff": " 1 ctx\n-2 old\n+2 new", "nested": [1, 2, {"deep": true}]},
        });
        let line = format!(
            r#"{{"type":"event","sid":"01H","event":{event}}}"#,
            event = serde_json::to_string(&event).expect("serializes")
        );

        let Ok(Outgoing::Event { event: decoded, .. }) = decode(line.as_bytes()) else {
            panic!("expected an event");
        };

        assert_eq!(decoded, event);
    }

    #[test]
    fn a_line_missing_a_required_field_is_an_error_not_a_default() {
        // Leniency here would turn a daemon bug into a session that renders
        // wrong, which is far harder to diagnose than a failed read.
        for line in [
            r#"{"type":"event","sid":"01H"}"#,
            r#"{"type":"event","event":{"type":"agent_settled"}}"#,
            r#"{"type":"event_gap","sid":"01H"}"#,
            r#"{"type":"harvester_response","id":1}"#,
            r#"{"type":"telepathy","sid":"01H"}"#,
        ] {
            assert!(decode(line.as_bytes()).is_err(), "must reject: {line}");
        }
    }

    // --- HarvesterResponse::into_result ------------------------------------

    #[test]
    fn into_result_yields_the_data_on_success() {
        let response = HarvesterResponse::ok(json!({"stopping": true}));

        assert_eq!(response.into_result(), Ok(json!({"stopping": true})));
    }

    #[test]
    fn into_result_yields_null_when_a_success_carries_no_data() {
        let response = HarvesterResponse {
            success: true,
            data: None,
            error: None,
        };

        assert_eq!(response.into_result(), Ok(Value::Null));
    }

    #[test]
    fn into_result_yields_the_error_message_on_failure() {
        let response = HarvesterResponse::err("no such session");

        assert_eq!(response.into_result(), Err("no such session".to_string()));
    }

    #[test]
    fn into_result_falls_back_when_a_failure_carries_no_message() {
        let response = HarvesterResponse {
            success: false,
            data: None,
            error: None,
        };

        assert_eq!(response.into_result(), Err("unknown error".to_string()));
    }
}
