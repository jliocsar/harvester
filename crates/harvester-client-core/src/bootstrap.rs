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

    let outcome = match probe.installed_version {
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
            daemon_version: daemon_version.to_string(),
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
            daemon_version: daemon_version.to_string(),
            outcome: BootstrapOutcome::Upgraded,
            remote_home: probe.home,
        })
    } else {
        Err(Error::VersionMismatch {
            expected: PROTOCOL_VERSION.to_string(),
            actual: daemon_version.to_string(),
        })
    }
}

struct Probe {
    home: String,
    installed_version: Option<u32>,
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

    // Unparseable reads as "no daemon installed", which reinstalls — the
    // self-healing direction. That is also how a pre-C4 binary is retired:
    // it prints a crate version string ("0.1.0"), which is not a `u32`.
    let installed_version = if output.status.success() {
        stdout_lines.next().and_then(|version| version.parse().ok())
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
///
/// The `wc -c` assertion is what makes "complete" true. `cat` cannot tell a
/// finished upload from a broken stdin pipe — both are EOF, both exit 0 — and
/// a failed stdin write is non-fatal on our side ([`Host::exec`]), so without
/// the size check a truncated binary would be `chmod +x`'d and `mv`'d into
/// place exactly like a whole one. The swap is atomic against ETXTBSY; it was
/// never atomic against a short write.
fn install_script(kill_running_daemon: KillRunningDaemon, binary_len: usize) -> String {
    let replace = format!(
        "mkdir -p ~/.harvester/bin ~/.harvester/log \
         && chmod 700 ~/.harvester \
         && cat > ~/{DAEMON_BINARY_PATH}.next.$$ \
         && {{ [ \"$(wc -c < ~/{DAEMON_BINARY_PATH}.next.$$)\" -eq {binary_len} ] \
               || {{ rm -f ~/{DAEMON_BINARY_PATH}.next.$$; \
                     echo 'harvesterd install: upload truncated ({binary_len} bytes expected)' >&2; \
                     exit 1; }}; }} \
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
            &install_script(kill_running_daemon, daemon_binary.len()),
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

/// The bootstrap scripts are the least verifiable code in the client — they
/// only ever run inside someone else's shell, and a broken one bricks every
/// machine at once. These tests are the part that can be checked locally:
/// the shell parses them, and the guards that must be in the chain are.
#[cfg(test)]
mod tests {
    use super::*;

    /// Hands the script to a real `/bin/sh` in no-execute mode — the same
    /// check `sh -n providers/exe-dev` performs, for a string built at runtime.
    fn assert_shell_parses(script: &str) {
        let status = std::process::Command::new("sh")
            .args(["-n", "-c", script])
            .status()
            .expect("spawning sh");

        assert!(status.success(), "sh rejected the script:\n{script}");
    }

    #[test]
    fn every_bootstrap_script_is_valid_shell() {
        assert_shell_parses(&start_script());
        assert_shell_parses(&install_script(KillRunningDaemon::No, 1234));
        assert_shell_parses(&install_script(KillRunningDaemon::Yes, 1234));
    }

    #[test]
    fn the_install_asserts_the_uploaded_byte_count() {
        // `cat` reports EOF and exit 0 for a broken stdin pipe exactly as it
        // does for a finished upload, so the size is the only evidence the
        // remote side has that it received the whole binary.
        let script = install_script(KillRunningDaemon::No, 4096);

        assert!(
            script.contains("-eq 4096"),
            "the expected length must reach the remote shell:\n{script}"
        );
    }

    #[test]
    fn the_size_assertion_gates_the_swap_into_place() {
        let script = install_script(KillRunningDaemon::No, 4096);
        let assertion = script.find("wc -c").expect("size assertion");
        let chmod = script.find("chmod +x").expect("chmod");
        let swap = script.find("mv ").expect("swap");

        assert!(
            assertion < chmod && chmod < swap,
            "a truncated upload must never be made executable or installed:\n{script}"
        );
    }

    #[test]
    fn a_failed_size_assertion_stops_the_chain_before_the_daemon_starts() {
        let script = install_script(KillRunningDaemon::Yes, 4096);
        let assertion = script.find("wc -c").expect("size assertion");
        let start = script.find("setsid").expect("start");

        assert!(assertion < start);
        assert!(
            script[assertion..start].contains("exit 1"),
            "the guard must exit, not just report:\n{script}"
        );
    }
}
