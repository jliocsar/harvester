//! The add-workspace folder picker (docs/SPEC.md §Flows "Add workspace"):
//! the walk, the host ops that drive it, and its rendering.
//!
//! A child module of `workbench` so it can own this flow end to end without
//! opening the workbench's state to the rest of the crate. The picker is the
//! one surface that talks to the machines worker rather than to a bridge:
//! walking a machine's directories is plain SSH exec work (`$HOME`, `ls`,
//! `git rev-parse`), so browsing a machine costs one round trip and never
//! waits on a harvesterd bootstrap it would not use.
//!
//! Split in two on purpose. [`Walk`] is where it is, what it is waiting on,
//! and what a click decides — no window, no entities, driven end to end in
//! this file's tests. [`FolderPicker`] is the view: the walk plus the
//! virtualized list its rows live in.

use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, IntoElement, Render, SharedString,
    Task, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::{ActiveTheme as _, Disableable as _, IndexPath, WindowExt as _};
use harvester_client_core::machines;
use harvester_client_core::{Host, Pending, RemoteDirEntry, RequestToken, RequestTokens};

use super::Workbench;

/// Fixed height for the entries list, so the dialog does not resize under the
/// pointer when a listing arrives or a filter narrows it.
const ENTRY_LIST_HEIGHT: f32 = 280.;

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

/// The two things the picker cannot do for itself. Everything else — walking,
/// gating on the outstanding request, dismissing its own dialog — it does
/// without telling anyone.
pub(super) enum PickerEvent {
    /// Run this on the machine being browsed. The workbench resolves the host
    /// and owns the machines worker, so the asking is its half of the split.
    Send(PickerCommand),
    /// A directory passed validation: store it and close the walk.
    Stored { machine: String, path: String },
}

impl EventEmitter<PickerEvent> for FolderPicker {}

/// What the picker needs run on its machine. The picker decides *what* to ask
/// for and correlates the answer; the workbench resolves the [`Host`] and owns
/// the machines worker, so it does the asking — the same decide/ask split
/// every workbench-owned surface uses, since only the workbench holds the
/// machines worker.
#[derive(Clone)]
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

/// The walk itself: everything about where the picker is that does not need a
/// window to decide. Its execs are plain host ops on the machines worker — the
/// picker never touches that machine's daemon, so it never waits on one.
struct Walk {
    /// The machine being browsed — where listings and validation run.
    machine: String,
    /// How that machine is named on screen. Carried rather than looked up: the
    /// store is the workbench's, and this walk does not have one.
    machine_label: String,
    /// Directory whose entries are displayed — what "Add" would store. Empty
    /// until `$HOME` answers.
    path: String,
    pending: Pending<PickerRequest>,
    /// Listing or validation failure, rendered inside the modal.
    error: Option<String>,
    /// Minted here now that the walk drives itself from its own clicks.
    tokens: RequestTokens,
}

impl Walk {
    /// A walk on `machine`, already asking for the `$HOME` it starts from. No
    /// connection involved: it is plain host execs (SPEC §Workspace, "no daemon
    /// or provider involvement"), so browsing a machine Harvester never
    /// bootstrapped costs one round trip, not an install.
    fn opening(
        machine: String,
        machine_label: String,
        tokens: RequestTokens,
    ) -> (Walk, PickerCommand) {
        let mut walk = Walk {
            machine,
            machine_label,
            path: String::new(),
            pending: Pending::default(),
            error: None,
            tokens,
        };
        let command = walk.ask(PickerRequest::Home);

        (walk, command)
    }

    /// Records what the walk is waiting on and hands back the command to send.
    /// Every request goes through here, so "one at a time, matched by its
    /// token" holds by construction.
    fn ask(&mut self, request: PickerRequest) -> PickerCommand {
        self.error = None;

        // The context stays in the `Pending` until its answer arrives, so the
        // copy the workbench needs to build the command is made here.
        PickerCommand {
            token: self.pending.ask(&self.tokens, request.clone()),
            request,
        }
    }

    /// The command never reached the worker, so no answer is coming. Without
    /// this the modal would stay gated on a request that does not exist.
    fn request_failed(&mut self) {
        self.pending.cancel();
    }

    /// The reason a host op could not even be started, shown in the modal
    /// rather than as a notification — the user is looking at the picker.
    fn fail(&mut self, detail: String) {
        self.pending.cancel();
        self.error = Some(detail);
    }

