mod machine_presentation;
mod modal;
mod picker;
mod sidebar;

use std::collections::HashMap;

use gpui::{div, prelude::*, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::{ActiveTheme as _, Root, WindowExt as _};
use harvester_client_core::{
    ChatId, ClientRuntime, ClientSnapshot, MachineId, MachineStatus, RuntimeEvent, SessionKey,
};

use machine_presentation::present_machine;
use modal::{ModalCoordinator, ModalOutcome};
use sidebar::{Sidebar, SidebarIntent};

use crate::chat::{Chat, ChatFlowRequest};
use crate::store::FleetStore;
use crate::ui::{self, Tone};

const NO_CHAT_HINT: &str = "start a new session to begin";

struct OpenChat {
    chat: Entity<Chat>,
    _flow_requests: Subscription,
}

pub(crate) struct Workbench {
    store: Entity<FleetStore>,
    sidebar: Entity<Sidebar>,
    modal: Entity<ModalCoordinator>,
    chats: HashMap<ChatId, OpenChat>,
    selected: Option<ChatId>,
    _store_repaint: Subscription,
    _store_events: Subscription,
    _sidebar_intents: Subscription,
    _modal_outcomes: Subscription,
}

impl Workbench {
    pub(crate) fn new(
        runtime: ClientRuntime,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let events = runtime.subscribe();
        let store = cx.new(|cx| FleetStore::new(runtime, events, cx));
        let sidebar = cx.new(|cx| Sidebar::new(store.clone(), cx));
        let modal = cx.new(|cx| ModalCoordinator::new(store.clone(), window, cx));
        let store_repaint = cx.observe(&store, |_, _, cx| cx.notify());
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);
        let sidebar_intents = cx.subscribe_in(&sidebar, window, Self::handle_sidebar_intent);
        let modal_outcomes = cx.subscribe_in(&modal, window, Self::handle_modal_outcome);

        Self {
            store,
            sidebar,
            modal,
            chats: HashMap::new(),
            selected: None,
            _store_repaint: store_repaint,
            _store_events: store_events,
            _sidebar_intents: sidebar_intents,
            _modal_outcomes: modal_outcomes,
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            RuntimeEvent::Changed
            | RuntimeEvent::PromptAccepted { .. }
            | RuntimeEvent::PromptCompleted { .. }
            | RuntimeEvent::WorkspaceAdded { .. } => {}
            RuntimeEvent::Failed(detail)
            | RuntimeEvent::Rejected(detail)
            | RuntimeEvent::PromptRejected { detail, .. }
            | RuntimeEvent::DialogAnswerFailed { detail, .. } => {
                self.report(detail.clone(), window, cx);
            }
            RuntimeEvent::DraftPromoted { draft, session } => {
                self.promote_draft(*draft, session.clone(), cx);
            }
            RuntimeEvent::MachineForgotten(machine) => self.machine_forgotten(machine, cx),
            RuntimeEvent::MachineForgetFailed { detail, .. } => {
                self.report(detail.clone(), window, cx);
            }
        }
    }

    fn handle_sidebar_intent(
        &mut self,
        _sidebar: &Entity<Sidebar>,
        intent: &SidebarIntent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match intent {
            SidebarIntent::NewSession => self.open_new_session_chat(window, cx),
            SidebarIntent::OpenSession { session } => {
                self.open_session(session.clone(), window, cx);
            }
            SidebarIntent::ForgetMachine { machine } => {
                self.open_forget_machine(machine.clone(), window, cx);
            }
        }
    }

    fn handle_modal_outcome(
        &mut self,
        _modal: &Entity<ModalCoordinator>,
        outcome: &ModalOutcome,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match outcome {
            ModalOutcome::RepoAdded { draft, workspace } => {
                self.store
                    .read(cx)
                    .select_workspace(*draft, workspace.clone());
                self.store.read(cx).request_controls(ChatId::Draft(*draft));
            }
        }
    }

    fn open_forget_machine(
        &mut self,
        machine: MachineId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.modal.update(cx, |modal, cx| {
            modal.open_forget_machine(machine, window, cx);
        });
    }

    fn report(&self, detail: String, window: &mut Window, cx: &mut Context<'_, Self>) {
        window.push_notification((NotificationType::Error, detail), cx);
    }

    fn install_chat(&mut self, id: ChatId, window: &mut Window, cx: &mut Context<'_, Self>) {
        let store = self.store.clone();
        let chat_id = id.clone();
        let chat = cx.new(|cx| Chat::new(chat_id, store, window, cx));
        let flow_requests = cx.subscribe_in(&chat, window, Self::handle_chat_flow_request);

        self.chats.insert(
            id.clone(),
            OpenChat {
                chat,
                _flow_requests: flow_requests,
            },
        );
        self.select(Some(id), cx);
    }

    fn select(&mut self, selected: Option<ChatId>, cx: &mut Context<'_, Self>) {
        self.selected = selected.clone();
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.select(selected, cx));
        cx.notify();
    }

    fn open_session(
        &mut self,
        session: SessionKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let id = ChatId::Session(session.clone());

        self.store.read(cx).open_session(session);

        if self.chats.contains_key(&id) {
            self.select(Some(id), cx);

            return;
        }

        self.install_chat(id, window, cx);
    }

    fn open_new_session_chat(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let draft = self.store.read(cx).create_draft();
        let id = ChatId::Draft(draft);

        self.install_chat(id.clone(), window, cx);

        if let Some(open) = self.chats.get(&id) {
            open.chat.read(cx).request_controls(cx);
        }
    }

    fn promote_draft(
        &mut self,
        draft: harvester_client_core::DraftId,
        session: SessionKey,
        cx: &mut Context<'_, Self>,
    ) {
        let draft_id = ChatId::Draft(draft);
        let session_id = ChatId::Session(session);
        let Some(open) = self.chats.remove(&draft_id) else {
            return;
        };
        let promoted_was_selected = self.selected.as_ref() == Some(&draft_id);
        let session_was_selected = self.selected.as_ref() == Some(&session_id);

        open.chat
            .update(cx, |chat, cx| chat.set_id(session_id.clone(), cx));
        self.chats.insert(session_id.clone(), open);

        if promoted_was_selected || session_was_selected {
            self.select(Some(session_id), cx);
        }
    }

    fn machine_forgotten(&mut self, machine: &MachineId, cx: &mut Context<'_, Self>) {
        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let removed: Vec<ChatId> = self
            .chats
            .keys()
            .filter(|id| match id {
                ChatId::Session(key) => key.machine() == machine,
                ChatId::Draft(id) => match snapshot.draft(*id) {
                    Some(draft) => draft.target.machine.as_ref() == Some(machine),
                    None => true,
                },
            })
            .cloned()
            .collect();

        for id in &removed {
            self.chats.remove(id);
        }

        if self
            .selected
            .as_ref()
            .is_some_and(|selected| removed.contains(selected))
        {
            self.select(None, cx);
        }
    }

    fn handle_chat_flow_request(
        &mut self,
        _chat: &Entity<Chat>,
        request: &ChatFlowRequest,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match request {
            ChatFlowRequest::AddRepo { draft, machine } => {
                self.open_add_repo(*draft, machine.clone(), window, cx);
            }
        }
    }

    fn open_add_repo(
        &mut self,
        draft: harvester_client_core::DraftId,
        machine: MachineId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.modal.update(cx, |modal, cx| {
            modal.open_add_repo(draft, machine, window, cx);
        });
    }

    fn render_content(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let content = div().flex_1().min_w_0().h_full();

        if let Some(open) = self
            .selected
            .as_ref()
            .and_then(|selected| self.chats.get(selected))
        {
            return content.child(open.chat.clone());
        }

        let store = self.store.read(cx);
        let hint = present_empty_pane(store.snapshot());

        content.child(ui::empty_pane_hint(hint.tone, hint.text, cx))
    }

    fn render_statusbar(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let link = present_statusbar_link(snapshot);

        ui::statusbar_frame(cx).child(div().text_color(link.tone.color(cx)).child(link.text))
    }
}

