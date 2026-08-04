//! The client backbone for Harvester: bootstrap harvesterd on a machine
//! (plain SSH or the local shell — one exec boundary, `Host`), forward its
//! Unix socket, and speak the docs/DAEMON.md envelope. Deliberately gpui-free:
//! this crate plus its `bridge`/`machines` workers is the whole client any
//! frontend (gpui today, mobile later) would reuse.
//!
//! Two entry points matter: [`bridge::start`] (one machine's full worker —
//! bootstrap, connect, command flows, event fan-in) and, underneath it,
//! [`bootstrap_and_connect`] + [`DaemonClient`] for anyone driving the
//! protocol directly (the `harvester-cli` dev tool does).

mod bootstrap;
pub mod bridge;
mod client;
mod connection;
mod host;
pub mod machines;
mod provider;
mod ssh;
mod workspace;

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
pub use client::{DaemonClient, SessionEvent};
pub use connection::{DaemonConnection, DaemonReader, DaemonWriter};
pub use host::Host;
pub use provider::{
    discover_providers, install_bundled_providers, MachineKey, MachineStatus, Provider,
    ProviderMachine, SshInfo, SUPPORTED_PROVIDER_PROTOCOL,
};
pub use ssh::{probe_reachable, SshTarget};
pub use workspace::{list_directory, validate_git_workspace, RemoteDirEntry};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// Local I/O: spawning ssh, the forwarded socket, serialization.
    Io(std::io::Error),
    /// A control-plane exec failed (transport error or non-zero exit) —
    /// over SSH or through the local shell, same boundary (`host.rs`).
    Exec {
        context: &'static str,
        detail: String,
    },
    /// The daemon never became reachable: the socket forward failed to come
    /// up, or the handshake went unanswered. Covers the local machine too,
    /// where there is no forward at all.
    DaemonConnect(String),
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
            Error::Exec { context, detail } => write!(f, "{context}: {detail}"),
            Error::DaemonConnect(detail) => write!(f, "daemon connect: {detail}"),
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
