use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::time::UNIX_EPOCH;

use harvester_proto::{SessionInfo, SessionStatus, LF};
use serde_json::Value;

use crate::daemon::RunningSession;
use crate::paths;
use crate::sid::Sid;

const SESSION_INFO_MARKER: &[u8] = br#""session_info""#;

pub fn list_sessions(running: &HashMap<Sid, RunningSession>) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    let mut line_buffer = Vec::new();

    let Ok(dirs) = std::fs::read_dir(paths::pi_sessions_root()) else {
        return sessions;
    };

    for dir in dirs.flatten() {
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };

        for file in files.flatten() {
            let path = file.path();

            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let mtime_ms = entry_mtime_ms(&file);

            if let Some(info) = read_session_file(&path, mtime_ms, running, &mut line_buffer) {
                sessions.push(info);
            }
        }
    }

    append_fileless_running(&mut sessions, running);

    sessions.sort_by_key(|session| std::cmp::Reverse(session.mtime_ms));

    sessions
}

fn append_fileless_running(
    sessions: &mut Vec<SessionInfo>,
    running: &HashMap<Sid, RunningSession>,
) {
    for (sid, session) in running {
        let on_disk = sessions.iter().any(|found| found.sid == sid.as_str());

        if on_disk {
            continue;
        }

        sessions.push(SessionInfo {
            sid: sid.as_str().to_string(),
            name: None,
            cwd: session.cwd.clone(),
            file: None,
            mtime_ms: session.spawned_at_ms,
            status: SessionStatus::Running,
        });
    }
}

fn entry_mtime_ms(entry: &std::fs::DirEntry) -> u64 {
    entry
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn read_session_file(
    path: &Path,
    mtime_ms: u64,
    running: &HashMap<Sid, RunningSession>,
    line_buffer: &mut Vec<u8>,
) -> Option<SessionInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);

    line_buffer.clear();
    reader.read_until(LF, line_buffer).ok()?;
    let header: Value = serde_json::from_slice(line_buffer).ok()?;

    if header.get("type")?.as_str()? != "session" {
        return None;
    }

    let sid = header.get("id")?.as_str()?.to_owned();
    let cwd = header.get("cwd")?.as_str()?.to_owned();

    let name = scan_for_last_name(&mut reader, path, line_buffer);

    let status = if running.contains_key(sid.as_str()) {
        SessionStatus::Running
    } else {
        SessionStatus::Exited
    };

    Some(SessionInfo {
        sid,
        name,
        cwd,
        file: Some(path.display().to_string()),
        mtime_ms,
        status,
    })
}

fn scan_for_last_name(
    reader: &mut impl BufRead,
    path: &Path,
    line_buffer: &mut Vec<u8>,
) -> Option<String> {
    let finder = memchr::memmem::Finder::new(SESSION_INFO_MARKER);
    let mut name = None;

    loop {
        line_buffer.clear();

        match reader.read_until(LF, line_buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!(
                    "list_sessions: {} became unreadable: {error}",
                    path.display()
                );

                break;
            }
        }

        if finder.find(line_buffer).is_none() {
            continue;
        }

        if let Ok(entry) = serde_json::from_slice::<Value>(line_buffer) {
            if entry.get("type").and_then(Value::as_str) == Some("session_info") {
                name = entry_display_name(&entry);
            }
        }
    }

    name
}

