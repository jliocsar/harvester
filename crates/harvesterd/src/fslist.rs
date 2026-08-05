//! `list_sessions`: readdir pi's session dirs. No daemon state involved —
//! the header line gives sid/cwd, the last `session_info` entry gives the name.
//!
//! Blocking std IO on purpose — the caller runs this on the blocking pool
//! (`spawn_blocking`); the daemon's single runtime thread never waits on disk.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;
use std::time::UNIX_EPOCH;

use harvester_proto::{SessionInfo, SessionStatus, LF};
use serde_json::Value;

use crate::paths;
use crate::sid::Sid;

/// The substring every `session_info` entry must contain. Matched on raw bytes
/// before anything is UTF-8-validated or parsed — the overwhelming majority of
/// lines in a conversation log are messages and never match.
const SESSION_INFO_MARKER: &[u8] = br#""session_info""#;

/// Scan all per-cwd dirs (optionally filtered to one cwd). `running` contains
/// the sids with a live pi child, provided by the caller.
pub fn list_sessions(cwd_filter: Option<&str>, running: &HashSet<Sid>) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    // Reused across files: line reads allocate once, not per line.
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

            if let Some(info) =
                read_session_file(&path, mtime_ms, cwd_filter, running, &mut line_buffer)
            {
                sessions.push(info);
            }
        }
    }

    // Newest first — the sidebar order.
    sessions.sort_by_key(|session| std::cmp::Reverse(session.mtime_ms));

    sessions
}

/// Milliseconds since epoch from the direntry's own metadata (no extra stat).
fn entry_mtime_ms(entry: &std::fs::DirEntry) -> u64 {
    entry
        .metadata()
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Stream the file line by line instead of slurping it — session files are
/// full conversation logs and can run to many MB; only the header and the
/// last `session_info` entry matter. A cwd-filter miss costs one line.
///
/// Read as **bytes** (`read_until(LF, …)`, pi's LF-only framing), never as
/// text: `read_line` would UTF-8-validate every byte of the whole log just to
/// find two lines, and its `Err(InvalidData)` on one non-UTF-8 byte is
/// indistinguishable from EOF here — which used to truncate the name scan
/// silently. Bytes make a corrupt line just a line that fails the prefilter.
fn read_session_file(
    path: &Path,
    mtime_ms: u64,
    cwd_filter: Option<&str>,
    running: &HashSet<Sid>,
    line_buffer: &mut Vec<u8>,
) -> Option<SessionInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);

    // Line 1: `{"type":"session","version":3,"id":...,"cwd":...}`
    line_buffer.clear();
    reader.read_until(LF, line_buffer).ok()?;
    let header: Value = serde_json::from_slice(line_buffer).ok()?;

    if header.get("type")?.as_str()? != "session" {
        return None;
    }

    let sid = header.get("id")?.as_str()?.to_owned();
    let cwd = header.get("cwd")?.as_str()?.to_owned();

    if let Some(filter) = cwd_filter {
        if cwd != filter {
            return None;
        }
    }

    let name = scan_for_last_name(&mut reader, path, line_buffer);

    let status = if running.contains(sid.as_str()) {
        SessionStatus::Running
    } else {
        SessionStatus::Exited
    };

    Some(SessionInfo {
        sid,
        name,
        cwd,
        file: path.display().to_string(),
        mtime_ms,
        status,
    })
}

/// The display name: the LAST `session_info` entry wins (rename history is
/// append-only). Every other line is rejected by a byte-level substring
/// prefilter, so only a candidate is ever UTF-8-validated and parsed.
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

