//! The add-workspace folder picker (docs/SPEC.md §Flows "Add workspace"):
//! its modal state, the host ops that drive it, and its rendering.
//!
//! A child module of `workbench` so it can own this flow end to end without
//! opening the workbench's state to the rest of the crate. The picker is the
//! one surface that talks to the machines worker rather than to a bridge:
//! walking a machine's directories is plain SSH exec work (`$HOME`, `ls`,
//! `git rev-parse`), so browsing a machine costs one round trip and never
//! waits on a harvesterd bootstrap it would not use.

use gpui::{div, prelude::*, rgb, Context};
use harvester_client_core::machines;
use harvester_client_core::{Host, Pending, RemoteDirEntry, RequestToken, RequestTokens};

use super::Workbench;
use crate::theme;
use crate::ui;

/// The one host op a folder picker has in flight, and what its answer needs in
/// order to be applied. One at a time: the rows and the Add button ignore
/// clicks while a request is out, so the modal never has two answers to
/// reconcile. The correlating token is [`Pending`]'s, not this enum's.
#[derive(Clone)]
enum PickerRequest {
    /// The machine's `$HOME`, which starts the walk — one exec, no daemon.
    Home,
    /// The directory the answer would move the picker to. The request's, never
    /// the answer's: nothing is echoed back over the wire for it to trust.
    Listing { path: String },
    /// The displayed directory, as it was when "Add" was clicked.
    Validation { path: String },
}

/// The add-workspace modal's state (SPEC §Flows "Add workspace"). Its execs
/// are plain host ops on the machines worker — the picker never touches that
/// machine's daemon, so it never waits on one.
pub(super) struct FolderPicker {
    /// The machine being browsed — where listings and validation run.
    machine: String,
    /// Directory whose entries are displayed — what "Add" would store. Empty
    /// until `$HOME` answers.
    path: String,
    entries: Vec<RemoteDirEntry>,
    pending: Pending<PickerRequest>,
    /// Listing or validation failure, rendered inside the modal.
    error: Option<String>,
}

/// What the picker needs run on its machine. The picker decides *what* to ask
/// for and correlates the answer; the workbench resolves the [`Host`] and owns
/// the machines worker, so it does the asking (mirrors `ghost`'s split).
pub(super) struct PickerCommand {
    request: PickerRequest,
    token: RequestToken,
}

impl PickerCommand {
    /// The worker's own vocabulary, once the workbench has resolved the host.
    fn into_machines_command(self, host: Host) -> machines::Command {
        match self.request {
            PickerRequest::Home => machines::Command::ResolveHome {
                host,
                token: self.token,
            },
            PickerRequest::Listing { path } => machines::Command::ListDir {
                host,
                path,
                token: self.token,
            },
            PickerRequest::Validation { path } => machines::Command::ValidateWorkspace {
                host,
                path,
                token: self.token,
            },
        }
    }
}

