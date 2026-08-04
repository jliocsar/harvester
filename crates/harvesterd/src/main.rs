//! harvesterd — per-machine daemon owning `pi --mode rpc` children.
//! Contract: docs/DAEMON.md (protocol v1). Stateless by design: pi's session
//! files are the only source of truth; we hold pids, pipes and subscribers.

mod daemon;
mod fslist;
mod ops;
mod passthrough;
mod server;
mod session;
mod sid;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;

/// The single-instance lock, next to the socket in `~/.harvester`. A lock file
/// rather than the socket file because the socket cannot answer the question:
/// no exit path unlinks it, so a stale socket is the normal case after a crash
/// or a `pkill`, and "connect refused therefore unlink" races a second daemon
/// doing exactly the same thing.
const LOCK_FILE_NAME: &str = "harvesterd.lock";

/// The socket path and its parent directory, both derived from the one shared
/// constant so the client and the daemon cannot disagree about where to meet.
fn socket_and_dir() -> (PathBuf, PathBuf) {
    let home = std::env::var("HOME").expect("HOME must be set");
    let socket = PathBuf::from(home).join(harvester_proto::DAEMON_SOCKET_PATH);
    let dir = socket
        .parent()
        .expect("socket path has a parent")
        .to_path_buf();

    (socket, dir)
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

/// Unlink whatever sits at the socket path. Only ever called with the
/// single-instance lock held, which is what makes it safe: the one process
/// that could be listening there would have had to take the lock first.
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // CLI surface is exactly two forms: `harvesterd` (run) and
    // `harvesterd version` (bootstrap handshake). An arg parser arrives with
    // the third form, not before.
    match std::env::args().nth(1) {
        None => {}
        Some(arg) if arg == "version" => {
            println!("{}", harvester_proto::PROTOCOL_VERSION);
            return;
        }
        Some(arg) => {
            eprintln!("unknown argument '{arg}' (usage: harvesterd [version])");
            std::process::exit(2);
        }
    }

    let (sock_path, dir) = socket_and_dir();
    let log_dir = dir.join("log");
    std::fs::create_dir_all(&log_dir).expect("create ~/.harvester/log");
    // Socket auth *is* these permissions (docs/DAEMON.md §Transport): anyone
    // who can reach the socket can drive pi sessions. Enforced here, in the
    // process that owns the socket, so it holds however the daemon was
    // started and whatever the machine's umask is.
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

    let daemon = daemon::Daemon::new(log_dir);
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
