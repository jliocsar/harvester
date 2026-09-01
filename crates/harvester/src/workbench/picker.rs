use gpui::{
    div, prelude::*, rems, App, Context, Entity, IntoElement, Render, SharedString, Task, Window,
};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::{Disableable as _, IndexPath, Sizable as _, WindowExt as _};
use harvester_client_core::{
    RemoteDirEntry, RuntimeEvent, WorkspaceBrowserId, WorkspaceBrowserStatus,
};

use crate::store::FleetStore;
use crate::ui::dialog::{action_button, button_row, dismiss_button, ActionAccent, CANCEL_LABEL};
use crate::ui::{self, Tone};

const ENTRY_LIST_HEIGHT_IN_REMS: f32 = 20.;

const ADD_REPOSITORY_TITLE: &str = "Add repository";
const ADD_REPOSITORY_ACTION_LABEL: &str = "Add";
const ENTRY_FILTER_PLACEHOLDER: &str = "Filter…";
const FINDING_HOME_NOTE: &str = "finding home directory…";
const LISTING_DIRECTORY_NOTE: &str = "loading…";
const VALIDATING_REPOSITORY_NOTE: &str = "checking repository…";

enum EntryRow {
    Parent,
    Directory(SharedString),
    File(SharedString),
}

impl EntryRow {
    fn label(&self) -> SharedString {
        match self {
            Self::Parent => "../".into(),
            Self::Directory(name) => format!("{name}/").into(),
            Self::File(name) => name.clone(),
        }
    }
}

struct EntriesDelegate {
    rows: Vec<EntryRow>,
    matches: Vec<usize>,
}

impl EntriesDelegate {
    const fn new() -> Self {
        Self {
            rows: Vec::new(),
            matches: Vec::new(),
        }
    }

    fn show(&mut self, has_parent: bool, entries: &[RemoteDirEntry]) {
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
        _cx: &mut Context<'_, ListState<Self>>,
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
        _cx: &mut Context<'_, ListState<Self>>,
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
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
    }
}

pub(super) struct FolderPicker {
    store: Entity<FleetStore>,
    browser: WorkspaceBrowserId,
    entries: Entity<ListState<EntriesDelegate>>,
    shown_listing: Option<(String, Vec<(String, bool)>)>,
    _list_events: gpui::Subscription,
    _store_events: gpui::Subscription,
}

