mod bootstrap;
pub mod bridge;
mod client;
mod connection;
pub mod dialog;
mod dialog_obligation;
mod fleet;
mod host;
mod machine;
pub mod machines;
mod provider;
mod ssh;
mod token;
mod tokio_thread;
pub mod transcript;
mod workspace;

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
pub use client::{DaemonClient, SessionEvent};
#[cfg(feature = "raw-protocol")]
pub use connection::raw;
pub use connection::{DaemonConnection, DaemonReader, DaemonWriter};
pub use dialog::{DialogAnswer, DialogKind, DialogRequest};
pub use dialog_obligation::{
    AnsweredDialog, DialogKey, DialogObligation, DialogObligations, DialogRoom, EnqueueResult,
    Retirement,
};
pub use fleet::{
    BridgeEvents, Fleet, FleetError, FleetEvents, MachineCandidate, MachineState, MachinesEvents,
    Unapplied,
};
pub use host::Host;
pub use machine::{MachineRef, LOCAL_MACHINE};
pub use ssh::SshTarget;
pub use token::{Pending, RequestToken, RequestTokens};
pub use transcript::Transcript;
pub use workspace::{
    list_directory, resolve_git_branch, resolve_home, validate_git_workspace, RemoteDirEntry,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Exec {
        context: &'static str,
        detail: String,
    },
    DaemonConnect(String),
    VersionMismatch {
        expected: String,
        actual: String,
    },
    Provider {
        provider: String,
        detail: String,
    },
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
