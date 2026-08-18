//! One session's conversation, from before it exists until after its pi exits.
//!
//! A chat opens **unstarted** — client-side only, no session on any machine
//! (docs/SPEC.md §Flows "Start session") — and its first prompt mints the sid
//! that makes it live. These are one component with a state rather than two
//! components with a shared vocabulary, because everything the user touches is
//! the same on both sides of that line: the same pane, the same prompt box,
//! the same model control, the same input. What actually differs is small and
//! named in [`SessionPhase`] and [`ModelSelection`]: what a model pick *means* (a launch-line argument
//! versus a `set_model` the running pi must confirm), and whether the machine
//! and the repository can still be picked — a started session's launch line
//! (`cd <repo> && pi`) fixes both for its pi's lifetime.
//!
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, Focusable, ScrollHandle,
    SharedString, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, IconName};
use serde_json::Value;

use crate::assets::GIT_BRANCH_ICON_PATH;
use crate::models::ModelChoice;
use crate::session_state::{self, ModelSelection, ModelsState, SessionPhase, SessionState};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;
use harvester_client_core::bridge::Event as BridgeEvent;
use harvester_client_core::{machines, MachineCandidate, RequestToken};

/// The unstarted chat's "no model chosen" row: pi launches with its own
/// default, and saying so is more honest than naming a model we did not pick.
const DEFAULT_MODEL_ROW: &str = "default (pi chooses)";
/// The repo picker's last row: the folder picker, for a repository Harvester
/// has not been told about yet. It is also the only row an empty picker has,
/// which is what makes a fresh client's first session reachable.
const ADD_REPO_ROW: &str = "Add new…";
/// What the machine control says before anything is picked, and when there is
/// nothing to pick — the fleet is empty until a provider lists.
const NO_MACHINE_LABEL: &str = "no machines";
const CHOOSE_MACHINE_LABEL: &str = "choose machine";
/// What the repository control says before a repo is picked.
const CHOOSE_REPO_LABEL: &str = "choose repository";

pub use session_state::{ChatIntent as ChatRequest, ChatTarget};

impl EventEmitter<session_state::ChatIntent> for Chat {}

/// What a chat needs the composition shell to run on its behalf.
///
/// Separate from [`ChatRequest`], which is bridge traffic the shell only
/// routes: this one needs Root's modal stack, which a pane inside the shell
/// does not own and must not arbitrate for itself (SPEC C11).
pub enum ChatFlowRequest {
    /// Browse `machine` for a repository to store and then select.
    AddRepo { machine: String },
}

impl EventEmitter<ChatFlowRequest> for Chat {}

pub struct Chat {
    /// The window-free owner of the session lifecycle, model state, request
    /// correlation and transcript. The fields below are GPUI adaptation only.
    state: SessionState,
    /// Where this chat's machine label, its machine candidates and its stored
    /// repositories come from, read on every render. Observed, so a provider
    /// listing that renames a machine, or a repo the user just added, reaches
    /// this pane without anyone remembering to push it (SPEC C12).
    store: Entity<FleetStore>,
    /// Message/tool/markdown policy and stable tool-expansion state. It reads
    /// the client-core transcript without copying or normalizing its items.
    transcript_presentation: ui::TranscriptPresentation,
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    /// The input's Enter, the store's repaints, and the store's events. Held
    /// rather than detached so all three die with this pane.
    _subscriptions: [Subscription; 3],
}

impl Chat {
    /// A chat with no session behind it yet. What its prompt strip needs is
    /// asked for by [`Chat::request_prompt_controls`], which the workbench
    /// calls once it has subscribed — the seconds the uncached `list_models`
    /// costs still hide behind the user typing.
    pub fn unstarted(
        target: ChatTarget,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokens = store.read(cx).tokens();

        Self::new(SessionState::unstarted(target, tokens), store, window, cx)
    }

    /// A chat over a session that already exists: `entries` is pi's history,
    /// verbatim (empty for a session that has never been prompted).
    pub fn live(
        sid: String,
        target: ChatTarget,
        entries: &[Value],
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokens = store.read(cx).tokens();
        Self::new(
            SessionState::live(sid, target, entries, tokens),
            store,
            window,
            cx,
        )
    }

