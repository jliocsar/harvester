use gpui::SharedString;
use harvester_client_core::MachineCandidate;

use crate::models::ModelChoice;
use crate::session_state::{ModelSelection, ModelsState};
use crate::workspaces::folder_name;

pub(super) const DEFAULT_MODEL_ROW: &str = "Default (pi chooses)";
pub(super) const ADD_REPO_ROW: &str = "Add new…";
pub(super) const NO_MACHINE_LABEL: &str = "No machines";
pub(super) const CHOOSE_MACHINE_LABEL: &str = "Choose machine";
pub(super) const CHOOSE_REPO_LABEL: &str = "Choose repository";

pub(super) fn model_control_state(
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
            ModelsState::Loading => ("Loading models…".into(), false),
            ModelsState::Loaded(_) => ("Default model".into(), true),
            ModelsState::NotRequested | ModelsState::Failed(_) => ("Default model".into(), false),
        },
        ModelSelection::LivePending => ("Model".into(), choices_available),
        ModelSelection::LiveUnavailable => ("Model unavailable".into(), choices_available),
    }
}

pub(super) fn model_row_label(pick: Option<&ModelChoice>) -> SharedString {
    match pick {
        Some(choice) => choice.name.clone().into(),
        None => DEFAULT_MODEL_ROW.into(),
    }
}

pub(super) fn machine_control_label(
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

pub(super) fn repo_control_label(workspace_path: &str) -> SharedString {
    if workspace_path.is_empty() {
        return CHOOSE_REPO_LABEL.into();
    }

    folder_name(workspace_path).to_string().into()
}

#[cfg(test)]
mod tests {
    use super::*;

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
