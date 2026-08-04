//! The add-workspace folder picker (docs/SPEC.md §Flows "Add workspace"):
//! its modal state, the host ops that drive it, and its rendering.
//!
//! A child module of `workbench` so it can own this flow end to end without
//! opening the workbench's state to the rest of the crate. The picker is the
//! one surface that talks to the machines worker rather than to a bridge:
//! walking a machine's directories is plain SSH exec work (`$HOME`, `ls`,
//! `git rev-parse`), so browsing a machine costs one round trip and never
//! waits on a harvesterd bootstrap it would not use.

use gpui::{div, prelude::*, px, rgb, Context};
use harvester_client_core::machines;
use harvester_client_core::{Host, RemoteDirEntry, RequestToken};

use super::Workbench;
use crate::theme;
use crate::ui;
use crate::workbench::MODAL_LIST_HEIGHT;

/// The one host op a folder picker has in flight. One at a time: the rows and
/// the Add button ignore clicks while a request is out, so the modal never
/// has two answers to reconcile.
enum PickerRequest {
    /// The machine's `$HOME`, which starts the walk — one exec, no daemon.
    Home(RequestToken),
    /// A listing, plus the directory it would move the picker to.
    Listing {
        token: RequestToken,
        path: String,
    },
    Validation(RequestToken),
}

impl PickerRequest {
    fn token(&self) -> RequestToken {
        match self {
            PickerRequest::Home(token) => *token,
            PickerRequest::Listing { token, .. } => *token,
            PickerRequest::Validation(token) => *token,
        }
    }
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
    pending: Option<PickerRequest>,
    /// Listing or validation failure, rendered inside the modal.
    error: Option<String>,
}

impl FolderPicker {
    fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// The answer with this token, if it is the one still outstanding —
    /// consuming the slot, so a duplicate cannot be applied twice.
    fn take_pending(&mut self, token: RequestToken) -> Option<PickerRequest> {
        if self.pending.as_ref()?.token() != token {
            return None;
        }

        self.pending.take()
    }
}