    fn new(
        state: SessionState,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Multi-line so a prompt can hold a paragraph, but Enter still sends —
        // `submit_on_enter` defaults to false, and without it Enter would only
        // ever insert a newline. Shift-Enter is the newline.
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Message pi — Enter to send")
                .multi_line(true)
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let subscriptions = [
            cx.subscribe_in(&input, window, Self::handle_prompt_submitted),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&store, Self::handle_store_event),
        ];
        Self {
            state,
            store,
            transcript_presentation: ui::TranscriptPresentation::default(),
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The store's events, routed to the two this pane asks questions of: its
    /// machine's bridge, and the machines worker its branch indicator execs on.
    fn handle_store_event(
        &mut self,
        _store: Entity<FleetStore>,
        event: &StoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Bridge { machine, event } => self.handle_bridge_event(machine, event, cx),
            // The one machines-worker answer a chat asks for itself: the branch
            // indicator is a plain SSH exec, so it never goes near a daemon and
            // never reaches a bridge (SPEC §Workspace).
            StoreEvent::Machines(machines::Event::BranchResolved { token, result }) => {
                self.handle_branch_resolved(*token, result.clone(), cx)
            }
            StoreEvent::Machines(_) | StoreEvent::Failed(_) => {}
        }
    }

    /// Every answer pi produces for this pane arrives here rather than being
    /// routed in from above: a chat knows which sid it renders and which
    /// machine it runs on, and that is the whole of the routing rule (SPEC
    /// C12). Guards rather than a total match because *whose* an event is,
    /// is the question — the fall-through is another chat's, another
    /// machine's, or `Attached`, which builds chats and so is the workbench's.
    fn handle_bridge_event(&mut self, machine: &str, event: &BridgeEvent, cx: &mut Context<Self>) {
        match event {
            // Machine-scoped: the bridge this chat runs on died, or answered
            // the throwaway `list_models` an unstarted chat asked it for —
            // that one has no sid to be found by.
            BridgeEvent::ConnectFailed { detail } if self.runs_on(machine) => {
                self.fail_pending_requests(detail.clone(), cx)
            }
            BridgeEvent::Disconnected if self.runs_on(machine) => {
                self.fail_pending_requests("connection lost".to_string(), cx)
            }
            BridgeEvent::RequestFailed { .. } if self.runs_on(machine) => {
                if self.state.fail_start() {
                    cx.notify();
                }
            }
            BridgeEvent::ModelsListed { token, result } if self.runs_on(machine) => {
                self.handle_models_listed(*token, result.clone(), cx)
            }
            // Sid-scoped: sids are uuids, unique across machines, so these
            // need no machine to be routed by.
            BridgeEvent::HistoryReplaced { sid, entries } if self.renders(sid) => {
                self.replace_history(entries, cx)
            }
            BridgeEvent::HistoryExtended { sid, entries } if self.renders(sid) => {
                self.extend_history(entries, cx)
            }
            BridgeEvent::Pi { sid, event } if self.renders(sid) => self.handle_pi_event(event, cx),
            BridgeEvent::SessionExit { sid, exit_code } if self.renders(sid) => {
                self.handle_session_exit(*exit_code, cx)
            }
            BridgeEvent::SessionModelsListed { sid, token, result } if self.renders(sid) => {
                self.handle_models_listed(*token, result.clone(), cx)
            }
            BridgeEvent::SessionState { sid, token, result } if self.renders(sid) => {
                self.handle_session_state(*token, result.clone(), cx)
            }
            // A failed `set_model` is the store's to report; this pane only
            // moves its label when pi confirms one.
            BridgeEvent::ModelSet { sid, token, result } if self.renders(sid) => {
                self.handle_model_set(*token, result, cx)
            }
            _ => {}
        }
    }

    /// Whether this chat runs on `machine`.
    ///
    /// Scoping by machine is what keeps one machine's failure from cancelling
    /// a start still in flight on another — without it a retry mints a second
    /// sid and starts a second pi for one prompt.
    fn runs_on(&self, machine: &str) -> bool {
        self.state.target().machine == machine
    }

    /// Whether this chat is the one rendering `sid`.
    fn renders(&self, sid: &str) -> bool {
        self.state.sid() == Some(sid)
    }