    /// `$HOME` answered: the walk starts there, so the answer is immediately a
    /// listing request. `None` means the answer was stale or the exec failed
    /// (the reason is in the modal).
    fn accept_home(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
    ) -> Option<PickerCommand> {
        self.pending.accept(token)?;

        match result {
            Ok(home) => {
                self.path = home.clone();

                Some(self.ask(PickerRequest::Listing { path: home }))
            }
            // Nothing to browse without a start directory; the user cancels and
            // retries (or picks another machine).
            Err(detail) => {
                self.error = Some(detail);

                None
            }
        }
    }

    /// A listing answered. `Some` is the directory's entries, for the view to
    /// display — handed back rather than stored, so the walk keeps no copy of
    /// what is only ever rendered.
    fn accept_listing(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    ) -> Option<Vec<RemoteDirEntry>> {
        let PickerRequest::Listing { path } = self.pending.accept(token)? else {
            return None;
        };

        match result {
            Ok(entries) => {
                self.path = path;
                self.error = None;

                Some(entries)
            }
            // Listing failed (permissions, vanished dir): keep showing the
            // previous directory, surface the reason.
            Err(detail) => {
                self.error = Some(detail);

                None
            }
        }
    }

    /// Validation answered. `Some` is the workspace to store — the walk is
    /// spent at that point and the workbench drops it.
    fn accept_validation(
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

    /// Walk into a listed directory, by name — the walk holds no listing to
    /// index into, and a name is what the row carries anyway. `None` while a
    /// request is out, so the modal never has two answers to reconcile.
    fn open_dir(&mut self, name: &str) -> Option<PickerCommand> {
        if self.busy() {
            return None;
        }

        let path = join_remote_path(&self.path, name);

        Some(self.ask(PickerRequest::Listing { path }))
    }

    fn go_up(&mut self) -> Option<PickerCommand> {
        if self.busy() {
            return None;
        }

        let path = parent_remote_path(&self.path);

        Some(self.ask(PickerRequest::Listing { path }))
    }

    /// "Add": validate the displayed directory. The store write happens when
    /// the answer lands.
    fn confirm(&mut self) -> Option<PickerCommand> {
        if !self.can_submit() {
            return None;
        }

        let path = self.path.clone();

        Some(self.ask(PickerRequest::Validation { path }))
    }

    fn busy(&self) -> bool {
        self.pending.is_waiting()
    }

    /// Nothing to add before `$HOME` lands (or after it failed): the displayed
    /// directory is what "Add" would store.
    fn can_submit(&self) -> bool {
        !self.busy() && !self.path.is_empty()
    }

    /// Whether the walk has somewhere above it to go.
    fn has_parent(&self) -> bool {
        !self.path.is_empty() && self.path != "/"
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

/// One row of the displayed listing.
enum EntryRow {
    /// The step back up; present unless the walk is at the root.
    Parent,
    Directory(SharedString),
    /// Shown for orientation only — a walk descends into directories.
    File(SharedString),
}

impl EntryRow {
    fn label(&self) -> SharedString {
        match self {
            EntryRow::Parent => "../".into(),
            EntryRow::Directory(name) => format!("{name}/").into(),
            EntryRow::File(name) => name.clone(),
        }
    }
}

/// The rows on screen and which of them the filter leaves standing. Display
/// state only — the walk keeps none of it.
struct EntriesDelegate {
    rows: Vec<EntryRow>,
    /// Indices into `rows`, in display order.
    matches: Vec<usize>,
}

impl EntriesDelegate {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            matches: Vec::new(),
        }
    }

    /// Directories before files, both in `ls`'s alphabetical order.
    fn show(&mut self, has_parent: bool, entries: Vec<RemoteDirEntry>) {
        let mut rows = Vec::with_capacity(entries.len() + 1);

        if has_parent {
            rows.push(EntryRow::Parent);
        }

        rows.extend(
            entries
                .iter()
                .filter(|entry| entry.is_dir)
                .map(|entry| EntryRow::Directory(entry.name.clone().into())),
        );
        rows.extend(
            entries
                .iter()
                .filter(|entry| !entry.is_dir)
                .map(|entry| EntryRow::File(entry.name.clone().into())),
        );

        self.matches = (0..rows.len()).collect();
        self.rows = rows;
    }

    fn row(&self, displayed: usize) -> Option<&EntryRow> {
        self.rows.get(*self.matches.get(displayed)?)
    }
}

impl ListDelegate for EntriesDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matches.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let row = self.row(ix.row)?;

        Some(
            ListItem::new(ix.row)
                .disabled(matches!(row, EntryRow::File(_)))
                .child(row.label()),
        )
    }

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_lowercase();

        self.matches = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| query.is_empty() || row.label().to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();

        Task::ready(())
    }

    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
    }
}