impl FolderPicker {
    pub(super) fn new(
        browser: WorkspaceBrowserId,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let entries =
            cx.new(|cx| ListState::new(EntriesDelegate::new(), window, cx).searchable(true));
        let list_events = cx.subscribe_in(&entries, window, Self::handle_list_event);
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            store,
            browser,
            entries,
            shown_listing: None,
            _list_events: list_events,
            _store_events: store_events,
        }
    }

    pub(super) fn cancel(&self, cx: &App) {
        self.store.read(cx).cancel_browser(self.browser);
    }

    fn handle_list_event(
        &mut self,
        entries: &Entity<ListState<EntriesDelegate>>,
        event: &ListEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let ListEvent::Confirm(ix) = event else {
            return;
        };

        match entries.read(cx).delegate().row(ix.row) {
            Some(EntryRow::Parent) => self.store.read(cx).browser_up(self.browser),
            Some(EntryRow::Directory(name)) => self
                .store
                .read(cx)
                .open_browser_directory(self.browser, name.to_string()),
            Some(EntryRow::File(_)) | None => {}
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &RuntimeEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !matches!(event, RuntimeEvent::Changed) {
            return;
        }

        self.sync_entries(cx);
        cx.notify();
    }

    fn sync_entries(&mut self, cx: &mut Context<'_, Self>) {
        let store = self.store.read(cx);
        let Some(browser) = store.snapshot().browser(self.browser) else {
            return;
        };
        let fingerprint: Vec<(String, bool)> = browser
            .entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.is_dir))
            .collect();
        let listing = (browser.path.clone(), fingerprint);

        if self.shown_listing.as_ref() == Some(&listing) {
            return;
        }

        let has_parent = browser.has_parent();
        let entries = browser.entries.clone();

        self.shown_listing = Some(listing);
        self.entries.update(cx, |list, cx| {
            list.delegate_mut().show(has_parent, &entries);
            cx.notify();
        });
    }

    pub(super) fn open_dialog<T: 'static>(
        picker: &Entity<Self>,
        dismissed: impl Fn(&mut T, &mut Window, &mut Context<'_, T>) -> bool + 'static,
        window: &mut Window,
        cx: &mut Context<'_, T>,
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

    fn render_location(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let browser = store.snapshot().browser(self.browser);
        let machine = browser
            .map(|browser| browser.machine_label.as_str())
            .unwrap_or_default();
        let path = browser
            .map(|browser| browser.path.as_str())
            .unwrap_or_default();
        let waiting = browser.and_then(|browser| waiting_note(&browser.status));

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
                    .child(format!("on {machine} — {path}")),
            )
            .children(waiting.map(|note| div().flex_none().child(note)))
    }

    fn render_buttons(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let can_submit = self
            .store
            .read(cx)
            .snapshot()
            .browser(self.browser)
            .is_some_and(harvester_client_core::WorkspaceBrowser::can_submit);

        button_row(
            dismiss_button("picker-cancel", CANCEL_LABEL),
            action_button(
                "picker-add",
                ADD_REPOSITORY_ACTION_LABEL,
                ActionAccent::Primary,
            )
            .disabled(!can_submit)
            .on_click(cx.listener(|picker, _, _, cx| {
                picker.store.read(cx).confirm_browser(picker.browser);
            })),
        )
    }
}

impl Render for FolderPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.sync_entries(cx);

        let error = self
            .store
            .read(cx)
            .snapshot()
            .browser(self.browser)
            .and_then(|browser| browser.error.clone());

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(self.render_location(cx))
            .child(
                List::new(&self.entries)
                    .search_placeholder(ENTRY_FILTER_PLACEHOLDER)
                    .large()
                    .h(rems(ENTRY_LIST_HEIGHT_IN_REMS)),
            )
            .children(error.map(|error| {
                ui::inline_note(Tone::Failed, error, cx)
                    .overflow_hidden()
                    .text_ellipsis()
            }))
            .child(self.render_buttons(cx))
    }
}

fn waiting_note(status: &WorkspaceBrowserStatus) -> Option<&'static str> {
    match status {
        WorkspaceBrowserStatus::LoadingHome => Some(FINDING_HOME_NOTE),
        WorkspaceBrowserStatus::LoadingDirectory => Some(LISTING_DIRECTORY_NOTE),
        WorkspaceBrowserStatus::Validating => Some(VALIDATING_REPOSITORY_NOTE),
        WorkspaceBrowserStatus::Idle => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(name: &str, is_dir: bool) -> RemoteDirEntry {
        RemoteDirEntry {
            name: name.to_string(),
            is_dir,
        }
    }

    #[test]
    fn the_rows_put_the_parent_first_then_directories_then_files() {
        let mut delegate = EntriesDelegate::new();

        delegate.show(true, &[listed("notes.md", false), listed("src", true)]);

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

        delegate.show(false, &[listed("srv", true)]);

        assert_eq!(delegate.rows.len(), 1);
    }

    #[test]
    fn waiting_copy_tracks_the_runtime_operation() {
        assert_eq!(
            waiting_note(&WorkspaceBrowserStatus::LoadingHome),
            Some(FINDING_HOME_NOTE)
        );
        assert_eq!(
            waiting_note(&WorkspaceBrowserStatus::Validating),
            Some(VALIDATING_REPOSITORY_NOTE)
        );
        assert_eq!(waiting_note(&WorkspaceBrowserStatus::Idle), None);
    }
}
