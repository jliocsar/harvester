mod forget_machine_flow;
mod machine_presentation;
mod picker;
mod repo_flow;
mod sidebar;

use gpui::{div, prelude::*, px, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::{ActiveTheme as _, Root, WindowExt as _};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use serde_json::Value;

use forget_machine_flow::{ForgetMachineFlow, MachineForgotten};
use machine_presentation::present_machine;
use repo_flow::{AddRepoFlow, RepoAdded};
use sidebar::{Sidebar, SidebarIntent};

use crate::chat::{Chat, ChatFlowRequest, ChatRequest, ChatTarget};
use crate::dialogs::DialogCoordinator;
use crate::modal::ModalCoordinator;
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};

const NO_CHAT_HINT: &str = "start a new session to begin";

pub struct Workbench {
    store: Entity<FleetStore>,
    sidebar: Entity<Sidebar>,
    add_repo_flow: Entity<AddRepoFlow>,
    forget_machine_flow: Entity<ForgetMachineFlow>,
    _dialogs: Entity<DialogCoordinator>,
    chat: Option<Entity<Chat>>,
    chat_subscriptions: Option<[Subscription; 2]>,
    _subscriptions: [Subscription; 5],
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
        let modals = cx.new(|_| ModalCoordinator::default());
        let add_repo_flow = cx.new(|_| AddRepoFlow::new(store.clone(), modals.clone()));
        let forget_machine_flow = cx.new(|_| ForgetMachineFlow::new(store.clone(), modals.clone()));
        let dialogs = cx.new(|cx| DialogCoordinator::new(store.clone(), modals, window, cx));
        let subscriptions = [
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&store, window, Self::handle_store_event),
            cx.subscribe_in(&sidebar, window, Self::handle_sidebar_intent),
            cx.subscribe_in(&add_repo_flow, window, Self::handle_repo_added),
            cx.subscribe_in(&forget_machine_flow, window, Self::handle_machine_forgotten),
        ];

        Self {
            store,
            sidebar,
            add_repo_flow,
            forget_machine_flow,
            _dialogs: dialogs,
            chat: None,
            chat_subscriptions: None,
            _subscriptions: subscriptions,
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
            } => self.defer_attached(machine.clone(), sid.clone(), entries.clone(), window, cx),
            StoreEvent::Failed(detail) => self.report(detail.clone(), window, cx),
            StoreEvent::Bridge { .. } | StoreEvent::Machines(_) => {}
        }
    }

    fn defer_attached(
        &self,
        machine: String,
        sid: String,
        entries: Vec<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workbench = cx.entity();

        window.defer(cx, move |window, cx| {
            workbench.update(cx, |workbench, cx| {
                workbench.handle_attached(&machine, &sid, &entries, window, cx)
            });
        });
    }

    fn handle_sidebar_intent(
        &mut self,
        _sidebar: &Entity<Sidebar>,
        intent: &SidebarIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let intent = intent.clone();
        let workbench = cx.entity();

        window.defer(cx, move |window, cx| {
            workbench.update(cx, |workbench, cx| {
                workbench.dispatch_sidebar_intent(&intent, window, cx)
            });
        });
    }

    fn dispatch_sidebar_intent(
        &mut self,
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
        self.forget_machine_flow
            .update(cx, |flow, cx| flow.open(machine, window, cx));
    }

    fn handle_machine_forgotten(
        &mut self,
        _flow: &Entity<ForgetMachineFlow>,
        forgotten: &MachineForgotten,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets_machine = self
            .chat
            .as_ref()
            .is_some_and(|chat| chat.read(cx).machine() == forgotten.machine);

        if !targets_machine {
            return;
        }

        self.chat = None;
        self.chat_subscriptions = None;
        self.select_current_session(None, cx);
        cx.notify();
    }

    fn handle_repo_added(
        &mut self,
        _flow: &Entity<AddRepoFlow>,
        added: &RepoAdded,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat.clone() else {
            return;
        };

        if chat.read(cx).machine() != added.machine {
            return;
        }

        let path = added.path.clone();

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
            self.select_current_session(Some(sid.to_string()), cx);

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
        let subscriptions = [
            cx.subscribe_in(&chat, window, Self::handle_chat_request),
            cx.subscribe_in(&chat, window, Self::handle_chat_flow_request),
        ];
        let selected_sid = chat.read(cx).sid().map(str::to_string);

        chat.update(cx, |chat, cx| chat.request_prompt_controls(cx));

        self.chat = Some(chat);
        self.chat_subscriptions = Some(subscriptions);
        self.select_current_session(selected_sid, cx);
        cx.notify();
    }

    fn select_current_session(&self, sid: Option<String>, cx: &mut Context<Self>) {
        self.sidebar
            .update(cx, |sidebar, cx| sidebar.select_session(sid, cx));
    }

    fn workspace_path_of(&self, sid: &str, cx: &Context<Self>) -> String {
        self.store
            .read(cx)
            .session_cwd(sid)
            .unwrap_or_default()
            .to_string()
    }

    fn handle_chat_request(
        &mut self,
        chat: &Entity<Chat>,
        request: &ChatRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let machine = chat.read(cx).machine().to_string();

        if !self.machine_ready_for(request, &machine, cx) {
            let detail = format!(
                "{}: not connected — nothing was sent",
                self.store.read(cx).label(&machine)
            );

            chat.update(cx, |chat, cx| {
                chat.fail_pending_requests(detail.clone(), cx)
            });
            self.report(detail, window, cx);

            return;
        }

        match request {
            ChatRequest::ListModels { cwd, token } => self.send_command(
                &machine,
                Command::ListModels {
                    cwd: cwd.clone(),
                    token: *token,
                },
                cx,
            ),
            ChatRequest::Start { .. } => self.start_session(&machine, request, cx),
            ChatRequest::Prompt { sid, text } => self.send_command(
                &machine,
                Command::Prompt {
                    sid: sid.clone(),
                    text: text.clone(),
                },
                cx,
            ),
            ChatRequest::SetModel {
                sid,
                provider,
                model_id,
                token,
            } => self.send_command(
                &machine,
                Command::SetModel {
                    sid: sid.clone(),
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    token: *token,
                },
                cx,
            ),
            ChatRequest::ModelControls { sid, models, state } => {
                self.request_model_controls(&machine, sid, *models, *state, cx)
            }
        }
    }

    fn start_session(&mut self, machine: &str, request: &ChatRequest, cx: &mut Context<Self>) {
        let ChatRequest::Start {
            sid,
            cwd,
            provider,
            model,
            name,
            first_prompt,
        } = request
        else {
            return;
        };

        self.store.update(cx, |store, cx| {
            store.begin_start(machine.to_string(), sid.clone(), cx)
        });
        self.send_command(
            machine,
            Command::StartSession {
                sid: sid.clone(),
                cwd: cwd.clone(),
                provider: provider.clone(),
                model: model.clone(),
                name: name.clone(),
                first_prompt: first_prompt.clone(),
            },
            cx,
        );
    }

    fn request_model_controls(
        &self,
        machine: &str,
        sid: &str,
        models: harvester_client_core::RequestToken,
        state: harvester_client_core::RequestToken,
        cx: &Context<Self>,
    ) {
        self.send_command(
            machine,
            Command::ListSessionModels {
                sid: sid.to_string(),
                token: models,
            },
            cx,
        );
        self.send_command(
            machine,
            Command::GetSessionState {
                sid: sid.to_string(),
                token: state,
            },
            cx,
        );
    }

    fn machine_ready_for(
        &mut self,
        request: &ChatRequest,
        machine: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if request.is_pre_session() {
            return self
                .store
                .update(cx, |store, cx| store.ensure_connection(machine, cx));
        }

        self.store.read(cx).is_live(machine)
    }

    fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        if !self
            .store
            .update(cx, |store, _| store.may_open_session(sid))
        {
            return;
        }

        self.discard_unstarted_chat(cx);

        let already_open = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

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
            .is_some_and(|chat| chat.read(cx).sid().is_none());

        if !unstarted {
            return;
        }

        self.chat = None;
        self.chat_subscriptions = None;
        self.select_current_session(None, cx);
    }

    fn take_starting_chat(&mut self, sid: &str, cx: &mut Context<Self>) -> Option<Entity<Chat>> {
        let chat = self.chat.clone()?;
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
        let already_unstarted = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

            chat.starting_session().is_none() && chat.sid().is_none()
        });

        if self.store.read(cx).opening_sid().is_some() || already_unstarted {
            return;
        }

        let target = self.store.read(cx).new_session_target();
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::unstarted(target, store, window, cx));

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
        self.add_repo_flow
            .update(cx, |flow, cx| flow.open(machine, window, cx));
    }

    fn send_command(&self, machine: &str, command: Command, cx: &Context<Self>) {
        self.store.read(cx).send(machine, command);
    }

    fn render_content(&self, cx: &Context<Self>) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        if let Some(chat) = &self.chat {
            return content.child(chat.clone());
        }

        let (hint, color) = match self.store.read(cx).argv_machine() {
            Some(machine) => self.argv_machine_hint(machine, cx),
            None => (NO_CHAT_HINT.to_string(), Tone::Idle),
        };

        content.child(ui::empty_pane_hint(color, hint, cx))
    }

    fn argv_machine_hint(&self, machine: &str, cx: &Context<Self>) -> (String, Tone) {
        let display = present_machine(self.store.read(cx).state(machine), machine);

        match display.pane_message {
            Some(message) => (message, display.dot.tone()),
            None => (NO_CHAT_HINT.to_string(), Tone::Idle),
        }
    }

    fn render_statusbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let (link_text, link_tone) = self.statusbar_link(cx);

        ui::statusbar_frame(cx).child(div().text_color(link_tone.color(cx)).child(link_text))
    }

    fn statusbar_link(&self, cx: &Context<Self>) -> (String, Tone) {
        if let Some(link) = self.argv_statusbar_link(cx) {
            return link;
        }

        let store = self.store.read(cx);

        (
            format!(
                "{} of {} machines connected",
                store.connected_machine_count(),
                store.linked_machine_count()
            ),
            Tone::Idle,
        )
    }

    fn argv_statusbar_link(&self, cx: &Context<Self>) -> Option<(String, Tone)> {
        let store = self.store.read(cx);
        let machine = store.argv_machine()?;
        let display = present_machine(store.state(machine), machine);

        Some((display.statusbar_link?, display.dot.tone()))
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
            .text_size(px(ui::BASE_TEXT_SIZE))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.sidebar.clone())
                    .child(self.render_content(cx)),
            )
            .child(self.render_statusbar(cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