impl FolderPicker {
    /// A picker on `machine`, already asking for the `$HOME` its walk starts
    /// from. No connection involved: the walk is plain host execs (SPEC
    /// §Workspace, "no daemon or provider involvement"), so browsing a machine
    /// Harvester never bootstrapped costs one round trip, not an install.
    pub fn opening(machine: String, tokens: &RequestTokens) -> (FolderPicker, PickerCommand) {
        let mut picker = FolderPicker {
            machine,
            path: String::new(),
            entries: Vec::new(),
            pending: Pending::default(),
            error: None,
        };
        let command = picker.ask(PickerRequest::Home, tokens);

        (picker, command)
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// Records what the picker is waiting on and hands back the command to
    /// send. Every request goes through here, so "one at a time, matched by its
    /// token" holds by construction.
    fn ask(&mut self, request: PickerRequest, tokens: &RequestTokens) -> PickerCommand {
        self.error = None;

        // The context stays in the `Pending` until its answer arrives, so the
        // copy the workbench needs to build the command is made here.
        PickerCommand {
            token: self.pending.ask(tokens, request.clone()),
            request,
        }
    }

    /// The command never reached the worker, so no answer is coming. Without
    /// this the modal would stay gated on a request that does not exist.
    pub fn request_failed(&mut self) {
        self.pending.cancel();
    }

    /// The reason a host op could not even be started, shown in the modal
    /// rather than the statusbar — the user is looking at the picker.
    pub fn fail(&mut self, detail: String) {
        self.pending.cancel();
        self.error = Some(detail);
    }

    /// `$HOME` answered: the walk starts there, so the answer is immediately a
    /// listing request. `None` means the answer was stale or the exec failed
    /// (the reason is in the modal).
    pub fn accept_home(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        tokens: &RequestTokens,
    ) -> Option<PickerCommand> {
        self.pending.accept(token)?;

        match result {
            Ok(home) => {
                self.path = home.clone();

                Some(self.ask(PickerRequest::Listing { path: home }, tokens))
            }
            // Nothing to browse without a start directory; the user cancels and
            // retries (or picks another machine).
            Err(detail) => {
                self.error = Some(detail);

                None
            }
        }
    }

    pub fn accept_listing(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    ) {
        let Some(PickerRequest::Listing { path }) = self.pending.accept(token) else {
            return;
        };

        match result {
            Ok(entries) => {
                self.path = path;
                self.entries = entries;
                self.error = None;
            }
            // Listing failed (permissions, vanished dir): keep showing the
            // previous directory, surface the reason.
            Err(detail) => self.error = Some(detail),
        }
    }

    /// Validation answered. `Some` is the workspace to store — the picker is
    /// spent at that point and the workbench drops it.
    pub fn accept_validation(
        &mut self,
        token: RequestToken,
        error: Option<String>,
    ) -> Option<(String, String)> {
        self.pending.accept(token)?;

        match error {
            // Not a git repo: clear error in the modal, user re-picks.
            Some(detail) => {
                self.error = Some(detail);

                None
            }
            None => Some((self.machine.clone(), self.path.clone())),
        }
    }

    /// Walk into a listed directory. `None` while a request is out — the rows
    /// ignore clicks then, so the modal never has two answers to reconcile.
    pub fn open_dir(
        &mut self,
        entry_index: usize,
        tokens: &RequestTokens,
    ) -> Option<PickerCommand> {
        if self.busy() {
            return None;
        }

        let entry = self.entries.get(entry_index)?;
        let path = join_remote_path(&self.path, &entry.name);

        Some(self.ask(PickerRequest::Listing { path }, tokens))
    }

    pub fn go_up(&mut self, tokens: &RequestTokens) -> Option<PickerCommand> {
        if self.busy() {
            return None;
        }

        let path = parent_remote_path(&self.path);

        Some(self.ask(PickerRequest::Listing { path }, tokens))
    }

    /// "Add": validate the displayed directory. The store write happens when
    /// the answer lands.
    pub fn confirm(&mut self, tokens: &RequestTokens) -> Option<PickerCommand> {
        if self.busy() || self.path.is_empty() {
            return None;
        }

        let path = self.path.clone();

        Some(self.ask(PickerRequest::Validation { path }, tokens))
    }

    fn busy(&self) -> bool {
        self.pending.is_waiting()
    }

    /// The modal's progress note, or `None` when nothing is in flight. Which
    /// note is a question about the walk, not about the request: an empty path
    /// means `$HOME` has not landed yet, and that is the only state the user
    /// needs named. Everything else — a listing, and the `git rev-parse` behind
    /// "Add", which used to show nothing at all — is "loading…".
    fn waiting_note(&self) -> Option<&'static str> {
        if !self.busy() {
            return None;
        }

        if self.path.is_empty() {
            return Some("finding home directory…");
        }

        Some("loading…")
    }
}

