use std::time::Duration;

use harvester_proto::{fingerprint_bytes, PingData, PROTOCOL_VERSION};

use crate::connection::{self, DaemonConnection};
use crate::host::{parse_home, Host, HOME_SCRIPT};
use crate::{Error, Result};

const BOOTSTRAP_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) const DAEMON_BINARY_PATH: &str = ".harvester/bin/harvesterd";
pub(crate) const DAEMON_LOG_PATH: &str = ".harvester/log/harvesterd.log";

#[derive(Clone, Copy, PartialEq, Eq)]
enum KillRunningDaemon {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    AlreadyCurrent,
    Installed,
    Upgraded,
}

pub struct Connected {
    pub connection: DaemonConnection,
    pub daemon_version: String,
    pub outcome: BootstrapOutcome,
}

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

    drop(connection);
    install_and_start(host, daemon_binary, KillRunningDaemon::Yes).await?;

    let (connection, ping) = connection::connect_host(host, &probe.home).await?;

    if ping.build != shipped_build {
        eprintln!(
            "harvester: daemon still reports build {} after a forced reinstall (we ship {shipped_build}); \
             continuing because the protocol version is what governs compatibility",
            ping.build
        );
    }

    connected_if_protocol_matches(connection, ping, BootstrapOutcome::Upgraded)
}

fn plan_install(installed_build: Option<&str>, shipped_build: &str) -> BootstrapOutcome {
    match installed_build {
        Some(build) if build == shipped_build => BootstrapOutcome::AlreadyCurrent,
        Some(_) => BootstrapOutcome::Upgraded,
        None => BootstrapOutcome::Installed,
    }
}

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

async fn probe(host: &Host) -> Result<Probe> {
    let output = host
        .exec(&probe_script(), None, BOOTSTRAP_EXEC_TIMEOUT)
        .await?;

    parse_probe(&output.stdout, output.status.success()).ok_or_else(|| Error::Exec {
        context: "probe home",
        detail: output.describe_failure(),
    })
}

fn probe_script() -> String {
    format!("{HOME_SCRIPT}; exec ~/{DAEMON_BINARY_PATH} build")
}

fn parse_probe(stdout: &str, exec_succeeded: bool) -> Option<Probe> {
    let home = parse_home(stdout)?;
    let installed_build = exec_succeeded
        .then(|| {
            stdout
                .lines()
                .nth(1)
                .map(str::trim)
                .filter(|build| !build.is_empty())
        })
        .flatten();

    Some(Probe {
        home: home.to_string(),
        installed_build: installed_build.map(str::to_string),
    })
}

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

fn install_script(kill_running_daemon: KillRunningDaemon, binary_len: usize) -> String {
    let replace = format!(
        "mkdir -p ~/.harvester/bin \
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            parse_probe("/home/exedev\n", false),
            probe_of("/home/exedev", None)
        );
    }

    #[test]
    fn a_daemon_too_old_to_know_build_reinstalls() {
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
        assert_eq!(
            parse_probe("hello from your motd\n/home/exedev\nfnv1a64-1-0\n", true),
            None
        );
    }

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
        let installed = fingerprint_bytes(b"harvesterd build A");
        let shipped = fingerprint_bytes(b"harvesterd build B");

        assert_eq!(
            plan_install(Some(&installed), &shipped),
            BootstrapOutcome::Upgraded
        );
    }

    #[test]
    fn no_answer_from_the_probe_installs_fresh() {
        let shipped = fingerprint_bytes(b"the daemon binary");

        assert_eq!(plan_install(None, &shipped), BootstrapOutcome::Installed);
    }

    #[test]
    fn the_probe_asks_for_the_build_not_the_version() {
        let script = probe_script();

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
