use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    DialogObligation, DialogRequest, DraftId, MachineId, ModelSelection, ModelsState,
    RemoteDirEntry, SessionKey, SessionReplica, SessionSummary, WorkspaceBrowserId, WorkspaceId,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineStatus {
    Missing,
    Unknown,
    Reachable,
    Unreachable,
    Connecting,
    Connected,
    ConnectFailed(String),
    ConnectionLost,
}

impl MachineStatus {
    pub const fn is_live(&self) -> bool {
        matches!(self, Self::Connecting | Self::Connected)
    }

    pub const fn is_available(&self) -> bool {
        !matches!(self, Self::Missing | Self::Unreachable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSnapshot {
    pub id: MachineId,
    pub label: String,
    pub provider: Option<String>,
    pub status: MachineStatus,
    pub connectable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatTarget {
    pub machine: Option<MachineId>,
    pub workspace: Option<WorkspaceId>,
}

impl ChatTarget {
    pub const fn is_resolved(&self) -> bool {
        self.machine.is_some() && self.workspace.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftLifecycle {
    Ready,
    Starting(SessionKey),
}

#[derive(Debug, Clone)]
pub struct DraftSession {
    pub id: DraftId,
    pub target: ChatTarget,
    pub lifecycle: DraftLifecycle,
    pub models: ModelsState,
    pub model_selection: ModelSelection,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLifecycle {
    Opening,
    Starting,
    Attached,
    Failed(String),
    Disconnected(String),
    Exited(Option<i32>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairState {
    Idle,
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayState {
    pub cursor: Option<String>,
    pub repair: RepairState,
}

#[derive(Debug, Clone)]
pub struct LiveSession {
    pub key: SessionKey,
    pub workspace: WorkspaceId,
    pub lifecycle: SessionLifecycle,
    pub attachment_generation: u64,
    pub replay: ReplayState,
    pub model_catalog: ModelsState,
    pub model_selection: ModelSelection,
    pub branch: Option<String>,
    pub replica: SessionReplica,
    pub dialogs: Vec<DialogRequest>,
    pub(super) event_revision: u64,
}

impl LiveSession {
    pub const fn can_stop(&self) -> bool {
        matches!(self.lifecycle, SessionLifecycle::Attached) && self.replica.transcript().is_busy()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceBrowserStatus {
    LoadingHome,
    LoadingDirectory,
    Idle,
    Validating,
}

#[derive(Debug, Clone)]
pub struct WorkspaceBrowser {
    pub id: WorkspaceBrowserId,
    pub machine: MachineId,
    pub machine_label: String,
    pub path: String,
    pub entries: Vec<RemoteDirEntry>,
    pub status: WorkspaceBrowserStatus,
    pub error: Option<String>,
}

impl WorkspaceBrowser {
    pub const fn can_submit(&self) -> bool {
        matches!(self.status, WorkspaceBrowserStatus::Idle) && !self.path.is_empty()
    }

    pub const fn busy(&self) -> bool {
        matches!(
            self.status,
            WorkspaceBrowserStatus::LoadingHome
                | WorkspaceBrowserStatus::LoadingDirectory
                | WorkspaceBrowserStatus::Validating
        )
    }

    pub fn has_parent(&self) -> bool {
        !self.path.is_empty() && self.path != "/"
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClientSnapshot {
    pub(super) revision: u64,
    pub(super) startup_machine: Option<MachineId>,
    pub(super) machines: Arc<Vec<MachineSnapshot>>,
    pub(super) workspaces: Arc<Vec<WorkspaceId>>,
    pub(super) sessions: Arc<Vec<SessionSummary>>,
    pub(super) drafts: Arc<HashMap<DraftId, DraftSession>>,
    pub(super) live_sessions: Arc<HashMap<SessionKey, Arc<LiveSession>>>,
    pub(super) browsers: Arc<HashMap<WorkspaceBrowserId, WorkspaceBrowser>>,
    pub(super) pending_dialog: Option<DialogObligation>,
    pub(super) fatal_error: Option<String>,
}

impl ClientSnapshot {
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn startup_machine(&self) -> Option<&MachineId> {
        self.startup_machine.as_ref()
    }

    pub fn machines(&self) -> &[MachineSnapshot] {
        &self.machines
    }

    pub fn machine(&self, id: &MachineId) -> Option<&MachineSnapshot> {
        self.machines.iter().find(|machine| &machine.id == id)
    }

    pub fn machine_label(&self, id: &MachineId) -> String {
        self.machine(id)
            .map(|machine| machine.label.clone())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn machine_candidates(&self) -> Vec<&MachineSnapshot> {
        self.machines
            .iter()
            .filter(|machine| machine.connectable && machine.status.is_available())
            .collect()
    }

    pub fn workspaces(&self) -> &[WorkspaceId] {
        &self.workspaces
    }

    pub fn repos_on(&self, machine: &MachineId) -> Vec<&WorkspaceId> {
        self.workspaces
            .iter()
            .rev()
            .filter(|workspace| workspace.machine() == machine)
            .collect()
    }

    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    pub fn draft(&self, id: DraftId) -> Option<&DraftSession> {
        self.drafts.get(&id)
    }

    pub fn live_session(&self, key: &SessionKey) -> Option<&LiveSession> {
        self.live_sessions.get(key).map(AsRef::as_ref)
    }

    pub fn browser(&self, id: WorkspaceBrowserId) -> Option<&WorkspaceBrowser> {
        self.browsers.get(&id)
    }

    pub const fn pending_dialog(&self) -> Option<&DialogObligation> {
        self.pending_dialog.as_ref()
    }

    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal_error.as_deref()
    }

    pub fn connected_machine_count(&self) -> usize {
        self.machines
            .iter()
            .filter(|machine| machine.status == MachineStatus::Connected)
            .count()
    }

    pub fn linked_machine_count(&self) -> usize {
        self.machines
            .iter()
            .filter(|machine| {
                matches!(
                    machine.status,
                    MachineStatus::Connecting
                        | MachineStatus::Connected
                        | MachineStatus::ConnectFailed(_)
                        | MachineStatus::ConnectionLost
                )
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(id: &str, status: MachineStatus, connectable: bool) -> MachineSnapshot {
        MachineSnapshot {
            id: MachineId::new(id).expect("valid machine"),
            label: id.to_string(),
            provider: None,
            status,
            connectable,
        }
    }

    #[test]
    fn unknown_connectable_machines_are_available_before_the_first_probe() {
        let snapshot = ClientSnapshot {
            machines: Arc::new(vec![
                machine("unknown", MachineStatus::Unknown, true),
                machine("missing", MachineStatus::Missing, false),
                machine("unreachable", MachineStatus::Unreachable, true),
            ]),
            ..ClientSnapshot::default()
        };
        let candidates: Vec<&str> = snapshot
            .machine_candidates()
            .into_iter()
            .map(|machine| machine.id.as_str())
            .collect();

        assert_eq!(candidates, ["unknown"]);
    }
}
