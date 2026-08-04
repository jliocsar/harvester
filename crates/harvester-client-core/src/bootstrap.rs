//! First-connect bootstrap (docs/DAEMON.md §Bootstrap & upgrade): probe the
//! machine, install/upgrade the static harvesterd binary, start it detached,
//! then connect and version-handshake. Written once against [`Host::exec`] —
//! the local machine runs the exact same scripts through `sh -c`, so the
//! install/upgrade policy cannot drift between machine kinds.

use std::time::Duration;

use harvester_proto::PROTOCOL_VERSION;

use crate::connection::{self, DaemonConnection};
use crate::host::Host;
use crate::{Error, Result};

/// Ceiling on the probe and start execs (each about one round trip).
const BOOTSTRAP_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Ceiling on the install exec: the binary upload over a slow link plus the
/// bounded old-daemon wait (10s worst case) need more room than a round trip.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Install location, relative to `$HOME` (tilde-expanded by the shell).
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
    /// The machine's `$HOME`, needed for any later plain path work
    /// (stderr-log tails, workspace browsing).
    pub remote_home: String,
}

/// The whole first-connect contract in one call. Also the *every*-connect
/// contract: the version is re-checked on each ping, and a mismatched running
/// daemon is upgraded in place (exactly once — a second mismatch is an error).
pub async fn bootstrap_and_connect(host: &Host, daemon_binary: &[u8]) -> Result<Connected> {
    let probe = probe(host).await?;

    let outcome = match probe.installed_version.as_deref() {
        Some(PROTOCOL_VERSION) => {
            start(host).await?;

            BootstrapOutcome::AlreadyCurrent
        }
        Some(_) => {
            install_and_start(host, daemon_binary, KillRunningDaemon::Yes).await?;

            BootstrapOutcome::Upgraded
        }
        None => {
            install_and_start(host, daemon_binary, KillRunningDaemon::No).await?;

            BootstrapOutcome::Installed
        }
    };

    let (connection, daemon_version) = connection::connect_host(host, &probe.home).await?;

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
    install_and_start(host, daemon_binary, KillRunningDaemon::Yes).await?;

    let (connection, daemon_version) = connection::connect_host(host, &probe.home).await?;

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
/// `~` in `-L`; the local connect joins it the same way), and what harvesterd
/// version is on disk, if any.
async fn probe(host: &Host) -> Result<Probe> {
    let script = format!(r#"printf '%s\n' "$HOME"; exec ~/{DAEMON_BINARY_PATH} version"#);
    let output = host.exec(&script, None, BOOTSTRAP_EXEC_TIMEOUT).await?;

    // The home line prints even when the binary is missing (exec fails with
    // 126/127 after it); no home line at all means the exec itself failed.
    let mut stdout_lines = output.stdout.lines();
    let Some(home) = stdout_lines.next().filter(|home| home.starts_with('/')) else {
        return Err(Error::Exec {
            context: "probe home",
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

/// Starts harvesterd detached from its parent shell. harvesterd does NOT
/// self-daemonize — `setsid` + the redirects here are the whole daemonization
/// story, on the local machine too (a plain child would share the client's
/// terminal and die with it). Idempotent: a second daemon loses the
/// socket-bind race and exits.
///
/// The subshell around the `&` is load-bearing: a bare trailing `&` would
/// background the entire preceding `&&` list, and POSIX gives async lists
/// stdin from /dev/null — which silently truncated the `cat` upload to zero
/// bytes when this script is appended to the install chain (found live).
///
/// That same backgrounding makes the launch itself unobservable — the subshell
/// exits 0 whether or not the daemon ever ran — so the preflights ahead of it
/// carry the diagnosis. Without them a missing `setsid`, a wrong-architecture
/// binary, or a truncated upload all surface only as a connect timeout ten
/// seconds later, with the real error stranded in the remote log.
/// Stays a single `&&` chain (guards brace their own `exit`) because
/// `install_script` appends it with `&&` — a bare `;` here would let the start
/// run even after a failed install.
fn start_script() -> String {
    format!(
        "mkdir -p ~/.harvester/log \
         && chmod 700 ~/.harvester \
         && {{ command -v setsid >/dev/null 2>&1 \
               || {{ echo 'harvesterd start: setsid not found' >&2; exit 1; }}; }} \
         && {{ ~/{DAEMON_BINARY_PATH} version >/dev/null 2>&1 \
               || {{ echo 'harvesterd start: binary is missing or not runnable' >&2; exit 1; }}; }} \
         && (setsid ~/{DAEMON_BINARY_PATH} < /dev/null >> ~/{DAEMON_LOG_PATH} 2>&1 &)"
    )
}

/// Receives the binary on stdin. The staging + `mv` dance swaps atomically and
/// dodges ETXTBSY from a still-running daemon. On upgrades, SIGTERM to the old
/// daemon closes its pi children's stdin pipes — their clean shutdown path —
/// and the bounded wait keeps the new daemon from losing the bind race to a
/// dying predecessor (100 × 0.1s, then fail loudly rather than spin forever
/// on an unkillable process).
///
/// The staging file is per-shell (`$$`): multiple clients are first-class, so
/// two of them upgrading the same machine at once would otherwise interleave
/// their writes into one path and `mv` a corrupt binary into place. Whoever
/// finishes last still wins, but with a complete binary.
fn install_script(kill_running_daemon: KillRunningDaemon) -> String {
    let replace = format!(
        "mkdir -p ~/.harvester/bin ~/.harvester/log \
         && chmod 700 ~/.harvester \
         && cat > ~/{DAEMON_BINARY_PATH}.next.$$ \
         && chmod +x ~/{DAEMON_BINARY_PATH}.next.$$ \
         && mv ~/{DAEMON_BINARY_PATH}.next.$$ ~/{DAEMON_BINARY_PATH}"
    );
    let kill = match kill_running_daemon {
        KillRunningDaemon::Yes => {
            " && { pkill -x harvesterd 2>/dev/null \
             && attempts=0 \
             && while pgrep -x harvesterd >/dev/null; do \
                    attempts=$((attempts+1)); \
                    [ \"$attempts\" -ge 100 ] && { echo 'old harvesterd refused to die' >&2; exit 1; }; \
                    sleep 0.1; \
                done; true; }"
        }
        KillRunningDaemon::No => "",
    };
    let start = start_script();

    format!("{replace}{kill} && {start}")
}

async fn install_and_start(
    host: &Host,
    daemon_binary: &[u8],
    kill_running_daemon: KillRunningDaemon,
) -> Result<()> {
    let output = host
        .exec(
            &install_script(kill_running_daemon),
            Some(daemon_binary),
            INSTALL_TIMEOUT,
        )
        .await?;

    output.require_success("install harvesterd")
}

async fn start(host: &Host) -> Result<()> {
    let output = host
        .exec(&start_script(), None, BOOTSTRAP_EXEC_TIMEOUT)
        .await?;

    output.require_success("start harvesterd")
}
