//! The window composition shell (docs/SPEC.md C12).
//!
//! Workbench owns the current chat selection and the orchestration that truly
//! needs the root window: constructing a chat input, raising fleet failures,
//! and placing Root's overlay layers. Sidebar, add-workspace, dialog, and chat
//! flows own themselves behind narrow event seams.

mod machine_presentation;
mod picker;
mod sidebar;
mod workspace_flow;

use gpui::{div, prelude::*, px, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::{ActiveTheme as _, Root, WindowExt as _};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use serde_json::Value;

use machine_presentation::present_machine;
use sidebar::{Sidebar, SidebarIntent};
use workspace_flow::AddWorkspaceFlow;

use crate::chat::{Chat, ChatRequest, ChatTarget};
use crate::dialogs::DialogCoordinator;
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};

/// The empty content pane when there is no argv machine to report on — the
/// workspace rows are the way in.
const NO_ARGV_MACHINE_HINT: &str = "open a workspace to begin";

pub struct Workbench {
    /// FleetStore is the single observable store. The shell reads it for the
    /// empty pane/statusbar and for root-window chat orchestration.
    store: Entity<FleetStore>,
    sidebar: Entity<Sidebar>,
    add_workspace_flow: Entity<AddWorkspaceFlow>,
    /// Retained for its store subscription and modal lifecycle; the Root
    /// overlay itself remains placed by this view.
    _dialogs: Entity<DialogCoordinator>,
    /// The current conversation, whether or not a session exists behind it.
    chat: Option<Entity<Chat>>,
    /// Dies with the current chat so requests from an abandoned unstarted pane
    /// cannot route through this shell.
    chat_requests: Option<Subscription>,
    /// Store repaint/event and sidebar-intent subscriptions.
    _subscriptions: [Subscription; 3],
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
        let add_workspace_flow = cx.new(|_| AddWorkspaceFlow::new(store.clone()));
        let dialogs = cx.new(|cx| DialogCoordinator::new(store.clone(), window, cx));
        let subscriptions = [
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&store, window, Self::handle_store_event),
            cx.subscribe_in(&sidebar, window, Self::handle_sidebar_intent),
        ];

        Self {
            store,
            sidebar,
            add_workspace_flow,
            _dialogs: dialogs,
            chat: None,
            chat_requests: None,
            _subscriptions: subscriptions,
        }
    }

    /// The only bridge event the shell owns builds or promotes a chat and
    /// therefore needs a `Window`. Dialog events are consumed by
    /// [`DialogCoordinator`]; chat answers are consumed by [`Chat`].
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

    /// `StoreEvent` subscribers run while FleetStore is emitting. Attached may
    /// need to update the store's opening catalog, so finish that source update
    /// before constructing or promoting the chat.
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

    /// Sidebar events are emitted while the sidebar entity is leased. Some
    /// intents immediately change its selected-session highlight, so dispatch
    /// after that lease ends rather than re-entering the child entity.
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
            SidebarIntent::AddWorkspace => self.open_add_workspace(window, cx),
            SidebarIntent::NewSession { machine, path } => {
                self.open_new_session_chat(machine, path, window, cx)
            }
            SidebarIntent::OpenSession { sid } => self.open_session(sid, cx),
        }
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
        // The chat that minted this sid is already on screen with the first
        // prompt rendered. Promote it in place rather than rebuilding it.
        if let Some(chat) = self.take_starting_chat(sid, cx) {
            chat.update(cx, |chat, cx| {
                chat.promote_to_live(sid.to_string(), entries, cx);
                chat.request_model_controls(cx);
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
        let requests = cx.subscribe_in(&chat, window, Self::handle_chat_request);

        chat.update(cx, |chat, cx| chat.request_model_controls(cx));
        self.install_chat(chat, requests, cx);
    }

    fn install_chat(&mut self, chat: Entity<Chat>, requests: Subscription, cx: &mut Context<Self>) {
        let selected_sid = chat.read(cx).sid().map(str::to_string);

        self.chat = Some(chat);
        self.chat_requests = Some(requests);
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

    /// Chat intents route against the bridge that is live now. The chat owns
    /// request state and rejects stale answers; the shell only supplies the
    /// window-level route and user-visible refusal.
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
        self.chat_requests = None;
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

    fn open_new_session_chat(
        &mut self,
        machine: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_workspace = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

            chat.starting_session().is_none()
                && chat.sid().is_none()
                && chat.machine() == machine
                && chat.workspace_path() == path
        });

        if self.store.read(cx).opening_sid().is_some() || same_workspace {
            return;
        }

        let target = ChatTarget {
            machine: machine.to_string(),
            workspace_path: path.to_string(),
        };
        let store = self.store.clone();
        let chat = cx.new(|cx| Chat::unstarted(target, store, window, cx));
        let requests = cx.subscribe_in(&chat, window, Self::handle_chat_request);

        self.install_chat(chat, requests, cx);
    }

    fn open_add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_workspace_flow
            .update(cx, |flow, cx| flow.open(window, cx));
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
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
        };

        content.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(color.color(cx))
                .child(hint),
        )
    }

    fn argv_machine_hint(&self, machine: &str, cx: &Context<Self>) -> (String, Tone) {
        let display = present_machine(self.store.read(cx).state(machine), machine);

        match display.pane_message {
            Some(message) => (message, display.dot.tone()),
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
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
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.sidebar.clone())
                    .child(self.render_content(cx)),
            )
            .child(self.render_statusbar(cx))
            // Root owns these stacks but renders neither. Placement remains a
            // top-level shell responsibility.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
