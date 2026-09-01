mod browser;

use super::{
    extract_model_choices, json, lock, machines, model_choice_from_value, unix_millis_now,
    workspace, Arc, BridgeCommand, BridgeEvent, BridgeOperation, ChatId, ChatTarget,
    ClientSnapshot, DialogAnswer, DialogKey, DialogObligation, DialogObligations, DialogRequest,
    DialogRoom, DraftId, DraftLifecycle, DraftSession, Fleet, FleetEvents, HashMap, HashSet,
    Intent, LiveSession, MachineId, MachineSnapshot, MachineState, MachineStatus, ModelChoice,
    ModelSelection, ModelsState, Mutex, RemoteDirEntry, RepairState, ReplayState, RequestToken,
    RequestTokens, RuntimeEvent, RuntimeEventHub, SessionId, SessionIndex, SessionKey,
    SessionLifecycle, SessionReplica, SessionStatus, SubmissionId, UnboundedReceiver, Value,
    WorkspaceBrowser, WorkspaceBrowserId, WorkspaceBrowserStatus, WorkspaceId,
    CONNECTION_LOST_DETAIL, PI_AGENT_SETTLED, PI_GET_AVAILABLE_MODELS, PI_GET_STATE, PI_SET_MODEL,
    SESSION_NAME_MAX_CHARS,
};
use browser::BrowserRequest;

struct PendingPrompt {
    chat: ChatId,
    text: String,
    session: SessionKey,
    generation: u64,
    delivered: bool,
    settled: bool,
    turn_was_busy: bool,
    event_revision: u64,
}

struct Engine {
    fleet: Fleet,
    state: ClientSnapshot,
    index: SessionIndex,
    dialogs: DialogObligations,
    workspace_store: workspace::WorkspaceStore,
    argv_machine: Option<MachineId>,
    snapshot: Arc<Mutex<Arc<ClientSnapshot>>>,
    events: Arc<RuntimeEventHub>,
    tokens: RequestTokens,
    latest_models: HashMap<ChatId, RequestToken>,
    latest_session_state: HashMap<SessionKey, (RequestToken, u64)>,
    latest_model_set: HashMap<SessionKey, RequestToken>,
    displaced_models: HashMap<RequestToken, ModelSelection>,
    latest_branch: HashMap<ChatId, RequestToken>,
    branch_requests: HashMap<RequestToken, ChatId>,
    browser_requests: HashMap<RequestToken, (WorkspaceBrowserId, BrowserRequest)>,
    latest_browser: HashMap<WorkspaceBrowserId, RequestToken>,
    starting_drafts: HashMap<SessionKey, DraftId>,
    session_generations: HashMap<SessionKey, u64>,
    pending_prompts: HashMap<SubmissionId, PendingPrompt>,
    answering_dialogs: HashMap<DialogKey, DialogAnswer>,
}

pub(super) async fn run(
    argv_machine: Option<MachineId>,
    daemon_binary: Vec<u8>,
    mut commands: UnboundedReceiver<Intent>,
    events: Arc<RuntimeEventHub>,
    snapshot: Arc<Mutex<Arc<ClientSnapshot>>>,
) {
    let (fleet, fleet_events) = Fleet::start(daemon_binary);
    let FleetEvents {
        bridges: mut bridge_events,
        machines: mut machine_events,
    } = fleet_events;
    let mut engine = Engine::new(fleet, argv_machine, snapshot, events);

    engine.initialize();

    loop {
        tokio::select! {
            intent = commands.recv() => match intent {
                Some(intent) => engine.handle_intent(intent),
                None => break,
            },
            event = bridge_events.next() => match event {
                Some(event) => engine.handle_bridge_event(event),
                None => {
                    engine.worker_stopped("bridge event channel closed");

                    break;
                }
            },
            event = machine_events.next() => match event {
                Some(event) => engine.handle_machine_event(event),
                None => {
                    engine.worker_stopped("machines event channel closed");

                    break;
                }
            },
        }
    }

    engine.events.close();
}

impl Engine {
    fn new(
        fleet: Fleet,
        argv_machine: Option<MachineId>,
        snapshot: Arc<Mutex<Arc<ClientSnapshot>>>,
        events: Arc<RuntimeEventHub>,
    ) -> Self {
        let workspace_store = workspace::WorkspaceStore::discover();
        let workspaces = workspace_store.load();

        Self {
            fleet,
            state: ClientSnapshot {
                startup_machine: argv_machine.clone(),
                workspaces: Arc::new(workspaces),
                ..ClientSnapshot::default()
            },
            index: SessionIndex::default(),
            dialogs: DialogObligations::default(),
            workspace_store,
            argv_machine,
            snapshot,
            events,
            tokens: RequestTokens::default(),
            latest_models: HashMap::new(),
            latest_session_state: HashMap::new(),
            latest_model_set: HashMap::new(),
            displaced_models: HashMap::new(),
            latest_branch: HashMap::new(),
            branch_requests: HashMap::new(),
            browser_requests: HashMap::new(),
            latest_browser: HashMap::new(),
            starting_drafts: HashMap::new(),
            session_generations: HashMap::new(),
            pending_prompts: HashMap::new(),
            answering_dialogs: HashMap::new(),
        }
    }

    fn initialize(&mut self) {
        self.track_machines();
        self.sync_machines();
        self.publish();
    }

    fn handle_intent(&mut self, intent: Intent) {
        match intent {
            Intent::CreateDraft { id } => self.create_draft(id),
            Intent::RetargetDraft { draft, machine } => self.retarget_draft(draft, machine),
            Intent::SelectWorkspace { draft, workspace } => {
                self.select_workspace(draft, workspace);
            }
            Intent::RequestControls { chat } => self.request_controls(&chat),
            Intent::OpenSession { session } => self.open_session(session),
            Intent::SubmitPrompt {
                submission,
                chat,
                text,
            } => self.submit_prompt(submission, chat, text),
            Intent::PickModel { chat, model } => self.pick_model(chat, model),
            Intent::StopSession { session } => self.stop_session(&session),
            Intent::AnswerDialog { key, answer } => self.answer_dialog(&key, answer),
            Intent::BeginWorkspaceBrowser { id, machine } => {
                self.begin_workspace_browser(id, machine);
            }
            Intent::OpenBrowserDirectory { browser, name } => {
                self.open_browser_directory(browser, &name);
            }
            Intent::BrowserUp { browser } => self.browser_up(browser),
            Intent::ConfirmBrowser { browser } => self.confirm_browser(browser),
            Intent::CancelBrowser { browser } => self.cancel_browser(browser),
            Intent::ForgetMachine { machine } => self.forget_machine(&machine),
        }
    }

    fn create_draft(&mut self, id: DraftId) {
        let target = self.default_target();

        Arc::make_mut(&mut self.state.drafts).insert(
            id,
            DraftSession {
                id,
                target,
                lifecycle: DraftLifecycle::Ready,
                models: ModelsState::NotRequested,
                model_selection: ModelSelection::LaunchDefault,
                branch: None,
            },
        );
        self.publish();
    }

