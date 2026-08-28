use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const PROTOCOL_VERSION: u32 = 1;

pub const DAEMON_SOCKET_PATH: &str = ".harvester/harvesterd.sock";

pub const LF: u8 = b'\n';
pub const CR: u8 = b'\r';

pub const FRAME_BUFFER_START_BYTES: usize = 64 * 1024;

const FINGERPRINT_CHUNK_BYTES: usize = 64 * 1024;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

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

pub fn fingerprint_bytes(bytes: &[u8]) -> String {
    fingerprint_reader(&mut &bytes[..]).expect("reading from a slice cannot fail")
}

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

pub fn encode_frame<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut frame = serde_json::to_vec(value)?;
    frame.push(LF);

    Ok(frame)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Harvester {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        cmd: HarvesterCmd,
    },
    Pi {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        sid: String,
        cmd: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarvesterCmd {
    Ping {
        version: u32,
    },
    ListSessions,
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
    ListModels {
        cwd: String,
    },
    Attach {
        sid: String,
    },
    Detach {
        sid: String,
    },
    Stop {
        sid: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    HarvesterResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        response: HarvesterResponse,
    },
    PiResponse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Value>,
        sid: String,
        #[serde(default)]
        response: Value,
    },
    Event {
        sid: String,
        event: Value,
    },
    EventGap {
        sid: String,
        skipped: u64,
    },
    SessionExit {
        sid: String,
        exit_code: Option<i32>,
    },
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

    pub fn into_result(self) -> Result<Value, String> {
        if self.success {
            Ok(self.data.unwrap_or(Value::Null))
        } else {
            Err(self.error.unwrap_or_else(|| "unknown error".to_string()))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingData {
    pub version: u32,
    pub build: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionData {
    pub sid: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachData {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsData {
    pub sessions: Vec<SessionInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub sid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
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

    async fn read_owned_frame<R: AsyncBufRead + Unpin>(
        reader: &mut R,
        buf: &mut Vec<u8>,
    ) -> Option<Vec<u8>> {
        read_frame(reader, buf)
            .await
            .expect("reading from an in-memory slice cannot fail")
            .map(<[u8]>::to_vec)
    }

    async fn read_all_frames(input: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = BufReader::new(input);
        let mut buf = Vec::new();
        let mut frames = Vec::new();

        while let Some(frame) = read_owned_frame(&mut reader, &mut buf).await {
            frames.push(frame);
        }

        frames
    }

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

    #[tokio::test]
    async fn request_harvester_matches_the_documented_envelope() {
        let request = Request::Harvester {
            id: Some(json!(1)),
            cmd: HarvesterCmd::ListSessions,
        };

        let (parsed, _) = round_trip(&request).await;

        assert_eq!(
            parsed,
            json!({"id": 1, "type": "harvester", "cmd": {"type": "list_sessions"}})
        );
    }

    #[tokio::test]
    async fn request_pi_matches_the_documented_envelope() {
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
        let cases = vec![
            (
                HarvesterCmd::Ping { version: 1 },
                json!({"type": "ping", "version": 1}),
            ),
            (HarvesterCmd::ListSessions, json!({"type": "list_sessions"})),
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
    async fn list_sessions_ignores_a_stale_cwd_from_an_older_client() {
        let decoded: HarvesterCmd =
            serde_json::from_value(json!({"type": "list_sessions", "cwd": "/work/repo"}))
                .expect("a field the daemon no longer knows must not fail the whole request");

        assert!(matches!(decoded, HarvesterCmd::ListSessions));
    }

    #[tokio::test]
    async fn outgoing_harvester_response_matches_the_documented_envelope() {
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
        ];

        for (encoded, expected) in cases {
            assert_eq!(encoded, expected);
        }
    }

    #[tokio::test]
    async fn an_absent_already_decodes_as_false() {
        let created: CreateSessionData =
            serde_json::from_value(json!({"sid": "01H"})).expect("decodes");
        let attached: AttachData = serde_json::from_value(json!({})).expect("decodes");

        assert_eq!(created.sid, "01H");
        assert!(!created.already);
        assert!(!attached.already);
    }

    #[tokio::test]
    async fn sessions_data_matches_the_documented_response() {
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
        let mut bytes = vec![0_u8; FINGERPRINT_CHUNK_BYTES + 64];
        let unchanged = fingerprint_bytes(&bytes);
        bytes[FINGERPRINT_CHUNK_BYTES] = 1;

        assert_ne!(unchanged, fingerprint_bytes(&bytes));
    }

    #[test]
    fn a_truncated_binary_does_not_match_the_whole_one() {
        let bytes = vec![3_u8; 4096];

        assert_ne!(fingerprint_bytes(&bytes), fingerprint_bytes(&bytes[..2048]));
    }

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
