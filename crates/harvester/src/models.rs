use gpui::SharedString;
use harvester_client_core::{folder_name, MachineCandidate};
use serde_json::Value;

use crate::session_state::{ModelSelection, ModelsState, SessionPhase, SessionState};
use crate::ui::{self, DEFAULT_MODEL_ROW};

const NO_MACHINE_LABEL: &str = "No machines";
const CHOOSE_MACHINE_LABEL: &str = "Choose machine";
const CHOOSE_REPO_LABEL: &str = "Choose repository";
const LOADING_MODELS_LABEL: &str = "Loading models…";
const DEFAULT_MODEL_LABEL: &str = "Default model";
const PENDING_MODEL_LABEL: &str = "Model";
const UNAVAILABLE_MODEL_LABEL: &str = "Model unavailable";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub name: String,
    pub provider: String,
    pub id: String,
}

pub fn model_choice_from_value(model: &Value) -> Option<ModelChoice> {
    let id = model.get("id").and_then(Value::as_str)?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);

    Some(ModelChoice {
        name: name.to_string(),
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

pub fn extract_model_choices(data: &Value) -> Vec<ModelChoice> {
    let Some(models) = data.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };

    models.iter().filter_map(model_choice_from_value).collect()
}

pub fn present_prompt_strip(
    state: &SessionState,
    model_picks: &[Option<ModelChoice>],
    machine_candidates: &[MachineCandidate],
    machine_label: &str,
    repo_candidates: &[String],
    focused: bool,
) -> ui::PromptStrip {
    ui::PromptStrip {
        notice: prompt_strip_notice(state),
        model: model_control(state, model_picks),
        machine: machine_control(state, machine_candidates, machine_label),
        repo: repo_control(state, repo_candidates),
        branch: state.branch().map(|branch| branch.to_string().into()),
        can_send: can_send(state),
        can_stop: state.can_stop(),
        focused,
    }
}

pub fn model_picks(state: &SessionState) -> Vec<Option<ModelChoice>> {
    if matches!(state.phase(), SessionPhase::Starting { .. }) {
        return Vec::new();
    }

    let offer_default = matches!(state.phase(), SessionPhase::Unstarted);

    offer_default
        .then_some(None)
        .into_iter()
        .chain(state.model_choices().iter().cloned().map(Some))
        .collect()
}

fn prompt_strip_notice(state: &SessionState) -> Option<ui::PromptStripNotice> {
    match (state.exited_notice(), state.phase()) {
        (Some(notice), _) => Some(ui::PromptStripNotice::SessionExited(
            notice.to_string().into(),
        )),
        (None, SessionPhase::Starting { .. }) => Some(ui::PromptStripNotice::SessionStarting),
        (None, _) => None,
    }
}

fn is_unstarted(state: &SessionState) -> bool {
    matches!(state.phase(), SessionPhase::Unstarted)
}

fn can_send(state: &SessionState) -> bool {
    !is_unstarted(state) || state.target().is_resolved()
}

fn model_control(state: &SessionState, picks: &[Option<ModelChoice>]) -> ui::PromptStripControl {
    let (label, enabled) =
        model_control_state(state.selection(), state.models(), state.model_choices());
    let rows = picks
        .iter()
        .map(|pick| ui::DropdownRow {
            label: model_row_label(pick.as_ref()),
            selected: is_current_model(state, pick),
            divider_above: false,
        })
        .collect();

    ui::PromptStripControl {
        label,
        enabled,
        rows,
    }
}

fn is_current_model(state: &SessionState, pick: &Option<ModelChoice>) -> bool {
    match (pick, state.selection()) {
        (None, ModelSelection::LaunchDefault) => true,
        (Some(choice), ModelSelection::Launch(current) | ModelSelection::Live(current)) => {
            choice.provider == current.provider && choice.id == current.id
        }
        _ => false,
    }
}

fn machine_control(
    state: &SessionState,
    candidates: &[MachineCandidate],
    machine_label: &str,
) -> ui::PromptStripControl {
    let label = machine_control_label(&state.target().machine, machine_label, candidates);
    let enabled = is_unstarted(state) && !candidates.is_empty();
    let rows = candidates
        .iter()
        .map(|candidate| ui::DropdownRow {
            label: candidate.label.clone().into(),
            selected: candidate.key == state.target().machine,
            divider_above: false,
        })
        .collect();

    ui::PromptStripControl {
        label,
        enabled,
        rows,
    }
}

fn repo_control(state: &SessionState, repos: &[String]) -> ui::PromptStripControl {
    let path = &state.target().workspace_path;
    let label = repo_control_label(path);
    let enabled = is_unstarted(state) && !state.target().machine.is_empty();
    let mut rows: Vec<ui::DropdownRow> = repos
        .iter()
        .map(|repo| ui::DropdownRow {
            label: repo.clone().into(),
            selected: repo == path,
            divider_above: false,
        })
        .collect();

    rows.push(ui::DropdownRow {
        label: ui::ADD_REPO_ROW.into(),
        selected: false,
        divider_above: !repos.is_empty(),
    });

    ui::PromptStripControl {
        label,
        enabled,
        rows,
    }
}

fn model_control_state(
    selection: &ModelSelection,
    models: &ModelsState,
    choices: &[ModelChoice],
) -> (SharedString, bool) {
    let choices_available = !choices.is_empty();

    match selection {
        ModelSelection::Launch(choice) | ModelSelection::Live(choice) => {
            (choice.name.clone().into(), choices_available)
        }
        ModelSelection::LaunchDefault => match models {
            ModelsState::Loading => (LOADING_MODELS_LABEL.into(), false),
            ModelsState::Loaded(_) => (DEFAULT_MODEL_LABEL.into(), true),
            ModelsState::NotRequested | ModelsState::Failed(_) => {
                (DEFAULT_MODEL_LABEL.into(), false)
            }
        },
        ModelSelection::LivePending => (PENDING_MODEL_LABEL.into(), choices_available),
        ModelSelection::LiveUnavailable => (UNAVAILABLE_MODEL_LABEL.into(), choices_available),
    }
}

fn model_row_label(pick: Option<&ModelChoice>) -> SharedString {
    match pick {
        Some(choice) => choice.name.clone().into(),
        None => DEFAULT_MODEL_ROW.into(),
    }
}

fn machine_control_label(
    machine: &str,
    machine_label: &str,
    candidates: &[MachineCandidate],
) -> SharedString {
    if !machine.is_empty() {
        return machine_label.to_string().into();
    }

    if candidates.is_empty() {
        return NO_MACHINE_LABEL.into();
    }

    CHOOSE_MACHINE_LABEL.into()
}

fn repo_control_label(workspace_path: &str) -> SharedString {
    if workspace_path.is_empty() {
        return CHOOSE_REPO_LABEL.into();
    }

    folder_name(workspace_path).to_string().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_state::ChatTarget;
    use harvester_client_core::RequestTokens;

    fn choice(name: &str) -> ModelChoice {
        ModelChoice {
            name: name.to_string(),
            provider: "anthropic".to_string(),
            id: "claude".to_string(),
        }
    }

    fn candidate(key: &str) -> MachineCandidate {
        MachineCandidate {
            key: key.to_string(),
            label: key.to_string(),
        }
    }

    fn target() -> ChatTarget {
        ChatTarget {
            machine: "exe-dev/vm".to_string(),
            workspace_path: "/home/exedev/bloggy".to_string(),
        }
    }

    fn unstarted_state(target: ChatTarget) -> SessionState {
        SessionState::unstarted(target, RequestTokens::default())
    }

    fn live_state() -> SessionState {
        SessionState::live("01H".to_string(), target(), &[], RequestTokens::default())
    }

    fn strip(
        state: &SessionState,
        candidates: &[MachineCandidate],
        repos: &[String],
    ) -> ui::PromptStrip {
        let picks = model_picks(state);

        present_prompt_strip(state, &picks, candidates, "vm", repos, false)
    }

    #[test]
    fn an_unresolved_target_leaves_the_send_button_off() {
        let unresolved = strip(&unstarted_state(ChatTarget::unresolved()), &[], &[]);
        let resolved = strip(&unstarted_state(target()), &[], &[]);

        assert!(!unresolved.can_send);
        assert!(resolved.can_send);
    }

    #[test]
    fn a_live_session_can_send_but_can_no_longer_move_machine_or_repo() {
        let candidates = [candidate("exe-dev/vm")];
        let repos = ["/home/exedev/bloggy".to_string()];
        let live = strip(&live_state(), &candidates, &repos);
        let unstarted = strip(&unstarted_state(target()), &candidates, &repos);

        assert!(live.can_send);
        assert!(!live.machine.enabled);
        assert!(!live.repo.enabled);
        assert!(unstarted.machine.enabled);
        assert!(unstarted.repo.enabled);
    }

    #[test]
    fn the_repo_control_always_offers_the_add_row_last() {
        let repos = ["/home/exedev/bloggy".to_string()];
        let empty = strip(&unstarted_state(target()), &[], &[]);
        let listed = strip(&unstarted_state(target()), &[], &repos);
        let last = listed.repo.rows.last().expect("the add row");

        assert_eq!(empty.repo.rows.len(), 1);
        assert_eq!(empty.repo.rows[0].label, ui::ADD_REPO_ROW);
        assert!(!empty.repo.rows[0].divider_above);
        assert_eq!(last.label, ui::ADD_REPO_ROW);
        assert!(
            last.divider_above,
            "the add row is separated from real repositories"
        );
        assert!(listed.repo.rows[0].selected, "the picked repo is checked");
    }

    #[test]
    fn a_starting_session_says_so_and_offers_no_model_to_pick() {
        let mut state = unstarted_state(target());
        let _ = state.submit_prompt("fix it".to_string());
        let starting = strip(&state, &[], &[]);

        assert!(matches!(
            starting.notice,
            Some(ui::PromptStripNotice::SessionStarting)
        ));
        assert!(model_picks(&state).is_empty());
        assert!(starting.model.rows.is_empty());
    }

    #[test]
    fn an_exited_session_carries_pis_own_notice() {
        let mut state = live_state();

        assert!(state.exit(Some(17)));

        let exited = strip(&state, &[], &[]);
        let Some(ui::PromptStripNotice::SessionExited(notice)) = exited.notice else {
            panic!("an exited session must say so above the prompt box");
        };

        assert!(notice.contains("17"));
        assert!(!exited.can_stop);
    }

    #[test]
    fn only_an_unstarted_session_offers_the_default_model_row() {
        let unstarted = model_picks(&unstarted_state(target()));
        let live = model_picks(&live_state());

        assert_eq!(unstarted, vec![None]);
        assert!(live.is_empty());
    }

    #[test]
    fn the_picked_model_is_the_checked_row() {
        let mut state = unstarted_state(target());
        let token = model_list_token(&mut state);

        assert!(state.accept_models(
            token,
            Ok(serde_json::json!({"models": [
                {"id": "claude", "provider": "anthropic", "name": "Opus"},
                {"id": "other", "provider": "anthropic", "name": "Haiku"}
            ]}))
        ));
        state.pick_model(Some(choice("Opus")));

        let rows = strip(&state, &[], &[]).model.rows;

        assert_eq!(rows[0].label, DEFAULT_MODEL_ROW);
        assert!(!rows[0].selected);
        assert!(rows[1].selected, "the picked model is the checked row");
        assert!(!rows[2].selected);
    }

    fn model_list_token(state: &mut SessionState) -> harvester_client_core::RequestToken {
        match state.request_model_list().expect("a repo to ask about") {
            harvester_client_core::bridge::Command::ListModels { token, .. } => token,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_model_list_still_loading_disables_the_control() {
        let (label, enabled) =
            model_control_state(&ModelSelection::LaunchDefault, &ModelsState::Loading, &[]);

        assert_eq!(label, "Loading models…");
        assert!(!enabled);
    }

    #[test]
    fn a_loaded_list_makes_the_default_row_pickable() {
        let choices = vec![choice("Sonnet")];
        let (label, enabled) = model_control_state(
            &ModelSelection::LaunchDefault,
            &ModelsState::Loaded(choices.clone()),
            &choices,
        );

        assert_eq!(label, "Default model");
        assert!(enabled);
    }

    #[test]
    fn a_failed_list_keeps_the_default_label_but_offers_nothing() {
        let (label, enabled) = model_control_state(
            &ModelSelection::LaunchDefault,
            &ModelsState::Failed("no gateway".to_string()),
            &[],
        );

        assert_eq!(label, "Default model");
        assert!(!enabled);
    }

    #[test]
    fn a_selection_names_its_own_model_and_is_pickable_while_choices_exist() {
        let choices = vec![choice("Opus")];
        let (live_label, live_enabled) = model_control_state(
            &ModelSelection::Live(choice("Opus")),
            &ModelsState::Loaded(choices.clone()),
            &choices,
        );
        let (orphan_label, orphan_enabled) = model_control_state(
            &ModelSelection::Launch(choice("Opus")),
            &ModelsState::NotRequested,
            &[],
        );

        assert_eq!(live_label, "Opus");
        assert!(live_enabled);
        assert_eq!(orphan_label, "Opus");
        assert!(!orphan_enabled);
    }

    #[test]
    fn a_session_whose_model_pi_never_confirmed_says_so() {
        let choices = vec![choice("Opus")];
        let (pending_label, _) = model_control_state(
            &ModelSelection::LivePending,
            &ModelsState::Loaded(choices.clone()),
            &choices,
        );
        let (unavailable_label, _) = model_control_state(
            &ModelSelection::LiveUnavailable,
            &ModelsState::Loaded(choices.clone()),
            &choices,
        );

        assert_eq!(pending_label, "Model");
        assert_eq!(unavailable_label, "Model unavailable");
    }

    #[test]
    fn the_default_row_is_named_rather_than_blank() {
        assert_eq!(model_row_label(None), DEFAULT_MODEL_ROW);
        assert_eq!(model_row_label(Some(&choice("Sonnet"))), "Sonnet");
    }

    #[test]
    fn an_empty_fleet_says_there_is_nothing_to_pick() {
        assert_eq!(machine_control_label("", "", &[]), NO_MACHINE_LABEL);
    }

    #[test]
    fn an_unpicked_machine_with_candidates_invites_a_pick() {
        assert_eq!(
            machine_control_label("", "", &[candidate("exe-dev/vm")]),
            CHOOSE_MACHINE_LABEL
        );
    }

    #[test]
    fn a_picked_machine_shows_the_fleets_label_for_it() {
        assert_eq!(
            machine_control_label("exe-dev/vm", "vm", &[candidate("exe-dev/vm")]),
            "vm"
        );
    }

    #[test]
    fn the_repo_control_names_the_folder_not_the_path() {
        assert_eq!(repo_control_label(""), CHOOSE_REPO_LABEL);
        assert_eq!(repo_control_label("/home/exedev/bloggy"), "bloggy");
    }
}