    fn default_target(&self) -> ChatTarget {
        let reachable: HashSet<&MachineId> = self
            .state
            .machines
            .iter()
            .filter(|machine| machine.connectable && machine.status.is_available())
            .map(|machine| &machine.id)
            .collect();
        let recent = self.state.sessions.iter().find(|session| {
            reachable.contains(session.key.machine())
                && self.state.workspaces.contains(&session.workspace)
        });

        if let Some(session) = recent {
            return ChatTarget {
                machine: Some(session.key.machine().clone()),
                workspace: Some(session.workspace.clone()),
            };
        }

        if let Some(workspace) = self
            .state
            .workspaces
            .iter()
            .rev()
            .find(|workspace| reachable.contains(workspace.machine()))
        {
            return ChatTarget {
                machine: Some(workspace.machine().clone()),
                workspace: Some(workspace.clone()),
            };
        }

        ChatTarget {
            machine: self
                .state
                .machines
                .iter()
                .find(|machine| machine.connectable && machine.status.is_available())
                .map(|machine| machine.id.clone()),
            workspace: None,
        }
    }

    fn retarget_draft(&mut self, draft_id: DraftId, machine: MachineId) {
        if !self.machine_is_available(&machine) {
            self.reject(format!("machine {machine} is not available"));

            return;
        }

        let first_workspace = self
            .state
            .workspaces
            .iter()
            .rev()
            .find(|workspace| workspace.machine() == &machine)
            .cloned();
        let Some(draft) = self.state.drafts.get(&draft_id) else {
            self.reject("draft is no longer available".to_string());

            return;
        };

        if !matches!(draft.lifecycle, DraftLifecycle::Ready) {
            self.reject("a starting draft cannot be retargeted".to_string());

            return;
        }

        self.retire_chat_requests(&ChatId::Draft(draft_id));
        let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&draft_id) else {
            return;
        };

