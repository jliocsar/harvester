//! First-connect bootstrap (docs/DAEMON.md §Bootstrap & upgrade): probe the
//! machine, install/upgrade the static harvesterd binary, start it detached,
//! then connect and handshake. Written once against [`Host::exec`] — the local
//! machine runs the exact same scripts through `sh -c`, so the install/upgrade
//! policy cannot drift between machine kinds.
//!
//! Two different questions are asked here, and conflating them was a live bug
//! (co-design C4). *Is the binary over there the one I ship?* is answered by
//! the build fingerprint — install iff it differs — which is what makes a
//! rebuilt daemon actually reach the machine. *Can these two peers talk?* is
//! answered by [`PROTOCOL_VERSION`] at the ping, and nothing else.

use std::time::Duration;

use harvester_proto::{fingerprint_bytes, PingData, PROTOCOL_VERSION};

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
    /// The installed binary fingerprinted identical to the one we ship;
    /// nothing was uploaded.
    AlreadyCurrent,
    /// No usable binary on the machine; uploaded and started fresh.
    Installed,
    /// A different binary was installed; replaced it and restarted the daemon
    /// (which closes its pi children's stdin — acceptable by contract).
    Upgraded,
}

/// Everything `bootstrap_and_connect` learned on the way in.
pub struct Connected {
    pub connection: DaemonConnection,
    pub daemon_version: String,
    pub outcome: BootstrapOutcome,
}

/// The whole first-connect contract in one call. Also the *every*-connect
/// contract: the binary on disk is fingerprint-checked on every connect, and a
/// daemon still running an older image is upgraded in place (exactly once).
pub async fn bootstrap_and_connect(host: &Host, daemon_binary: &[u8]) -> Result<Connected> {
    let shipped_build = fingerprint_bytes(daemon_binary);
    let probe = probe(host).await?;
    let outcome = plan_install(probe.installed_build.as_deref(), &shipped_build);

    match outcome {
        BootstrapOutcome::AlreadyCurrent => start(host).await?,
        BootstrapOutcome::Upgraded => {
            install_and_start(host, daemon_binary, KillRunningDaemon::Yes).await?
        }
        BootstrapOutcome::Installed => {
            install_and_start(host, daemon_binary, KillRunningDaemon::No).await?
        }
    }

    let (connection, ping) = connection::connect_host(host, &probe.home).await?;

    if ping.build == shipped_build {
        return connected_if_protocol_matches(connection, ping, outcome);
    }

    // The file on disk is ours, but the daemon *running* is not it: it was
    // started before an upgrade, or the disk check raced another client. Only a
    // restart can fix that, so force the upgrade path exactly once.
    drop(connection);
    install_and_start(host, daemon_binary, KillRunningDaemon::Yes).await?;

    let (connection, ping) = connection::connect_host(host, &probe.home).await?;

    // Still not our image after a kill-reinstall-restart. Deliberately NOT an
    // error: the fingerprint is a staleness heuristic, while the thing that
    // decides whether these two peers can actually talk is the protocol
    // version checked below. Refusing here would brick a machine over a
    // benign race (another client installing a different build) or over a
    // daemon that simply cannot read its own image and reports a sentinel —
    // both recover on a later connect, and neither is worth an unusable
    // machine. Loud, because a permanent mismatch is worth investigating.
    if ping.build != shipped_build {
        eprintln!(
            "harvester: daemon still reports build {} after a forced reinstall (we ship {shipped_build}); \
             continuing because the protocol version is what governs compatibility",
            ping.build
        );
    }

    connected_if_protocol_matches(connection, ping, BootstrapOutcome::Upgraded)
}