impl Workbench {
    /// The machine's `$HOME` answered: the walk starts there. A picker closed
    /// and reopened meanwhile has a different token outstanding, so the late
    /// answer is dropped.
    pub(super) fn handle_home_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
    ) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.take_pending(token).is_none() {
            return;
        }

        let home = match result {
            Ok(home) => home,
            // Nothing to browse without a start directory; the user cancels
            // and retries (or picks another machine).
            Err(detail) => {
                picker.error = Some(detail);

                return;
            }
        };

        picker.path = home.clone();
        self.request_listing(home);
    }

    /// A listing answered.
    pub(super) fn handle_dir_listed(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    ) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let Some(PickerRequest::Listing { path, .. }) = picker.take_pending(token) else {
            return;
        };

        match result {
            Ok(entries) => {
                picker.path = path;
                picker.entries = entries;
                picker.error = None;
            }
            // Listing failed (permissions, vanished dir): keep showing the
            // previous directory, surface the reason.
            Err(detail) => picker.error = Some(detail),
        }
    }

    pub(super) fn handle_workspace_validated(
        &mut self,
        token: RequestToken,
        error: Option<String>,
    ) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.take_pending(token).is_none() {
            return;
        }

        match error {
            // Not a git repo: clear error in the modal, user re-picks.
            Some(detail) => picker.error = Some(detail),
            None => {
                let machine = picker.machine.clone();
                let path = picker.path.clone();

                self.picker = None;
                self.add_workspace(machine, path);
            }
        }
    }

    /// Opens the folder picker on `machine`. No connection involved: the walk
    /// is plain host execs (SPEC §Workspace "no daemon or provider
    /// involvement"), so a machine whose daemon was never bootstrapped starts
    /// browsing after one SSH round trip instead of a full install.
    pub(super) fn open_picker(&mut self, machine: String) {
        self.picker = Some(FolderPicker {
            machine,
            path: String::new(),
            entries: Vec::new(),
            pending: None,
            error: None,
        });
        self.request_home();
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker = None;
        cx.notify();
    }

    /// The host the open picker browses, or `None` — with the reason in the
    /// modal — when its machine resolves to none.
    fn picker_host(&mut self) -> Option<Host> {
        let machine = self.picker.as_ref()?.machine.clone();

        match self.fleet.host_for(&machine) {
            Ok(host) => Some(host),
            // The fleet's own sentence, rendered in the modal rather than the
            // statusbar — the user is looking at the picker, not at the bar.
            Err(error) => {
                if let Some(picker) = &mut self.picker {
                    picker.error = Some(error.to_string());
                }

                None
            }
        }
    }

    /// Records what the picker is waiting on. Every picker request goes
    /// through here, so "one request in flight, matched by its token" holds
    /// by construction.
    fn set_picker_pending(&mut self, request: PickerRequest) {
        if let Some(picker) = &mut self.picker {
            picker.error = None;
            picker.pending = Some(request);
        }
    }

    fn request_home(&mut self) {
        let Some(host) = self.picker_host() else {
            return;
        };
        let token = self.tokens.mint();

        if self.send_host_command(machines::Command::ResolveHome { host, token }) {
            self.set_picker_pending(PickerRequest::Home(token));
        }
    }

    fn request_listing(&mut self, path: String) {
        let Some(host) = self.picker_host() else {
            return;
        };
        let token = self.tokens.mint();
        let command = machines::Command::ListDir {
            host,
            path: path.clone(),
            token,
        };

        if self.send_host_command(command) {
            self.set_picker_pending(PickerRequest::Listing { token, path });
        }
    }

    fn picker_open_dir(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let Some(entry) = picker.entries.get(entry_index) else {
            return;
        };
        let target_path = join_remote_path(&picker.path, &entry.name);

        self.request_listing(target_path);
        cx.notify();
    }

    fn picker_go_up(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let target_path = parent_remote_path(&picker.path);

        self.request_listing(target_path);
        cx.notify();
    }

    /// "Add" — validate the displayed directory; the store write happens on
    /// `WorkspaceValidated` success.
    fn picker_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let path = picker.path.clone();
        let Some(host) = self.picker_host() else {
            return;
        };
        let token = self.tokens.mint();

        if self.send_host_command(machines::Command::ValidateWorkspace { host, path, token }) {
            self.set_picker_pending(PickerRequest::Validation(token));
        }

        cx.notify();
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
        let resolving_home = matches!(picker.pending, Some(PickerRequest::Home(_)));

        ui::modal_scrim("picker-scrim")
            .on_click(cx.listener(|workbench, _, _, cx| workbench.close_picker(cx)))
            .child(
                ui::modal_frame("Add workspace")
                    // Keeps clicks inside the dialog from reaching the
                    // scrim's dismiss handler.
                    .occlude()
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
                        div()
                            .id("picker-entries")
                            .h(px(MODAL_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .when(!picker.path.is_empty() && picker.path != "/", |list| {
                                list.child(ui::picker_dir_row("picker-up", "..".into()).on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.picker_go_up(cx)),
                                ))
                            })
                            .children(directory_rows)
                            .children(file_rows)
                            .when(resolving_home, |list| {
                                list.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_FAINT))
                                        .child("finding home directory…"),
                                )
                            })
                            .when(
                                matches!(picker.pending, Some(PickerRequest::Listing { .. })),
                                |list| {
                                    list.child(
                                        div()
                                            .px_2()
                                            .py_1()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_FAINT))
                                            .child("loading…"),
                                    )
                                },
                            ),
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

/// The picker's two testable halves: the path walk, and the token guard that
/// decides which answer the modal is allowed to apply. Everything else needs
/// a gpui context and a machine.
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

    fn picker_pending(request: PickerRequest) -> FolderPicker {
        FolderPicker {
            machine: "exe-dev/bloggy".to_string(),
            path: "/home/exedev".to_string(),
            entries: Vec::new(),
            pending: Some(request),
            error: None,
        }
    }

    #[test]
    fn only_the_outstanding_request_is_applied() {
        // The failure this replaces: a picker closed and reopened, or a
        // second listing of the same path, whose first answer would
        // otherwise land on the new walk.
        let tokens = RequestTokens::default();
        let outstanding = tokens.mint();
        let superseded = tokens.mint();
        let mut picker = picker_pending(PickerRequest::Home(outstanding));

        assert!(picker.take_pending(superseded).is_none());
        assert!(picker.busy(), "a stale answer must not clear the slot");
        assert!(picker.take_pending(outstanding).is_some());
        assert!(!picker.busy());
    }

    #[test]
    fn an_answer_is_applied_at_most_once() {
        let token = RequestTokens::default().mint();
        let mut picker = picker_pending(PickerRequest::Validation(token));

        assert!(picker.take_pending(token).is_some());
        assert!(
            picker.take_pending(token).is_none(),
            "the slot is consumed, so a duplicate answers nothing"
        );
    }

    #[test]
    fn a_listing_carries_the_directory_it_would_move_to() {
        // The path is the request's, not the answer's — nothing is echoed
        // back over the wire for the picker to trust.
        let token = RequestTokens::default().mint();
        let mut picker = picker_pending(PickerRequest::Listing {
            token,
            path: "/home/exedev/src".to_string(),
        });

        let Some(PickerRequest::Listing { path, .. }) = picker.take_pending(token) else {
            panic!("the outstanding request is a listing");
        };

        assert_eq!(path, "/home/exedev/src");
    }
}
