//! Plain-SSH plumbing: target addressing and the base invocation shared by
//! one-shot execs (via `host.rs`) and the long-lived socket forward
//! (`connection.rs`).

use std::path::PathBuf;
use std::sync::OnceLock;

/// How to reach a machine. `dest` is handed to ssh verbatim, so both
/// `user@host` and ssh-config aliases work.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub dest: String,
    pub port: Option<u16>,
}

/// Non-interactive by contract: the client must fail fast, never hang on a
/// password or host-key prompt. `accept-new` trusts a first-seen host key
/// without a prompt (fresh VMs would otherwise be unreachable under
/// BatchMode) but still hard-fails if a known key *changes*.
///
/// Multiplexing: the first connection to a machine becomes a master and every
/// later exec rides it, skipping the SSH handshake — a listing then costs
/// about one network round trip (measured on exe.dev: ~1.7s cold, ~0.35s
/// multiplexed), which makes the folder picker's per-click `ls` affordable.
/// `auto` degrades to a plain connection whenever the master is unusable, so
/// this is pure speedup, never a new failure mode.
///
/// Everything transport-level belongs here rather than on an individual
/// invocation, because whichever process becomes the *master* owns the
/// connection: options a mux client passes about the transport (keepalives) or
/// about a forward's listener (`StreamLocalBindUnlink`) are the master's to
/// apply, and are silently ignored on the client. Both landed on the forward
/// invocation once, where they could never fire.
const SSH_BASE_ARGS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ControlMaster=auto",
    "-o",
    // The master lingers this long past its last client, then exits on its
    // own — nothing for us to manage or clean up.
    "ControlPersist=60",
    "-o",
    // A dropped link that never sends a RST (laptop sleep, wifi switch, VM
    // migration) is otherwise invisible: requests carry no deadline, so every
    // pending one would hang until TCP gave up, with the machine still showing
    // as connected. Three missed 15s probes end the connection, which EOFs the
    // socket and surfaces as a real disconnect within ~45s.
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    // Unlink a leftover socket file before binding a `-L` forward: a respawn
    // racing a dying forward's teardown would otherwise fail with "Address
    // already in use".
    "StreamLocalBindUnlink=yes",
];

/// Control-socket filename inside the client socket dir. `%C` is ssh's own
/// short hash of (local host, remote host, port, user) — distinct per target
/// and well under the ~108-byte `sun_path` limit.
const CONTROL_SOCKET_TEMPLATE: &str = "harvester-ssh-%C";

/// Client-owned socket directory (`~/.harvester/run`, 0700). Control sockets
/// and forwards must never live in world-writable /tmp: a predictable path
/// there could be squatted before we bind, and ssh's mux protocol is
/// unauthenticated — a fake mux server would MITM every exec. Mirrors the
/// daemon-side policy (docs/DAEMON.md: socket auth is filesystem permissions).
/// Degrades to the temp dir (with a loud note) only when `$HOME` is unusable.
///
/// Resolved once per process: it is on the path of every ssh invocation the
/// client builds — every probe, every folder-picker click — and it was doing a
/// `create_dir_all` plus a `set_permissions` on each of them.
pub(crate) fn client_socket_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();

    DIR.get_or_init(|| match try_client_socket_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("~/.harvester/run unavailable ({error}); falling back to the temp dir");

            std::env::temp_dir()
        }
    })
}

fn try_client_socket_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME unset"))?;
    let dir = PathBuf::from(home).join(".harvester/run");

    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    Ok(dir)
}

impl SshTarget {
    /// An ssh invocation against this target; `extra_args` land before the
    /// destination (ssh requires options first).
    pub(crate) fn command(&self, extra_args: &[&str]) -> tokio::process::Command {
        let control_path = client_socket_dir().join(CONTROL_SOCKET_TEMPLATE);
        let mut command = tokio::process::Command::new("ssh");
        command.args(SSH_BASE_ARGS);
        command
            .arg("-o")
            .arg(format!("ControlPath={}", control_path.display()));
        command.args(extra_args);

        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }

        command.arg(&self.dest);
        command
    }
}
