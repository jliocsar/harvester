use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, IntoElement, Render, SharedString,
    Task, Window,
};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::{Disableable as _, IndexPath, WindowExt as _};
use harvester_client_core::machines;
use harvester_client_core::{Pending, RemoteDirEntry, RequestToken, RequestTokens};

use crate::store::{FleetStore, StoreEvent};
use crate::ui::dialog::{action_button, button_row, dismiss_button, ActionAccent, CANCEL_LABEL};
use crate::ui::{self, Tone};

const ENTRY_LIST_HEIGHT: f32 = 280.;

const ADD_REPOSITORY_TITLE: &str = "Add repository";
const ADD_REPOSITORY_ACTION_LABEL: &str = "Add";
const ENTRY_FILTER_PLACEHOLDER: &str = "Filter…";
const FINDING_HOME_NOTE: &str = "finding home directory…";
const LISTING_DIRECTORY_NOTE: &str = "loading…";

#[derive(Clone)]
enum PickerRequest {
    Home,
    Listing { path: String },
    Validation { path: String },
}

pub(super) struct WorkspacePicked {
    pub path: String,
}

impl EventEmitter<WorkspacePicked> for FolderPicker {}

struct PickerCommand {
    request: PickerRequest,
    token: RequestToken,
}

impl PickerCommand {
    fn into_machines_command(self, host: harvester_client_core::Host) -> machines::Command {
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

struct Walk {
    machine: String,
    machine_label: String,
    path: String,
    pending: Pending<PickerRequest>,
    error: Option<String>,
    tokens: RequestTokens,
}

impl Walk {
    fn new(machine: String, machine_label: String, tokens: RequestTokens) -> Self {
        Self {
            machine,
            machine_label,
            path: String::new(),
            pending: Pending::default(),
            error: None,
            tokens,
        }
    }

    fn ask(&mut self, request: PickerRequest) -> PickerCommand {
        self.error = None;

        PickerCommand {
            token: self.pending.ask(&self.tokens, request.clone()),
            request,
        }
    }

    fn stop_waiting(&mut self) {
        self.pending.cancel();
    }

    fn fail(&mut self, detail: String) {
        self.stop_waiting();
        self.error = Some(detail);
    }

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
            Err(detail) => {
                self.error = Some(detail);

                None
            }
        }
    }

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
            Err(detail) => {
                self.error = Some(detail);

                None
            }
        }
    }

    fn accept_validation(
        &mut self,
        token: RequestToken,
        result: Result<(), String>,
    ) -> Option<String> {
        self.pending.accept(token)?;

        match result {
            Err(detail) => {
                self.error = Some(detail);

                None
            }
            Ok(()) => Some(self.path.clone()),
        }
    }

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

    fn can_submit(&self) -> bool {
        !self.busy() && !self.path.is_empty()
    }

    fn has_parent(&self) -> bool {
        !self.path.is_empty() && self.path != "/"
    }

    fn waiting_note(&self) -> Option<&'static str> {
        if !self.busy() {
            return None;
        }

        if self.path.is_empty() {
            return Some(FINDING_HOME_NOTE);
        }

        Some(LISTING_DIRECTORY_NOTE)
    }
}

enum EntryRow {
    Parent,
    Directory(SharedString),
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

struct EntriesDelegate {
    rows: Vec<EntryRow>,
    matches: Vec<usize>,
}

impl EntriesDelegate {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            matches: Vec::new(),
        }
    }

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

pub(super) struct FolderPicker {
    store: Entity<FleetStore>,
    walk: Walk,
    entries: Entity<ListState<EntriesDelegate>>,
    _list_events: gpui::Subscription,
    _store_events: gpui::Subscription,
}