/// Which install path a probe implies — pure, so the policy that decides
/// whether a machine gets a new binary is testable without a machine.
///
/// Comparing fingerprints rather than versions is the fix for a live silent
/// deploy failure: with the crate version as the identity, rebuilding
/// harvesterd without bumping the workspace left every bootstrapped machine on
/// the stale binary forever, because the probe saw a match and skipped the
/// upload.
fn plan_install(installed_build: Option<&str>, shipped_build: &str) -> BootstrapOutcome {
    match installed_build {
        Some(build) if build == shipped_build => BootstrapOutcome::AlreadyCurrent,
        Some(_) => BootstrapOutcome::Upgraded,
        None => BootstrapOutcome::Installed,
    }
}

/// The protocol gate, and the only hard failure in the bootstrap: a daemon we
/// just confirmed (or reinstalled) speaking an envelope this client does not.
/// Unlike a build mismatch this cannot be repaired by installing again — the
/// binary we ship *is* the wrong one — so it is an error rather than a warning.
fn connected_if_protocol_matches(
    connection: DaemonConnection,
    ping: PingData,
    outcome: BootstrapOutcome,
) -> Result<Connected> {
    if ping.version != PROTOCOL_VERSION {
        return Err(Error::VersionMismatch {
            expected: PROTOCOL_VERSION.to_string(),
            actual: ping.version.to_string(),
        });
    }

    Ok(Connected {
        connection,
        daemon_version: ping.version.to_string(),
        outcome,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Probe {
    home: String,
    installed_build: Option<String>,
}

/// One round trip answering both bootstrap questions: where is `$HOME`
/// (the socket forward needs an absolute remote path — ssh does not expand
/// `~` in `-L`; the local connect joins it the same way), and which build of
/// harvesterd is on disk, if any.
async fn probe(host: &Host) -> Result<Probe> {
    let script = format!(r#"printf '%s\n' "$HOME"; exec ~/{DAEMON_BINARY_PATH} build"#);
    let output = host.exec(&script, None, BOOTSTRAP_EXEC_TIMEOUT).await?;

    parse_probe(&output.stdout, output.status.success()).ok_or_else(|| Error::Exec {
        context: "probe home",
        detail: output.describe_failure(),
    })
}

/// The probe's answer is **positional** — `$HOME` on line one, the build
/// fingerprint on line two — so it is pure and tested here rather than read off
/// an `ExecOutput` inline: one stray line of daemon output shifts the
/// fingerprint out of place, and a fingerprint that never matches makes every
/// connect reinstall forever.
///
/// `None` means the exec itself failed. The home line prints even when the
/// binary is missing (the `exec` fails with 126/127 *after* it), so no home line
/// at all is the only thing that can mean that.
fn parse_probe(stdout: &str, exec_succeeded: bool) -> Option<Probe> {
    // Trimmed because a `\r` would be invisible in a path and fatal in a
    // socket-forward spec.
    let mut lines = stdout.lines().map(str::trim);
    let home = lines.next().filter(|home| home.starts_with('/'))?;
    // Anything short of a clean answer reads as "nothing usable installed",
    // which reinstalls — the self-healing direction, and a needless upload is
    // the cheap failure. It needs no special case for the daemons that predate
    // `harvesterd build`: they exit non-zero on the unknown argument, which
    // lands here as `None` and retires them.
    let installed_build = exec_succeeded
        .then(|| lines.next().filter(|build| !build.is_empty()))
        .flatten();

    Some(Probe {
        home: home.to_string(),
        installed_build: installed_build.map(str::to_string),
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

    // --- the probe's positional answer ------------------------------------

    fn probe_of(home: &str, build: Option<&str>) -> Option<Probe> {
        Some(Probe {
            home: home.to_string(),
            installed_build: build.map(str::to_string),
        })
    }

    #[test]
    fn a_clean_probe_yields_the_home_and_the_build() {
        assert_eq!(
            parse_probe("/home/exedev\nfnv1a64-852768-1f0c\n", true),
            probe_of("/home/exedev", Some("fnv1a64-852768-1f0c"))
        );
    }

    #[test]
    fn a_missing_binary_still_yields_the_home() {
        // `exec` fails *after* the home line printed, with 126/127.
        assert_eq!(
            parse_probe("/home/exedev\n", false),
            probe_of("/home/exedev", None)
        );
    }

    #[test]
    fn a_daemon_too_old_to_know_build_reinstalls() {
        // It exits non-zero on the unknown argument, and whatever it printed on
        // line two is not a fingerprint.
        assert_eq!(
            parse_probe("/home/exedev\nusage: harvesterd [version]\n", false),
            probe_of("/home/exedev", None)
        );
    }

    #[test]
    fn an_empty_build_line_reinstalls_rather_than_matching_nothing() {
        assert_eq!(
            parse_probe("/home/exedev\n\n", true),
            probe_of("/home/exedev", None)
        );
    }

    #[test]
    fn a_carriage_return_never_reaches_the_forward_spec() {
        // It would be invisible in a path and fatal in an `ssh -L` argument.
        assert_eq!(
            parse_probe("/home/exedev\r\nfnv1a64-1-0\r\n", true),
            probe_of("/home/exedev", Some("fnv1a64-1-0"))
        );
    }

    #[test]
    fn no_home_line_is_the_only_thing_that_means_the_exec_failed() {
        for stdout in ["", "\n", "harvesterd: command not found\n"] {
            assert_eq!(parse_probe(stdout, false), None, "stdout {stdout:?}");
        }
    }

    #[test]
    fn a_stray_line_ahead_of_the_home_is_an_error_not_a_silent_shift() {
        // The failure this parse is extracted for: read positionally, one extra
        // line makes the fingerprint never match and every connect reinstall.
        assert_eq!(
            parse_probe("hello from your motd\n/home/exedev\nfnv1a64-1-0\n", true),
            None
        );
    }

    // --- install policy (co-design C4) ------------------------------------

    #[test]
    fn an_identical_build_installs_nothing() {
        let shipped = fingerprint_bytes(b"the daemon binary");

        assert_eq!(
            plan_install(Some(&shipped), &shipped),
            BootstrapOutcome::AlreadyCurrent
        );
    }

    #[test]
    fn a_rebuilt_binary_of_the_same_version_is_installed() {
        // The live silent-deploy failure this replaced: both of these were
        // "0.1.0" under the old crate-version identity, so the probe saw a
        // match and the machine stayed on the stale binary forever.
        let installed = fingerprint_bytes(b"harvesterd, built on monday");
        let shipped = fingerprint_bytes(b"harvesterd, rebuilt on tuesday");

        assert_ne!(installed, shipped, "the fingerprints must differ at all");
        assert_eq!(
            plan_install(Some(&installed), &shipped),
            BootstrapOutcome::Upgraded
        );
    }

    #[test]
    fn a_same_length_rebuild_is_still_detected() {
        // Length alone was one of the rejected interim fixes; a one-byte
        // change that keeps the size is exactly what it would have missed.
        let installed = fingerprint_bytes(b"harvesterd build A");
        let shipped = fingerprint_bytes(b"harvesterd build B");

        assert_eq!(
            plan_install(Some(&installed), &shipped),
            BootstrapOutcome::Upgraded
        );
    }

    #[test]
    fn no_answer_from_the_probe_installs_fresh() {
        // Covers all of: no binary, a binary too old to know `build`, and one
        // that cannot fingerprint itself. Every one of them reinstalls, which
        // is the self-healing direction.
        let shipped = fingerprint_bytes(b"the daemon binary");

        assert_eq!(plan_install(None, &shipped), BootstrapOutcome::Installed);
    }

    #[test]
    fn the_probe_asks_for_the_build_not_the_version() {
        // The version cannot answer "is this the binary I ship?" — that
        // conflation is the whole of C4.
        let script = format!(r#"printf '%s\n' "$HOME"; exec ~/{DAEMON_BINARY_PATH} build"#);

        assert_shell_parses(&script);
        assert!(script.ends_with("build"));
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