    /// How this chat's machine is named on screen — the store's answer, never
    /// a copy of it.
    fn machine_label(&self, cx: &Context<Self>) -> SharedString {
        self.store
            .read(cx)
            .label(&self.state.target().machine)
            .into()
    }

    /// Every machine this chat could run on: the whole reachable fleet, read
    /// live, so a provider listing that adds a machine reaches the picker
    /// without anyone pushing it. Only ever consulted while unstarted — a
    /// running session never moves.
    fn machine_candidates(&self, cx: &Context<Self>) -> Vec<MachineCandidate> {
        self.store.read(cx).machine_candidates()
    }

    /// The repositories stored for the picked machine, in the order they were
    /// added. Empty until a machine is picked — a path belongs to a machine.
    fn repo_candidates(&self, cx: &Context<Self>) -> Vec<String> {
        self.store.read(cx).repos_on(&self.state.target().machine)
    }

    /// `None` for an unstarted chat — it names no session (see [`SessionPhase`]).
    pub fn sid(&self) -> Option<&str> {
        self.state.sid()
    }

    pub fn machine(&self) -> &str {
        &self.state.target().machine
    }

    /// The sid this chat is waiting on `Attached` for, and the name it minted
    /// for it — what the workbench needs to synthesize the sidebar row.
    pub fn starting_session(&self) -> Option<(&str, &str)> {
        self.state.phase().starting_session()
    }

    pub fn workspace_path(&self) -> &str {
        &self.state.target().workspace_path
    }

    /// Ask for everything the prompt strip reports: the model list (or, live,
    /// the session's own model) and the branch the repo is on.
    ///
    /// Public, and separate from the constructor, because an entity cannot
    /// emit to a subscriber that does not exist yet: the workbench subscribes
    /// to the chat it just created, then calls this.
    pub fn request_prompt_controls(&mut self, cx: &mut Context<Self>) {
        self.request_models(cx);
        self.request_branch(cx);
    }

    /// Which model question this chat has depends on its phase: an unstarted
    /// one asks a throwaway pi in its cwd, a live one asks its own session,
    /// and a starting one has nothing to ask yet.
    fn request_models(&mut self, cx: &mut Context<Self>) {
        match self.state.phase() {
            SessionPhase::Unstarted => self.request_model_list(cx),
            SessionPhase::Live { .. } => self.request_model_controls(cx),
            SessionPhase::Starting { .. } => {}
        }
    }

    fn request_model_controls(&mut self, cx: &mut Context<Self>) {
        for intent in self.state.request_model_controls() {
            cx.emit(intent);
        }
    }

    /// The unstarted chat's model list: a throwaway pi in this chat's cwd.
    /// A chat with no repo picked has no cwd to run it in and asks nothing.
    fn request_model_list(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.request_model_list() {
            cx.emit(intent);
        }
    }

    /// Read the branch of the picked repo — one client-side SSH exec on the
    /// machines worker, the same daemon-free path the folder picker walks
    /// (SPEC §Workspace). Every failure here is silent by design: the
    /// indicator is the only thing that depends on it, and it reports nothing
    /// rather than blocking a prompt on a `git` call.
    fn request_branch(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.state.request_branch() else {
            return;
        };
        let target = self.state.target().clone();
        let host = match self.store.read(cx).host_for(&target.machine) {
            Ok(host) => host,
            Err(detail) => {
                eprintln!("harvester: branch indicator: {detail}");
                self.state.cancel_branch();

                return;
            }
        };
        let command = machines::Command::ResolveBranch {
            host,
            path: target.workspace_path,
            token,
        };
        let sent = self
            .store
            .update(cx, |store, cx| store.send_host_command(command, cx));

        if !sent {
            self.state.cancel_branch();
        }
    }

    fn handle_branch_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if let Err(detail) = &result {
            eprintln!("harvester: branch indicator: {detail}");
        }

