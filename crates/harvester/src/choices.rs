//! The one-of-many modal: pick a row, the dialog closes, the owner is told
//! which row it was.
//!
//! Three surfaces are the same thing — the chat's model list, the chat's
//! machine switch, and the add-workspace machine step — so they are one view
//! rather than three renderers. A view, and not a builder closure, because
//! `gpui-component` builds a dialog's body from the *root* view's render:
//! reaching for the workbench or the chat there would be reading an entity
//! that is mid-update, which gpui panics on (SPEC C11).
//!
//! The rows themselves are `gpui-component`'s virtualized [`List`], so a long
//! model list is searchable and costs only the rows on screen.

use gpui::{
    prelude::*, App, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window,
};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::IndexPath;
use gpui_component::{IconName, WindowExt as _};

/// The row the user picked, as an index into the rows the caller passed in.
pub struct ChoicePicked(pub usize);

/// Fixed height for a searchable choice list. A list that arrives
/// asynchronously (models being fetched) must not resize the dialog under the
/// pointer between one click and the next.
const CHOICE_LIST_HEIGHT: f32 = 280.;

pub struct ChoiceRow {
    pub label: SharedString,
    /// The row that is already in effect — it carries the check mark.
    pub selected: bool,
}

/// How a choice list presents itself.
///
/// The two cases travel together: a handful of machines is entirely on screen
/// and searching it would be noise, while a model list is long enough to
/// scroll, arrives late, and is worth filtering.
#[derive(Clone, Copy)]
pub enum ChoiceList {
    Short,
    Searchable,
}

impl ChoiceList {
    fn is_searchable(self) -> bool {
        matches!(self, ChoiceList::Searchable)
    }
}

/// The rows, and which of them the search query leaves standing.
struct ChoiceDelegate {
    rows: Vec<ChoiceRow>,
    /// Indices into `rows`, in display order. A pick is reported as the
    /// caller's index, not the display one — the caller holds a snapshot the
    /// row was built from, and a filtered list would otherwise point at the
    /// wrong entry in it.
    matches: Vec<usize>,
}

impl ChoiceDelegate {
    fn new(rows: Vec<ChoiceRow>) -> Self {
        let matches = (0..rows.len()).collect();

        Self { rows, matches }
    }

    /// The caller's index for a displayed row.
    fn origin(&self, row: usize) -> Option<usize> {
        self.matches.get(row).copied()
    }
}

impl ListDelegate for ChoiceDelegate {
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
        let row = self.rows.get(self.origin(ix.row)?)?;

        Some(
            ListItem::new(ix.row)
                .check_icon(IconName::Check)
                .confirmed(row.selected)
                .child(row.label.clone()),
        )
    }

    /// Case-insensitive substring match, which is what a model id or a machine
    /// name is picked by. An empty query is every row.
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
            .filter(|(_, row)| query.is_empty() || row.label.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();

        Task::ready(())
    }

    /// The list keeps its own cursor; nothing here needs to remember it.
    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
    }
}

pub struct Choices {
    list: Entity<ListState<ChoiceDelegate>>,
    presentation: ChoiceList,
    _picked: Subscription,
}

impl EventEmitter<ChoicePicked> for Choices {}

impl Choices {
    /// A click or Enter on a row. Escape emits nothing — the dialog's own
    /// dismissal handles it, which is why nothing here tracks being open.
    fn handle_list_event(
        &mut self,
        list: &Entity<ListState<ChoiceDelegate>>,
        event: &ListEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ListEvent::Confirm(ix) = event else {
            return;
        };
        let Some(picked) = list.read(cx).delegate().origin(ix.row) else {
            return;
        };

        cx.emit(ChoicePicked(picked));
    }
}

impl Render for Choices {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let list = List::new(&self.list).search_placeholder("Filter…");

        match self.presentation {
            ChoiceList::Short => list.max_h(gpui::px(CHOICE_LIST_HEIGHT)),
            ChoiceList::Searchable => list.h(gpui::px(CHOICE_LIST_HEIGHT)),
        }
    }
}

/// Open the modal. `picked` runs on the owner with the index of the row the
/// user chose, after the dialog is gone.
pub fn open<T: 'static>(
    title: &'static str,
    presentation: ChoiceList,
    rows: Vec<ChoiceRow>,
    picked: impl Fn(&mut T, usize, &mut Window, &mut Context<T>) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let choices = cx.new(|cx| {
        let list = cx.new(|cx| {
            ListState::new(ChoiceDelegate::new(rows), window, cx)
                .searchable(presentation.is_searchable())
        });

        Choices {
            _picked: cx.subscribe_in(&list, window, Choices::handle_list_event),
            list,
            presentation,
        }
    });

    cx.subscribe_in(
        &choices,
        window,
        move |owner, _, event: &ChoicePicked, window, cx| {
            window.close_dialog(cx);
            picked(owner, event.0, window, cx);
        },
    )
    .detach();

    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title(title)
            .close_button(false)
            .child(choices.clone())
    });
}