        draft.target = ChatTarget {
            machine: Some(machine),
            workspace: first_workspace,
        };
        draft.models = ModelsState::NotRequested;
        draft.model_selection = ModelSelection::LaunchDefault;
        draft.branch = None;
        self.publish();
    }

    fn select_workspace(&mut self, draft_id: DraftId, workspace: WorkspaceId) {
        if !self.state.workspaces.contains(&workspace) {
            self.reject("repository is not in the stored workspace list".to_string());

            return;
        }

        let Some(draft) = self.state.drafts.get(&draft_id) else {
            self.reject("draft is no longer available".to_string());

            return;
        };

        if !matches!(draft.lifecycle, DraftLifecycle::Ready)
            || draft.target.machine.as_ref() != Some(workspace.machine())
        {
            self.reject("repository does not belong to this ready draft".to_string());

            return;
        }

        self.retire_chat_requests(&ChatId::Draft(draft_id));
        let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&draft_id) else {
            return;
        };

        draft.target.workspace = Some(workspace);
        draft.models = ModelsState::NotRequested;
        draft.model_selection = ModelSelection::LaunchDefault;
        draft.branch = None;
        self.publish();
    }

    fn request_controls(&mut self, chat: &ChatId) {
        self.request_models(chat);
        self.request_branch(chat);
        self.publish();
    }

    fn request_models(&mut self, chat: &ChatId) {
        self.retire_model_requests(chat);
        let Some((machine, workspace)) = self.chat_location(chat) else {
            return;
        };

        if self.fleet.ensure_connection(machine.as_str()).is_err() {
            self.set_models_failed(chat, "Not connected".to_string());

            return;
        }

        let token = self.tokens.mint();

        self.latest_models.insert(chat.clone(), token);
        self.set_models_loading(chat);

        let delivery = match chat {
            ChatId::Draft(_) => self.fleet.send(
                machine.as_str(),
                BridgeCommand::ListModels {
                    cwd: workspace.path().to_string(),
                    token,
                },
            ),
            ChatId::Session(key) => {
                let Some(session) = self.state.live_sessions.get(key) else {
                    return;
                };
                let generation = session.attachment_generation;
                let event_revision = session.event_revision;
                let models_delivery = self.fleet.send(
                    machine.as_str(),
                    BridgeCommand::PiRequest {
                        sid: key.session().to_string(),
                        cmd: json!({"type": PI_GET_AVAILABLE_MODELS}),
                        token,
                        generation,
                    },
                );

                if let Err(detail) = models_delivery {
                    Err(detail)
                } else {
                    let state_token = self.tokens.mint();

                    self.latest_session_state
                        .insert(key.clone(), (state_token, event_revision));
                    self.fleet.send(
                        machine.as_str(),
                        BridgeCommand::PiRequest {
                            sid: key.session().to_string(),
                            cmd: json!({"type": PI_GET_STATE}),
                            token: state_token,
                            generation,
                        },
                    )
                }
            }
        };

        if let Err(detail) = delivery {
            self.retire_model_requests(chat);
            self.set_models_failed(chat, detail.clone());
            self.reject(detail);
        }
    }

    fn request_branch(&mut self, chat: &ChatId) {
        self.retire_branch_request(chat);
        let Some((machine, workspace)) = self.chat_location(chat) else {
            return;
        };
        let host = match self.fleet.host_for(machine.as_str()) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("harvester: branch indicator: {error}");

                return;
            }
        };
        let token = self.tokens.mint();

        self.latest_branch.insert(chat.clone(), token);
        self.branch_requests.insert(token, chat.clone());

        if let Err(error) = self.send_host_command(machines::Command::ResolveBranch {
            host,
            path: workspace.path().to_string(),
            token,
        }) {
            self.retire_branch_request(chat);
            eprintln!("harvester: branch indicator: {error}");
        }
    }

    fn open_session(&mut self, key: SessionKey) {
        let Some(summary) = self.index.session(&key).cloned() else {
            self.reject(format!("session {} is not listed", key.session()));

            return;
        };
        let retained = self.state.live_sessions.get(&key).map(|session| {
            (
                session.model_catalog.clone(),
                session.model_selection.clone(),
                session.branch.clone(),
                session.replica.clone(),
            )
        });
        let (model_catalog, model_selection, branch, replica) = retained.unwrap_or_else(|| {
            (
                ModelsState::NotRequested,
                ModelSelection::LivePending,
                None,
                SessionReplica::default(),
            )
        });

        if self.state.live_sessions.get(&key).is_some_and(|session| {
            matches!(
                session.lifecycle,
                SessionLifecycle::Opening | SessionLifecycle::Starting | SessionLifecycle::Attached
            )
        }) {
            return;
        }

        if let Err(error) = self.fleet.ensure_connection(key.machine().as_str()) {
            self.reject(error.to_string());

            return;
        }

        let generation = self.next_session_generation(&key);
        let command = BridgeCommand::Open {
            sid: key.session().to_string(),
            cwd: summary.workspace.path().to_string(),
            generation,
        };

        if let Err(detail) = self.fleet.send(key.machine().as_str(), command) {
            self.reject(detail);

            return;
        }

        self.retire_chat_requests(&ChatId::Session(key.clone()));
        self.dialogs.leave_room(&key);
        Arc::make_mut(&mut self.state.live_sessions).insert(
            key.clone(),
            Arc::new(LiveSession {
                key,
                workspace: summary.workspace,
                lifecycle: SessionLifecycle::Opening,
                attachment_generation: generation,
                replay: ReplayState {
                    cursor: None,
                    repair: RepairState::Idle,
                },
                model_catalog,
                model_selection,
                branch,
                replica,
                dialogs: Vec::new(),
                event_revision: 0,
            }),
        );
        self.sync_pending_dialog();
        self.publish();
    }

    fn submit_prompt(&mut self, submission: SubmissionId, chat: ChatId, text: String) {
        if text.trim().is_empty() {
            self.reject_prompt(submission, chat, text, "prompt is empty".to_string());

            return;
        }

        match chat.clone() {
            ChatId::Draft(draft) => self.start_draft(submission, draft, chat, text),
            ChatId::Session(session) => {
                self.prompt_session(submission, &session, chat, text);
            }
        }
    }

    fn start_draft(
        &mut self,
        submission: SubmissionId,
        draft_id: DraftId,
        chat: ChatId,
        prompt: String,
    ) {
        let Some(draft) = self.state.drafts.get(&draft_id).cloned() else {
            self.reject_prompt(
                submission,
                chat,
                prompt,
                "draft is no longer available".to_string(),
            );

            return;
        };
        let (Some(machine), Some(workspace)) =
            (draft.target.machine.clone(), draft.target.workspace.clone())
        else {
            self.reject_prompt(
                submission,
                chat,
                prompt,
                "pick a machine and repository".to_string(),
            );

            return;
        };

        if !matches!(draft.lifecycle, DraftLifecycle::Ready) {
            self.reject_prompt(
                submission,
                chat,
                prompt,
                "session is already starting".to_string(),
            );

            return;
        }

        if !self.machine_is_available(&machine) || !self.state.workspaces.contains(&workspace) {
            self.reject_prompt(
                submission,
                chat,
                prompt,
                "the selected machine or repository is no longer available".to_string(),
            );

            return;
        }

        if let Err(error) = self.fleet.ensure_connection(machine.as_str()) {
            self.reject_prompt(submission, chat, prompt, error.to_string());

            return;
        }

        let session_id =
            SessionId::new(uuid::Uuid::now_v7().to_string()).expect("uuid is non-empty");
        let key = SessionKey::new(machine.clone(), session_id);
        let generation = self.next_session_generation(&key);
        let name = session_name_from_prompt(&prompt);
        let Some((provider, model)) = self.draft_launch_model(&draft, submission, &chat, &prompt)
        else {
            return;
        };
        let command = BridgeCommand::StartSession {
            sid: key.session().to_string(),
            cwd: workspace.path().to_string(),
            provider,
            model,
            name: name.clone(),
            first_prompt: prompt.clone(),
            submission,
            generation,
        };

        if let Err(detail) = self.fleet.send(machine.as_str(), command) {
            self.reject_prompt(submission, chat, prompt, detail);

            return;
        }

        self.retire_chat_requests(&ChatId::Draft(draft_id));
        let mut replica = SessionReplica::default();

        replica.push_user_prompt(submission, prompt.clone());

        if let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&draft_id) {
            draft.lifecycle = DraftLifecycle::Starting(key.clone());
        }

        Arc::make_mut(&mut self.state.live_sessions).insert(
            key.clone(),
            Arc::new(LiveSession {
                key: key.clone(),
                workspace: workspace.clone(),
                lifecycle: SessionLifecycle::Starting,
                attachment_generation: generation,
                replay: ReplayState {
                    cursor: None,
                    repair: RepairState::Idle,
                },
                model_catalog: draft.models,
                model_selection: ModelSelection::LivePending,
                branch: draft.branch,
                replica,
                dialogs: Vec::new(),
                event_revision: 0,
            }),
        );
        self.index
            .insert_started(key.clone(), workspace, name, unix_millis_now());
        self.sync_sessions();
        self.starting_drafts.insert(key.clone(), draft_id);
        self.pending_prompts.insert(
            submission,
            PendingPrompt {
                chat,
                text: prompt,
                session: key,
                generation,
                delivered: false,
                settled: false,
                turn_was_busy: false,
                event_revision: 0,
            },
        );
        self.publish();
    }

    fn draft_launch_model(
        &self,
        draft: &DraftSession,
        submission: SubmissionId,
        chat: &ChatId,
        prompt: &str,
    ) -> Option<(Option<String>, Option<String>)> {
        match &draft.model_selection {
            ModelSelection::Launch(choice) if draft.models.choices().contains(choice) => {
                Some((Some(choice.provider.clone()), Some(choice.id.clone())))
            }
            ModelSelection::LaunchDefault => Some((None, None)),
            ModelSelection::Launch(_) => {
                self.reject_prompt(
                    submission,
                    chat.clone(),
                    prompt.to_string(),
                    "the selected model is no longer available".to_string(),
                );

                None
            }
            ModelSelection::LivePending
            | ModelSelection::Live(_)
            | ModelSelection::LiveUnavailable => {
                self.reject_prompt(
                    submission,
                    chat.clone(),
                    prompt.to_string(),
                    "draft model state is invalid".to_string(),
                );

                None
            }
        }
    }

    fn prompt_session(
        &mut self,
        submission: SubmissionId,
        key: &SessionKey,
        chat: ChatId,
        text: String,
    ) {
        let Some(session) = self.state.live_sessions.get(key) else {
            self.reject_prompt(
                submission,
                chat,
                text,
                "session is no longer available".to_string(),
            );

            return;
        };

        if !matches!(session.lifecycle, SessionLifecycle::Attached) {
            self.reject_prompt(
                submission,
                chat,
                text,
                "session is not attached".to_string(),
            );

            return;
        }

        let generation = session.attachment_generation;
        let turn_was_busy = session.replica.transcript().is_busy();
        let event_revision = session.event_revision;
        let command = BridgeCommand::Prompt {
            sid: key.session().to_string(),
            text: text.clone(),
            submission,
            generation,
        };

        if let Err(detail) = self.fleet.send(key.machine().as_str(), command) {
            self.reject_prompt(submission, chat, text, detail);

            return;
        }

        let Some(session) = live_session_mut(&mut self.state, key) else {
            return;
        };

        session.replica.push_user_prompt(submission, text.clone());
        self.pending_prompts.insert(
            submission,
            PendingPrompt {
                chat,
                text,
                session: key.clone(),
                generation,
                delivered: false,
                settled: false,
                turn_was_busy,
                event_revision,
            },
        );
        self.publish();
    }

    fn pick_model(&mut self, chat: ChatId, picked: Option<ModelChoice>) {
        match chat {
            ChatId::Draft(id) => {
                let Some(draft) = self.state.drafts.get(&id) else {
                    self.reject("draft is no longer available".to_string());

                    return;
                };

                if !matches!(draft.lifecycle, DraftLifecycle::Ready) {
                    self.reject("a starting draft cannot change models".to_string());

                    return;
                }

                if picked
                    .as_ref()
                    .is_some_and(|choice| !draft.models.choices().contains(choice))
                {
                    self.reject("model is not in the current catalog".to_string());

                    return;
                }

                let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&id) else {
                    return;
                };

                draft.model_selection = picked
                    .map(ModelSelection::Launch)
                    .unwrap_or(ModelSelection::LaunchDefault);
                self.publish();
            }
            ChatId::Session(key) => self.set_live_model(&key, picked),
        }
    }

    fn set_live_model(&mut self, key: &SessionKey, picked: Option<ModelChoice>) {
        let Some(choice) = picked else {
            return;
        };
        let Some(session) = self.state.live_sessions.get(key) else {
            return;
        };

        if !matches!(session.lifecycle, SessionLifecycle::Attached) {
            self.reject("session is not attached".to_string());

            return;
        }

        if !session.model_catalog.choices().contains(&choice) {
            self.reject("model is not in the current catalog".to_string());

            return;
        }

        let displaced = session.model_selection.clone();
        let token = self.tokens.mint();

        if let Some(stale) = self.latest_model_set.insert(key.clone(), token) {
            self.displaced_models.remove(&stale);
        }

        self.displaced_models.insert(token, displaced);
        let delivery = self.fleet.send(
            key.machine().as_str(),
            BridgeCommand::PiRequest {
                sid: key.session().to_string(),
                cmd: json!({
                    "type": PI_SET_MODEL,
                    "provider": choice.provider,
                    "modelId": choice.id,
                }),
                token,
                generation: session.attachment_generation,
            },
        );

        if let Err(detail) = delivery {
            self.latest_model_set.remove(key);
            self.displaced_models.remove(&token);
            self.handle_session_delivery_failure(key, &detail);
        }
    }

    fn stop_session(&mut self, key: &SessionKey) {
        let Some(session) = self.state.live_sessions.get(key) else {
            return;
        };

        if !session.can_stop() {
            return;
        }

        let delivery = self.fleet.send(
            key.machine().as_str(),
            BridgeCommand::Stop {
                sid: key.session().to_string(),
                generation: session.attachment_generation,
            },
        );

        if let Err(detail) = delivery {
            self.handle_session_delivery_failure(key, &detail);
        }
    }

    fn answer_dialog(&mut self, key: &DialogKey, answer: DialogAnswer) {
        let Some(generation) = self
            .state
            .live_sessions
            .get(key.session())
            .filter(|session| matches!(session.lifecycle, SessionLifecycle::Attached))
            .map(|session| session.attachment_generation)
        else {
            return;
        };
        let session_key = key.session().clone();
        let dialog_id = self
            .dialogs
            .front()
            .filter(|obligation| obligation.key() == *key)
            .map(|obligation| obligation.request.id.clone());
        let Some(dialog_id) = dialog_id else {
            self.reject("dialog is no longer pending".to_string());

            return;
        };

        if self.answering_dialogs.contains_key(key) {
            return;
        }

        self.answering_dialogs.insert(key.clone(), answer.clone());
        let delivery = self.fleet.send(
            session_key.machine().as_str(),
            BridgeCommand::AnswerDialog {
                sid: session_key.session().to_string(),
                dialog_id,
                answer,
                generation,
            },
        );

        if let Err(detail) = delivery {
            self.answering_dialogs.remove(key);
            self.emit(RuntimeEvent::DialogAnswerFailed {
                dialog: key.clone(),
                detail: detail.clone(),
            });
            self.handle_session_delivery_failure(&session_key, &detail);
            self.publish();
        }
    }

    fn forget_machine(&mut self, machine: &MachineId) {
        if !self
            .state
            .machine(machine)
            .is_some_and(|snapshot| snapshot.status == MachineStatus::Missing)
        {
            self.reject(format!("machine {machine} is not missing"));

            return;
        }

        let kept: Vec<WorkspaceId> = self
            .state
            .workspaces
            .iter()
            .filter(|workspace| workspace.machine() != machine)
            .cloned()
            .collect();

        if let Err(error) = self.workspace_store.save(&kept) {
            let detail = format!("save workspaces: {error}");

            self.emit(RuntimeEvent::MachineForgetFailed {
                machine: machine.clone(),
                detail,
            });

            return;
        }

        self.retire_machine_requests(machine);
        self.state.workspaces = Arc::new(kept);
        self.index.remove_machine(machine);
        let rejected: Vec<SubmissionId> = self
            .pending_prompts
            .iter()
            .filter(|(_, prompt)| prompt.session.machine() == machine)
            .map(|(submission, _)| *submission)
            .collect();

        for submission in rejected {
            self.settle_pending_prompt(submission, CONNECTION_LOST_DETAIL.to_string());
        }

        Arc::make_mut(&mut self.state.live_sessions).retain(|key, _| key.machine() != machine);
        Arc::make_mut(&mut self.state.drafts)
            .retain(|_, draft| draft.target.machine.as_ref() != Some(machine));
        Arc::make_mut(&mut self.state.browsers).retain(|_, browser| &browser.machine != machine);
        self.starting_drafts
            .retain(|key, _| key.machine() != machine);
        self.dialogs.leave_machine(machine);
        self.answering_dialogs
            .retain(|dialog, _| dialog.session().machine() != machine);
        self.sync_pending_dialog();
        self.sync_sessions();

        if self.argv_machine.as_ref() == Some(machine) {
            self.argv_machine = None;
        }

        self.track_machines();
        self.sync_machines();
        self.publish();
        self.emit(RuntimeEvent::MachineForgotten(machine.clone()));
    }

    fn handle_bridge_event(&mut self, event: crate::fleet::Unapplied<(String, u64, BridgeEvent)>) {
        let Some((machine, event)) = self.fleet.observe_bridge_event(event) else {
            return;
        };
        let Some(machine) = MachineId::new(machine) else {
            return;
        };

        match event {
            BridgeEvent::Connected => self.sync_machines(),
            BridgeEvent::ConnectFailed { detail } => self.handle_connect_failed(&machine, &detail),
            BridgeEvent::Sessions(sessions) => self.handle_sessions(machine.clone(), sessions),
            BridgeEvent::RoomJoined { sid, generation } => {
                self.handle_room_joined(machine, sid, generation);
            }
            BridgeEvent::RoomLeft { sid, generation } => {
                self.handle_room_left(machine, sid, generation);
            }
            BridgeEvent::Attached {
                sid,
                generation,
                entries,
                leaf_id,
                history_available,
            } => self.handle_attached(
                machine,
                sid,
                generation,
                entries,
                leaf_id,
                history_available,
            ),
            BridgeEvent::Detached { sid, generation } => {
                self.handle_detached(machine, sid, generation);
            }
            BridgeEvent::Repairing { sid, generation } => {
                self.handle_repairing(machine, sid, generation);
            }
            BridgeEvent::HistoryReplaced {
                sid,
                generation,
                entries,
                leaf_id,
            } => self.handle_history(machine, sid, generation, entries, leaf_id, true),
            BridgeEvent::HistoryExtended {
                sid,
                generation,
                entries,
                leaf_id,
            } => self.handle_history(machine, sid, generation, entries, leaf_id, false),
            BridgeEvent::Pi {
                sid,
                generation,
                event,
            } => self.handle_pi_event(machine, sid, generation, event),
            BridgeEvent::SessionExit {
                sid,
                generation,
                exit_code,
            } => self.handle_session_exit(machine, sid, generation, exit_code),
            BridgeEvent::OperationFailed {
                sid,
                generation,
                operation,
                detail,
            } => self.handle_operation_failed(machine, sid, generation, operation, detail),
            BridgeEvent::PromptDelivered {
                sid,
                generation,
                submission,
            } => self.handle_prompt_delivered(machine, sid, generation, submission),
            BridgeEvent::DialogAnswered {
                sid,
                generation,
                dialog_id,
                result,
            } => self.handle_dialog_answered(machine, sid, generation, dialog_id, result),
            BridgeEvent::PromptFailed {
                sid,
                generation,
                submission,
                detail,
            } => self.handle_prompt_failed(machine, sid, generation, submission, detail),
            BridgeEvent::ModelsListed { token, result } => {
                self.handle_models_answer(token, result);
            }
            BridgeEvent::PiAnswered {
                sid,
                token,
                method,
                result,
            } => self.handle_pi_answer(machine, sid, token, &method, result),
            BridgeEvent::Disconnected => self.handle_disconnected(&machine),
        }

        self.sync_machines();
        self.publish();
    }

    fn handle_sessions(&mut self, machine: MachineId, sessions: Vec<harvester_proto::SessionInfo>) {
        self.index.replace_machine(machine, sessions);
        self.sync_sessions();
    }

    fn handle_room_joined(&mut self, machine: MachineId, sid: String, generation: u64) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        self.dialogs.enter_room(DialogRoom { session: key });
        self.sync_pending_dialog();
    }

    fn handle_room_left(&mut self, machine: MachineId, sid: String, generation: u64) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        self.dialogs.leave_room(&key);
        self.answering_dialogs
            .retain(|dialog, _| dialog.session() != &key);

        if let Some(session) = live_session_mut(&mut self.state, &key) {
            session.dialogs.clear();
        }

        self.sync_pending_dialog();
    }

    fn handle_connect_failed(&mut self, machine: &MachineId, detail: &str) {
        self.fail(format!("{}: {detail}", self.state.machine_label(machine)));
        self.fail_machine_requests(machine, detail);
        self.disconnect_machine(machine, detail);
    }

    fn handle_attached(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        entries: Vec<Value>,
        leaf_id: Option<String>,
        history_available: bool,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };
        let Some(session) = current_session_mut(&mut self.state, &key, generation) else {
            return;
        };

        if history_available
            && (!entries.is_empty() || !matches!(session.lifecycle, SessionLifecycle::Starting))
        {
            session.replica.replace_history_owned(entries);
        }

        session.lifecycle = SessionLifecycle::Attached;
        session.replay.cursor = leaf_id;
        session.replay.repair = RepairState::Idle;
        self.dialogs.enter_room(DialogRoom {
            session: key.clone(),
        });
        self.index.set_status(&key, SessionStatus::Running);
        self.sync_sessions();

        if let Some(draft) = self.starting_drafts.remove(&key) {
            Arc::make_mut(&mut self.state.drafts).remove(&draft);
            self.emit(RuntimeEvent::DraftPromoted {
                draft,
                session: key.clone(),
            });
        }

        self.request_controls(&ChatId::Session(key));
    }

    fn handle_detached(&mut self, machine: MachineId, sid: String, generation: u64) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        self.dialogs.leave_room(&key);
        self.answering_dialogs
            .retain(|dialog, _| dialog.session() != &key);
        self.retire_chat_requests(&ChatId::Session(key.clone()));
        Arc::make_mut(&mut self.state.live_sessions).remove(&key);
        self.sync_pending_dialog();
    }

    fn handle_repairing(&mut self, machine: MachineId, sid: String, generation: u64) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };
        let Some(session) = current_session_mut(&mut self.state, &key, generation) else {
            return;
        };

        session.replay.repair = RepairState::InFlight;
    }

    fn handle_history(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        entries: Vec<Value>,
        leaf_id: Option<String>,
        replace: bool,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };
        let Some(session) = current_session_mut(&mut self.state, &key, generation) else {
            return;
        };

        if replace {
            session.replica.replace_history_owned(entries);
        } else {
            session.replica.extend_history_owned(entries);
        }

        session.replay.cursor = leaf_id;
        session.replay.repair = RepairState::Idle;
    }

    fn handle_pi_event(&mut self, machine: MachineId, sid: String, generation: u64, event: Value) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };
        let dialog_request = DialogRequest::parse(&event);
        let turn_settled = pi_event_ends_turn(&event);
        let Some(session) = current_session_mut(&mut self.state, &key, generation) else {
            return;
        };

        if turn_settled {
            session.event_revision = session
                .event_revision
                .checked_add(1)
                .expect("session turn revision exhausted");
        }

        session.replica.apply_event_owned(event);

        if let Some(request) = dialog_request {
            let obligation = DialogObligation {
                session: key.clone(),
                request: request.clone(),
            };

            if self.dialogs.enqueue(obligation) {
                if let Some(session) = current_session_mut(&mut self.state, &key, generation) {
                    session.dialogs.push(request);
                }

                self.sync_pending_dialog();
            }
        }

        if turn_settled {
            let mut completed = Vec::new();

            for (submission, prompt) in self
                .pending_prompts
                .iter_mut()
                .filter(|(_, prompt)| prompt.session == key && prompt.generation == generation)
            {
                if prompt.delivered {
                    completed.push(*submission);
                } else {
                    prompt.settled = true;
                }
            }

            for submission in completed {
                self.pending_prompts.remove(&submission);
                self.emit(RuntimeEvent::PromptCompleted { submission });
            }

            self.request_branch(&ChatId::Session(key.clone()));
        }
    }

    fn handle_session_exit(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        exit_code: Option<i32>,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };
        let Some(session) = current_session_mut(&mut self.state, &key, generation) else {
            return;
        };

        session.lifecycle = SessionLifecycle::Exited(exit_code);
        session.replica.end_turn();
        session.dialogs.clear();
        self.settle_prompts_for(
            &key,
            generation,
            "session exited before prompt delivery completed",
        );
        self.retire_chat_requests(&ChatId::Session(key.clone()));
        self.dialogs.leave_room(&key);
        self.answering_dialogs
            .retain(|dialog, _| dialog.session() != &key);
        self.sync_pending_dialog();
        self.index.set_status(&key, SessionStatus::Exited);
        self.sync_sessions();
    }

    fn handle_operation_failed(
        &mut self,
        machine: MachineId,
        sid: Option<String>,
        generation: Option<u64>,
        operation: BridgeOperation,
        detail: String,
    ) {
        self.fail(format!(
            "{}: {}: {detail}",
            self.state.machine_label(&machine),
            operation.label()
        ));

        let (Some(sid), Some(generation)) = (sid, generation) else {
            return;
        };
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        if let Some(draft_id) = self.starting_drafts.remove(&key) {
            self.reject_prompts_for(&key, generation, &detail);

            if let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&draft_id) {
                draft.lifecycle = DraftLifecycle::Ready;
            }
            Arc::make_mut(&mut self.state.live_sessions).remove(&key);
            self.index.remove_session(&key);
            self.sync_sessions();

            return;
        }

        let Some(session) = live_session_mut(&mut self.state, &key) else {
            return;
        };

        match operation {
            BridgeOperation::OpenHistory | BridgeOperation::RepairHistory
                if matches!(session.lifecycle, SessionLifecycle::Attached) =>
            {
                session.replay.repair = RepairState::Idle;
            }
            BridgeOperation::CreateSession
            | BridgeOperation::Attach
            | BridgeOperation::OpenHistory => {
                session.lifecycle = SessionLifecycle::Failed(detail);
                session.replay.repair = RepairState::Idle;
            }
            BridgeOperation::ListSessions
            | BridgeOperation::RepairHistory
            | BridgeOperation::Detach
            | BridgeOperation::Stop => {}
        }
    }

    fn handle_prompt_delivered(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        submission: SubmissionId,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        let Some(prompt) = self.pending_prompts.get_mut(&submission) else {
            return;
        };

        if prompt.session != key || prompt.generation != generation || prompt.delivered {
            return;
        }

        prompt.delivered = true;
        let chat = prompt.chat.clone();
        let settled = prompt.settled;

        self.emit(RuntimeEvent::PromptAccepted { submission, chat });

        if settled {
            self.pending_prompts.remove(&submission);
            self.emit(RuntimeEvent::PromptCompleted { submission });
        }
    }

    fn handle_dialog_answered(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        dialog_id: String,
        result: Result<(), String>,
    ) {
        let Some(session_key) = session_key(machine, sid) else {
            return;
        };
        let dialog_key = DialogKey::new(session_key.clone(), dialog_id.clone());
        let Some(answer) = self.answering_dialogs.remove(&dialog_key) else {
            return;
        };

        if current_session(&self.state, &session_key, generation).is_none() {
            return;
        }

        if let Err(detail) = result {
            self.emit(RuntimeEvent::DialogAnswerFailed {
                dialog: dialog_key,
                detail,
            });

            return;
        }

        if self.dialogs.answer_front(&dialog_key, answer).is_none() {
            return;
        }

        if let Some(session) = live_session_mut(&mut self.state, &session_key) {
            session.dialogs.retain(|dialog| dialog.id != dialog_id);
        }

        self.sync_pending_dialog();
    }

    fn handle_prompt_failed(
        &mut self,
        machine: MachineId,
        sid: String,
        generation: u64,
        submission: SubmissionId,
        detail: String,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        if current_session(&self.state, &key, generation).is_none() {
            return;
        }

        self.reject_pending_prompt(submission, detail);
    }

    fn handle_pi_answer(
        &mut self,
        machine: MachineId,
        sid: String,
        token: RequestToken,
        method: &str,
        result: Result<Value, String>,
    ) {
        let Some(key) = session_key(machine, sid) else {
            return;
        };

        match method {
            PI_GET_AVAILABLE_MODELS => {
                self.handle_models_answer_for(&ChatId::Session(key), token, result);
            }
            PI_GET_STATE => self.handle_session_state_answer(&key, token, result),
            PI_SET_MODEL => self.handle_model_set_answer(&key, token, result),
            _ => {}
        }
    }

    fn handle_models_answer(&mut self, token: RequestToken, result: Result<Value, String>) {
        let chat = self
            .latest_models
            .iter()
            .find_map(|(chat, current)| (*current == token).then_some(chat.clone()));

        if let Some(chat) = chat {
            self.handle_models_answer_for(&chat, token, result);
        }
    }

    fn handle_models_answer_for(
        &mut self,
        chat: &ChatId,
        token: RequestToken,
        result: Result<Value, String>,
    ) {
        if self.latest_models.get(chat) != Some(&token) {
            return;
        }

        self.latest_models.remove(chat);
        let models = match result {
            Ok(data) => ModelsState::Loaded(extract_model_choices(&data)),
            Err(detail) => ModelsState::Failed(detail),
        };

        self.set_models(chat, models);
    }

    fn handle_session_state_answer(
        &mut self,
        key: &SessionKey,
        token: RequestToken,
        result: Result<Value, String>,
    ) {
        let Some((current, event_revision)) = self.latest_session_state.get(key).copied() else {
            return;
        };

        if current != token {
            return;
        }

        self.latest_session_state.remove(key);
        let Some(session) = live_session_mut(&mut self.state, key) else {
            return;
        };

        match result {
            Ok(data) => {
                session.model_selection = data
                    .get("model")
                    .and_then(model_choice_from_value)
                    .map(ModelSelection::Live)
                    .unwrap_or(ModelSelection::LiveUnavailable);

                if session.event_revision == event_revision {
                    let streaming = data
                        .get("isStreaming")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let progress_observed = session.replica.transcript().is_busy();

                    if streaming || !progress_observed {
                        session.replica.reconcile_streaming(streaming);
                    }
                }
            }
            Err(_) => session.model_selection = ModelSelection::LiveUnavailable,
        }
    }

    fn handle_model_set_answer(
        &mut self,
        key: &SessionKey,
        token: RequestToken,
        result: Result<Value, String>,
    ) {
        if self.latest_model_set.get(key) != Some(&token) {
            self.displaced_models.remove(&token);

            return;
        }

        self.latest_model_set.remove(key);
        let Some(displaced) = self.displaced_models.remove(&token) else {
            return;
        };
        let (selection, refresh) = match result {
            Ok(data) => (
                model_choice_from_value(&data)
                    .map(ModelSelection::Live)
                    .unwrap_or(displaced),
                false,
            ),
            Err(detail) => {
                self.fail(format!("{PI_SET_MODEL}: {detail}"));

                (displaced, true)
            }
        };
        let Some(session) = live_session_mut(&mut self.state, key) else {
            return;
        };

        session.model_selection = selection;

        if refresh {
            self.request_controls(&ChatId::Session(key.clone()));
        }
    }

    fn handle_disconnected(&mut self, machine: &MachineId) {
        self.fail_machine_requests(machine, CONNECTION_LOST_DETAIL);
        self.disconnect_machine(machine, CONNECTION_LOST_DETAIL);
    }

    fn disconnect_machine(&mut self, machine: &MachineId, detail: &str) {
        let starting: Vec<(SessionKey, DraftId)> = self
            .starting_drafts
            .iter()
            .filter(|(key, _)| key.machine() == machine)
            .map(|(key, draft)| (key.clone(), *draft))
            .collect();

        for (key, draft_id) in starting {
            self.starting_drafts.remove(&key);
            let generation = self
                .state
                .live_sessions
                .get(&key)
                .map(|session| session.attachment_generation)
                .unwrap_or_default();

            self.reject_prompts_for(&key, generation, detail);
            Arc::make_mut(&mut self.state.live_sessions).remove(&key);
            self.index.remove_session(&key);

            if let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&draft_id) {
                draft.lifecycle = DraftLifecycle::Ready;
            }
        }

        let pending: Vec<SubmissionId> = self
            .pending_prompts
            .iter()
            .filter(|(_, prompt)| prompt.session.machine() == machine)
            .map(|(submission, _)| *submission)
            .collect();

        for submission in pending {
            self.settle_pending_prompt(submission, detail.to_string());
        }

        self.retire_machine_daemon_requests(machine);
        self.dialogs.leave_machine(machine);
        self.answering_dialogs
            .retain(|dialog, _| dialog.session().machine() != machine);

        for session in Arc::make_mut(&mut self.state.live_sessions)
            .values_mut()
            .map(Arc::make_mut)
            .filter(|session| session.key.machine() == machine)
        {
            session.lifecycle = SessionLifecycle::Disconnected(detail.to_string());
            session.replica.end_turn();
            session.dialogs.clear();
        }

        self.sync_pending_dialog();
        self.sync_sessions();
    }

    fn fail_machine_requests(&mut self, machine: &MachineId, detail: &str) {
        for draft in Arc::make_mut(&mut self.state.drafts)
            .values_mut()
            .filter(|draft| draft.target.machine.as_ref() == Some(machine))
        {
            if matches!(draft.models, ModelsState::Loading) {
                draft.models = ModelsState::Failed(detail.to_string());
            }
        }

        for session in Arc::make_mut(&mut self.state.live_sessions)
            .values_mut()
            .map(Arc::make_mut)
            .filter(|session| session.key.machine() == machine)
        {
            if matches!(session.model_catalog, ModelsState::Loading) {
                session.model_catalog = ModelsState::Failed(detail.to_string());
            }
        }
    }

    fn handle_machine_event(&mut self, event: crate::fleet::Unapplied<machines::Event>) {
        let event = self.fleet.observe_machines_event(event);

        match event {
            machines::Event::Listed { .. } | machines::Event::Probed { .. } => {
                self.sync_machines();
            }
            machines::Event::ProviderFailed { provider, detail } => {
                self.fail(format!("provider {provider}: {detail}"));
            }
            machines::Event::HomeResolved { token, result } => {
                self.handle_browser_home(token, result);
            }
            machines::Event::DirListed { token, result } => {
                self.handle_browser_listing(token, result);
            }
            machines::Event::WorkspaceValidated { token, result } => {
                self.handle_browser_validation(token, result);
            }
            machines::Event::BranchResolved { token, result } => {
                self.handle_branch_answer(token, result);
            }
        }

        self.publish();
    }

    fn handle_branch_answer(&mut self, token: RequestToken, result: Result<String, String>) {
        let Some(chat) = self.branch_requests.remove(&token) else {
            return;
        };

        if self.latest_branch.get(&chat) != Some(&token) {
            return;
        }

        self.latest_branch.remove(&chat);
        let branch = result.ok();

        match chat {
            ChatId::Draft(id) => {
                if let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(&id) {
                    draft.branch = branch;
                }
            }
            ChatId::Session(key) => {
                if let Some(session) = live_session_mut(&mut self.state, &key) {
                    session.branch = branch;
                }
            }
        }
    }

    fn chat_location(&self, chat: &ChatId) -> Option<(MachineId, WorkspaceId)> {
        match chat {
            ChatId::Draft(id) => {
                let draft = self.state.drafts.get(id)?;

                Some((
                    draft.target.machine.as_ref()?.clone(),
                    draft.target.workspace.as_ref()?.clone(),
                ))
            }
            ChatId::Session(key) => {
                let session = self.state.live_sessions.get(key)?;

                Some((key.machine().clone(), session.workspace.clone()))
            }
        }
    }

    fn set_models_loading(&mut self, chat: &ChatId) {
        self.set_models(chat, ModelsState::Loading);
    }

    fn set_models_failed(&mut self, chat: &ChatId, detail: String) {
        self.set_models(chat, ModelsState::Failed(detail));
    }

    fn set_models(&mut self, chat: &ChatId, models: ModelsState) {
        match chat {
            ChatId::Draft(id) => {
                if let Some(draft) = Arc::make_mut(&mut self.state.drafts).get_mut(id) {
                    draft.models = models;
                }
            }
            ChatId::Session(key) => {
                if let Some(session) = live_session_mut(&mut self.state, key) {
                    session.model_catalog = models;
                }
            }
        }
    }

    fn retire_model_requests(&mut self, chat: &ChatId) {
        self.latest_models.remove(chat);

        let ChatId::Session(key) = chat else {
            return;
        };

        self.latest_session_state.remove(key);

        if let Some(token) = self.latest_model_set.remove(key) {
            self.displaced_models.remove(&token);
        }
    }

    fn retire_branch_request(&mut self, chat: &ChatId) {
        let Some(token) = self.latest_branch.remove(chat) else {
            return;
        };

        self.branch_requests.remove(&token);
    }

    fn retire_chat_requests(&mut self, chat: &ChatId) {
        self.retire_model_requests(chat);
        self.retire_branch_request(chat);
    }

    fn retire_machine_daemon_requests(&mut self, machine: &MachineId) {
        let chats: Vec<ChatId> = self
            .latest_models
            .keys()
            .filter(|chat| self.chat_runs_on(chat, machine))
            .cloned()
            .collect();

        for chat in chats {
            self.retire_model_requests(&chat);
        }

        let session_keys: Vec<SessionKey> = self
            .latest_session_state
            .keys()
            .chain(self.latest_model_set.keys())
            .filter(|key| key.machine() == machine)
            .cloned()
            .collect();

        for key in session_keys {
            self.retire_model_requests(&ChatId::Session(key));
        }
    }

    fn retire_machine_requests(&mut self, machine: &MachineId) {
        self.retire_machine_daemon_requests(machine);
        let chats: Vec<ChatId> = self
            .latest_branch
            .keys()
            .filter(|chat| self.chat_runs_on(chat, machine))
            .cloned()
            .collect();
        let browsers: Vec<WorkspaceBrowserId> = self
            .state
            .browsers
            .values()
            .filter(|browser| &browser.machine == machine)
            .map(|browser| browser.id)
            .collect();

        for chat in chats {
            self.retire_branch_request(&chat);
        }

        for browser in browsers {
            self.latest_browser.remove(&browser);
            self.browser_requests
                .retain(|_, (pending_browser, _)| *pending_browser != browser);
        }
    }

    fn chat_runs_on(&self, chat: &ChatId, machine: &MachineId) -> bool {
        match chat {
            ChatId::Draft(id) => {
                self.state
                    .drafts
                    .get(id)
                    .and_then(|draft| draft.target.machine.as_ref())
                    == Some(machine)
            }
            ChatId::Session(key) => key.machine() == machine,
        }
    }

    fn send_host_command(&self, command: machines::Command) -> crate::Result<()> {
        self.fleet.send_host_command(command)
    }

    fn track_machines(&mut self) {
        let machines = self
            .argv_machine
            .iter()
            .chain(self.state.workspaces.iter().map(WorkspaceId::machine))
            .map(ToString::to_string)
            .collect();

        self.fleet.track_machines(machines);
    }

    fn sync_machines(&mut self) {
        let mut machines = Vec::new();
        let mut seen = HashSet::new();
        let local = MachineId::local();

        self.push_machine(
            &mut machines,
            &mut seen,
            local.clone(),
            local.to_string(),
            None,
        );

        for row in self.fleet.provider_machines().to_vec() {
            let Some(id) = MachineId::new(row.key) else {
                continue;
            };

            self.push_machine(&mut machines, &mut seen, id, row.label, Some(row.provider));
        }

        for missing in self.fleet.missing_tracked_machines() {
            let Some(id) = MachineId::new(missing) else {
                continue;
            };
            let label = self.fleet.label(id.as_str());

            self.push_machine(&mut machines, &mut seen, id, label, None);
        }

        for ad_hoc in self.fleet.ad_hoc_machines() {
            let Some(id) = MachineId::new(ad_hoc) else {
                continue;
            };
            let label = id.to_string();

            self.push_machine(&mut machines, &mut seen, id, label, Some("ssh".to_string()));
        }

        self.state.machines = Arc::new(machines);
    }

    fn push_machine(
        &self,
        machines: &mut Vec<MachineSnapshot>,
        seen: &mut HashSet<MachineId>,
        id: MachineId,
        label: String,
        provider: Option<String>,
    ) {
        if !seen.insert(id.clone()) {
            return;
        }

        machines.push(MachineSnapshot {
            status: machine_status(self.fleet.state(id.as_str())),
            connectable: self.fleet.host_for(id.as_str()).is_ok(),
            id,
            label,
            provider,
        });
    }

    fn sync_sessions(&mut self) {
        self.state.sessions = Arc::new(
            self.index
                .visible(&self.state.workspaces)
                .cloned()
                .collect(),
        );
    }

    fn sync_pending_dialog(&mut self) {
        self.state.pending_dialog = self.dialogs.front().cloned();
    }

    fn next_session_generation(&mut self, key: &SessionKey) -> u64 {
        let generation = self.session_generations.entry(key.clone()).or_insert(0);

        *generation = generation
            .checked_add(1)
            .expect("attachment generation exhausted");

        *generation
    }

    fn machine_is_available(&self, machine: &MachineId) -> bool {
        self.state
            .machine(machine)
            .is_some_and(|snapshot| snapshot.connectable && snapshot.status.is_available())
    }

    fn handle_session_delivery_failure(&mut self, key: &SessionKey, detail: &str) {
        self.reject(detail.to_string());
        self.disconnect_machine(key.machine(), detail);
        self.publish();
    }

    fn reject_prompt(&self, submission: SubmissionId, chat: ChatId, text: String, detail: String) {
        self.emit(RuntimeEvent::PromptRejected {
            submission,
            chat,
            text,
            detail,
        });
    }

    fn reject_pending_prompt(&mut self, submission: SubmissionId, detail: String) {
        let Some(prompt) = self.pending_prompts.remove(&submission) else {
            return;
        };

        let another_pending = self.pending_prompts.values().any(|pending| {
            pending.session == prompt.session && pending.generation == prompt.generation
        });

        if let Some(session) = live_session_mut(&mut self.state, &prompt.session) {
            if session.attachment_generation == prompt.generation {
                let preserve_turn = prompt.turn_was_busy
                    || another_pending
                    || session.event_revision != prompt.event_revision;

                session
                    .replica
                    .fail_prompt(submission, detail.clone(), preserve_turn);
            }
        }

        self.reject_prompt(submission, prompt.chat, prompt.text, detail);
    }

    fn settle_pending_prompt(&mut self, submission: SubmissionId, detail: String) {
        if self
            .pending_prompts
            .get(&submission)
            .is_some_and(|prompt| prompt.delivered)
        {
            self.pending_prompts.remove(&submission);
            self.emit(RuntimeEvent::PromptCompleted { submission });

            return;
        }

        self.reject_pending_prompt(submission, detail);
    }

    fn settle_prompts_for(&mut self, key: &SessionKey, generation: u64, detail: &str) {
        let submissions: Vec<SubmissionId> = self
            .pending_prompts
            .iter()
            .filter(|(_, prompt)| &prompt.session == key && prompt.generation == generation)
            .map(|(submission, _)| *submission)
            .collect();

        for submission in submissions {
            self.settle_pending_prompt(submission, detail.to_string());
        }
    }

    fn reject_prompts_for(&mut self, key: &SessionKey, generation: u64, detail: &str) {
        let submissions: Vec<SubmissionId> = self
            .pending_prompts
            .iter()
            .filter(|(_, prompt)| &prompt.session == key && prompt.generation == generation)
            .map(|(submission, _)| *submission)
            .collect();

        for submission in submissions {
            self.reject_pending_prompt(submission, detail.to_string());
        }
    }

    fn reject(&self, detail: String) {
        self.emit(RuntimeEvent::Rejected(detail));
    }

    fn worker_stopped(&mut self, detail: &str) {
        let detail = format!("client runtime stopped: {detail}");

        self.state.fatal_error = Some(detail.clone());
        self.publish();
        self.fail(detail);
    }

    fn fail(&self, detail: String) {
        self.emit(RuntimeEvent::Failed(detail));
    }

    fn emit(&self, event: RuntimeEvent) {
        self.events.emit(event);
    }

    fn publish(&mut self) {
        self.state.revision = self
            .state
            .revision
            .checked_add(1)
            .expect("client snapshot revision exhausted");
        *lock(&self.snapshot, "client snapshot") = Arc::new(self.state.clone());
        self.emit(RuntimeEvent::Changed);
    }
}

