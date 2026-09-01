mod bootstrap;
mod bridge;
mod connection;
mod dialog;
mod fleet;
mod host;
mod identity;
mod machines;
mod model;
mod provider;
mod replica;
mod runtime;
mod session_index;
mod token;
mod transcript;
mod workspace;

pub(crate) const PI_AGENT_SETTLED: &str = "agent_settled";

pub use bootstrap::{bootstrap_and_connect, BootstrapOutcome, Connected};
#[cfg(feature = "raw-protocol")]
pub use connection::raw;
pub use connection::{DaemonClient, DaemonConnection, DaemonReader, DaemonWriter, SessionEvent};
pub use dialog::{DialogAnswer, DialogKey, DialogKind, DialogObligation, DialogRequest};
pub use host::{folder_name, Host, SshTarget};
pub use identity::{
    ChatId, DraftId, MachineId, SessionId, SessionKey, SubmissionId, WorkspaceBrowserId,
};
pub use machines::RemoteDirEntry;
pub use model::{ModelChoice, ModelSelection, ModelsState};
pub use provider::SUPPORTED_PROVIDER_PROTOCOL;
pub use replica::SessionReplica;
pub use runtime::{
    ChatTarget, ClientRuntime, ClientRuntimeOwner, ClientSnapshot, DraftLifecycle, DraftSession,
    LiveSession, MachineSnapshot, MachineStatus, RepairState, ReplayState, RuntimeEvent,
    RuntimeEvents, SessionLifecycle, WorkspaceBrowser, WorkspaceBrowserStatus,
};
pub use session_index::SessionSummary;
pub use transcript::{message_text, Item, Message, Role, ToolBlock, ToolStatus, Transcript};
pub use workspace::WorkspaceId;

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
    NotConnectable {
        machine: String,
    },
    HostWorkerMissing,
    HostWorkerStopped,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Exec { context, detail } => write!(f, "{context}: {detail}"),
            Self::DaemonConnect(detail) => write!(f, "daemon connect: {detail}"),
            Self::VersionMismatch { expected, actual } => write!(
                f,
                "daemon reports version {actual} after reinstall (expected {expected}) — \
                 is the daemon binary you are shipping stale?"
            ),
            Self::Provider { provider, detail } => write!(f, "provider {provider}: {detail}"),
            Self::NotConnectable { machine } => {
                write!(f, "machine {machine} is not connectable")
            }
            Self::HostWorkerMissing => write!(f, "machines worker is not running"),
            Self::HostWorkerStopped => write!(f, "machines worker stopped"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
pub(crate) mod scratch {
    use std::path::PathBuf;

    pub(crate) struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        pub(crate) fn create(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("harvester-{label}-{}", std::process::id()));

            std::fs::create_dir_all(&path).expect("a scratch directory under the temp dir");

            Self { path }
        }

        pub(crate) fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.path) {
                eprintln!(
                    "harvester: leaving scratch directory {} behind: {error}",
                    self.path.display()
                );
            }
        }
    }
}