/// The add-workspace modal's view.
///
/// A view of its own, because `gpui-component` builds a dialog's body from the
/// *root* view's render, where reading the workbench would be reading an entity
/// that is mid-update — see [`crate::choices`] for the whole rule.
pub(super) struct FolderPicker {
    walk: Walk,
    entries: Entity<ListState<EntriesDelegate>>,
    _confirmed: gpui::Subscription,
}

impl FolderPicker {
    fn new(walk: Walk, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let entries =
            cx.new(|cx| ListState::new(EntriesDelegate::new(), window, cx).searchable(true));

        Self {
            walk,
            _confirmed: cx.subscribe_in(&entries, window, Self::handle_list_event),
            entries,
        }
    }

    pub fn machine(&self) -> &str {
        &self.walk.machine
    }

    /// A row was clicked or Entered. Files confirm to nothing — the list
    /// reports every click, whatever the row's own disabled flag says.
    fn handle_list_event(
        &mut self,
        entries: &Entity<ListState<EntriesDelegate>>,
        event: &ListEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ListEvent::Confirm(ix) = event else {
            return;
        };
        let command = match entries.read(cx).delegate().row(ix.row) {
            Some(EntryRow::Parent) => self.walk.go_up(),
            Some(EntryRow::Directory(name)) => {
                let name = name.clone();

                self.walk.open_dir(&name)
            }
            Some(EntryRow::File(_)) | None => return,
        };

        self.dispatch(command, cx);
    }

    /// Send what an action decided, and redraw either way — a click that
    /// decided nothing (one was already out) still has a modal to repaint.
    fn dispatch(&mut self, command: Option<PickerCommand>, cx: &mut Context<Self>) {
        if let Some(command) = command {
            cx.emit(PickerEvent::Send(command));
        }

        cx.notify();
    }

    /// Where the picker sits on screen: `gpui-component`'s dialog, holding this
    /// view as its body. Dismissal (Escape, a click outside, Cancel) is the
    /// dialog's, which is why nothing here tracks whether it is open.
    fn open_dialog(picker: &Entity<Self>, window: &mut Window, cx: &mut Context<Workbench>) {
        let picker = picker.clone();

        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Add workspace")
                .close_button(false)
                .child(picker.clone())
        });
    }

    /// The walk's context line: which machine, where in it, and what it is
    /// waiting on.
    fn render_location(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(format!(
                        "on {} — {}",
                        self.walk.machine_label, self.walk.path
                    )),
            )
            .children(
                self.walk
                    .waiting_note()
                    .map(|note| div().flex_none().child(note)),
            )
    }

    fn render_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_submit = self.walk.can_submit();

        div()
            .flex()
            .justify_end()
            .gap_2()
            .child(Button::new("picker-cancel").label("Cancel").on_click(
                // The dialog is the picker's own; closing it is not news
                // for the workbench.
                cx.listener(|_, _, window, cx| window.close_dialog(cx)),
            ))
            .child(
                Button::new("picker-add")
                    .label("Add")
                    .primary()
                    .disabled(!can_submit)
                    .on_click(cx.listener(|picker, _, _, cx| {
                        let command = picker.walk.confirm();

                        picker.dispatch(command, cx);
                    })),
            )
    }
}

impl Render for FolderPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_location(cx))
            // A home directory with a thousand entries costs the dozen on
            // screen, and the search box filters what `ls` already returned —
            // no second round trip.
            .child(
                List::new(&self.entries)
                    .search_placeholder("Filter…")
                    .h(px(ENTRY_LIST_HEIGHT)),
            )
            .children(self.walk.error.clone().map(|error| {
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(error)
            }))
            .child(self.render_buttons(cx))
    }
}

