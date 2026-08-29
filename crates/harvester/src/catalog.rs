use harvester_client_core::LOCAL_MACHINE;
use harvester_proto::{unix_millis_now, SessionInfo, SessionStatus};

use crate::session_state::ChatTarget;
use crate::workspaces::{self, Workspace};

#[derive(Debug, Clone)]
pub struct MachineSession {
    pub machine: String,
    pub info: SessionInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpen {
    pub machine: String,
    pub sid: String,
    pub cwd: String,
}

pub struct Catalog {
    sessions: Vec<MachineSession>,
    workspaces: Vec<Workspace>,
    opening: Option<OpeningSession>,
    selected_sid: Option<String>,
}

struct OpeningSession {
    machine: String,
    sid: String,
}

impl Catalog {
    pub fn load() -> Self {
        Self::from_workspaces(workspaces::load())
    }

    pub fn from_workspaces(workspaces: Vec<Workspace>) -> Self {
        Self {
            sessions: Vec::new(),
            workspaces,
            opening: None,
            selected_sid: None,
        }
    }

    pub fn record_sessions(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
        self.sessions.retain(|session| session.machine != machine);
        self.sessions
            .extend(sessions.into_iter().map(|info| MachineSession {
                machine: machine.to_string(),
                info,
            }));
        self.sort_sessions();
    }

    pub fn set_session_status(&mut self, sid: &str, status: SessionStatus) -> bool {
        let mut changed = false;

        for session in &mut self.sessions {
            if session.info.sid == sid && session.info.status != status {
                session.info.status = status;
                changed = true;
            }
        }

        changed
    }

    pub fn session_attached(&mut self, machine: &str, sid: &str) -> bool {
        let changed = self.set_session_status(sid, SessionStatus::Running);

        self.selected_sid = Some(sid.to_string());

        self.abandon_open_on(machine) || changed
    }

    pub fn clear_selected_session(&mut self) {
        self.selected_sid = None;
    }

    pub fn is_selected_session(&self, sid: &str) -> bool {
        self.selected_sid.as_deref() == Some(sid)
    }

    fn forget_selection_without_a_session(&mut self) {
        let listed = self.selected_sid.as_ref().is_some_and(|selected| {
            self.sessions
                .iter()
                .any(|session| session.info.sid == *selected)
        });

        if listed {
            return;
        }

        self.selected_sid = None;
    }

    pub fn session_exited(&mut self, sid: &str) -> bool {
        self.set_session_status(sid, SessionStatus::Exited)
    }

    pub fn add_workspace(&mut self, workspace: Workspace) -> std::io::Result<bool> {
        if self.workspaces.contains(&workspace) {
            return Ok(false);
        }

        let mut next = self.workspaces.clone();
        next.push(workspace);
        workspaces::save(&next)?;
        self.workspaces = next;

        Ok(true)
    }

    pub fn forget_machine(&mut self, machine: &str) -> std::io::Result<()> {
        let kept = self.workspaces_without(machine);

        if kept.len() != self.workspaces.len() {
            workspaces::save(&kept)?;
            self.workspaces = kept;
        }

        self.sessions.retain(|session| session.machine != machine);
        self.forget_selection_without_a_session();
        self.abandon_open_on(machine);

        Ok(())
    }

