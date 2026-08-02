//! `list_sessions`: readdir pi's session dirs. No daemon state involved —
//! the header line gives sid/cwd, the last `session_info` entry gives the name.

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;

fn pi_sessions_root() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".pi/agent/sessions")
}

/// Scan all per-cwd dirs (optionally filtered to one cwd). `running` contains
/// the sids with a live pi child, provided by the caller.
pub fn list_sessions(cwd_filter: Option<&str>, running: &[String]) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    let Ok(dirs) = std::fs::read_dir(pi_sessions_root()) else {
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
            if let Some(info) = read_session_file(&path, cwd_filter, running) {
                sessions.push(info);
            }
        }
    }
    // Newest first — the sidebar order.
    sessions.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    sessions
}

fn read_session_file(
    path: &PathBuf,
    cwd_filter: Option<&str>,
    running: &[String],
) -> Option<SessionInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines = content.lines();

    // Line 1: `{"type":"session","version":3,"id":...,"cwd":...}`
    let header: Value = serde_json::from_str(lines.next()?).ok()?;
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

    // Display name: the LAST session_info entry wins (rename history is append-only).
    let mut name: Option<String> = None;
    for line in lines {
        // Cheap pre-filter before parsing: session files can be large.
        if !line.contains("\"session_info\"") {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            if entry.get("type").and_then(Value::as_str) == Some("session_info") {
                name = entry.get("name").and_then(Value::as_str).map(str::to_owned);
            }
        }
    }

    let mtime_ms = std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);

    let status = if running.iter().any(|running_sid| *running_sid == sid) {
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
