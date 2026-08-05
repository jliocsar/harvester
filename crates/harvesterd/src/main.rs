//! harvesterd — per-machine daemon owning `pi --mode rpc` children.
//! Contract: docs/DAEMON.md (protocol v1). Stateless by design: pi's session
//! files are the only source of truth; we hold pids, pipes and subscribers.

mod daemon;
mod fslist;
mod ops;
mod passthrough;
mod paths;
mod server;
mod session;
mod sid;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::UnixListener;

/// The single-instance lock, next to the socket in `~/.harvester`. A lock file
/// rather than the socket file because the socket cannot answer the question:
/// no exit path unlinks it, so a stale socket is the normal case after a crash
/// or a `pkill`, and "connect refused therefore unlink" races a second daemon
/// doing exactly the same thing.
const LOCK_FILE_NAME: &str = "harvesterd.lock";

/// Read rather than `std::env::current_exe()`'s path so the answer stays right
/// after an upgrade has replaced the file underneath: this is the image that is
/// *running*, which is exactly what `ping` claims to report (docs/DAEMON.md
/// §ping). For `harvesterd build` the process was just launched from the file on
/// disk, so the same read answers the on-disk question too.
const RUNNING_EXECUTABLE: &str = "/proc/self/exe";

/// Reported when the executable cannot be read at all. Never equal to a real
/// fingerprint, so a client comparing for equality re-installs — the safe
/// direction: a needless upload beats a machine pinned to a stale daemon.
const UNAVAILABLE_FINGERPRINT: &str = "unavailable";

/// The hash itself lives in `harvester-proto` because the client must compute
/// the identical string; which file to read is what is daemon-local.
fn fingerprint_running_executable() -> std::io::Result<String> {
    harvester_proto::fingerprint_reader(&mut std::fs::File::open(RUNNING_EXECUTABLE)?)
}

/// Take the single-instance lock or exit. The handle is deliberately leaked:
/// the kernel drops an `flock` when the process dies — however it dies,
/// SIGKILL included — so the lock has no stale state to clean up, and nothing
/// in the daemon may ever be tempted to release it early.
fn acquire_single_instance_lock(dir: &Path, sock_path: &Path) {
    let lock_path = dir.join(LOCK_FILE_NAME);
    let lock = match std::fs::File::create(&lock_path) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("cannot open {}: {error}", lock_path.display());
            std::process::exit(1);
        }
    };

    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            eprintln!("harvesterd already running on {}", sock_path.display());
            std::process::exit(1);
        }
        Err(std::fs::TryLockError::Error(error)) => {
            eprintln!("cannot lock {}: {error}", lock_path.display());
            std::process::exit(1);
        }
    }

    std::mem::forget(lock);
}

/// Only ever called with the single-instance lock held, which is what makes it
/// safe: the one process that could be listening there would have had to take
/// the lock first.
fn remove_stale_socket(sock_path: &Path) {
    match std::fs::remove_file(sock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            eprintln!("cannot unlink {}: {error}", sock_path.display());
            std::process::exit(1);
        }
    }
}

/// `harvesterd build`: the fingerprint of the binary that was just started —
/// i.e. of the file on disk. Fails loudly rather than printing the daemon's
/// degraded sentinel: a client comparing an unreadable answer against a real
/// fingerprint would reinstall on every single connect, forever.
fn print_build_fingerprint() {
    match fingerprint_running_executable() {
        Ok(fingerprint) => println!("{fingerprint}"),
        Err(error) => {
            eprintln!("cannot fingerprint this binary: {error}");
            std::process::exit(1);
        }
    }
}

/// The identity of the *running image*, taken once at startup so it stays true
/// after an upgrade replaces the file underneath us — precisely the case the
/// client needs to detect. Degrades rather than exits: an unreadable
/// `/proc/self/exe` (an exotic mount namespace) must not stop the daemon from
/// serving sessions.
fn startup_build_fingerprint() -> String {
    fingerprint_running_executable().unwrap_or_else(|error| {
        eprintln!("cannot fingerprint {RUNNING_EXECUTABLE}: {error}");

        UNAVAILABLE_FINGERPRINT.to_string()
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Three CLI forms: `harvesterd` (run), `harvesterd version` (the protocol
    // handshake the bootstrap probe reads) and `harvesterd build` (this
    // binary's fingerprint, for install-iff-it-differs). Still a match on one
    // argument — an arg parser would be a dependency bought for nothing.
    match std::env::args().nth(1) {
        None => {}
        Some(arg) if arg == "version" => {
            println!("{}", harvester_proto::PROTOCOL_VERSION);

            return;
        }
        Some(arg) if arg == "build" => {
            print_build_fingerprint();

            return;
        }
        Some(arg) => {
            eprintln!("unknown argument '{arg}' (usage: harvesterd [version|build])");
            std::process::exit(2);
        }
    }

    let sock_path = paths::socket();
    let dir = paths::harvester_dir();
    std::fs::create_dir_all(paths::log_dir()).expect("create ~/.harvester/log");
    // Enforced here, in the process that owns the socket, so it holds however
    // the daemon was started and whatever the machine's umask is.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .expect("chmod 700 ~/.harvester");

    // Lock first, then clear the rendezvous path, then bind: with the lock held
    // no second daemon can be between those steps, which is exactly what the
    // old connect-probe could not guarantee.
    acquire_single_instance_lock(&dir, &sock_path);
    remove_stale_socket(&sock_path);

    let listener = UnixListener::bind(&sock_path).expect("bind unix socket");
    eprintln!(
        "harvesterd {} listening on {}",
        harvester_proto::PROTOCOL_VERSION,
        sock_path.display()
    );

    let daemon = daemon::Daemon::new(paths::log_dir(), startup_build_fingerprint());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move { server::serve_connection(daemon, stream).await });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hash's own behaviour is pinned in `harvester-proto`; what is ours to
    /// prove is that we read the right file and get a real answer — the test
    /// binary is itself a `/proc/self/exe`.
    #[test]
    fn the_running_executable_fingerprints() {
        let fingerprint = fingerprint_running_executable().expect("the test binary is readable");

        assert!(fingerprint.starts_with("fnv1a64-"), "{fingerprint}");
        assert_ne!(fingerprint, UNAVAILABLE_FINGERPRINT);
    }

    /// What makes the sentinel safe: a client comparing it against any
    /// fingerprint sees a difference and re-installs.
    #[test]
    fn the_degraded_answer_is_never_a_real_fingerprint() {
        assert_ne!(
            UNAVAILABLE_FINGERPRINT,
            harvester_proto::fingerprint_bytes(b"")
        );
        assert!(!UNAVAILABLE_FINGERPRINT.starts_with("fnv1a64-"));
    }
}