impl Workbench {
    /// Act on the open picker and redraw — the ghost's `with_ghost` door, for
    /// the picker's own state.
    pub(in crate::workbench) fn with_picker(
        &mut self,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut FolderPicker),
    ) {
        if let Some(picker) = &mut self.picker {
            act(picker);
            cx.notify();
        }
    }

    /// Run what the picker asked for on the machine it is browsing. The host
    /// is resolved here because it is the fleet's answer, and a machine that
    /// resolves to none puts its reason in the modal rather than the statusbar
    /// — the user is looking at the dialog.
    fn send_picker_request(&mut self, command: Option<PickerCommand>, cx: &mut Context<Self>) {
        let Some(command) = command else {
            cx.notify();

            return;
        };
        let host = match self.picker_host() {
            Ok(host) => host,
            Err(detail) => {
                self.with_picker(cx, |picker| picker.fail(detail));

                return;
            }
        };
        let sent = self.send_host_command(command.into_machines_command(host));

        if !sent {
            self.with_picker(cx, FolderPicker::request_failed);
        }

        cx.notify();
    }

    /// The host the open picker browses.
    fn picker_host(&self) -> Result<Host, String> {
        let Some(picker) = &self.picker else {
            return Err("no picker open".to_string());
        };

        // The fleet's own sentence, carried verbatim.
        self.fleet
            .host_for(picker.machine())
            .map_err(|error| error.to_string())
    }

    pub(super) fn handle_home_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let tokens = self.tokens.clone();
        let next = self
            .picker
            .as_mut()
            .and_then(|picker| picker.accept_home(token, result, &tokens));

        self.send_picker_request(next, cx);
    }

    pub(super) fn handle_dir_listed(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        self.with_picker(cx, |picker| picker.accept_listing(token, result));
    }

    /// A validated workspace is stored and the picker is spent.
    pub(super) fn handle_workspace_validated(
        &mut self,
        token: RequestToken,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let stored = self
            .picker
            .as_mut()
            .and_then(|picker| picker.accept_validation(token, error));

        let Some((machine, path)) = stored else {
            cx.notify();

            return;
        };

        self.picker = None;
        self.add_workspace(machine, path);
        cx.notify();
    }

    pub(super) fn open_picker(&mut self, machine: String, cx: &mut Context<Self>) {
        let (picker, command) = FolderPicker::opening(machine, &self.tokens);

        self.picker = Some(picker);
        self.send_picker_request(Some(command), cx);
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker = None;
        cx.notify();
    }

    /// The three ways the modal asks for another listing or a validation; each
    /// is the picker's decision, sent from here.
    fn picker_open_dir(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let tokens = self.tokens.clone();
        let command = self
            .picker
            .as_mut()
            .and_then(|picker| picker.open_dir(entry_index, &tokens));

        self.send_picker_request(command, cx);
    }

    fn picker_go_up(&mut self, cx: &mut Context<Self>) {
        let tokens = self.tokens.clone();
        let command = self
            .picker
            .as_mut()
            .and_then(|picker| picker.go_up(&tokens));

        self.send_picker_request(command, cx);
    }

    fn picker_confirm(&mut self, cx: &mut Context<Self>) {
        let tokens = self.tokens.clone();
        let command = self
            .picker
            .as_mut()
            .and_then(|picker| picker.confirm(&tokens));

        self.send_picker_request(command, cx);
    }

    pub(super) fn render_picker(
        &self,
        picker: &FolderPicker,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Directories before files, both in ls's alphabetical order.
        let directory_rows: Vec<_> = picker
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_dir)
            .map(|(entry_index, entry)| {
                ui::picker_dir_row(("picker-dir", entry_index), entry.name.clone().into()).on_click(
                    cx.listener(move |workbench, _, _, cx| {
                        workbench.picker_open_dir(entry_index, cx)
                    }),
                )
            })
            .collect();
        let file_rows: Vec<_> = picker
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| ui::picker_file_row(entry.name.clone().into()))
            .collect();
        // Nothing to add before `$HOME` lands (or after it failed): the
        // displayed directory is what "Add" would store.
        let can_submit = !picker.busy() && !picker.path.is_empty();

        ui::modal_scrim("picker-scrim")
            .on_click(cx.listener(|workbench, _, _, cx| workbench.close_picker(cx)))
            .child(
                ui::modal_frame("Add workspace")
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!(
                                "on {} — {}",
                                self.fleet.label(&picker.machine),
                                picker.path
                            )),
                    )
                    .child(
                        ui::modal_scroll_list("picker", ui::ModalListHeight::Fixed)
                            .when(!picker.path.is_empty() && picker.path != "/", |list| {
                                list.child(ui::picker_dir_row("picker-up", "..".into()).on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.picker_go_up(cx)),
                                ))
                            })
                            .children(directory_rows)
                            .children(file_rows)
                            .children(picker.waiting_note().map(|note| {
                                div()
                                    .px_2()
                                    .py_1()
                                    .text_xs()
                                    .text_color(rgb(theme::TEXT_FAINT))
                                    .child(note)
                            })),
                    )
                    .children(picker.error.clone().map(|error| {
                        div()
                            .text_xs()
                            .text_color(rgb(theme::STATUS_ERROR))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(error)
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                ui::button(
                                    "picker-cancel",
                                    "Cancel",
                                    ui::ButtonKind::Neutral,
                                    true,
                                )
                                .on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.close_picker(cx)),
                                ),
                            )
                            .child(
                                ui::button(
                                    "picker-add",
                                    "Add",
                                    ui::ButtonKind::Primary,
                                    can_submit,
                                )
                                .when(can_submit, |button| {
                                    button.on_click(cx.listener(|workbench, _, _, cx| {
                                        workbench.picker_confirm(cx)
                                    }))
                                }),
                            ),
                    ),
            )
    }
}

