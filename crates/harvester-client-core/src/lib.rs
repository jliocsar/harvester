//! The client backbone for Harvester: bootstrap harvesterd on a machine
//! (plain SSH or the local shell — one exec boundary, `Host`), forward its
//! Unix socket, and speak the docs/DAEMON.md envelope. Deliberately gpui-free.
//!
//! Three layers, bottom up. *One machine at a time*: `bridge` (that machine's
//! daemon connection, command flows and event fan-in) and `machines`
//! (provider listing, reachability probes, and the daemon-free host ops the
//! folder picker runs). *Machine identity*: [`MachineRef`], the one place
//! that decides what a stored machine string means. *Across machines*:
//! [`Fleet`], which owns the bridge pool, the machine table, the probe
//! verdicts and the connect policy, and models a machine's reachability as
//! one [`MachineState`] so a probe verdict and a daemon link cannot disagree.
//!
//! So a second frontend gets the whole backbone: connect a machine, list and
//! open sessions, stream a turn, browse folders, read every machine's state,
//! and the two policies that are easy to get subtly wrong — when to start a
//! bridge, and which of a probe and a link to believe. Everything it drives
//! is synchronous and event-driven; nothing here needs a tokio reactor on the
//! caller's side (see `fleet`'s module docs).
//!
//! What it does *not* get, and would still write itself: the workspace store
//! (`~/.harvester/workspaces.json` lives in the gpui crate), the merged
//! newest-first session list across machines, and every rendering decision —
//! which colour a [`MachineState`] is, what a session row says, how the ghost
//! chat behaves.
//!
//! Three entry points matter: [`Fleet::start`] (the whole client backbone),
//! [`bridge::start`] (one machine's worker, if a frontend wants only that)
//! and, underneath both, [`bootstrap_and_connect`] + [`DaemonClient`] for
//! anyone driving the protocol directly (the `harvester-cli` dev tool does).

mod bootstrap;
pub mod bridge;
mod client;
mod connection;
mod fleet;
mod host;
mod machine;
pub mod machines;
mod provider;
mod ssh;
mod token;
mod workspace;

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
pub use client::{DaemonClient, SessionEvent};
pub use connection::{DaemonConnection, DaemonReader, DaemonWriter};
pub use fleet::{Fleet, FleetError, FleetEvents, MachineCandidate, MachineState};
pub use host::Host;
pub use machine::{MachineRef, LOCAL_MACHINE};
pub use provider::{
    discover_providers, install_bundled_providers, MachineKey, MachineStatus, Provider,
    ProviderMachine, SshInfo, SUPPORTED_PROVIDER_PROTOCOL,
};
pub use ssh::SshTarget;
pub use token::{Pending, RequestToken, RequestTokens};
pub use workspace::{list_directory, resolve_home, validate_git_workspace, RemoteDirEntry};

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