fn entry_display_name(entry: &Value) -> Option<String> {
    let name = entry.get("name")?.as_str()?.trim();

    if name.is_empty() {
        return None;
    }

    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    const FIXTURE_MTIME_MS: u64 = 1_700_000_000_123;
    const FIXTURE_SID: &str = "01JABCDEF0123456789ABCDEFG";
    const FIXTURE_CWD: &str = "/home/exedev/project";

    fn jsonl(lines: &[Value]) -> String {
        let mut out = String::new();

        for line in lines {
            out.push_str(&serde_json::to_string(line).expect("fixture value must serialize"));
            out.push('\n');
        }

        out
    }

    fn session_header(sid: &str, cwd: &str) -> Value {
        json!({ "type": "session", "version": 3, "id": sid, "cwd": cwd })
    }

    fn session_info_entry(name: &str) -> Value {
        json!({ "type": "session_info", "name": name })
    }

    fn read_fixture_bytes(
        dir: &tempfile::TempDir,
        contents: &[u8],
        running: &HashMap<Sid, RunningSession>,
    ) -> Option<SessionInfo> {
        let path = dir.path().join("fixture.jsonl");
        let mut line_buffer = Vec::new();

        std::fs::write(&path, contents).expect("fixture file must be writable");

        read_session_file(&path, FIXTURE_MTIME_MS, running, &mut line_buffer)
    }

    fn read_fixture(
        dir: &tempfile::TempDir,
        contents: &str,
        running: &HashMap<Sid, RunningSession>,
    ) -> Option<SessionInfo> {
        read_fixture_bytes(dir, contents.as_bytes(), running)
    }

    fn nothing_running() -> HashMap<Sid, RunningSession> {
        HashMap::new()
    }

    fn running(raw: &str, cwd: &str, spawned_at_ms: u64) -> HashMap<Sid, RunningSession> {
        HashMap::from([(
            sid(raw),
            RunningSession {
                cwd: cwd.to_string(),
                spawned_at_ms,
            },
        )])
    }

    fn sid(raw: &str) -> Sid {
        Sid::parse(raw).expect("fixture sid must be valid")
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir must be creatable")
    }

    #[test]
    fn reads_header_and_session_info_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("first draft"),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.sid, FIXTURE_SID);
        assert_eq!(info.cwd, FIXTURE_CWD);
        assert_eq!(info.name.as_deref(), Some("first draft"));
        assert_eq!(info.mtime_ms, FIXTURE_MTIME_MS);
        assert_eq!(
            info.file.as_deref(),
            Some(dir.path().join("fixture.jsonl").display().to_string()).as_deref()
        );
        assert_eq!(info.status, SessionStatus::Exited);
    }

    #[test]
    fn last_session_info_entry_wins() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("first name"),
            session_info_entry("second name"),
            session_info_entry("third name"),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("third name"));
    }

    #[test]
    fn missing_session_info_leaves_name_unset() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            json!({ "type": "message", "role": "user", "text": "hello" }),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn session_info_without_name_field_leaves_name_unset() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            json!({ "type": "session_info" }),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn nameless_session_info_after_named_one_clears_the_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("named"),
            json!({ "type": "session_info" }),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn non_session_header_type_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[
            json!({ "type": "message", "id": FIXTURE_SID, "cwd": FIXTURE_CWD }),
            session_info_entry("ignored"),
        ]);

        assert!(read_fixture(&dir, &contents, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_id_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "type": "session", "version": 3, "cwd": FIXTURE_CWD })]);

        assert!(read_fixture(&dir, &contents, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_cwd_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "type": "session", "version": 3, "id": FIXTURE_SID })]);

        assert!(read_fixture(&dir, &contents, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_type_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "version": 3, "id": FIXTURE_SID, "cwd": FIXTURE_CWD })]);

        assert!(read_fixture(&dir, &contents, &nothing_running()).is_none());
    }

    #[test]
    fn unparseable_header_yields_nothing() {
        let dir = temp_dir();

        assert!(read_fixture(&dir, "this is not json at all\n", &nothing_running()).is_none());
    }

    #[test]
    fn empty_file_yields_nothing() {
        let dir = temp_dir();

        assert!(read_fixture(&dir, "", &nothing_running()).is_none());
    }

    #[test]
    fn running_sid_reports_running_status() {
        let dir = temp_dir();
        let contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);
        let running = running(FIXTURE_SID, FIXTURE_CWD, 1);

        let info = read_fixture(&dir, &contents, &running).expect("valid file must parse");

        assert_eq!(info.status, SessionStatus::Running);
    }

    #[test]
    fn absent_sid_reports_exited_status() {
        let dir = temp_dir();
        let contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);
        let running = running("some-other-sid", FIXTURE_CWD, 1);

        let info = read_fixture(&dir, &contents, &running).expect("valid file must parse");

        assert_eq!(info.status, SessionStatus::Exited);
    }

    #[test]
    fn garbage_line_does_not_abort_the_name_scan() {
        let dir = temp_dir();
        let mut contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("before garbage"),
        ]);

        contents.push_str(&"}{ not json ".repeat(4096));
        contents.push('\n');
        contents.push_str(&jsonl(&[session_info_entry("after garbage")]));

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("after garbage"));
    }

    #[test]
    fn line_merely_mentioning_session_info_is_not_treated_as_one() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("real name"),
            json!({
                "type": "message",
                "role": "user",
                "name": "impostor",
                "text": "what does the \"session_info\" entry do?",
            }),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("real name"));
    }

    #[test]
    fn invalid_utf8_line_does_not_truncate_the_name_scan() {
        let dir = temp_dir();
        let mut contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("before bad bytes"),
        ])
        .into_bytes();

        contents.extend_from_slice(&[0xFF, 0xFE, b'\n']);
        contents.extend_from_slice(jsonl(&[session_info_entry("after bad bytes")]).as_bytes());

        let info = read_fixture_bytes(&dir, &contents, &nothing_running())
            .expect("valid header must still parse");

        assert_eq!(info.name.as_deref(), Some("after bad bytes"));
    }

    #[test]
    fn an_empty_name_clears_the_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("named"),
            session_info_entry(""),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn a_whitespace_only_name_clears_the_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("named"),
            session_info_entry("   \t "),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn a_padded_name_is_trimmed() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("  padded name  "),
        ]);

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("padded name"));
    }

    #[test]
    fn a_final_line_without_a_trailing_newline_is_still_scanned() {
        let dir = temp_dir();
        let mut contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);

        contents.push_str(
            &serde_json::to_string(&session_info_entry("unterminated"))
                .expect("fixture value must serialize"),
        );

        let info =
            read_fixture(&dir, &contents, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("unterminated"));
    }

    #[test]
    fn a_running_session_with_no_file_yet_is_still_listed() {
        let running = running(FIXTURE_SID, FIXTURE_CWD, 4_242);
        let mut sessions = Vec::new();

        append_fileless_running(&mut sessions, &running);

        let listed = sessions.first().expect("the live pi must be listed");

        assert_eq!(listed.sid, FIXTURE_SID);
        assert_eq!(listed.cwd, FIXTURE_CWD);
        assert_eq!(listed.status, SessionStatus::Running);
        assert_eq!(
            listed.file, None,
            "claiming a file pi has not written is worse than admitting there is none"
        );
        assert_eq!(
            listed.mtime_ms, 4_242,
            "being spawned is the only activity it has had"
        );
    }

    #[test]
    fn a_running_session_already_found_on_disk_is_not_listed_twice() {
        let running = running(FIXTURE_SID, FIXTURE_CWD, 4_242);
        let mut sessions = vec![SessionInfo {
            sid: FIXTURE_SID.to_string(),
            name: Some("from disk".to_string()),
            cwd: FIXTURE_CWD.to_string(),
            file: Some("/tmp/on-disk.jsonl".to_string()),
            mtime_ms: 9_000,
            status: SessionStatus::Running,
        }];

        append_fileless_running(&mut sessions, &running);

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name.as_deref(), Some("from disk"));
    }
}
