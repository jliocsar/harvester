//! The one pi launch line (docs/DAEMON.md §The pi launch line). Pinned to
//! pi 0.83.0 — `--session-id` (resume-or-create) is source-verified but
//! undocumented, so re-verify this module on every pi upgrade (CLAUDE.md
//! §Pi version discipline).

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use super::SpawnArgs;
use crate::daemon::Daemon;
use crate::sid::Sid;

pub(super) fn create_pi_command(args: &SpawnArgs, daemon: &Daemon) -> std::io::Result<Command> {
    let stderr_log = open_stderr_log(&daemon.inner().log_dir, &args.sid)?;
    let mut cli_args: Vec<&str> = vec![
        "--mode",
        "rpc",
        "--session-id",
        args.sid.as_str(),
        "--approve",
    ];

    if let Some(provider) = &args.provider {
        cli_args.extend(["--provider", provider]);
    }

    if let Some(model) = &args.model {
        cli_args.extend(["--model", model]);
    }

    if let Some(name) = &args.name {
        cli_args.extend(["--name", name]);
    }

    let mut command = Command::new("pi");
    command
        .current_dir(&args.cwd)
        // PI_OFFLINE disables update checks and telemetry; the two git guards
        // make a package install that wants credentials fail fast instead of
        // hanging the spawn — the risk `--approve` buys back.
        .env("PI_OFFLINE", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=5",
        )
        .args(cli_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_log));

    Ok(command)
}

/// **Append, never truncate**: spawn is also resume, so truncating would destroy
/// the record of why the previous run died — the very thing the user is looking
/// at when they click resume, and the sole exit-diagnosis channel the contract
/// gives clients (docs/DAEMON.md §Events). The banner is what makes appended
/// runs separable in a `tail`.
fn open_stderr_log(log_dir: &Path, sid: &Sid) -> std::io::Result<std::fs::File> {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join(format!("{sid}.stderr.log")))?;
    let started_at_unix_seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_secs())
        .unwrap_or_default();

    writeln!(
        log,
        "--- harvesterd: spawning pi for {sid} at {started_at_unix_seconds} (unix seconds) ---"
    )?;

    Ok(log)
}
