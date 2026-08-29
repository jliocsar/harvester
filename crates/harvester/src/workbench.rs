mod machine_presentation;
mod modal;
mod picker;
mod sidebar;

use gpui::{div, prelude::*, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::{ActiveTheme as _, Root, WindowExt as _};
use harvester_client_core::bridge::Event as BridgeEvent;
use harvester_client_core::MachineState;
use serde_json::Value;

use machine_presentation::present_machine;
use modal::{ModalCoordinator, ModalOutcome};
use sidebar::{Sidebar, SidebarIntent};

use crate::chat::{Chat, ChatFlowRequest, ChatTarget};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};

const NO_CHAT_HINT: &str = "start a new session to begin";

struct OpenChat {
    chat: Entity<Chat>,
    _flow_requests: Subscription,
}

pub struct Workbench {
    store: Entity<FleetStore>,
    sidebar: Entity<Sidebar>,
    modal: Entity<ModalCoordinator>,
    chat: Option<OpenChat>,
    _store_repaint: Subscription,
    _store_events: Subscription,
    _sidebar_intents: Subscription,
    _modal_outcomes: Subscription,
}

impl Workbench {
    pub fn new(
        argv_machine: Option<String>,
        daemon_binary: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = cx.new(|cx| FleetStore::new(argv_machine, daemon_binary, cx));
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
            chat: None,
            _store_repaint: store_repaint,
            _store_events: store_events,
            _sidebar_intents: sidebar_intents,
            _modal_outcomes: modal_outcomes,
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Attached { sid, entries },
            } => self.handle_attached(machine, sid, entries, window, cx),
            StoreEvent::Failed(detail) => self.report(detail.clone(), window, cx),
            StoreEvent::Bridge { .. } | StoreEvent::Machines(_) => {}
        }
    }

    fn handle_sidebar_intent(
        &mut self,
        _sidebar: &Entity<Sidebar>,
        intent: &SidebarIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match intent {
            SidebarIntent::NewSession => self.open_new_session_chat(window, cx),
            SidebarIntent::OpenSession { sid } => self.open_session(sid, cx),
            SidebarIntent::ForgetMachine { machine } => {
                self.open_forget_machine(machine.clone(), window, cx)
            }
        }
    }

    fn open_forget_machine(
        &mut self,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.modal.update(cx, |modal, cx| {
            modal.open_forget_machine(machine, window, cx)
        });
    }

    fn handle_modal_outcome(
        &mut self,
        _modal: &Entity<ModalCoordinator>,
        outcome: &ModalOutcome,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            ModalOutcome::RepoAdded { machine, path } => self.handle_repo_added(machine, path, cx),
            ModalOutcome::MachineForgotten { machine } => {
                self.handle_machine_forgotten(machine, cx)
            }
        }
    }

    fn handle_machine_forgotten(&mut self, machine: &str, cx: &mut Context<Self>) {
        let targets_machine = self
            .chat
            .as_ref()
            .is_some_and(|open| open.chat.read(cx).machine() == machine);

        if !targets_machine {
            return;
        }

        self.chat = None;
        cx.notify();
    }

    fn handle_repo_added(&mut self, machine: &str, path: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.as_ref().map(|open| open.chat.clone()) else {
            return;
        };

        if chat.read(cx).machine() != machine {
            return;
        }

        let path = path.to_string();

        chat.update(cx, |chat, cx| chat.select_repo(path, cx));
    }

    fn report(&self, detail: String, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification((NotificationType::Error, detail), cx);
    }

    fn handle_attached(
        &mut self,
        machine: &str,
        sid: &str,
        entries: &[Value],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.take_starting_chat(sid, cx) {
            chat.update(cx, |chat, cx| {
                chat.promote_to_live(sid.to_string(), entries, cx);
                chat.request_prompt_controls(cx);
            });

            return;
        }

        let target = ChatTarget {
            machine: machine.to_string(),
            workspace_path: self.workspace_path_of(sid, cx),
        };
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::live(sid.to_string(), target, entries, store, window, cx));

        self.install_chat(chat, window, cx);
    }

    fn install_chat(&mut self, chat: Entity<Chat>, window: &mut Window, cx: &mut Context<Self>) {
        let flow_requests = cx.subscribe_in(&chat, window, Self::handle_chat_flow_request);

        chat.update(cx, |chat, cx| chat.request_prompt_controls(cx));

        self.chat = Some(OpenChat {
            chat,
            _flow_requests: flow_requests,
        });
        cx.notify();
    }

    fn workspace_path_of(&self, sid: &str, cx: &Context<Self>) -> String {
        self.store
            .read(cx)
            .catalog()
            .session_cwd(sid)
            .unwrap_or_default()
            .to_string()
    }

    fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        if !self
            .store
            .update(cx, |store, _| store.may_open_session(sid))
        {
            return;
        }

        self.discard_unstarted_chat(cx);

        let already_open = self.chat.as_ref().is_some_and(|open| {
            let chat = open.chat.read(cx);

            chat.sid() == Some(sid) && chat.is_live()
        });

        if !already_open {
            self.store
                .update(cx, |store, cx| store.open_session(sid, cx));
        }

        cx.notify();
    }

    fn discard_unstarted_chat(&mut self, cx: &mut Context<Self>) {
        let unstarted = self
            .chat
            .as_ref()
            .is_some_and(|open| open.chat.read(cx).sid().is_none());

        if !unstarted {
            return;
        }

        self.chat = None;
    }

    fn take_starting_chat(&mut self, sid: &str, cx: &mut Context<Self>) -> Option<Entity<Chat>> {
        let chat = self.chat.as_ref()?.chat.clone();
        let (machine, cwd, name) = {
            let chat = chat.read(cx);
            let (starting_sid, name) = chat.starting_session()?;

            if starting_sid != sid {
                return None;
            }

            (
                chat.machine().to_string(),
                chat.workspace_path().to_string(),
                name.to_string(),
            )
        };

        self.store.update(cx, |store, cx| {
            store.insert_started_session(machine, sid.to_string(), name, cwd, cx)
        });

        Some(chat)
    }

    fn open_new_session_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let already_unstarted = self.chat.as_ref().is_some_and(|open| {
            let chat = open.chat.read(cx);

            chat.starting_session().is_none() && chat.sid().is_none()
        });

        if self.store.read(cx).catalog().opening_sid().is_some() || already_unstarted {
            return;
        }

        let target = self.store.read(cx).new_session_target();
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::unstarted(target, store, window, cx));

        self.store
            .update(cx, |store, cx| store.clear_selected_session(cx));
        self.install_chat(chat, window, cx);
    }

    fn handle_chat_flow_request(
        &mut self,
        _chat: &Entity<Chat>,
        request: &ChatFlowRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match request {
            ChatFlowRequest::AddRepo { machine } => self.open_add_repo(machine.clone(), window, cx),
        }
    }

    fn open_add_repo(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        self.modal
            .update(cx, |modal, cx| modal.open_add_repo(machine, window, cx));
    }

    fn render_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let content = div().flex_1().min_w_0().h_full();

        if let Some(open) = &self.chat {
            return content.child(open.chat.clone());
        }

        let store = self.store.read(cx);
        let hint = present_empty_pane(argv_machine_state(store));

        content.child(ui::empty_pane_hint(hint.tone, hint.text, cx))
    }

    fn render_statusbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let link = present_statusbar_link(
            argv_machine_state(store),
            store.connected_machine_count(),
            store.linked_machine_count(),
        );

        ui::statusbar_frame(cx).child(div().text_color(link.tone.color(cx)).child(link.text))
    }
}

struct ShellMessage {
    text: String,
    tone: Tone,
}

fn argv_machine_state(store: &FleetStore) -> Option<(&str, MachineState<'_>)> {
    let machine = store.argv_machine()?;

    Some((machine, store.state(machine)))
}

fn present_empty_pane(argv_machine: Option<(&str, MachineState<'_>)>) -> ShellMessage {
    let Some((machine, state)) = argv_machine else {
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

fn present_statusbar_link(
    argv_machine: Option<(&str, MachineState<'_>)>,
    connected_machine_count: usize,
    linked_machine_count: usize,
) -> ShellMessage {
    if let Some((machine, state)) = argv_machine {
        let display = present_machine(state, machine);

        if let Some(link) = display.statusbar_link {
            return ShellMessage {
                text: link,
                tone: display.tone,
            };
        }
    }

    ShellMessage {
        text: format!("{connected_machine_count} of {linked_machine_count} machines connected"),
        tone: Tone::Idle,
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