impl Workbench {
    /// Act on the open picker and redraw — the picker's own state, the shape
    /// `with_chat` used to have for a sid-scoped one.
    pub(in crate::workbench) fn with_picker(
        &mut self,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut FolderPicker, &mut Context<FolderPicker>),
    ) {
        let Some(picker) = self.picker.clone() else {
            return;
        };

        picker.update(cx, |picker, cx| {
            act(picker, cx);
            cx.notify();
        });
    }

    /// Run what the picker asked for on the machine it is browsing. The host
    /// is resolved here because it is the fleet's answer, and a machine that
    /// resolves to none puts its reason in the modal rather than in a
    /// notification — the user is looking at the dialog.
    fn send_picker_request(&mut self, command: PickerCommand, cx: &mut Context<Self>) {
        let host = match self.picker_host(cx) {
            Ok(host) => host,
            Err(detail) => {
                self.with_picker(cx, |picker, _| picker.walk.fail(detail));

                return;
            }
        };
        let sent = self.store.update(cx, |store, cx| {
            store.send_host_command(command.into_machines_command(host), cx)
        });

        if !sent {
            self.with_picker(cx, |picker, _| picker.walk.request_failed());
        }
    }

    /// The host the open picker browses.
    fn picker_host(&self, cx: &Context<Self>) -> Result<Host, String> {
        let Some(picker) = &self.picker else {
            return Err("no picker open".to_string());
        };

        // The fleet's own sentence, carried verbatim.
        self.store.read(cx).host_for(picker.read(cx).machine())
    }

    pub(super) fn handle_home_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.with_picker(cx, |picker, cx| {
            let command = picker.walk.accept_home(token, result);

            picker.dispatch(command, cx);
        });
    }

    pub(super) fn handle_dir_listed(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        self.with_picker(cx, |picker, cx| {
            let Some(entries) = picker.walk.accept_listing(token, result) else {
                return;
            };
            let has_parent = picker.walk.has_parent();

            picker.entries.update(cx, |list, cx| {
                list.delegate_mut().show(has_parent, entries);
                cx.notify();
            });
        });
    }

    /// A validated workspace is stored and the picker is spent.
    pub(super) fn handle_workspace_validated(
        &mut self,
        token: RequestToken,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.with_picker(cx, |picker, cx| {
            let Some((machine, path)) = picker.walk.accept_validation(token, error) else {
                return;
            };

            cx.emit(PickerEvent::Stored { machine, path });
        });
    }

    /// Browse `machine`, in a dialog of its own. The picker is kept because
    /// answers from the machines worker are routed to it; whether it is on
    /// screen is the dialog's business, not this field's.
    pub(super) fn open_picker(
        &mut self,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = self.store.read(cx).label(&machine);
        let tokens = self.store.read(cx).tokens();
        let (walk, home) = Walk::opening(machine, label, tokens);
        let picker = cx.new(|cx| FolderPicker::new(walk, window, cx));

        cx.subscribe_in(&picker, window, Self::handle_picker_event)
            .detach();
        FolderPicker::open_dialog(&picker, window, cx);

        self.picker = Some(picker);
        self.send_picker_request(home, cx);
    }

    fn handle_picker_event(
        &mut self,
        _picker: &Entity<FolderPicker>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PickerEvent::Send(command) => self.send_picker_request(command.clone(), cx),
            PickerEvent::Stored { machine, path } => {
                window.close_dialog(cx);
                self.picker = None;
                self.store.update(cx, |store, cx| {
                    store.add_workspace(machine.clone(), path.clone(), cx)
                });
            }
        }
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

/// The walk, end to end, with no `Workbench`, no window and no list — which is
/// what [`Walk`] exists to make possible. The token guard itself is
/// [`Pending`]'s, and tested there.
#[cfg(test)]
mod tests {
    use super::*;
    use harvester_client_core::RequestTokens;

    fn opening_walk() -> (Walk, PickerCommand) {
        Walk::opening(
            "exe-dev/bloggy".to_string(),
            "bloggy".to_string(),
            RequestTokens::default(),
        )
    }

    fn listed(name: &str, is_dir: bool) -> RemoteDirEntry {
        RemoteDirEntry {
            name: name.to_string(),
            is_dir,
        }
    }

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

    #[test]
    fn the_walk_goes_home_then_lists_then_validates() {
        let (mut walk, home) = opening_walk();

        assert!(matches!(home.request, PickerRequest::Home));
        assert!(walk.busy(), "the modal is gated until $HOME lands");
        assert_eq!(walk.waiting_note(), Some("finding home directory…"));

        let listing = walk
            .accept_home(home.token, Ok("/home/exedev".to_string()))
            .expect("home starts the walk");

        assert!(
            matches!(&listing.request, PickerRequest::Listing { path } if path == "/home/exedev")
        );

        let entries = walk
            .accept_listing(listing.token, Ok(vec![listed("repo", true)]))
            .expect("a listing to display");

        assert_eq!(entries.len(), 1);
        assert!(!walk.busy());
        assert_eq!(walk.path, "/home/exedev");

        let descend = walk.open_dir("repo").expect("a listed directory");

        assert!(
            matches!(&descend.request, PickerRequest::Listing { path } if path == "/home/exedev/repo")
        );

        walk.accept_listing(descend.token, Ok(Vec::new()));

        let validate = walk.confirm().expect("a directory to add");

        assert!(
            matches!(&validate.request, PickerRequest::Validation { path } if path == "/home/exedev/repo")
        );
        assert_eq!(
            walk.accept_validation(validate.token, None),
            Some((
                "exe-dev/bloggy".to_string(),
                "/home/exedev/repo".to_string()
            ))
        );
    }