        if self.state.accept_branch(token, result) {
            cx.notify();
        }
    }

    /// The create → attach → prompt chain landed: this chat is a live session
    /// now. An empty snapshot is the brand-new-session case (docs/SPEC.md C7),
    /// where the prompt already on screen is the only history there is — so it
    /// stays rather than being blanked out from under the user.
    pub fn promote_to_live(&mut self, sid: String, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.promote_to_live(sid, entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// A resync (`event_gap`, or a reconnect's refetch) answered: the entries
    /// are authoritative, so they replace what is rendered. Deliberately not a
    /// new chat entity — rebuilding is what let a late resync clobber the chat
    /// on screen, discarding the input's contents and its focus with it. The
    /// in-flight partial message goes because the snapshot supersedes it; the
    /// next `message_update` carries the whole partial again.
    fn replace_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.replace_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The same repair, cheap: only the entries after the last fetch's cursor
    /// (docs/SPEC.md C6). The transcript owns what that means for the items it
    /// built from live events in the meantime.
    fn extend_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.extend_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The machine's bridge died: nothing this chat asked for will arrive.
    pub fn fail_pending_requests(&mut self, detail: String, cx: &mut Context<Self>) {
        if self.state.fail_pending(detail) {
            cx.notify();
        }
    }

    /// A model list answered, from either source. A chat that has since been
    /// retargeted is waiting on a different token, so the abandoned request's
    /// answer is dropped.
    fn handle_models_listed(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.accept_models(token, result) {
            cx.notify();
        }
    }

    /// A `GetSessionState` answered; the current model labels the control.
    /// Failure is cosmetic (generic label), so it is only logged.
    fn handle_session_state(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.accept_session_state(token, result) {
            cx.notify();
        }
    }

    /// A `set_model` succeeded; pi's `data` is the full Model object now in
    /// effect (failures surface in the statusbar, workbench-side).
    fn handle_model_set(
        &mut self,
        token: RequestToken,
        result: &Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .state
            .accept_model_set(token, result.as_ref().map_err(|error| error.clone()))
        {
            cx.notify();
        }
    }

    /// Every model this chat can offer, in row order.
    ///
    /// Only a chat with no session yet offers the "default (pi chooses)" row —
    /// a running session always has a concrete model (SPEC §Flows "Switch
    /// model") — and that is derived from the state, not stored: a stored
    /// answer would be one more thing that could disagree with it.
    ///
    /// The picked model travels with the row rather than as an index into a
    /// list that may be refetched while the dropdown is open, so a pick always
    /// applies the model the user actually read.
    fn model_picks(&self) -> Vec<Option<ModelChoice>> {
        if matches!(self.state.phase(), SessionPhase::Starting { .. }) {
            return Vec::new();
        }

        let offer_default = matches!(self.state.phase(), SessionPhase::Unstarted);

        offer_default
            .then_some(None)
            .into_iter()
            .chain(self.state.model_choices().iter().cloned().map(Some))
            .collect()
    }

    /// Whether `pick` is the model this chat is already on.
    fn is_current_model(&self, pick: &Option<ModelChoice>) -> bool {
        match (pick, self.state.selection()) {
            (None, ModelSelection::LaunchDefault) => true,
            (Some(choice), ModelSelection::Launch(current) | ModelSelection::Live(current)) => {
                choice.provider == current.provider && choice.id == current.id
            }
            _ => false,
        }
    }

    /// A picker row was clicked; `None` is the "default (pi chooses)" row.
    /// Unstarted the pick is local (it becomes a launch-line argument); live it
    /// costs a `set_model` the session must confirm, so the label does not move
    /// until [`Chat::handle_model_set`] says pi agreed.
    fn pick_model(&mut self, picked: Option<ModelChoice>, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.pick_model(picked) {
            cx.emit(intent);
        }

        cx.notify();
    }

    /// Retarget to another machine: the typed prompt survives, only the
    /// destination changes. A pick of the current machine changes nothing, so
    /// nothing is refetched. The stale answer from the old machine is dropped
    /// by the model control's own token.
    ///
    /// A stored repo path belongs to the machine it was stored for, so the
    /// repo follows the machine: the new machine's first repo, or nothing when
    /// it has none.
    fn pick_machine(&mut self, machine: String, cx: &mut Context<Self>) {
        if self.state.target().machine == machine {
            cx.notify();

            return;
        }

        self.state.cancel_model_fetch("machine changed".to_string());

        if self.state.retarget(machine) {
            let first_repo = self.repo_candidates(cx).first().cloned();

            self.state.pick_repo(first_repo.unwrap_or_default());
            self.request_prompt_controls(cx);
        }

        cx.notify();
    }

    /// Select a stored repository. Also the landing point for a folder the
    /// user just added, so a validated path is selected exactly the way a
    /// stored one is.
    pub fn select_repo(&mut self, path: String, cx: &mut Context<Self>) {
        if self.state.pick_repo(path) {
            self.request_prompt_controls(cx);
        }

        cx.notify();
    }

    /// The repo picker's last row: browse this machine for a repository
    /// Harvester has not been told about. The shell runs it — the folder
    /// picker is a Root dialog.
    fn add_repo(&mut self, cx: &mut Context<Self>) {
        let machine = self.state.target().machine.clone();

        if machine.is_empty() {
            return;
        }

        cx.emit(ChatFlowRequest::AddRepo { machine });
    }

    /// Whether this chat's pi is still believed to be running. Based on the
    /// `session_exit` events this chat actually received, so unlike a cached
    /// list status it cannot claim "running" about a session that has died.
    pub fn is_live(&self) -> bool {
        self.state.is_live()
    }

    fn handle_session_exit(&mut self, exit_code: Option<i32>, cx: &mut Context<Self>) {
        if self.state.exit(exit_code) {
            cx.notify();
        }
    }

    fn handle_pi_event(&mut self, event: &Value, cx: &mut Context<Self>) {
        // A settled turn is the one moment the checkout can have moved without
        // the user touching a control, and the only thing that refreshes the
        // branch indicator — there is no polling behind it.
        if session_state::is_turn_settled(event) {
            self.request_branch(cx);
        }

        // `false` is an event this model does not cover (retries, compaction,
        // queue): nothing moved, so nothing is redrawn.
        if !self.state.apply_pi_event(event) {
            return;
        }

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Enter in the prompt box. The input is multi-line, so `submit_on_enter`
    /// is what separates a send from a newline: shift-Enter never reaches
    /// here.
    fn handle_prompt_submitted(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::PressEnter { .. }) {
            return;
        }

        self.submit_prompt(window, cx);
    }

    /// Both send paths end here: take what is typed, clear the box, route it
    /// by state.
    fn submit_text(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.submit_prompt(text) {
            cx.emit(intent);
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The send button's path; Enter inside the input reaches the same
    /// `PromptSubmitted` through the `Submit` action.
    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.clone();
        let text = input.read(cx).value().to_string();

        if text.trim().is_empty() {
            return;
        }

        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.submit_text(text, cx);
    }

    /// The unstarted chat's header line: which repository, on which machine —
    /// or what is still missing before this chat can send anything. A live chat
    /// has no header: its prompt box already names both, and the conversation
    /// identifies it better than any title could.
    fn unstarted_subtitle(&self, cx: &Context<Self>) -> SharedString {
        if !self.state.target().is_resolved() {
            return "pick a machine and a repository below".into();
        }

        format!(
            "{} on {}",
            folder_name(&self.state.target().workspace_path),
            self.machine_label(cx)
        )
        .into()
    }

    /// Adapt the transcript presentation's tool toggle back into this chat's
    /// GPUI entity. The presentation owns expansion; the chat owns the entity
    /// lifetime and notification that follows a click.
    fn render_transcript(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = cx.entity();

        self.transcript_presentation.render(
            self.state.transcript(),
            &self.scroll,
            move |tool_call_id, _, cx| {
                chat.update(cx, |chat, cx| {
                    chat.transcript_presentation.toggle_tool_block(tool_call_id);
                    cx.notify();
                });
            },
            cx,
        )
    }

    /// The pane's bottom row: the prompt box, or the one sentence that
    /// replaces it when this chat cannot accept input.
    fn render_bottom_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The prompt box floats in the pane's padding (no separator line —
        // the card's own border is the boundary).
        let row = div().flex_none().px_3().pb_3();
        let notice = match (self.state.exited_notice(), self.state.phase()) {
            (Some(notice), _) => Some(notice.to_string()),
            (None, SessionPhase::Starting { .. }) => Some("starting session…".to_string()),
            (None, _) => None,
        };

        if let Some(notice) = notice {
            return row.child(
                div()
                    .text_xs()
                    .text_color(Tone::Idle.color(cx))
                    .child(notice),
            );
        }

        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);

        row.child(self.render_prompt_box(input_focused, cx))
    }

    /// One prompt box for both states (SPEC §Flows: the agent-input pattern,
    /// `model · machine · repository · branch`).
    ///
    /// Machine and repository are pickers only while the chat is unstarted:
    /// the daemon launches `cd <repo> && pi` and pi's cwd is fixed for its
    /// lifetime, so a started session's own launch line is what locks them.
    /// The model stays switchable — pi confirms a `set_model` at runtime — and
    /// the branch only ever reports.
    fn render_prompt_box(&self, input_focused: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let unstarted = matches!(self.state.phase(), SessionPhase::Unstarted);
        let (model_label, model_enabled) = self.model_control_state();
        // Nothing to send to until both halves resolve; the state machine
        // refuses the prompt either way, this is only the strip saying so. A
        // started session is resolved by construction, and its prompts go to a
        // pi that already exists — the target is not consulted again.
        let can_send = !unstarted || self.state.target().is_resolved();

        ui::prompt_box(input_focused, cx)
            .child(ui::prompt_box_input(
                Input::new(&self.input).appearance(false).p_0(),
            ))
            .child(
                ui::prompt_box_controls()
                    .child(self.render_model_control(model_label, model_enabled, cx))
                    .child(self.render_machine_control(unstarted, cx))
                    .child(self.render_repo_control(unstarted, cx))
                    .children(self.render_branch_indicator(cx))
                    .child(div().flex_1())
                    .child(ui::prompt_box_send("chat-send", can_send, cx).on_click(
                        cx.listener(|chat, _, window, cx| chat.submit_prompt(window, cx)),
                    )),
            )
    }

    /// The branch the repo is on, when an exec has answered for it. Absent
    /// otherwise: an indicator nobody can act on is not worth a placeholder
    /// that has to be explained.
    fn render_branch_indicator(&self, cx: &App) -> Option<impl IntoElement> {
        let branch: SharedString = self.state.branch()?.to_string().into();

        Some(ui::prompt_box_control(
            "chat-branch",
            Icon::default().path(GIT_BRANCH_ICON_PATH),
            branch,
            false,
            cx,
        ))
    }

    /// What the model control says and whether it can be clicked. Unstarted, a
    /// list that never arrived is not an error the user must clear — pi still
    /// launches with its default — so the control just reads that way.
    fn model_control_state(&self) -> (SharedString, bool) {
        let choices_available = !self.state.model_choices().is_empty();

        match self.state.selection() {
            ModelSelection::Launch(choice) | ModelSelection::Live(choice) => {
                (choice.name.clone().into(), choices_available)
            }
            ModelSelection::LaunchDefault => match self.state.models() {
                ModelsState::Loading => ("loading models…".into(), false),
                ModelsState::Loaded(_) => ("default model".into(), true),
                ModelsState::NotRequested | ModelsState::Failed(_) => {
                    ("default model".into(), false)
                }
            },
            ModelSelection::LivePending => ("model".into(), choices_available),
            ModelSelection::LiveUnavailable => ("model unavailable".into(), choices_available),
        }
    }

    /// The model control, and the list it drops. A control with nothing to
    /// offer renders as the bare label: a dropdown that opens onto no rows is
    /// worse than one that does not open.
    fn render_model_control(
        &self,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control =
            ui::prompt_box_control("chat-model", Icon::new(IconName::Bot), label, enabled, cx);
        let picks = self.model_picks();

        if !enabled || picks.is_empty() {
            return control.into_any_element();
        }

        let rows = picks
            .iter()
            .map(|pick| ui::DropdownRow {
                label: match pick {
                    Some(choice) => choice.name.clone().into(),
                    None => DEFAULT_MODEL_ROW.into(),
                },
                selected: self.is_current_model(pick),
                divider_above: false,
            })
            .collect();
        let chat = cx.entity();

        ui::prompt_box_dropdown("chat-model-dropdown", control, rows, move |index, _, cx| {
            let Some(pick) = picks.get(index).cloned() else {
                return;
            };

            chat.update(cx, |chat, cx| chat.pick_model(pick, cx));
        })
        .into_any_element()
    }

    /// The machine control, and the fleet it drops. Only an unstarted chat
    /// drops a list — a running session never moves — and a client with no
    /// machines at all says so rather than opening onto nothing.
    fn render_machine_control(&self, unstarted: bool, cx: &mut Context<Self>) -> AnyElement {
        let candidates = self.machine_candidates(cx);
        let label = self.machine_control_label(&candidates, cx);
        let enabled = unstarted && !candidates.is_empty();
        let control = ui::prompt_box_control(
            "chat-machine",
            Icon::new(IconName::HardDrive),
            label,
            enabled,
            cx,
        );

        if !enabled {
            return control.into_any_element();
        }

        let rows = candidates
            .iter()
            .map(|candidate| ui::DropdownRow {
                label: candidate.label.clone().into(),
                selected: candidate.key == self.state.target().machine,
                divider_above: false,
            })
            .collect();
        // The keys travel with the pick, so a listing landing while the list is
        // open cannot move a row out from under the click.
        let keys: Vec<String> = candidates
            .into_iter()
            .map(|candidate| candidate.key)
            .collect();
        let chat = cx.entity();

        ui::prompt_box_dropdown(
            "chat-machine-dropdown",
            control,
            rows,
            move |index, _, cx| {
                let Some(machine) = keys.get(index).cloned() else {
                    return;
                };

                chat.update(cx, |chat, cx| chat.pick_machine(machine, cx));
            },
        )
        .into_any_element()
    }

    /// What the machine control says: the machine's own label once one is
    /// picked, and otherwise whether there is anything to pick.
    fn machine_control_label(
        &self,
        candidates: &[MachineCandidate],
        cx: &Context<Self>,
    ) -> SharedString {
        if !self.state.target().machine.is_empty() {
            return self.machine_label(cx);
        }

        if candidates.is_empty() {
            return NO_MACHINE_LABEL.into();
        }

        CHOOSE_MACHINE_LABEL.into()
    }

    /// The repository control: the machine's stored repos, then "Add new…"
    /// below a divider. A machine with nothing stored drops a list of exactly
    /// that one row, which is how a fresh client reaches its first session.
    fn render_repo_control(&self, unstarted: bool, cx: &mut Context<Self>) -> AnyElement {
        let path = &self.state.target().workspace_path;
        let label: SharedString = match path.is_empty() {
            true => CHOOSE_REPO_LABEL.into(),
            false => folder_name(path).to_string().into(),
        };
        let enabled = unstarted && !self.state.target().machine.is_empty();
        let control =
            ui::prompt_box_control("chat-repo", Icon::new(IconName::Folder), label, enabled, cx);

        if !enabled {
            return control.into_any_element();
        }

        let repos = self.repo_candidates(cx);
        let mut rows: Vec<ui::DropdownRow> = repos
            .iter()
            .map(|repo| ui::DropdownRow {
                label: repo.clone().into(),
                selected: repo == path,
                divider_above: false,
            })
            .collect();

        rows.push(ui::DropdownRow {
            label: ADD_REPO_ROW.into(),
            selected: false,
            divider_above: !repos.is_empty(),
        });

        let chat = cx.entity();

        ui::prompt_box_dropdown("chat-repo-dropdown", control, rows, move |index, _, cx| {
            let picked = repos.get(index).cloned();

            chat.update(cx, |chat, cx| match picked {
                Some(repo) => chat.select_repo(repo, cx),
                None => chat.add_repo(cx),
            });
        })
        .into_any_element()
    }

    /// The unstarted body: empty, except when the model fetch failed — which
    /// is informational, not blocking, since pi still launches with a default.
    fn render_unstarted_body(&self, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .children(match self.state.models() {
                ModelsState::Failed(detail) => Some(
                    div()
                        .text_xs()
                        .text_color(Tone::Failed.color(cx))
                        .child(format!("{detail} — pi will use its default model")),
                ),
                _ => None,
            })
    }
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accepts_input = self.state.exited_notice().is_none()
            && !matches!(self.state.phase(), SessionPhase::Starting { .. });

        if self.focus_input && accepts_input {
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            self.focus_input = false;
        }

        let pane = div().size_full().flex().flex_col();
        let pane = match self.state.phase() {
            // No conversation yet, so the header is what identifies the pane.
            SessionPhase::Unstarted | SessionPhase::Starting { .. } => pane
                .child(ui::chat_header(
                    "New session".into(),
                    self.unstarted_subtitle(cx),
                    cx,
                ))
                .child(self.render_unstarted_body(cx)),
            SessionPhase::Live { .. } => pane.child(self.render_transcript(cx)),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}