    fn workspaces_without(&self, machine: &str) -> Vec<Workspace> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.machine != machine)
            .cloned()
            .collect()
    }

    pub fn begin_open(&mut self, machine: String, sid: String) {
        self.opening = Some(OpeningSession { machine, sid });
    }

    pub fn may_open_session(&mut self, sid: &str) -> bool {
        let Some(opening) = &self.opening else {
            return true;
        };

        if opening.sid != sid {
            return false;
        }

        self.opening = None;

        true
    }

    pub fn open_session(&mut self, sid: &str) -> Option<SessionOpen> {
        let session = self
            .sessions
            .iter()
            .find(|session| session.info.sid == sid)?;
        let open = SessionOpen {
            machine: session.machine.clone(),
            sid: session.info.sid.clone(),
            cwd: session.info.cwd.clone(),
        };

        self.begin_open(open.machine.clone(), open.sid.clone());

        Some(open)
    }

    pub fn insert_started_session(
        &mut self,
        machine: String,
        sid: String,
        name: String,
        cwd: String,
    ) {
        self.insert_started_session_at(machine, sid, name, cwd, unix_millis_now());
    }

    fn insert_started_session_at(
        &mut self,
        machine: String,
        sid: String,
        name: String,
        cwd: String,
        mtime_ms: u64,
    ) {
        self.sessions.retain(|session| session.info.sid != sid);
        self.sessions.push(MachineSession {
            machine,
            info: SessionInfo {
                sid,
                name: Some(name),
                cwd,
                file: None,
                mtime_ms,
                status: SessionStatus::Running,
            },
        });
        self.sort_sessions();
    }

    pub fn visible_sessions(&self) -> Vec<&MachineSession> {
        self.sessions
            .iter()
            .filter(|session| self.session_visible(session))
            .collect()
    }

    pub fn visible_session_count_on(&self, machine: &str) -> usize {
        self.visible_sessions()
            .iter()
            .filter(|session| session.machine == machine)
            .count()
    }

    pub fn session_cwd(&self, sid: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|session| session.info.sid == sid)
            .map(|session| session.info.cwd.as_str())
    }

    #[cfg(test)]
    fn sessions(&self) -> &[MachineSession] {
        &self.sessions
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn repos_on(&self, machine: &str) -> Vec<String> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.machine == machine)
            .map(|workspace| workspace.path.clone())
            .collect()
    }

    pub fn default_new_session_target(&self, machines: &[String]) -> ChatTarget {
        let reachable = |machine: &str| machines.iter().any(|candidate| candidate == machine);
        let recent = self.sessions.iter().find(|session| {
            reachable(&session.machine) && self.is_stored(&session.machine, &session.info.cwd)
        });

        if let Some(session) = recent {
            return ChatTarget {
                machine: session.machine.clone(),
                workspace_path: session.info.cwd.clone(),
            };
        }

        let stored = self
            .workspaces
            .iter()
            .find(|workspace| reachable(&workspace.machine));

        if let Some(workspace) = stored {
            return ChatTarget {
                machine: workspace.machine.clone(),
                workspace_path: workspace.path.clone(),
            };
        }

        match machines.first() {
            Some(machine) => ChatTarget {
                machine: machine.clone(),
                workspace_path: String::new(),
            },
            None => ChatTarget::unresolved(),
        }
    }

    fn is_stored(&self, machine: &str, path: &str) -> bool {
        self.workspaces
            .iter()
            .any(|workspace| workspace.machine == machine && workspace.path == path)
    }

    pub fn opening_sid(&self) -> Option<&str> {
        self.opening.as_ref().map(|opening| opening.sid.as_str())
    }

    pub fn abandon_open_on(&mut self, machine: &str) -> bool {
        if self
            .opening
            .as_ref()
            .is_some_and(|opening| opening.machine == machine)
        {
            self.opening = None;

            return true;
        }

        false
    }

    fn session_visible(&self, session: &MachineSession) -> bool {
        if session.machine != LOCAL_MACHINE {
            return true;
        }

        self.workspaces.iter().any(|workspace| {
            workspace.machine == LOCAL_MACHINE && workspace.path == session.info.cwd
        })
    }

    fn sort_sessions(&mut self) {
        self.sessions.sort_by(|a, b| {
            b.info
                .mtime_ms
                .cmp(&a.info.mtime_ms)
                .then_with(|| b.info.sid.cmp(&a.info.sid))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(machine: &str, path: &str) -> Workspace {
        Workspace {
            machine: machine.to_string(),
            path: path.to_string(),
        }
    }

    fn session(sid: &str, cwd: &str, mtime_ms: u64, status: SessionStatus) -> SessionInfo {
        SessionInfo {
            sid: sid.to_string(),
            name: None,
            cwd: cwd.to_string(),
            file: None,
            mtime_ms,
            status,
        }
    }

    fn catalog(workspaces: &[Workspace]) -> Catalog {
        Catalog::from_workspaces(workspaces.to_vec())
    }

    #[test]
    fn sessions_are_ordered_by_activity_then_sid() {
        let mut catalog = catalog(&[]);

        catalog.record_sessions(
            "remote",
            vec![
                session("older", "/repo", 10, SessionStatus::Exited),
                session("newer", "/repo", 30, SessionStatus::Running),
                session("tie-a", "/repo", 20, SessionStatus::Exited),
                session("tie-b", "/repo", 20, SessionStatus::Exited),
            ],
        );

        let sids: Vec<_> = catalog
            .sessions()
            .iter()
            .map(|session| session.info.sid.as_str())
            .collect();

        assert_eq!(sids, ["newer", "tie-b", "tie-a", "older"]);
    }

    #[test]
    fn local_sessions_are_scoped_but_remote_sessions_are_not() {
        let mut catalog = catalog(&[workspace(LOCAL_MACHINE, "/local/kept")]);

        catalog.record_sessions(
            LOCAL_MACHINE,
            vec![
                session("local-kept", "/local/kept", 2, SessionStatus::Exited),
                session("local-hidden", "/local/other", 1, SessionStatus::Exited),
            ],
        );
        catalog.record_sessions(
            "remote",
            vec![session(
                "remote-anywhere",
                "/remote/other",
                0,
                SessionStatus::Exited,
            )],
        );

        let sids: Vec<_> = catalog
            .visible_sessions()
            .iter()
            .map(|session| session.info.sid.as_str())
            .collect();

        assert_eq!(sids, ["local-kept", "remote-anywhere"]);
        assert_eq!(catalog.visible_sessions()[1].machine, "remote");
    }

    #[test]
    fn opening_guards_block_other_sessions_and_allow_same_session_retry() {
        let mut catalog = catalog(&[]);
        catalog.begin_open("machine-a".to_string(), "sid-a".to_string());

        assert!(!catalog.may_open_session("sid-b"));
        assert_eq!(catalog.opening_sid(), Some("sid-a"));
        assert!(!catalog.abandon_open_on("machine-b"));
        assert_eq!(catalog.opening_sid(), Some("sid-a"));
        assert!(catalog.may_open_session("sid-a"));
        assert_eq!(catalog.opening_sid(), None);
        assert!(catalog.may_open_session("sid-b"));
    }

    fn machines(machines: &[&str]) -> Vec<String> {
        machines.iter().map(|machine| machine.to_string()).collect()
    }

    #[test]
    fn a_new_session_opens_on_the_most_recent_stored_repo() {
        let mut catalog = catalog(&[
            workspace("machine-a", "/repo/first"),
            workspace("machine-b", "/repo/recent"),
        ]);

        catalog.record_sessions(
            "machine-b",
            vec![
                session("recent", "/repo/recent", 20, SessionStatus::Exited),
                session("older", "/repo/first", 10, SessionStatus::Exited),
            ],
        );

        assert_eq!(
            catalog.default_new_session_target(&machines(&["machine-a", "machine-b"])),
            ChatTarget {
                machine: "machine-b".to_string(),
                workspace_path: "/repo/recent".to_string(),
            }
        );
    }

    #[test]
    fn a_session_on_an_unstored_or_unreachable_repo_is_not_a_default() {
        let mut catalog = catalog(&[workspace("machine-a", "/repo/first")]);

        catalog.record_sessions(
            "machine-b",
            vec![session(
                "elsewhere",
                "/repo/elsewhere",
                30,
                SessionStatus::Exited,
            )],
        );
        catalog.record_sessions(
            "machine-gone",
            vec![session("gone", "/repo/first", 40, SessionStatus::Exited)],
        );

        assert_eq!(
            catalog.default_new_session_target(&machines(&["machine-a", "machine-b"])),
            ChatTarget {
                machine: "machine-a".to_string(),
                workspace_path: "/repo/first".to_string(),
            }
        );
    }

    #[test]
    fn a_fleet_with_no_stored_repo_resolves_only_its_first_machine() {
        let catalog = catalog(&[workspace("machine-gone", "/repo/first")]);

        assert_eq!(
            catalog.default_new_session_target(&machines(&["local", "machine-b"])),
            ChatTarget {
                machine: "local".to_string(),
                workspace_path: String::new(),
            }
        );
        assert_eq!(
            catalog.default_new_session_target(&[]),
            ChatTarget::unresolved()
        );
    }

    #[test]
    fn repos_are_offered_per_machine_in_the_order_they_were_added() {
        let catalog = catalog(&[
            workspace("machine-a", "/repo/first"),
            workspace("machine-b", "/repo/other"),
            workspace("machine-a", "/repo/second"),
        ]);

        assert_eq!(
            catalog.repos_on("machine-a"),
            vec!["/repo/first".to_string(), "/repo/second".to_string()]
        );
        assert!(catalog.repos_on("machine-c").is_empty());
    }

    #[test]
    fn duplicate_workspaces_are_noops() {
        let existing = workspace("machine", "/repo");
        let mut catalog = catalog(std::slice::from_ref(&existing));

        assert!(!catalog
            .add_workspace(existing)
            .expect("duplicate workspace does not write"));
        assert_eq!(catalog.workspaces(), &[workspace("machine", "/repo")]);
    }

    #[test]
    fn forgetting_a_machine_keeps_only_the_other_machines_repos() {
        let catalog = catalog(&[
            workspace("machine-gone", "/repo/first"),
            workspace("machine-kept", "/repo/other"),
            workspace("machine-gone", "/repo/second"),
        ]);

        assert_eq!(
            catalog.workspaces_without("machine-gone"),
            vec![workspace("machine-kept", "/repo/other")]
        );
        assert_eq!(
            catalog.workspaces_without("machine-unknown").len(),
            catalog.workspaces().len(),
            "a machine with no stored repo leaves the file alone"
        );
    }

    #[test]
    fn forgetting_a_machine_drops_its_session_rows_and_its_open_guard() {
        let mut catalog = catalog(&[]);

        catalog.record_sessions(
            "machine-gone",
            vec![session("gone", "/repo", 20, SessionStatus::Running)],
        );
        catalog.record_sessions(
            "machine-kept",
            vec![session("kept", "/repo", 10, SessionStatus::Running)],
        );
        catalog.begin_open("machine-gone".to_string(), "gone".to_string());

        assert_eq!(catalog.visible_session_count_on("machine-gone"), 1);

        catalog
            .forget_machine("machine-gone")
            .expect("no stored repo means no write to fail");

        assert_eq!(catalog.visible_session_count_on("machine-gone"), 0);
        assert_eq!(catalog.visible_session_count_on("machine-kept"), 1);
        assert_eq!(catalog.opening_sid(), None);
    }

    #[test]
    fn the_attached_session_is_the_selected_row_until_a_new_chat_clears_it() {
        let mut catalog = catalog(&[]);

        catalog.record_sessions(
            "machine",
            vec![
                session("first", "/repo", 20, SessionStatus::Running),
                session("second", "/repo", 10, SessionStatus::Running),
            ],
        );

        assert!(!catalog.is_selected_session("first"));

        catalog.session_attached("machine", "first");

        assert!(catalog.is_selected_session("first"));

        catalog.session_attached("machine", "second");

        assert!(!catalog.is_selected_session("first"));
        assert!(catalog.is_selected_session("second"));

        catalog.clear_selected_session();

        assert!(!catalog.is_selected_session("second"));
    }

    #[test]
    fn a_starting_session_is_selected_only_once_its_row_exists() {
        let mut catalog = catalog(&[]);

        catalog.begin_open("machine".to_string(), "started".to_string());

        assert!(!catalog.is_selected_session("started"));
        assert!(catalog.visible_sessions().is_empty());

        catalog.insert_started_session_at(
            "machine".to_string(),
            "started".to_string(),
            "first prompt".to_string(),
            "/repo".to_string(),
            1,
        );
        catalog.session_attached("machine", "started");

        assert!(catalog.is_selected_session("started"));
    }

    #[test]
    fn forgetting_a_machine_drops_only_the_selection_that_lost_its_row() {
        let mut catalog = catalog(&[]);

        catalog.record_sessions(
            "machine-gone",
            vec![session("gone", "/repo", 20, SessionStatus::Running)],
        );
        catalog.record_sessions(
            "machine-kept",
            vec![session("kept", "/repo", 10, SessionStatus::Running)],
        );
        catalog.session_attached("machine-gone", "gone");
        catalog
            .forget_machine("machine-gone")
            .expect("no stored repo means no write to fail");

        assert!(!catalog.is_selected_session("gone"));

        catalog.session_attached("machine-kept", "kept");
        catalog
            .forget_machine("machine-gone")
            .expect("no stored repo means no write to fail");

        assert!(catalog.is_selected_session("kept"));
    }

    #[test]
    fn session_status_transitions_only_change_known_rows() {
        let mut catalog = catalog(&[]);
        catalog.record_sessions(
            "machine",
            vec![session("sid", "/repo", 1, SessionStatus::Exited)],
        );

        assert!(catalog.set_session_status("sid", SessionStatus::Running));
        assert!(!catalog.set_session_status("sid", SessionStatus::Running));
        assert!(!catalog.set_session_status("unknown", SessionStatus::Exited));
        assert_eq!(catalog.sessions()[0].info.status, SessionStatus::Running);

        catalog.begin_open("machine".to_string(), "sid".to_string());
        assert!(catalog.session_attached("machine", "sid"));
        assert_eq!(catalog.opening_sid(), None);
        assert!(catalog.session_exited("sid"));
        assert_eq!(catalog.sessions()[0].info.status, SessionStatus::Exited);
    }

    #[test]
    fn started_sessions_are_inserted_in_order_without_duplicate_sids() {
        let mut catalog = catalog(&[]);
        catalog.insert_started_session_at(
            "machine".to_string(),
            "sid".to_string(),
            "first".to_string(),
            "/repo".to_string(),
            1,
        );
        catalog.insert_started_session_at(
            "machine".to_string(),
            "sid".to_string(),
            "retry".to_string(),
            "/repo".to_string(),
            2,
        );

        assert_eq!(catalog.sessions().len(), 1);
        assert_eq!(catalog.sessions()[0].info.name.as_deref(), Some("retry"));
    }
}