/// Appends one entry name; `path` is always absolute (the walk starts at the
/// remote `$HOME`), so the only special case is the root's trailing slash.
fn join_remote_path(path: &str, name: &str) -> String {
    format!("{}/{name}", path.trim_end_matches('/'))
}

/// One step up; the root is its own parent.
fn parent_remote_path(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(separator_index) => path[..separator_index].to_string(),
    }
}

/// The picker's two testable halves: the path walk, and the context an answer
/// is applied with. The token guard itself is [`Pending`]'s, and tested there.
/// Everything else needs a gpui context and a machine.
#[cfg(test)]
mod tests {
    use super::*;
    use harvester_client_core::RequestTokens;

    #[test]
    fn walking_down_appends_one_component() {
        assert_eq!(join_remote_path("/home/exedev", "src"), "/home/exedev/src");
    }

    #[test]
    fn the_root_does_not_double_its_separator() {
        assert_eq!(join_remote_path("/", "srv"), "/srv");
    }

    #[test]
    fn walking_up_drops_the_last_component() {
        assert_eq!(parent_remote_path("/home/exedev/src"), "/home/exedev");
    }

    #[test]
    fn the_root_is_its_own_parent() {
        assert_eq!(parent_remote_path("/"), "/");
        assert_eq!(parent_remote_path("/home"), "/");
    }

    /// The walk, driven end to end with no `Workbench` and no window — which
    /// is the point of the split: this file used to be `impl Workbench` and
    /// none of it could be reached without one.
    #[test]
    fn the_walk_goes_home_then_lists_then_validates() {
        let tokens = RequestTokens::default();
        let (mut picker, home) = FolderPicker::opening("exe-dev/bloggy".to_string(), &tokens);

        assert!(matches!(home.request, PickerRequest::Home));
        assert!(picker.busy(), "the modal is gated until $HOME lands");
        assert_eq!(picker.waiting_note(), Some("finding home directory…"));

        let listing = picker
            .accept_home(home.token, Ok("/home/exedev".to_string()), &tokens)
            .expect("home starts the walk");

        assert!(
            matches!(&listing.request, PickerRequest::Listing { path } if path == "/home/exedev")
        );

        picker.accept_listing(
            listing.token,
            Ok(vec![RemoteDirEntry {
                name: "repo".to_string(),
                is_dir: true,
            }]),
        );

        assert!(!picker.busy());
        assert_eq!(picker.path, "/home/exedev");

        let descend = picker.open_dir(0, &tokens).expect("a listed directory");

        assert!(
            matches!(&descend.request, PickerRequest::Listing { path } if path == "/home/exedev/repo")
        );

        picker.accept_listing(descend.token, Ok(Vec::new()));

        let validate = picker.confirm(&tokens).expect("a directory to add");

        assert!(
            matches!(&validate.request, PickerRequest::Validation { path } if path == "/home/exedev/repo")
        );
        assert_eq!(
            picker.accept_validation(validate.token, None),
            Some((
                "exe-dev/bloggy".to_string(),
                "/home/exedev/repo".to_string()
            ))
        );
    }