    #[test]
    fn a_click_while_a_request_is_out_is_ignored() {
        let (mut walk, _home) = opening_walk();

        assert!(walk.open_dir("repo").is_none());
        assert!(walk.go_up().is_none());
        assert!(walk.confirm().is_none());
    }

    /// A failed exec keeps the walk where it was and says why, rather than
    /// leaving the modal gated on an answer that is not coming.
    #[test]
    fn a_failed_listing_keeps_the_previous_directory() {
        let (mut walk, home) = opening_walk();
        let listing = walk
            .accept_home(home.token, Ok("/home/exedev".to_string()))
            .expect("home starts the walk");

        walk.accept_listing(listing.token, Ok(Vec::new()));

        let descend = walk.ask(PickerRequest::Listing {
            path: "/root".to_string(),
        });

        assert!(walk
            .accept_listing(descend.token, Err("permission denied".to_string()))
            .is_none());
        assert_eq!(walk.path, "/home/exedev");
        assert_eq!(walk.error.as_deref(), Some("permission denied"));
        assert!(!walk.busy());
    }

    #[test]
    fn a_command_that_never_left_ungates_the_modal() {
        let (mut walk, _home) = opening_walk();

        walk.request_failed();

        assert!(!walk.busy());
    }

    #[test]
    fn a_rejected_workspace_stores_nothing_and_says_why() {
        let (mut walk, home) = opening_walk();
        let listing = walk
            .accept_home(home.token, Ok("/home/exedev".to_string()))
            .expect("home starts the walk");

        walk.accept_listing(listing.token, Ok(Vec::new()));

        let validate = walk.confirm().expect("a directory to add");
        let stored = walk.accept_validation(validate.token, Some("not a git repo".to_string()));

        assert_eq!(stored, None);
        assert_eq!(walk.error.as_deref(), Some("not a git repo"));
    }

    fn idle_walk() -> Walk {
        Walk {
            machine: "exe-dev/bloggy".to_string(),
            machine_label: "bloggy".to_string(),
            path: "/home/exedev".to_string(),
            pending: Pending::default(),
            error: None,
            tokens: RequestTokens::default(),
        }
    }

    #[test]
    fn a_listing_carries_the_directory_it_would_move_to() {
        // The path is the request's, not the answer's — nothing is echoed
        // back over the wire for the walk to trust.
        let mut walk = idle_walk();
        let tokens = walk.tokens.clone();
        let token = walk.pending.ask(
            &tokens,
            PickerRequest::Listing {
                path: "/home/exedev/src".to_string(),
            },
        );

        assert!(walk.busy(), "the modal is gated while the answer is out");
        assert_eq!(walk.waiting_note(), Some("loading…"));

        let Some(PickerRequest::Listing { path }) = walk.pending.accept(token) else {
            panic!("the outstanding request is a listing");
        };

        assert_eq!(path, "/home/exedev/src");
        assert!(!walk.busy());
    }

    /// The modal must not stay gated on a request that never left: a host
    /// command the worker refused has no answer coming.
    #[test]
    fn a_request_that_never_left_stops_gating_the_modal() {
        let mut walk = idle_walk();
        let tokens = walk.tokens.clone();

        walk.pending.ask(&tokens, PickerRequest::Home);
        walk.pending.cancel();

        assert!(!walk.busy());
    }

    /// The `..` row exists only when there is somewhere to go, and directories
    /// sort ahead of the files that are there for orientation.
    #[test]
    fn the_rows_put_the_parent_first_then_directories_then_files() {
        let mut delegate = EntriesDelegate::new();

        delegate.show(true, vec![listed("notes.md", false), listed("src", true)]);

        let labels: Vec<String> = delegate
            .rows
            .iter()
            .map(|row| row.label().to_string())
            .collect();

        assert_eq!(labels, vec!["../", "src/", "notes.md"]);
    }

    #[test]
    fn the_root_listing_has_no_parent_row() {
        let mut delegate = EntriesDelegate::new();

        delegate.show(false, vec![listed("srv", true)]);

        assert_eq!(delegate.rows.len(), 1);
    }
}
