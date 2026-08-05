//! Every path the daemon touches, derived from `$HOME` in exactly one place.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

/// The rendezvous, spelled with the constant the client reads too, so the two
/// cannot disagree about where to meet.
pub fn socket() -> PathBuf {
    home().join(harvester_proto::DAEMON_SOCKET_PATH)
}

/// `~/.harvester`: the socket's parent, and the 0700 that is the daemon's whole
/// authentication story (docs/DAEMON.md §Transport).
pub fn harvester_dir() -> PathBuf {
    socket()
        .parent()
        .expect("socket path has a parent")
        .to_path_buf()
}

/// Where each session's `<sid>.stderr.log` lands — the only exit-diagnosis
/// channel the contract gives clients.
pub fn log_dir() -> PathBuf {
    harvester_dir().join("log")
}

/// Pi's own per-cwd session dirs. Read-only to us: they are the source of truth.
pub fn pi_sessions_root() -> PathBuf {
    home().join(".pi/agent/sessions")
}