struct ShellMessage {
    text: String,
    tone: Tone,
}

fn startup_machine_state(snapshot: &ClientSnapshot) -> Option<(&str, &MachineStatus)> {
    let machine = snapshot.startup_machine()?;
    let state = &snapshot.machine(machine)?.status;

    Some((machine.as_str(), state))
}

fn present_empty_pane(snapshot: &ClientSnapshot) -> ShellMessage {
    if let Some(detail) = snapshot.fatal_error() {
        return ShellMessage {
            text: detail.to_string(),
            tone: Tone::Failed,
        };
    }

    let Some((machine, state)) = startup_machine_state(snapshot) else {
        return no_chat_hint();
    };
    let display = present_machine(state, machine);

    match display.pane_message {
        Some(message) => ShellMessage {
            text: message,
            tone: display.tone,
        },
        None => no_chat_hint(),
    }
}

fn no_chat_hint() -> ShellMessage {
    ShellMessage {
        text: NO_CHAT_HINT.to_string(),
        tone: Tone::Idle,
    }
}

fn present_statusbar_link(snapshot: &ClientSnapshot) -> ShellMessage {
    if let Some(detail) = snapshot.fatal_error() {
        return ShellMessage {
            text: detail.to_string(),
            tone: Tone::Failed,
        };
    }

    if let Some((machine, state)) = startup_machine_state(snapshot) {
        let display = present_machine(state, machine);

        if let Some(link) = display.statusbar_link {
            return ShellMessage {
                text: link,
                tone: display.tone,
            };
        }
    }

    ShellMessage {
        text: format!(
            "{} of {} machines connected",
            snapshot.connected_machine_count(),
            snapshot.linked_machine_count()
        ),
        tone: Tone::Idle,
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .text_size(cx.theme().font_size)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.sidebar.clone())
                    .child(self.render_content(cx)),
            )
            .child(self.render_statusbar(cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
