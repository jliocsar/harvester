//! Client-side library for Harvester: bootstrap harvesterd on a machine over
//! plain SSH, forward its Unix socket, and speak the docs/DAEMON.md envelope.
//! Consumed by the gpui client and by the `harvester-cli` dev tool.
//!
//! The one entry point that matters is [`bootstrap_and_connect`]: it performs
//! the whole first-connect contract (probe → install/upgrade → start → forward
//! → ping version handshake) and hands back a live [`DaemonConnection`].

mod bootstrap;
mod connection;
mod local;
mod provider;
mod ssh;
mod workspace;

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
pub use connection::{DaemonConnection, DaemonReader, DaemonWriter};
pub use local::{bootstrap_and_connect_local, list_directory_local, validate_git_workspace_local};
pub use provider::{
    discover_providers, install_bundled_providers, MachineKey, MachineStatus, Provider,
    ProviderInfo, ProviderMachine, SshInfo, SUPPORTED_PROVIDER_PROTOCOL,
};
pub use ssh::{probe_reachable, SshTarget};
pub use workspace::{list_directory, validate_git_workspace, RemoteDirEntry};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// Local I/O: spawning ssh, the forwarded socket, serialization.
    Io(std::io::Error),
    /// A remote exec over SSH failed (transport error or non-zero exit).
    Ssh {
        context: &'static str,
        detail: String,
    },
    /// The socket forward or the daemon handshake never came up.
    Forward(String),
    /// The daemon still reports the wrong version after a forced reinstall —
    /// almost always a stale local daemon binary (rebuild the musl target).
    VersionMismatch { expected: String, actual: String },
    /// A provider executable misbehaved: contract violation, timeout, or its
    /// own `{"error"}` object (surfaced verbatim, docs/PROVIDERS.md).
    Provider { provider: String, detail: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(error) => write!(f, "{error}"),
            Error::Ssh { context, detail } => write!(f, "{context}: {detail}"),
            Error::Forward(detail) => write!(f, "socket forward: {detail}"),
            Error::VersionMismatch { expected, actual } => write!(
                f,
                "daemon reports version {actual} after reinstall (expected {expected}) — \
                 is the daemon binary you are shipping stale?"
            ),
            Error::Provider { provider, detail } => write!(f, "provider {provider}: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}