/// Pi's own rule (`session-manager.ts:713-714`, "use latest, including explicit
/// clears"): the name is trimmed, and an empty or whitespace-only one is an
/// explicit *clear*, not a blank title.
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

    /// Arbitrary but stable mtime for fixtures — asserted as a passthrough,
    /// never derived from the filesystem (which would make tests time-dependent).
    const FIXTURE_MTIME_MS: u64 = 1_700_000_000_123;
    const FIXTURE_SID: &str = "01JABCDEF0123456789ABCDEFG";
    const FIXTURE_CWD: &str = "/home/exedev/project";

    /// pi session files are LF-framed JSONL: one JSON value per line.
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

    /// Write `contents` as a `.jsonl` file inside `dir` and read it back through
    /// `read_session_file` with the given filter/running set.
    fn read_fixture_bytes(
        dir: &tempfile::TempDir,
        contents: &[u8],
        cwd_filter: Option<&str>,
        running: &HashSet<Sid>,
    ) -> Option<SessionInfo> {
        let path = dir.path().join("fixture.jsonl");
        let mut line_buffer = Vec::new();

        std::fs::write(&path, contents).expect("fixture file must be writable");

        read_session_file(
            &path,
            FIXTURE_MTIME_MS,
            cwd_filter,
            running,
            &mut line_buffer,
        )
    }

    fn read_fixture(
        dir: &tempfile::TempDir,
        contents: &str,
        cwd_filter: Option<&str>,
        running: &HashSet<Sid>,
    ) -> Option<SessionInfo> {
        read_fixture_bytes(dir, contents.as_bytes(), cwd_filter, running)
    }

    /// The common case: nothing is running.
    fn nothing_running() -> HashSet<Sid> {
        HashSet::new()
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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.sid, FIXTURE_SID);
        assert_eq!(info.cwd, FIXTURE_CWD);
        assert_eq!(info.name.as_deref(), Some("first draft"));
        assert_eq!(info.mtime_ms, FIXTURE_MTIME_MS);
        assert_eq!(
            info.file,
            dir.path().join("fixture.jsonl").display().to_string()
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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    /// A later nameless `session_info` clears an earlier name: the code assigns
    /// unconditionally, so "last entry wins" includes losing the name.
    #[test]
    fn nameless_session_info_after_named_one_clears_the_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("named"),
            json!({ "type": "session_info" }),
        ]);

        let info =
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    #[test]
    fn cwd_filter_match_yields_the_session() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("in scope"),
        ]);

        let info = read_fixture(&dir, &contents, Some(FIXTURE_CWD), &nothing_running())
            .expect("matching cwd must parse");

        assert_eq!(info.cwd, FIXTURE_CWD);
    }

    #[test]
    fn cwd_filter_miss_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("out of scope"),
        ]);

        assert!(
            read_fixture(&dir, &contents, Some("/some/other/dir"), &nothing_running()).is_none()
        );
    }

    #[test]
    fn non_session_header_type_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[
            json!({ "type": "message", "id": FIXTURE_SID, "cwd": FIXTURE_CWD }),
            session_info_entry("ignored"),
        ]);

        assert!(read_fixture(&dir, &contents, None, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_id_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "type": "session", "version": 3, "cwd": FIXTURE_CWD })]);

        assert!(read_fixture(&dir, &contents, None, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_cwd_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "type": "session", "version": 3, "id": FIXTURE_SID })]);

        assert!(read_fixture(&dir, &contents, None, &nothing_running()).is_none());
    }

    #[test]
    fn header_missing_type_yields_nothing() {
        let dir = temp_dir();
        let contents = jsonl(&[json!({ "version": 3, "id": FIXTURE_SID, "cwd": FIXTURE_CWD })]);

        assert!(read_fixture(&dir, &contents, None, &nothing_running()).is_none());
    }

    #[test]
    fn unparseable_header_yields_nothing() {
        let dir = temp_dir();

        assert!(
            read_fixture(&dir, "this is not json at all\n", None, &nothing_running()).is_none()
        );
    }

    #[test]
    fn empty_file_yields_nothing() {
        let dir = temp_dir();

        assert!(read_fixture(&dir, "", None, &nothing_running()).is_none());
    }

    #[test]
    fn running_sid_reports_running_status() {
        let dir = temp_dir();
        let contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);
        let running = HashSet::from([sid(FIXTURE_SID)]);

        let info = read_fixture(&dir, &contents, None, &running).expect("valid file must parse");

        assert_eq!(info.status, SessionStatus::Running);
    }

    #[test]
    fn absent_sid_reports_exited_status() {
        let dir = temp_dir();
        let contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);
        let running = HashSet::from([sid("some-other-sid")]);

        let info = read_fixture(&dir, &contents, None, &running).expect("valid file must parse");

        assert_eq!(info.status, SessionStatus::Exited);
    }

    /// Unparseable garbage mid-file must be skipped, not abort the name scan.
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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("after garbage"));
    }

    /// The cheap `contains("\"session_info\"")` pre-filter lets non-entries
    /// through; the real parse behind it must reject them.
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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("real name"));
    }

    /// A non-UTF-8 byte mid-file must not end the name scan. It used to:
    /// `read_line` returns `Err(InvalidData)` there and the match treated that
    /// identically to `Ok(0)` (EOF). Reading bytes makes it just another line
    /// that fails the prefilter, so the last `session_info` still wins.
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

        let info = read_fixture_bytes(&dir, &contents, None, &nothing_running())
            .expect("valid header must still parse");

        assert_eq!(info.name.as_deref(), Some("after bad bytes"));
    }

    /// Pi treats an empty name as an explicit clear (`session-manager.ts:713`),
    /// so the sidebar must show an unnamed session, not a blank title.
    #[test]
    fn an_empty_name_clears_the_name() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("named"),
            session_info_entry(""),
        ]);

        let info =
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

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
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert!(info.name.is_none());
    }

    /// Pi trims before storing, so surrounding whitespace is never part of the
    /// displayed name.
    #[test]
    fn a_padded_name_is_trimmed() {
        let dir = temp_dir();
        let contents = jsonl(&[
            session_header(FIXTURE_SID, FIXTURE_CWD),
            session_info_entry("  padded name  "),
        ]);

        let info =
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("padded name"));
    }

    /// The last entry wins even when the file's final line has no LF — bytes
    /// are read with `read_until`, which yields the unterminated tail too.
    #[test]
    fn a_final_line_without_a_trailing_newline_is_still_scanned() {
        let dir = temp_dir();
        let mut contents = jsonl(&[session_header(FIXTURE_SID, FIXTURE_CWD)]);

        contents.push_str(
            &serde_json::to_string(&session_info_entry("unterminated"))
                .expect("fixture value must serialize"),
        );

        let info =
            read_fixture(&dir, &contents, None, &nothing_running()).expect("valid file must parse");

        assert_eq!(info.name.as_deref(), Some("unterminated"));
    }
}