fn pi_event_ends_turn(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some(PI_AGENT_SETTLED)
}

fn current_session<'a>(
    state: &'a ClientSnapshot,
    key: &SessionKey,
    generation: u64,
) -> Option<&'a LiveSession> {
    state
        .live_sessions
        .get(key)
        .map(AsRef::as_ref)
        .filter(|session| session.attachment_generation == generation)
}

fn live_session_mut<'a>(
    state: &'a mut ClientSnapshot,
    key: &SessionKey,
) -> Option<&'a mut LiveSession> {
    let sessions = Arc::make_mut(&mut state.live_sessions);
    let session = sessions.get_mut(key)?;

    Some(Arc::make_mut(session))
}

fn current_session_mut<'a>(
    state: &'a mut ClientSnapshot,
    key: &SessionKey,
    generation: u64,
) -> Option<&'a mut LiveSession> {
    live_session_mut(state, key).filter(|session| session.attachment_generation == generation)
}

fn session_key(machine: MachineId, sid: String) -> Option<SessionKey> {
    Some(SessionKey::new(machine, SessionId::new(sid)?))
}

fn machine_status(state: MachineState<'_>) -> MachineStatus {
    match state {
        MachineState::Missing => MachineStatus::Missing,
        MachineState::Unknown => MachineStatus::Unknown,
        MachineState::Reachable => MachineStatus::Reachable,
        MachineState::Unreachable => MachineStatus::Unreachable,
        MachineState::Connecting => MachineStatus::Connecting,
        MachineState::Connected => MachineStatus::Connected,
        MachineState::ConnectFailed { detail } => MachineStatus::ConnectFailed(detail.to_string()),
        MachineState::ConnectionLost => MachineStatus::ConnectionLost,
    }
}

fn session_name_from_prompt(prompt: &str) -> String {
    prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(SESSION_NAME_MAX_CHARS)
        .collect()
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
mod tests;
