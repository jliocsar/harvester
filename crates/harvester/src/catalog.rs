//! The window-free session/workspace catalog behind [`FleetStore`](crate::store::FleetStore).
//!
//! This is display state derived from Pi's session listings, not a second
//! persistence layer. Pi's files on the machine remain authoritative; this
//! catalog is replaced by the next listing and can be discarded at any time.
//! Workspace configuration is the one exception: it is user configuration and
//! is coordinated with `~/.harvester/workspaces.json` here.

use harvester_client_core::LOCAL_MACHINE;
use harvester_proto::{SessionInfo, SessionStatus};

use crate::session_state::ChatTarget;
use crate::workspaces::{self, Workspace};

/// One sidebar session row, tagged with the machine whose daemon reported it.
#[derive(Debug, Clone)]
pub struct MachineSession {
    pub machine: String,
    pub info: SessionInfo,
}

/// The target needed to issue an open/resume command after the catalog's guard
/// has admitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOpen {
    pub machine: String,
    pub sid: String,
    pub cwd: String,
}

/// Session rows, workspace configuration, and the single in-flight open guard.
/// No GPUI types or machine connectivity live here.
pub struct Catalog {
    sessions: Vec<MachineSession>,
    workspaces: Vec<Workspace>,
    opening: Option<OpeningSession>,
}

struct OpeningSession {
    machine: String,
    sid: String,
}

impl Catalog {
    /// Load the user's workspace configuration. Session rows deliberately do
    /// not load from disk: only a daemon's `list_sessions` answer can populate
    /// them, so Pi remains the source of truth.
    pub fn load() -> Self {
        Self::from_workspaces(workspaces::load())
    }

    /// Construct a catalog from already-loaded workspace configuration.
    /// Keeping this small constructor also makes the catalog's rules directly
    /// testable without starting GPUI or touching a user's home directory.
    pub fn from_workspaces(workspaces: Vec<Workspace>) -> Self {
        Self {
            sessions: Vec::new(),
            workspaces,
            opening: None,
        }
    }

    /// Replace one machine's listing, then merge all machines by Pi's last
    /// activity time. The listing is display state; it is never used to infer
    /// or manufacture session-file contents.
    pub fn record_sessions(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
        self.sessions.retain(|session| session.machine != machine);
        self.sessions
            .extend(sessions.into_iter().map(|info| MachineSession {
                machine: machine.to_string(),
                info,
            }));
        self.sort_sessions();
    }

    /// Apply a running/exited transition to a known row. An unknown sid is
    /// ignored: the next authoritative listing will introduce it.
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

    /// Mark the session attached to a machine as running and finish that
    /// machine's opening guard.
    pub fn session_attached(&mut self, machine: &str, sid: &str) -> bool {
        let changed = self.set_session_status(sid, SessionStatus::Running);
        self.abandon_open_on(machine) || changed
    }

    /// Mark a known session exited. The guard is intentionally not cleared by
    /// an exit from another machine; opening is scoped to its machine.
    pub fn session_exited(&mut self, sid: &str) -> bool {
        self.set_session_status(sid, SessionStatus::Exited)
    }

    /// Add a workspace once and atomically coordinate the user configuration
    /// write before exposing it in memory. A duplicate is a no-op and does not
    /// rewrite the file.
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

    /// Forget a machine outright: the repositories stored for it leave the user
    /// configuration, and the session rows its daemon once reported leave the
    /// table with them.
    ///
    /// The file write comes first and a failure aborts the whole removal, so
    /// what the sidebar shows and what `workspaces.json` holds cannot disagree
    /// — the file is the only place a machine is persisted, and a machine that
    /// survives the next launch must survive this one too.
    ///
    /// Session rows go because they are display state derived from a daemon
    /// this client will never reach again: a missing machine's transcripts live
    /// only on its own disk, and nothing client-side could render them
    /// (docs/SPEC.md §Flows "Forget machine").
    pub fn forget_machine(&mut self, machine: &str) -> std::io::Result<()> {
        let kept = self.workspaces_without(machine);

        if kept.len() != self.workspaces.len() {
            workspaces::save(&kept)?;
            self.workspaces = kept;
        }

        self.sessions.retain(|session| session.machine != machine);
        self.abandon_open_on(machine);

        Ok(())
    }

    /// The stored repositories that survive forgetting `machine`. Split out
    /// from [`Self::forget_machine`] so the rule can be tested without writing
    /// a real user's configuration file.
    fn workspaces_without(&self, machine: &str) -> Vec<Workspace> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.machine != machine)
            .cloned()
            .collect()
    }

    /// Start or retry an open guard for a session. The guard is cleared by
    /// [`Self::session_attached`] or a machine failure in the observable root.
    pub fn begin_open(&mut self, machine: String, sid: String) {
        self.opening = Some(OpeningSession { machine, sid });
    }

    /// Whether another open may be issued. A second click on the same row is
    /// the explicit retry escape hatch; a different row stays blocked while an
    /// open is in flight.
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

    /// Resolve a session by sid and install its open guard. Display indexes
    /// never enter this path because the sidebar is a filtered view.
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

    /// Insert the display row for a newly-created session. This is not a
    /// client-side session journal: a later daemon listing replaces it, and a
    /// duplicate sid is replaced rather than rendered twice while that listing
    /// is in flight.
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

    /// The complete daemon-derived table. Remote rows are all visible because
    /// the remote disk is authoritative. Local rows are scoped to workspaces
    /// so the local daemon does not expose every repo ever used on this host.
    pub fn visible_sessions(&self) -> Vec<&MachineSession> {
        self.sessions
            .iter()
            .filter(|session| self.session_visible(session))
            .collect()
    }

    /// How many sidebar rows a machine would take with it — the number the
    /// forget confirmation quotes. It counts the *visible* table, because what
    /// the dialog promises to remove is what the user can see.
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

    /// The repositories stored for one machine, in the order the user added
    /// them — the rows the chat's repo picker offers above its "Add new…".
    pub fn repos_on(&self, machine: &str) -> Vec<String> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.machine == machine)
            .map(|workspace| workspace.path.clone())
            .collect()
    }

    /// Where a New session opens, given the machines the fleet can currently
    /// reach (docs/SPEC.md §Flows "Start session"). Derived on every open and
    /// never persisted: this is a guess at what the user wants next, and a
    /// guess that survives restarts is a preference nobody agreed to keep.
    ///
    /// In order: the repo of the most recent session that is still a stored
    /// repo on a reachable machine; else the first stored repo on a reachable
    /// machine; else the first reachable machine with no repo picked (the
    /// picker then offers only "Add new…"); else nothing at all.
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

    /// Clear an open only when the failing machine owns it.
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

/// Wall-clock milliseconds, the units Pi's session listing orders by.
fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as u64)
        .unwrap_or(0)
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
        // No stored repository, so this exercises the whole cascade without
        // writing a configuration file: the machine is tracked from argv.
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
