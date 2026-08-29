mod bootstrap;
pub mod bridge;
mod connection;
mod dialog;
mod fleet;
mod host;
pub mod machines;
mod provider;
mod token;
pub mod transcript;

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
#[cfg(feature = "raw-protocol")]
pub use connection::raw;
pub use connection::{DaemonClient, DaemonConnection, DaemonReader, DaemonWriter, SessionEvent};
pub use dialog::{
    AnsweredDialog, DialogAnswer, DialogKey, DialogKind, DialogObligation, DialogObligations,
    DialogRequest, DialogRoom, EnqueueResult,
};
pub use fleet::{
    BridgeEvents, Fleet, FleetError, FleetEvents, MachineCandidate, MachineState, MachinesEvents,
    Unapplied, LOCAL_MACHINE,
};
pub use host::{Host, SshTarget};
pub use machines::RemoteDirEntry;
pub use token::{Pending, RequestToken, RequestTokens};
pub use transcript::Transcript;

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
