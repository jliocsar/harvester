use serde_json::{json, Value};

use crate::models::{self, ModelChoice};
use harvester_client_core::bridge::Command;
use harvester_client_core::transcript::Transcript;
use harvester_client_core::{Pending, RequestToken, RequestTokens};

const SESSION_NAME_MAX_CHARS: usize = 48;

const PI_AGENT_SETTLED: &str = "agent_settled";
const PI_GET_AVAILABLE_MODELS: &str = "get_available_models";
const PI_GET_STATE: &str = "get_state";

pub const PI_SET_MODEL: &str = "set_model";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatTarget {
    pub machine: String,
    pub workspace_path: String,
}

impl ChatTarget {
    pub fn unresolved() -> Self {
        Self::default()
    }

    pub fn is_resolved(&self) -> bool {
        !self.machine.is_empty() && !self.workspace_path.is_empty()
    }
}

pub fn is_turn_settled(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some(PI_AGENT_SETTLED)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPhase {
    Unstarted,
    Starting { sid: String, name: String },
    Live { sid: String },
}

impl SessionPhase {
    pub fn sid(&self) -> Option<&str> {
        match self {
            Self::Unstarted => None,
            Self::Starting { sid, .. } | Self::Live { sid } => Some(sid),
        }
    }

    pub fn starting_session(&self) -> Option<(&str, &str)> {
        match self {
            Self::Starting { sid, name } => Some((sid, name)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModelsState {
    #[default]
    NotRequested,
    Loading,
    Loaded(Vec<ModelChoice>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelection {
    LaunchDefault,
    Launch(ModelChoice),
    LivePending,
    Live(ModelChoice),
    LiveUnavailable,
}

pub struct SessionState {
    phase: SessionPhase,
    target: ChatTarget,
    transcript: Transcript,
    models: ModelsState,
    selection: ModelSelection,
    branch: Option<String>,
    exited: Option<String>,
    tokens: RequestTokens,
    model_request: Pending<()>,
    session_state_request: Pending<()>,
    set_model_request: Pending<ModelSelection>,
    branch_request: Pending<()>,
}

impl SessionState {
    pub fn unstarted(target: ChatTarget, tokens: RequestTokens) -> Self {
        Self {
            phase: SessionPhase::Unstarted,
            target,
            transcript: Transcript::new(&[]),
            models: ModelsState::NotRequested,
            selection: ModelSelection::LaunchDefault,
            branch: None,
            exited: None,
            tokens,
            model_request: Pending::default(),
            session_state_request: Pending::default(),
            set_model_request: Pending::default(),
            branch_request: Pending::default(),
        }
    }

    pub fn live(sid: String, target: ChatTarget, entries: &[Value], tokens: RequestTokens) -> Self {
        Self {
            phase: SessionPhase::Live { sid },
            target,
            transcript: Transcript::new(entries),
            models: ModelsState::NotRequested,
            selection: ModelSelection::LivePending,
            branch: None,
            exited: None,
            tokens,
            model_request: Pending::default(),
            session_state_request: Pending::default(),
            set_model_request: Pending::default(),
            branch_request: Pending::default(),
        }
    }

    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    pub fn sid(&self) -> Option<&str> {
        self.phase.sid()
    }

    pub fn target(&self) -> &ChatTarget {
        &self.target
    }

    pub fn retarget(&mut self, machine: String) -> bool {
        if !matches!(self.phase, SessionPhase::Unstarted) || self.target.machine == machine {
            return false;
        }

        self.target.machine = machine;
        self.selection = ModelSelection::LaunchDefault;
        self.models = ModelsState::NotRequested;
        self.forget_branch();

        true
    }

    pub fn pick_repo(&mut self, path: String) -> bool {
        if !matches!(self.phase, SessionPhase::Unstarted) || self.target.workspace_path == path {
            return false;
        }

        self.target.workspace_path = path;
        self.selection = ModelSelection::LaunchDefault;
        self.models = ModelsState::NotRequested;
        self.forget_branch();

        true
    }

    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    pub fn request_branch(&mut self) -> Option<RequestToken> {
        if !self.target.is_resolved() {
            return None;
        }

        Some(self.branch_request.ask(&self.tokens, ()))
    }

    pub fn cancel_branch(&mut self) {
        self.branch_request.cancel();
    }

    pub fn accept_branch(&mut self, token: RequestToken, result: Result<String, String>) -> bool {
        if self.branch_request.accept(token).is_none() {
            return false;
        }

        self.branch = result.ok();

        true
    }

    fn forget_branch(&mut self) {
        self.branch_request.cancel();
        self.branch = None;
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn models(&self) -> &ModelsState {
        &self.models
    }

    pub fn model_choices(&self) -> &[ModelChoice] {
        match &self.models {
            ModelsState::Loaded(choices) => choices,
            _ => &[],
        }
    }

    pub fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    pub fn exited_notice(&self) -> Option<&str> {
        self.exited.as_deref()
    }

    pub fn is_live(&self) -> bool {
        matches!(self.phase, SessionPhase::Live { .. }) && self.exited.is_none()
    }

    pub fn request_model_list(&mut self) -> Option<Command> {
        if !self.target.is_resolved() {
            return None;
        }

        Some(Command::ListModels {
            cwd: self.target.workspace_path.clone(),
            token: self.begin_model_fetch(),
        })
    }

    fn begin_model_fetch(&mut self) -> RequestToken {
        self.models = ModelsState::Loading;
        self.model_request.ask(&self.tokens, ())
    }

    pub fn request_model_controls(&mut self) -> Vec<Command> {
        let SessionPhase::Live { sid } = &self.phase else {
            return Vec::new();
        };
        let sid = sid.clone();

        self.selection = ModelSelection::LivePending;

        let models = self.begin_model_fetch();
        let state = self.session_state_request.ask(&self.tokens, ());

        vec![
            Command::PiRequest {
                sid: sid.clone(),
                cmd: json!({"type": PI_GET_AVAILABLE_MODELS}),
                token: models,
            },
            Command::PiRequest {
                sid,
                cmd: json!({"type": PI_GET_STATE}),
                token: state,
            },
        ]
    }

    pub fn accept_pi_answer(
        &mut self,
        method: &str,
        token: RequestToken,
        result: Result<Value, String>,
    ) -> bool {
        match method {
            PI_GET_AVAILABLE_MODELS => self.accept_models(token, result),
            PI_GET_STATE => self.accept_session_state(token, result),
            PI_SET_MODEL => self.accept_model_set(token, result),
            _ => false,
        }
    }

    pub fn accept_models(&mut self, token: RequestToken, result: Result<Value, String>) -> bool {
        if self.model_request.accept(token).is_none() {
            return false;
        }

        self.models = match result {
            Ok(data) => ModelsState::Loaded(models::extract_model_choices(&data)),
            Err(detail) => ModelsState::Failed(detail),
        };

        true
    }

    pub fn fail_pending(&mut self, detail: String) -> bool {
        let mut changed = false;

        if matches!(self.phase, SessionPhase::Starting { .. }) {
            self.phase = SessionPhase::Unstarted;
            self.transcript.replace_history(&[]);
            self.selection = ModelSelection::LaunchDefault;
            changed = true;
        }

        if matches!(self.models, ModelsState::Loading) {
            self.model_request.cancel();
            self.models = ModelsState::Failed(detail);
            changed = true;
        }

        self.session_state_request.cancel();
        self.set_model_request.cancel();
        self.branch_request.cancel();

        changed
    }

    pub fn cancel_model_fetch(&mut self, detail: String) -> bool {
        if !matches!(self.models, ModelsState::Loading) {
            return false;
        }

        self.model_request.cancel();
        self.models = ModelsState::Failed(detail);

        true
    }

    pub fn fail_prompt(&mut self, detail: String) -> bool {
        if !matches!(self.phase, SessionPhase::Live { .. }) {
            return false;
        }

        self.transcript.fail_turn(detail);

        true
    }

    pub fn fail_start(&mut self) -> bool {
        if !matches!(self.phase, SessionPhase::Starting { .. }) {
            return false;
        }

        self.phase = SessionPhase::Unstarted;
        self.transcript.replace_history(&[]);
        self.selection = ModelSelection::LaunchDefault;

        true
    }

    pub fn promote_to_live(&mut self, sid: String, entries: &[Value]) -> bool {
        if !matches!(&self.phase, SessionPhase::Starting { sid: expected, .. } if expected == &sid)
        {
            return false;
        }

        if !entries.is_empty() {
            self.transcript.replace_history(entries);
        }

        self.phase = SessionPhase::Live { sid };
        self.selection = ModelSelection::LivePending;
        self.exited = None;

        true
    }

    pub fn replace_history(&mut self, entries: &[Value]) -> bool {
        if self.sid().is_none() {
            return false;
        }

        self.transcript.replace_history(entries);

        true
    }

    pub fn extend_history(&mut self, entries: &[Value]) -> bool {
        if self.sid().is_none() {
            return false;
        }

        self.transcript.extend_history(entries);

        true
    }

    pub fn apply_pi_event(&mut self, event: &Value) -> bool {
        self.transcript.apply(event)
    }

    pub fn exit(&mut self, exit_code: Option<i32>) -> bool {
        if self.sid().is_none() || self.exited.is_some() {
            return false;
        }

        let code_text = exit_code
            .map(|code| format!(" (exit code {code})"))
            .unwrap_or_default();
        self.transcript.end_turn();
        self.exited = Some(format!(
            "Session exited{code_text} — click it in the sidebar to resume"
        ));

        true
    }

    pub fn pick_model(&mut self, picked: Option<ModelChoice>) -> Option<Command> {
        match &self.phase {
            SessionPhase::Unstarted => {
                self.selection = match picked {
                    Some(choice) => ModelSelection::Launch(choice),
                    None => ModelSelection::LaunchDefault,
                };
                None
            }
            SessionPhase::Live { sid } => {
                let choice = picked?;
                let sid = sid.clone();
                let displaced = self.selection.clone();
                let token = self.set_model_request.ask(&self.tokens, displaced);

                Some(Command::PiRequest {
                    sid,
                    cmd: json!({
                        "type": PI_SET_MODEL,
                        "provider": choice.provider,
                        "modelId": choice.id,
                    }),
                    token,
                })
            }
            SessionPhase::Starting { .. } => None,
        }
    }

    pub fn accept_model_set(&mut self, token: RequestToken, result: Result<Value, String>) -> bool {
        let Some(displaced) = self.set_model_request.accept(token) else {
            return false;
        };

        self.selection = match result {
            Ok(data) => models::model_choice_from_value(&data)
                .map(ModelSelection::Live)
                .unwrap_or(displaced),
            Err(_) => displaced,
        };

        true
    }

    pub fn accept_session_state(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
    ) -> bool {
        if self.session_state_request.accept(token).is_none() {
            return false;
        }

        self.selection = match result {
            Ok(data) => data
                .get("model")
                .and_then(models::model_choice_from_value)
                .map(ModelSelection::Live)
                .unwrap_or(ModelSelection::LiveUnavailable),
            Err(_) => ModelSelection::LiveUnavailable,
        };

        true
    }

    pub fn can_stop(&self) -> bool {
        matches!(self.phase, SessionPhase::Live { .. })
            && self.exited.is_none()
            && self.transcript.is_busy()
    }

    pub fn stop_command(&self) -> Option<Command> {
        if !self.can_stop() {
            return None;
        }

        Some(Command::Stop {
            sid: self.sid()?.to_string(),
        })
    }

    pub fn submit_prompt(&mut self, text: String) -> Option<Command> {
        if text.trim().is_empty() || self.exited.is_some() {
            return None;
        }

        match &self.phase {
            SessionPhase::Unstarted => self.start(text),
            SessionPhase::Live { sid } => {
                self.transcript.push_user_prompt(text.clone());

                Some(Command::Prompt {
                    sid: sid.clone(),
                    text,
                })
            }
            SessionPhase::Starting { .. } => None,
        }
    }

    fn start(&mut self, prompt: String) -> Option<Command> {
        if !self.target.is_resolved() {
            return None;
        }

        let sid = uuid::Uuid::now_v7().to_string();
        let name = session_name_from_prompt(&prompt);
        let (provider, model) = match &self.selection {
            ModelSelection::Launch(choice) => {
                (Some(choice.provider.clone()), Some(choice.id.clone()))
            }
            ModelSelection::LaunchDefault => (None, None),
            _ => return None,
        };

        self.transcript.push_user_prompt(prompt.clone());
        self.phase = SessionPhase::Starting {
            sid: sid.clone(),
            name: name.clone(),
        };

        Some(Command::StartSession {
            sid,
            cwd: self.target.workspace_path.clone(),
            provider,
            model,
            name,
            first_prompt: prompt,
        })
    }
}

pub fn session_name_from_prompt(prompt: &str) -> String {
    prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(SESSION_NAME_MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvester_client_core::transcript::Item;
    use serde_json::json;

    fn target() -> ChatTarget {
        ChatTarget {
            machine: "local".to_string(),
            workspace_path: "/work/repo".to_string(),
        }
    }

    fn model(id: &str) -> ModelChoice {
        models::model_choice_from_value(&json!({
            "id": id,
            "provider": "test",
            "name": id
        }))
        .unwrap()
    }

    #[test]
    fn lifecycle_is_one_entity_from_unstarted_to_live() {
        let tokens = RequestTokens::default();
        let mut state = SessionState::unstarted(target(), tokens);
        let command = state.submit_prompt("fix it".to_string()).unwrap();
        let sid = match command {
            Command::StartSession { sid, .. } => sid,
            _ => panic!("first prompt must start the session"),
        };

        assert!(matches!(state.phase(), SessionPhase::Starting { .. }));
        assert!(state.submit_prompt("second".to_string()).is_none());
        assert!(state.promote_to_live(sid.clone(), &[]));
        assert_eq!(state.sid(), Some(sid.as_str()));
        assert!(matches!(state.phase(), SessionPhase::Live { .. }));
    }

    #[test]
    fn failed_start_returns_to_unstarted_without_a_ghost_session() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());
        let _ = state.submit_prompt("fix it".to_string());

        assert!(state.fail_start());
        assert!(matches!(state.phase(), SessionPhase::Unstarted));
        assert_eq!(state.sid(), None);
        assert!(state.transcript().items().is_empty());
    }

    #[test]
    fn exit_ends_the_turn_and_rejects_more_prompts() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let _ = state.submit_prompt("running".to_string());

        assert!(state.exit(Some(17)));
        assert!(!state.is_live());
        assert!(state.exited_notice().unwrap().contains("17"));
        assert!(state.submit_prompt("after exit".to_string()).is_none());
    }

    fn model_list_token(state: &mut SessionState) -> RequestToken {
        match state.request_model_list().expect("a repo to ask about") {
            Command::ListModels { token, .. } => token,
            _ => unreachable!(),
        }
    }

    fn pi_request(command: Command) -> (Value, RequestToken) {
        match command {
            Command::PiRequest { cmd, token, .. } => (cmd, token),
            _ => panic!("a session-scoped request must ride the generic pi round trip"),
        }
    }

    fn pi_method(command: &Command) -> String {
        match command {
            Command::PiRequest { cmd, .. } => cmd["type"].as_str().unwrap_or_default().to_string(),
            _ => panic!("a session-scoped request must ride the generic pi round trip"),
        }
    }

    #[test]
    fn an_unresolved_target_asks_for_nothing_and_starts_nothing() {
        let mut state = SessionState::unstarted(ChatTarget::unresolved(), RequestTokens::default());

        assert!(state.request_model_list().is_none());
        assert!(state.request_branch().is_none());
        assert!(state.submit_prompt("go".to_string()).is_none());
        assert!(matches!(state.models(), ModelsState::NotRequested));

        assert!(state.retarget("local".to_string()));
        assert!(state.submit_prompt("go".to_string()).is_none());

        assert!(state.pick_repo("/work/repo".to_string()));
        assert!(state.submit_prompt("go".to_string()).is_some());
    }

    #[test]
    fn a_repo_pick_drops_the_branch_and_the_answer_it_was_waiting_on() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());
        let token = state.request_branch().expect("a repo to ask about");

        assert!(state.accept_branch(token, Ok("main".to_string())));
        assert_eq!(state.branch(), Some("main"));

        let stale = state.request_branch().expect("a repo to ask about");

        assert!(state.pick_repo("/work/other".to_string()));
        assert_eq!(state.branch(), None);
        assert!(!state.accept_branch(stale, Ok("main".to_string())));
    }

    #[test]
    fn a_failed_branch_exec_clears_the_indicator_without_a_notice() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());
        let token = state.request_branch().expect("a repo to ask about");

        assert!(state.accept_branch(token, Err("not a git repo".to_string())));
        assert_eq!(state.branch(), None);
        assert!(state.exited_notice().is_none());
    }

    #[test]
    fn only_agent_settled_refreshes_the_branch() {
        assert!(is_turn_settled(&json!({"type": "agent_settled"})));
        assert!(!is_turn_settled(&json!({"type": "agent_end"})));
        assert!(!is_turn_settled(&json!({"other": "shape"})));
    }

    #[test]
    fn a_live_chat_neither_moves_machine_nor_repo() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());

        assert!(!state.retarget("other".to_string()));
        assert!(!state.pick_repo("/work/other".to_string()));
        assert_eq!(state.target(), &target());
    }

    #[test]
    fn cancelled_model_fetch_rejects_a_late_answer() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());
        let token = model_list_token(&mut state);

        assert!(state.cancel_model_fetch("connection lost".to_string()));
        assert!(!state.accept_models(token, Ok(json!({"models": []}))));
        assert!(matches!(state.models(), ModelsState::Failed(_)));
    }

    #[test]
    fn stale_model_answers_cannot_replace_a_newer_fetch() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());
        let first = model_list_token(&mut state);
        let second = model_list_token(&mut state);

        assert!(!state.accept_models(first, Ok(json!({"models": []}))));
        assert!(state.accept_models(
            second,
            Ok(json!({"models": [{"id": "new", "provider": "test"}]}))
        ));
        assert_eq!(state.model_choices()[0].id, "new");
    }

    #[test]
    fn unstarted_and_live_model_picks_have_different_meanings() {
        let choice = model("chosen");
        let mut unstarted = SessionState::unstarted(target(), RequestTokens::default());
        assert!(unstarted.pick_model(Some(choice.clone())).is_none());
        let start = unstarted.submit_prompt("go".to_string()).unwrap();
        assert!(matches!(
            start,
            Command::StartSession {
                provider: Some(_),
                model: Some(_),
                ..
            }
        ));

        let mut live =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let (cmd, _token) = pi_request(live.pick_model(Some(choice)).unwrap());

        assert_eq!(cmd["type"], PI_SET_MODEL);
        assert!(matches!(live.selection(), ModelSelection::LivePending));
    }

    #[test]
    fn stale_model_selection_answers_cannot_rewind_a_newer_pick() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let (_, first) = pi_request(state.pick_model(Some(model("first"))).unwrap());
        let (_, second) = pi_request(state.pick_model(Some(model("second"))).unwrap());

        assert!(!state.accept_model_set(first, Ok(json!({"id": "first", "provider": "test"}))));
        assert!(state.accept_model_set(second, Ok(json!({"id": "second", "provider": "test"}))));
        assert!(matches!(
            state.selection(),
            ModelSelection::Live(ModelChoice { id, .. }) if id == "second"
        ));
    }

    #[test]
    fn a_refused_model_switch_puts_the_displayed_model_back() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let (_, confirmed) = pi_request(state.pick_model(Some(model("first"))).unwrap());

        assert!(state.accept_model_set(confirmed, Ok(json!({"id": "first", "provider": "test"}))));

        let (_, refused) = pi_request(state.pick_model(Some(model("second"))).unwrap());

        assert!(state.accept_model_set(refused, Err("Model not found".to_string())));
        assert!(
            matches!(state.selection(), ModelSelection::Live(ModelChoice { id, .. }) if id == "first"),
            "a refused switch leaves the model the session is actually running on screen"
        );
    }

    #[test]
    fn a_prompt_that_never_left_the_client_ends_its_turn_with_a_reason() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let _ = state.submit_prompt("run it".to_string());

        assert!(state.transcript().is_busy());
        assert!(state.fail_prompt("Not connected".to_string()));
        assert!(!state.transcript().is_busy());

        let last = state.transcript().items().last().expect("a failure item");

        assert!(matches!(last, Item::Failure(detail) if detail == "Not connected"));
    }

    #[test]
    fn only_a_live_session_with_a_running_turn_offers_a_stop() {
        let unstarted = SessionState::unstarted(target(), RequestTokens::default());

        assert!(!unstarted.can_stop());
        assert!(unstarted.stop_command().is_none());

        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());

        assert!(
            !state.can_stop(),
            "an idle session has no turn to interrupt"
        );

        let _ = state.submit_prompt("run it".to_string());

        assert!(state.can_stop());
        assert!(
            matches!(state.stop_command(), Some(Command::Stop { sid }) if sid == "sid"),
            "the stop targets the sid this chat renders"
        );
        assert!(state.exit(Some(0)));
        assert!(
            !state.can_stop(),
            "a session pi has already left has nothing to stop"
        );
    }

    #[test]
    fn an_unstarted_chat_has_no_turn_to_fail() {
        let mut state = SessionState::unstarted(target(), RequestTokens::default());

        assert!(!state.fail_prompt("Not connected".to_string()));
    }

    #[test]
    fn session_state_reads_the_current_model_from_pi_state_data() {
        let mut state =
            SessionState::live("sid".to_string(), target(), &[], RequestTokens::default());
        let controls = state.request_model_controls();
        let methods: Vec<String> = controls.iter().map(pi_method).collect();

        assert_eq!(methods, [PI_GET_AVAILABLE_MODELS, PI_GET_STATE]);

        let (_, token) = pi_request(controls.into_iter().last().unwrap());

        assert!(state.accept_pi_answer(
            PI_GET_STATE,
            token,
            Ok(json!({
                "model": {"id": "current", "provider": "test", "name": "Current"}
            }))
        ));
        assert!(matches!(
            state.selection(),
            ModelSelection::Live(ModelChoice { id, name, .. })
                if id == "current" && name == "Current"
        ));
    }
}
