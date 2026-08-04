//! First-connect bootstrap (docs/DAEMON.md §Bootstrap & upgrade): probe the
//! machine over plain SSH, install/upgrade the static harvesterd binary,
//! start it detached, then forward the socket and version-handshake.

use harvester_proto::PROTOCOL_VERSION;

use crate::connection::{self, DaemonConnection};
use crate::ssh::{exec, ExecOutput, SshTarget};
use crate::{Error, Result};

/// Install location, relative to `$HOME` (tilde-expanded by the remote
/// shell; joined locally by the local-machine bootstrap).
pub(crate) const DAEMON_BINARY_PATH: &str = ".harvester/bin/harvesterd";
/// Daemon stdout/stderr land here — appended, so logs survive upgrades.
pub(crate) const DAEMON_LOG_PATH: &str = ".harvester/log/harvesterd.log";

/// Whether the install must displace a live daemon first. A dedicated type
/// (not a bool) so call sites read as what they do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KillRunningDaemon {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// The installed binary already matched; nothing was uploaded.
    AlreadyCurrent,
    /// No binary on the machine; uploaded and started fresh.
    Installed,
    /// Version mismatch; replaced the binary and restarted the daemon
    /// (which closes its pi children's stdin — acceptable by contract).
    Upgraded,
}

/// Everything `bootstrap_and_connect` learned on the way in.
pub struct Connected {
    pub connection: DaemonConnection,
    pub daemon_version: String,
    pub outcome: BootstrapOutcome,
    /// Remote `$HOME`, needed for any later plain-SSH path work
    /// (stderr-log tails, workspace browsing).
    pub remote_home: String,
}

/// The whole first-connect contract in one call. Also the *every*-connect
/// contract: the version is re-checked on each ping, and a mismatched running
/// daemon is upgraded in place (exactly once — a second mismatch is an error).
pub async fn bootstrap_and_connect(target: &SshTarget, daemon_binary: &[u8]) -> Result<Connected> {
    let probe = probe(target).await?;

    let outcome = match probe.installed_version.as_deref() {
        Some(PROTOCOL_VERSION) => {
            start(target).await?;

            BootstrapOutcome::AlreadyCurrent
        }
        Some(_) => {
            install_and_start(target, daemon_binary, KillRunningDaemon::Yes).await?;

            BootstrapOutcome::Upgraded
        }
        None => {
            install_and_start(target, daemon_binary, KillRunningDaemon::No).await?;

            BootstrapOutcome::Installed
        }
    };

    let (connection, daemon_version) = connection::connect(target, &probe.home).await?;

    if daemon_version == PROTOCOL_VERSION {
        return Ok(Connected {
            connection,
            daemon_version,
            outcome,
            remote_home: probe.home,
        });
    }

    // A running daemon can predate the on-disk binary we just checked (it was
    // started before an upgrade, or the disk check raced another client).
    // Force the upgrade path exactly once: kill, reinstall, restart, re-ping.
    drop(connection);
    install_and_start(target, daemon_binary, KillRunningDaemon::Yes).await?;

    let (connection, daemon_version) = connection::connect(target, &probe.home).await?;

    if daemon_version == PROTOCOL_VERSION {
        Ok(Connected {
            connection,
            daemon_version,
            outcome: BootstrapOutcome::Upgraded,
            remote_home: probe.home,
        })
    } else {
        Err(Error::VersionMismatch {
            expected: PROTOCOL_VERSION.to_string(),
            actual: daemon_version,
        })
    }
}

struct Probe {
    home: String,
    installed_version: Option<String>,
}

/// One round trip answering both bootstrap questions: where is `$HOME`
/// (the socket forward needs an absolute remote path — ssh does not expand
/// `~` in `-L`), and what harvesterd version is on disk, if any.
async fn probe(target: &SshTarget) -> Result<Probe> {
    let script = format!(r#"printf '%s\n' "$HOME"; exec ~/{DAEMON_BINARY_PATH} version"#);
    let output = exec(target, &script, None).await?;

    // The home line prints even when the binary is missing (exec fails with
    // 126/127 after it); no home line at all means SSH itself failed.
    let mut stdout_lines = output.stdout.lines();
    let Some(home) = stdout_lines.next().filter(|home| home.starts_with('/')) else {
        return Err(Error::Ssh {
            context: "probe remote home",
            detail: output.describe_failure(),
        });
    };

    let installed_version = if output.status.success() {
        stdout_lines
            .next()
            .map(str::to_string)
            .filter(|version| !version.is_empty())
    } else {
        None
    };

    Ok(Probe {
        home: home.to_string(),
        installed_version,
    })
}

/// Starts harvesterd detached from the SSH session. harvesterd does NOT
/// self-daemonize — `setsid` + the redirects here are the whole daemonization
/// story. Idempotent: a second daemon loses the socket-bind race and exits.
///
/// The subshell around the `&` is load-bearing: a bare trailing `&` would
/// background the entire preceding `&&` list, and POSIX gives async lists
/// stdin from /dev/null — which silently truncated the `cat` upload to zero
/// bytes when this script is appended to the install chain (found live).
fn start_script() -> String {
    format!("mkdir -p ~/.harvester/log && (setsid ~/{DAEMON_BINARY_PATH} < /dev/null >> ~/{DAEMON_LOG_PATH} 2>&1 &)")
}

/// Receives the binary on stdin. The `.next` + `mv` dance swaps atomically and
/// dodges ETXTBSY from a still-running daemon. On upgrades, SIGTERM to the old
/// daemon closes its pi children's stdin pipes — their clean shutdown path —
/// and the wait-loop keeps the new daemon from losing the bind race to a
/// dying predecessor.
fn install_script(kill_running_daemon: KillRunningDaemon) -> String {
    let replace = format!(
        "mkdir -p ~/.harvester/bin ~/.harvester/log \
         && cat > ~/{DAEMON_BINARY_PATH}.next \
         && chmod +x ~/{DAEMON_BINARY_PATH}.next \
         && mv ~/{DAEMON_BINARY_PATH}.next ~/{DAEMON_BINARY_PATH}"
    );
    let kill = match kill_running_daemon {
        KillRunningDaemon::Yes => {
            " && { pkill -x harvesterd 2>/dev/null \
             && while pgrep -x harvesterd >/dev/null; do sleep 0.1; done; true; }"
        }
        KillRunningDaemon::No => "",
    };
    let start = start_script();

    format!("{replace}{kill} && {start}")
}

async fn install_and_start(
    target: &SshTarget,
    daemon_binary: &[u8],
    kill_running_daemon: KillRunningDaemon,
) -> Result<()> {
    let output = exec(
        target,
        &install_script(kill_running_daemon),
        Some(daemon_binary),
    )
    .await?;

    require_success(&output, "install harvesterd")
}

async fn start(target: &SshTarget) -> Result<()> {
    let output = exec(target, &start_script(), None).await?;

    require_success(&output, "start harvesterd")
}

fn require_success(output: &ExecOutput, context: &'static str) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Ssh {
            context,
            detail: output.describe_failure(),
        })
    }
}
