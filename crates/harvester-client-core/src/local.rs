//! The hardcoded `local` machine (docs/SPEC.md §Machine): harvesterd spawned
//! as a detached child on this host, spoken to over its Unix socket directly
//! — no SSH anywhere on the path. The daemon is orphaned on purpose (handle
//! dropped after spawn), so it — and its pi sessions — survive the client.
//!
//! Workspace browsing and validation run the exact commands the remote paths
//! run (`ls -1Ap`, `git rev-parse`), just locally, so both machines behave
//! identically in the picker.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use harvester_proto::PROTOCOL_VERSION;

use crate::bootstrap::{BootstrapOutcome, Connected, DAEMON_BINARY_PATH, DAEMON_LOG_PATH};
use crate::connection;
use crate::workspace::parse_ls_entries;
use crate::{Error, RemoteDirEntry, Result};

/// Poll interval while waiting for a killed daemon to release the socket
/// bind (mirrors the remote install script's wait loop).
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `bootstrap_and_connect`, local edition: same contract (probe version →
/// install/upgrade → start detached → connect → ping handshake, forced
/// upgrade exactly once on a version mismatch), no SSH.
pub async fn bootstrap_and_connect_local(daemon_binary: &[u8]) -> Result<Connected> {
    let home = home_dir()?;
    let binary_path = home.join(DAEMON_BINARY_PATH);

    let outcome = match installed_version(&binary_path).await.as_deref() {
        Some(PROTOCOL_VERSION) => {
            start_daemon(&home, &binary_path)?;

            BootstrapOutcome::AlreadyCurrent
        }
        Some(_) => {
            kill_running_daemon().await?;
            install(&binary_path, daemon_binary)?;
            start_daemon(&home, &binary_path)?;

            BootstrapOutcome::Upgraded
        }
        None => {
            install(&binary_path, daemon_binary)?;
            start_daemon(&home, &binary_path)?;

            BootstrapOutcome::Installed
        }
    };

    let home_string = home.to_string_lossy().into_owned();
    let (connection, daemon_version) = connection::connect_local(&home_string).await?;

    if daemon_version == PROTOCOL_VERSION {
        return Ok(Connected {
            connection,
            daemon_version,
            outcome,
            remote_home: home_string,
        });
    }

    // A running daemon can predate the on-disk binary we just checked.
    // Force the upgrade path exactly once, like the SSH bootstrap.
    drop(connection);
    kill_running_daemon().await?;
    install(&binary_path, daemon_binary)?;
    start_daemon(&home, &binary_path)?;

    let (connection, daemon_version) = connection::connect_local(&home_string).await?;

    if daemon_version == PROTOCOL_VERSION {
        Ok(Connected {
            connection,
            daemon_version,
            outcome: BootstrapOutcome::Upgraded,
            remote_home: home_string,
        })
    } else {
        Err(Error::VersionMismatch {
            expected: PROTOCOL_VERSION.to_string(),
            actual: daemon_version,
        })
    }
}

/// Local picker listing — same command and parsing as the SSH path.
pub async fn list_directory_local(path: &str) -> Result<Vec<RemoteDirEntry>> {
    let output = tokio::process::Command::new("ls")
        .args(["-1Ap", "--", path])
        .output()
        .await?;

    if output.status.success() {
        Ok(parse_ls_entries(&String::from_utf8_lossy(&output.stdout)))
    } else {
        Err(Error::Ssh {
            context: "list directory (local)",
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

/// Local git validation — same command as the SSH path; the error carries
/// git's own explanation for the modal.
pub async fn validate_git_workspace_local(path: &str) -> Result<()> {
    let output = tokio::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--git-dir"])
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Ssh {
            context: "validate workspace (local)",
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

fn home_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home)),
        None => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset; local machine unavailable",
        ))),
    }
}

/// The installed binary's self-reported version; `None` when missing or
/// unrunnable (both mean: install).
async fn installed_version(binary_path: &Path) -> Option<String> {
    let output = tokio::process::Command::new(binary_path)
        .arg("version")
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

        (!version.is_empty()).then_some(version)
    } else {
        None
    }
}

/// The `.next` + rename dance from the remote install script, in std::fs:
/// atomic swap, no ETXTBSY from a still-running daemon.
fn install(binary_path: &Path, daemon_binary: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = binary_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let staging_path = binary_path.with_extension("next");

    std::fs::write(&staging_path, daemon_binary)?;

    let mut permissions = std::fs::metadata(&staging_path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&staging_path, permissions)?;
    std::fs::rename(&staging_path, binary_path)?;

    Ok(())
}

/// Starts harvesterd detached: stdio to the append-mode log, handle dropped
/// so the daemon outlives the client. Idempotent — a second daemon loses the
/// socket-bind race and exits.
fn start_daemon(home: &Path, binary_path: &Path) -> Result<()> {
    std::fs::create_dir_all(home.join(".harvester/log"))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join(DAEMON_LOG_PATH))?;
    let log_for_stderr = log.try_clone()?;

    std::process::Command::new(binary_path)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_for_stderr)
        .spawn()?;

    Ok(())
}

/// SIGTERM to any running local harvesterd (its pi children get their clean
/// stdin-close shutdown), then wait for the socket bind to free — the same
/// sequence as the remote install script. Shells out to pkill/pgrep (house
/// rule: no libc dependency for one syscall).
async fn kill_running_daemon() -> Result<()> {
    let killed = tokio::process::Command::new("pkill")
        .args(["-x", "harvesterd"])
        .status()
        .await?
        .success();

    if !killed {
        // Nothing matched — no daemon to wait out.
        return Ok(());
    }

    loop {
        let still_running = tokio::process::Command::new("pgrep")
            .args(["-x", "harvesterd"])
            .status()
            .await?
            .success();

        if !still_running {
            return Ok(());
        }

        tokio::time::sleep(KILL_POLL_INTERVAL).await;
    }
}
