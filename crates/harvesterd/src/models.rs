use std::process::Stdio;
use std::time::Duration;

use harvester_proto::{encode_frame, read_frame, HarvesterResponse, FRAME_BUFFER_START_BYTES};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(15);

const MODELS_REQUEST_ID: &str = "hv-models";

const PI_STDERR_CAPTURE_BYTES: usize = 8 * 1024;
const PI_STDERR_CHUNK_BYTES: usize = 1024;
const PI_STDERR_SUMMARY_CHARS: usize = 200;
const PI_STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const PI_EXITED_EARLY: &str = "pi exited before answering";

pub async fn list_models(cwd: String) -> HarvesterResponse {
    match tokio::time::timeout(LIST_MODELS_TIMEOUT, query_available_models(&cwd)).await {
        Ok(Ok(data)) => HarvesterResponse::ok(data),
        Ok(Err(detail)) => HarvesterResponse::err(format!("list_models: {detail}")),
        Err(_elapsed) => HarvesterResponse::err(format!(
            "list_models: pi gave no answer within {LIST_MODELS_TIMEOUT:?}"
        )),
    }
}

async fn query_available_models(cwd: &str) -> Result<Value, String> {
    let mut child = tokio::process::Command::new("pi")
        .current_dir(cwd)
        .env("PI_OFFLINE", "1")
        .args(["--mode", "rpc", "--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("spawn pi: {error}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = capture_pi_stderr(child.stderr.take().expect("piped stderr"));

    let request = encode_frame(&json!({"id": MODELS_REQUEST_ID, "type": "get_available_models"}))
        .expect("serializable");
    stdin
        .write_all(&request)
        .await
        .map_err(|error| format!("write to pi: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("write to pi: {error}"))?;

    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(FRAME_BUFFER_START_BYTES);

    loop {
        let line = match read_frame(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err(explain_pi_failure(PI_EXITED_EARLY, stderr).await),
            Err(error) => {
                return Err(explain_pi_failure(&format!("read from pi: {error}"), stderr).await)
            }
        };

        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };

        if value.get("id").and_then(Value::as_str) != Some(MODELS_REQUEST_ID) {
            continue;
        }

        if value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            drop(stdin);
            let _ = child.wait().await;

            return Ok(value.get("data").cloned().unwrap_or(json!({})));
        }

        let detail = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("pi rejected get_available_models");

        return Err(detail.to_string());
    }
}

fn capture_pi_stderr(stderr: tokio::process::ChildStderr) -> JoinHandle<String> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut captured: Vec<u8> = Vec::new();
        let mut chunk = [0u8; PI_STDERR_CHUNK_BYTES];

        loop {
            let read = match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    eprintln!("harvesterd: read pi stderr: {error}");

                    break;
                }
            };
            let room = PI_STDERR_CAPTURE_BYTES.saturating_sub(captured.len());

            if room > 0 {
                captured.extend_from_slice(&chunk[..read.min(room)]);
            }
        }

        String::from_utf8_lossy(&captured).into_owned()
    })
}

async fn explain_pi_failure(reason: &str, stderr: JoinHandle<String>) -> String {
    let captured = match tokio::time::timeout(PI_STDERR_DRAIN_TIMEOUT, stderr).await {
        Ok(Ok(captured)) => captured,
        Ok(Err(error)) => {
            eprintln!("harvesterd: collect pi stderr: {error}");

            String::new()
        }
        Err(_elapsed) => String::new(),
    };
    let summary = pi_stderr_summary(&captured);

    if summary.is_empty() {
        return reason.to_string();
    }

    eprintln!("harvesterd: pi stderr follows\n{}", captured.trim_end());

    format!("{reason} \u{2014} {summary}")
}

fn pi_stderr_summary(captured: &str) -> String {
    let nonempty = || {
        captured
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    let worded = nonempty().find(|line| line.contains(char::is_whitespace));
    let summary = worded.or_else(|| nonempty().next()).unwrap_or_default();

    summary.chars().take(PI_STDERR_SUMMARY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crash_summary_prefers_the_message_over_the_source_location() {
        let captured = "/usr/local/lib/node_modules/pi/undici/cachestorage.js:20\n    webidl.util.markAsUncloneable(this)\n                ^\n\nTypeError: webidl.util.markAsUncloneable is not a function\n    at new CacheStorage (/usr/local/lib/node_modules/pi/undici/cachestorage.js:20:17)\n";

        assert_eq!(
            super::pi_stderr_summary(captured),
            "TypeError: webidl.util.markAsUncloneable is not a function"
        );
    }

    #[test]
    fn a_summary_falls_back_to_a_lone_bare_token() {
        assert_eq!(super::pi_stderr_summary("\n\n  ENOENT  \n"), "ENOENT");
    }

    #[test]
    fn silence_summarizes_to_nothing_so_the_caller_keeps_its_own_reason() {
        assert_eq!(super::pi_stderr_summary("   \n\n"), "");
    }

    #[test]
    fn a_runaway_summary_is_capped() {
        let captured = format!(
            "{} and then some",
            "x y".repeat(super::PI_STDERR_SUMMARY_CHARS)
        );

        assert_eq!(
            super::pi_stderr_summary(&captured).chars().count(),
            super::PI_STDERR_SUMMARY_CHARS
        );
    }
}