impl FolderPicker {
    pub(super) fn new(
        machine: String,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label = store.read(cx).label(&machine);
        let tokens = store.read(cx).tokens();
        let walk = Walk::new(machine, label, tokens);
        let entries =
            cx.new(|cx| ListState::new(EntriesDelegate::new(), window, cx).searchable(true));
        let list_events = cx.subscribe_in(&entries, window, Self::handle_list_event);
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            store,
            walk,
            entries,
            _list_events: list_events,
            _store_events: store_events,
        }
    }

    pub(super) fn start(&mut self, cx: &mut Context<Self>) {
        let home = self.walk.ask(PickerRequest::Home);

        self.send_request(home, cx);
    }

    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        self.walk.stop_waiting();
        cx.notify();
    }

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

    fn dispatch(&mut self, command: Option<PickerCommand>, cx: &mut Context<Self>) {
        if let Some(command) = command {
            self.send_request(command, cx);
        }

        cx.notify();
    }

    fn send_request(&mut self, command: PickerCommand, cx: &mut Context<Self>) {
        let host = match self.store.read(cx).host_for(&self.walk.machine) {
            Ok(host) => host,
            Err(error) => {
                self.walk.fail(error.to_string());
                cx.notify();

                return;
            }
        };
        let sent = self.store.update(cx, |store, cx| {
            store.send_host_command(command.into_machines_command(host), cx)
        });

        if !sent {
            self.walk.stop_waiting();
            cx.notify();
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Machines(machines::Event::HomeResolved { token, result }) => {
                self.handle_home_resolved(*token, result.clone(), cx)
            }
            StoreEvent::Machines(machines::Event::DirListed { token, result }) => {
                self.handle_dir_listed(*token, result.clone(), cx)
            }
            StoreEvent::Machines(machines::Event::WorkspaceValidated { token, result }) => {
                self.handle_workspace_validated(*token, result.clone(), cx)
            }
            StoreEvent::Bridge { .. }
            | StoreEvent::Machines(
                machines::Event::Listed { .. }
                | machines::Event::Probed { .. }
                | machines::Event::ProviderFailed { .. }
                | machines::Event::BranchResolved { .. },
            )
            | StoreEvent::Failed(_) => {}
        }
    }

    fn handle_home_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let command = self.walk.accept_home(token, result);

        self.dispatch(command, cx);
    }

    fn handle_dir_listed(
        &mut self,
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(entries) = self.walk.accept_listing(token, result) else {
            cx.notify();

            return;
        };
        let has_parent = self.walk.has_parent();

        self.entries.update(cx, |list, cx| {
            list.delegate_mut().show(has_parent, entries);
            cx.notify();
        });
        cx.notify();
    }

    fn handle_workspace_validated(
        &mut self,
        token: RequestToken,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.walk.accept_validation(token, result) else {
            cx.notify();

            return;
        };

        cx.emit(WorkspacePicked { path });
    }

    pub(super) fn open_dialog<T: 'static>(
        picker: &Entity<Self>,
        dismissed: impl Fn(&mut T, &mut Window, &mut Context<T>) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
        let owner = cx.entity();
        let picker = picker.clone();
        let dismissed = std::rc::Rc::new(dismissed);

        window.open_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            let dismissed = dismissed.clone();

            dialog
                .title(ADD_REPOSITORY_TITLE)
                .close_button(false)
                .child(picker.clone())
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, window, cx| {
                    owner.update(cx, |owner, cx| dismissed(owner, window, cx))
                })
        });
    }

    fn render_location(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .text_sm()
            .text_color(Tone::Muted.color(cx))
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

        button_row(
            dismiss_button("picker-cancel", CANCEL_LABEL),
            action_button(
                "picker-add",
                ADD_REPOSITORY_ACTION_LABEL,
                ActionAccent::Primary,
            )
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
            .child(
                List::new(&self.entries)
                    .search_placeholder(ENTRY_FILTER_PLACEHOLDER)
                    .h(px(ENTRY_LIST_HEIGHT)),
            )
            .children(self.walk.error.clone().map(|error| {
                ui::inline_note(Tone::Failed, error, cx)
                    .overflow_hidden()
                    .text_ellipsis()
            }))
            .child(self.render_buttons(cx))
    }
}

fn join_remote_path(path: &str, name: &str) -> String {
    format!("{}/{name}", path.trim_end_matches('/'))
}

fn parent_remote_path(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(separator_index) => path[..separator_index].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvester_client_core::RequestTokens;

    fn opening_walk() -> (Walk, PickerCommand) {
        let mut walk = Walk::new(
            "exe-dev/bloggy".to_string(),
            "bloggy".to_string(),
            RequestTokens::default(),
        );
        let home = walk.ask(PickerRequest::Home);

        (walk, home)
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
    fn a_new_walk_gates_nothing_until_it_asks_for_home() {
        let mut walk = Walk::new(
            "exe-dev/bloggy".to_string(),
            "bloggy".to_string(),
            RequestTokens::default(),
        );

        assert!(!walk.busy());
        assert_eq!(walk.waiting_note(), None);

        walk.ask(PickerRequest::Home);

        assert!(walk.busy());
        assert_eq!(walk.waiting_note(), Some("finding home directory…"));
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
            walk.accept_validation(validate.token, Ok(())),
            Some("/home/exedev/repo".to_string())
        );
    }

    #[test]
    fn a_click_while_a_request_is_out_is_ignored() {
        let (mut walk, _home) = opening_walk();

        assert!(walk.open_dir("repo").is_none());
        assert!(walk.go_up().is_none());
        assert!(walk.confirm().is_none());
    }

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

        walk.stop_waiting();

        assert!(!walk.busy());
    }

    #[test]
    fn cancellation_invalidates_an_outstanding_validation() {
        let (mut walk, home) = opening_walk();
        let listing = walk
            .accept_home(home.token, Ok("/home/exedev".to_string()))
            .expect("home starts the walk");

        walk.accept_listing(listing.token, Ok(Vec::new()));

        let validation = walk.confirm().expect("validation request");

        walk.stop_waiting();

        assert!(walk.accept_validation(validation.token, Ok(())).is_none());
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
        let stored = walk.accept_validation(validate.token, Err("not a git repo".to_string()));

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

    #[test]
    fn a_request_that_never_left_stops_gating_the_modal() {
        let mut walk = idle_walk();
        let tokens = walk.tokens.clone();

        walk.pending.ask(&tokens, PickerRequest::Home);
        walk.pending.cancel();

        assert!(!walk.busy());
    }

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