    #[test]
    fn a_click_while_a_request_is_out_is_ignored() {
        let tokens = RequestTokens::default();
        let (mut picker, _home) = FolderPicker::opening("exe-dev/bloggy".to_string(), &tokens);

        assert!(picker.open_dir(0, &tokens).is_none());
        assert!(picker.go_up(&tokens).is_none());
        assert!(picker.confirm(&tokens).is_none());
    }

    /// A failed exec keeps the walk where it was and says why, rather than
    /// leaving the modal gated on an answer that is not coming.
    #[test]
    fn a_failed_listing_keeps_the_previous_directory() {
        let tokens = RequestTokens::default();
        let (mut picker, home) = FolderPicker::opening("exe-dev/bloggy".to_string(), &tokens);
        let listing = picker
            .accept_home(home.token, Ok("/home/exedev".to_string()), &tokens)
            .expect("home starts the walk");

        picker.accept_listing(listing.token, Ok(Vec::new()));

        let descend = picker.ask(
            PickerRequest::Listing {
                path: "/root".to_string(),
            },
            &tokens,
        );

        picker.accept_listing(descend.token, Err("permission denied".to_string()));

        assert_eq!(picker.path, "/home/exedev");
        assert_eq!(picker.error.as_deref(), Some("permission denied"));
        assert!(!picker.busy());
    }

    #[test]
    fn a_command_that_never_left_ungates_the_modal() {
        let tokens = RequestTokens::default();
        let (mut picker, _home) = FolderPicker::opening("exe-dev/bloggy".to_string(), &tokens);

        picker.request_failed();

        assert!(!picker.busy());
    }

    #[test]
    fn a_rejected_workspace_stores_nothing_and_says_why() {
        let tokens = RequestTokens::default();
        let (mut picker, home) = FolderPicker::opening("exe-dev/bloggy".to_string(), &tokens);
        let listing = picker
            .accept_home(home.token, Ok("/home/exedev".to_string()), &tokens)
            .expect("home starts the walk");

        picker.accept_listing(listing.token, Ok(Vec::new()));

        let validate = picker.confirm(&tokens).expect("a directory to add");
        let stored = picker.accept_validation(validate.token, Some("not a git repo".to_string()));

        assert_eq!(stored, None);
        assert_eq!(picker.error.as_deref(), Some("not a git repo"));
    }

    fn idle_picker() -> FolderPicker {
        FolderPicker {
            machine: "exe-dev/bloggy".to_string(),
            path: "/home/exedev".to_string(),
            entries: Vec::new(),
            pending: Pending::default(),
            error: None,
        }
    }

    #[test]
    fn a_listing_carries_the_directory_it_would_move_to() {
        // The path is the request's, not the answer's — nothing is echoed
        // back over the wire for the picker to trust.
        let tokens = RequestTokens::default();
        let mut picker = idle_picker();
        let token = picker.pending.ask(
            &tokens,
            PickerRequest::Listing {
                path: "/home/exedev/src".to_string(),
            },
        );

        assert!(picker.busy(), "the modal is gated while the answer is out");
        assert_eq!(picker.waiting_note(), Some("loading…"));

        let Some(PickerRequest::Listing { path }) = picker.pending.accept(token) else {
            panic!("the outstanding request is a listing");
        };

        assert_eq!(path, "/home/exedev/src");
        assert!(!picker.busy());
    }

    /// The modal must not stay gated on a request that never left: a host
    /// command the worker refused has no answer coming.
    #[test]
    fn a_request_that_never_left_stops_gating_the_modal() {
        let tokens = RequestTokens::default();
        let mut picker = idle_picker();

        picker.pending.ask(&tokens, PickerRequest::Home);
        picker.pending.cancel();

        assert!(!picker.busy());
    }
}
