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
                HarvesterCmd::Ping {
                    version: "0.1.0".to_string(),
                },
                json!({"type": "ping", "version": "0.1.0"}),
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
        // {"id": 1, "type": "harvester_response", "response": {"success": true, "data": {"version": "0.1.0"}}}
        let outgoing = Outgoing::HarvesterResponse {
            id: Some(json!(1)),
            response: HarvesterResponse::ok(
                serde_json::to_value(PingData {
                    version: "0.1.0".to_string(),
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
                "response": {"success": true, "data": {"version": "0.1.0"}},
            })
        );
    }

    #[tokio::test]
    async fn sessions_data_matches_the_documented_response() {
        // docs/DAEMON.md §list_sessions, literal example.
        let sessions = SessionsData {
            sessions: vec![SessionInfo {
                sid: "01H".to_string(),
                name: Some("Refactor auth".to_string()),
                cwd: "/work/repo".to_string(),
                file: "/home/u/.pi/agent/sessions/--work-repo--/2026-..._01H....jsonl".to_string(),
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
            file: "/tmp/s.jsonl".to_string(),
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
