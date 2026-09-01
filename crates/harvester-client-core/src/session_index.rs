use std::collections::HashMap;

use harvester_proto::{SessionInfo, SessionStatus};

use crate::{MachineId, SessionId, SessionKey, WorkspaceId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub name: Option<String>,
    pub workspace: WorkspaceId,
    pub file: Option<String>,
    pub mtime_ms: u64,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionIndex {
    by_machine: HashMap<MachineId, Vec<SessionSummary>>,
    merged: Vec<SessionSummary>,
}

impl SessionIndex {
    pub(crate) fn replace_machine(&mut self, machine: MachineId, sessions: Vec<SessionInfo>) {
        let rows = sessions
            .into_iter()
            .filter_map(|info| summary_from_info(&machine, info))
            .collect();

        self.by_machine.insert(machine, rows);
        self.rebuild();
    }

    pub(crate) fn insert_started(
        &mut self,
        key: SessionKey,
        workspace: WorkspaceId,
        name: String,
        mtime_ms: u64,
    ) {
        let machine = key.machine().clone();
        let rows = self.by_machine.entry(machine).or_default();

        rows.retain(|row| row.key != key);
        rows.push(SessionSummary {
            key,
            name: Some(name),
            workspace,
            file: None,
            mtime_ms,
            status: SessionStatus::Running,
        });
        self.rebuild();
    }

    pub(crate) fn set_status(&mut self, key: &SessionKey, status: SessionStatus) -> bool {
        let Some(rows) = self.by_machine.get_mut(key.machine()) else {
            return false;
        };
        let mut changed = false;

        for row in rows.iter_mut().filter(|row| &row.key == key) {
            if row.status != status {
                row.status = status;
                changed = true;
            }
        }

        if changed {
            self.rebuild();
        }

        changed
    }

    pub(crate) fn remove_session(&mut self, key: &SessionKey) {
        let Some(rows) = self.by_machine.get_mut(key.machine()) else {
            return;
        };

        rows.retain(|row| &row.key != key);
        self.rebuild();
    }

    pub(crate) fn remove_machine(&mut self, machine: &MachineId) {
        self.by_machine.remove(machine);
        self.rebuild();
    }

    pub(crate) fn session(&self, key: &SessionKey) -> Option<&SessionSummary> {
        self.by_machine
            .get(key.machine())?
            .iter()
            .find(|row| &row.key == key)
    }

    #[cfg(test)]
    pub(crate) fn merged(&self) -> &[SessionSummary] {
        &self.merged
    }

    pub(crate) fn visible<'a>(
        &'a self,
        workspaces: &'a [WorkspaceId],
    ) -> impl Iterator<Item = &'a SessionSummary> + 'a {
        self.merged.iter().filter(move |row| {
            row.key.machine().as_str() != "local" || workspaces.contains(&row.workspace)
        })
    }

    fn rebuild(&mut self) {
        self.merged = self.by_machine.values().flatten().cloned().collect();
        self.merged.sort_by(|a, b| {
            b.mtime_ms
                .cmp(&a.mtime_ms)
                .then_with(|| b.key.session().cmp(a.key.session()))
                .then_with(|| a.key.machine().cmp(b.key.machine()))
        });
    }
}

fn summary_from_info(machine: &MachineId, info: SessionInfo) -> Option<SessionSummary> {
    let session = SessionId::new(info.sid)?;
    let workspace = WorkspaceId::new(machine.clone(), info.cwd)?;

    Some(SessionSummary {
        key: SessionKey::new(machine.clone(), session),
        name: info.name,
        workspace,
        file: info.file,
        mtime_ms: info.mtime_ms,
        status: info.status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(sid: &str, cwd: &str, mtime_ms: u64) -> SessionInfo {
        SessionInfo {
            sid: sid.to_string(),
            name: None,
            cwd: cwd.to_string(),
            file: None,
            mtime_ms,
            status: SessionStatus::Exited,
        }
    }

    #[test]
    fn listings_are_replaced_per_machine_and_merged_by_activity() {
        let first = MachineId::new("first").expect("non-empty");
        let second = MachineId::new("second").expect("non-empty");
        let mut index = SessionIndex::default();

        index.replace_machine(first.clone(), vec![info("old", "/a", 1)]);
        index.replace_machine(second, vec![info("new", "/b", 3)]);
        index.replace_machine(first, vec![info("middle", "/a", 2)]);

        let sessions: Vec<&str> = index
            .merged()
            .iter()
            .map(|row| row.key.session().as_str())
            .collect();

        assert_eq!(sessions, ["new", "middle"]);
    }

    #[test]
    fn removing_one_session_preserves_other_machine_rows_with_the_same_sid() {
        let first = MachineId::new("first").expect("non-empty");
        let second = MachineId::new("second").expect("non-empty");
        let first_key = SessionKey::new(first.clone(), SessionId::new("same").expect("non-empty"));
        let mut index = SessionIndex::default();

        index.replace_machine(first, vec![info("same", "/a", 1)]);
        index.replace_machine(second, vec![info("same", "/b", 1)]);
        index.remove_session(&first_key);

        assert_eq!(index.merged().len(), 1);
        assert_eq!(index.merged()[0].key.machine().as_str(), "second");
    }

    #[test]
    fn equal_session_ids_on_two_machines_remain_distinct() {
        let first = MachineId::new("first").expect("non-empty");
        let second = MachineId::new("second").expect("non-empty");
        let mut index = SessionIndex::default();

        index.replace_machine(first, vec![info("same", "/a", 1)]);
        index.replace_machine(second, vec![info("same", "/b", 1)]);

        assert_eq!(index.merged().len(), 2);
        assert_ne!(index.merged()[0].key, index.merged()[1].key);
    }
}
