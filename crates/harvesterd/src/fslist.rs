//! `list_sessions`: readdir pi's session dirs. No daemon state involved —
//! the header line gives sid/cwd, the last `session_info` entry gives the name.
//!
//! Blocking std IO on purpose — the caller runs this on the blocking pool
//! (`spawn_blocking`); the daemon's single runtime thread never waits on disk.

use std::io::BufRead;
use std::path::Path;
use std::time::UNIX_EPOCH;

use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;

fn pi_sessions_root() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    std::path::PathBuf::from(home).join(".pi/agent/sessions")
}

/// Scan all per-cwd dirs (optionally filtered to one cwd). `running` contains
/// the sids with a live pi child, provided by the caller.
pub fn list_sessions(cwd_filter: Option<&str>, running: &[String]) -> Vec<SessionInfo> {
    let mut sessions = Vec::new();
    // Reused across files: line reads allocate once, not per line.
    let mut line_buffer = String::new();

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
fn read_session_file(
    path: &Path,
    mtime_ms: u64,
    cwd_filter: Option<&str>,
    running: &[String],
    line_buffer: &mut String,
) -> Option<SessionInfo> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);

    // Line 1: `{"type":"session","version":3,"id":...,"cwd":...}`
    line_buffer.clear();
    reader.read_line(line_buffer).ok()?;
    let header: Value = serde_json::from_str(line_buffer).ok()?;

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

    loop {
        line_buffer.clear();

        match reader.read_line(line_buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        // Cheap pre-filter before parsing.
        if !line_buffer.contains("\"session_info\"") {
            continue;
        }

        if let Ok(entry) = serde_json::from_str::<Value>(line_buffer) {
            if entry.get("type").and_then(Value::as_str) == Some("session_info") {
                name = entry.get("name").and_then(Value::as_str).map(str::to_owned);
            }
        }
    }

    let status = if running.contains(&sid) {
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
