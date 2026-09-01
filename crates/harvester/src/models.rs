use gpui::SharedString;
use harvester_client_core::{
    folder_name, ChatId, ClientSnapshot, DraftLifecycle, MachineSnapshot, ModelChoice,
    ModelSelection, ModelsState, SessionLifecycle, WorkspaceId,
};

use crate::ui::{self, DEFAULT_MODEL_ROW};

const NO_MACHINE_LABEL: &str = "No machines";
const CHOOSE_MACHINE_LABEL: &str = "Choose machine";
const CHOOSE_REPO_LABEL: &str = "Choose repository";
const LOADING_MODELS_LABEL: &str = "Loading models…";
const DEFAULT_MODEL_LABEL: &str = "Default model";
const PENDING_MODEL_LABEL: &str = "Model";
const UNAVAILABLE_MODEL_LABEL: &str = "Model unavailable";
const OPENING_SESSION_NOTICE: &str = "Opening session…";
const FAILED_SESSION_PREFIX: &str = "Session failed: ";
const DISCONNECTED_SESSION_PREFIX: &str = "Session disconnected: ";
const EXITED_SESSION_NOTICE: &str = "Session exited";

pub(crate) fn present_prompt_strip(
    snapshot: &ClientSnapshot,
    chat: &ChatId,
    focused: bool,
    accepts_prompt: bool,
) -> ui::PromptStrip {
    let picks = model_picks(snapshot, chat);
    let machine = chat_machine(snapshot, chat);
    let machine_candidates = snapshot.machine_candidates();
    let repos = machine
        .map(|machine| snapshot.repos_on(machine))
        .unwrap_or_default();

    ui::PromptStrip {
        notice: prompt_strip_notice(snapshot, chat),
        model: model_control(snapshot, chat, &picks),
        machine: machine_control(snapshot, chat, &machine_candidates),
        repo: repo_control(snapshot, chat, &repos),
        branch: chat_branch(snapshot, chat).map(|branch| branch.to_string().into()),
        can_send: accepts_prompt && can_send(snapshot, chat),
        can_stop: can_stop(snapshot, chat),
        focused,
    }
}

pub(crate) fn model_picks(snapshot: &ClientSnapshot, chat: &ChatId) -> Vec<Option<ModelChoice>> {
    let Some(models) = chat_models(snapshot, chat) else {
        return Vec::new();
    };
    let offer_default = match chat {
        ChatId::Draft(id) => snapshot
            .draft(*id)
            .is_some_and(|draft| matches!(draft.lifecycle, DraftLifecycle::Ready)),
        ChatId::Session(_) => false,
    };

    offer_default
        .then_some(None)
        .into_iter()
        .chain(models.choices().iter().cloned().map(Some))
        .collect()
}

pub(crate) fn machine_candidates(snapshot: &ClientSnapshot) -> Vec<&MachineSnapshot> {
    snapshot.machine_candidates()
}

pub(crate) fn repo_candidates<'a>(
    snapshot: &'a ClientSnapshot,
    chat: &ChatId,
) -> Vec<&'a WorkspaceId> {
    chat_machine(snapshot, chat)
        .map(|machine| snapshot.repos_on(machine))
        .unwrap_or_default()
}

fn prompt_strip_notice(snapshot: &ClientSnapshot, chat: &ChatId) -> Option<ui::PromptStripNotice> {
    match chat {
        ChatId::Draft(id) => match &snapshot.draft(*id)?.lifecycle {
            DraftLifecycle::Ready => None,
            DraftLifecycle::Starting(_) => Some(ui::PromptStripNotice::SessionStarting),
        },
        ChatId::Session(key) => match &snapshot.live_session(key)?.lifecycle {
            SessionLifecycle::Opening | SessionLifecycle::Starting => Some(
                ui::PromptStripNotice::SessionUnavailable(OPENING_SESSION_NOTICE.into()),
            ),
            SessionLifecycle::Attached => None,
            SessionLifecycle::Failed(detail) => Some(ui::PromptStripNotice::SessionUnavailable(
                format!("{FAILED_SESSION_PREFIX}{detail}").into(),
            )),
            SessionLifecycle::Disconnected(detail) => {
                Some(ui::PromptStripNotice::SessionUnavailable(
                    format!("{DISCONNECTED_SESSION_PREFIX}{detail}").into(),
                ))
            }
            SessionLifecycle::Exited(Some(code)) => Some(ui::PromptStripNotice::SessionExited(
                format!("{EXITED_SESSION_NOTICE} with code {code}").into(),
            )),
            SessionLifecycle::Exited(None) => Some(ui::PromptStripNotice::SessionExited(
                EXITED_SESSION_NOTICE.into(),
            )),
        },
    }
}

fn model_control(
    snapshot: &ClientSnapshot,
    chat: &ChatId,
    picks: &[Option<ModelChoice>],
) -> ui::PromptStripControl {
    let selection = chat_selection(snapshot, chat);
    let models = chat_models(snapshot, chat);
    let (label, enabled) = match (selection, models) {
        (Some(selection), Some(models)) => model_control_state(selection, models),
        _ => (PENDING_MODEL_LABEL.into(), false),
    };
    let rows = picks
        .iter()
        .map(|pick| ui::DropdownRow {
            label: model_row_label(pick.as_ref()),
            selected: selection.is_some_and(|selection| is_current_model(selection, pick)),
            divider_above: false,
        })
        .collect();

    ui::PromptStripControl {
        label,
        enabled,
        rows,
    }
}

fn machine_control(
    snapshot: &ClientSnapshot,
    chat: &ChatId,
    candidates: &[&MachineSnapshot],
) -> ui::PromptStripControl {
    let machine = chat_machine(snapshot, chat);
    let label = machine_control_label(
        machine.map(|machine| snapshot.machine_label(machine)),
        candidates.is_empty(),
    );
    let enabled = draft_ready(snapshot, chat) && !candidates.is_empty();
    let rows = candidates
        .iter()
        .map(|candidate| ui::DropdownRow {
            label: candidate.label.clone().into(),
            selected: machine == Some(&candidate.id),
            divider_above: false,
        })
        .collect();

    ui::PromptStripControl {
        label,
        enabled,
        rows,
    }
}

fn repo_control(
    snapshot: &ClientSnapshot,
    chat: &ChatId,
    repos: &[&WorkspaceId],
) -> ui::PromptStripControl {
    let selected = chat_workspace(snapshot, chat);
    let label = repo_control_label(selected.map(WorkspaceId::path));
    let enabled = draft_ready(snapshot, chat) && chat_machine(snapshot, chat).is_some();
    let mut rows: Vec<ui::DropdownRow> = repos
        .iter()
        .map(|repo| ui::DropdownRow {
            label: repo.path().to_string().into(),
            selected: selected == Some(*repo),
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

fn model_control_state(selection: &ModelSelection, models: &ModelsState) -> (SharedString, bool) {
    let choices_available = !models.choices().is_empty();

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

fn is_current_model(selection: &ModelSelection, pick: &Option<ModelChoice>) -> bool {
    match (pick, selection) {
        (None, ModelSelection::LaunchDefault) => true,
        (Some(choice), ModelSelection::Launch(current) | ModelSelection::Live(current)) => {
            choice.provider == current.provider && choice.id == current.id
        }
        _ => false,
    }
}

fn model_row_label(pick: Option<&ModelChoice>) -> SharedString {
    match pick {
        Some(choice) => choice.name.clone().into(),
        None => DEFAULT_MODEL_ROW.into(),
    }
}

fn machine_control_label(machine_label: Option<String>, candidates_empty: bool) -> SharedString {
    match machine_label {
        Some(label) => label.into(),
        None if candidates_empty => NO_MACHINE_LABEL.into(),
        None => CHOOSE_MACHINE_LABEL.into(),
    }
}

fn repo_control_label(workspace_path: Option<&str>) -> SharedString {
    match workspace_path {
        Some(path) => folder_name(path).to_string().into(),
        None => CHOOSE_REPO_LABEL.into(),
    }
}

fn draft_ready(snapshot: &ClientSnapshot, chat: &ChatId) -> bool {
    matches!(
        chat,
        ChatId::Draft(id)
            if snapshot
                .draft(*id)
                .is_some_and(|draft| matches!(draft.lifecycle, DraftLifecycle::Ready))
    )
}

pub(crate) fn can_send(snapshot: &ClientSnapshot, chat: &ChatId) -> bool {
    match chat {
        ChatId::Draft(id) => snapshot.draft(*id).is_some_and(|draft| {
            matches!(draft.lifecycle, DraftLifecycle::Ready) && draft.target.is_resolved()
        }),
        ChatId::Session(key) => snapshot
            .live_session(key)
            .is_some_and(|session| matches!(session.lifecycle, SessionLifecycle::Attached)),
    }
}

fn can_stop(snapshot: &ClientSnapshot, chat: &ChatId) -> bool {
    match chat {
        ChatId::Draft(_) => false,
        ChatId::Session(key) => snapshot
            .live_session(key)
            .is_some_and(harvester_client_core::LiveSession::can_stop),
    }
}

fn chat_machine<'a>(
    snapshot: &'a ClientSnapshot,
    chat: &'a ChatId,
) -> Option<&'a harvester_client_core::MachineId> {
    match chat {
        ChatId::Draft(id) => snapshot.draft(*id)?.target.machine.as_ref(),
        ChatId::Session(key) => Some(key.machine()),
    }
}

fn chat_workspace<'a>(snapshot: &'a ClientSnapshot, chat: &ChatId) -> Option<&'a WorkspaceId> {
    match chat {
        ChatId::Draft(id) => snapshot.draft(*id)?.target.workspace.as_ref(),
        ChatId::Session(key) => Some(&snapshot.live_session(key)?.workspace),
    }
}

fn chat_models<'a>(snapshot: &'a ClientSnapshot, chat: &ChatId) -> Option<&'a ModelsState> {
    match chat {
        ChatId::Draft(id) => Some(&snapshot.draft(*id)?.models),
        ChatId::Session(key) => Some(&snapshot.live_session(key)?.model_catalog),
    }
}

fn chat_selection<'a>(snapshot: &'a ClientSnapshot, chat: &ChatId) -> Option<&'a ModelSelection> {
    match chat {
        ChatId::Draft(id) => Some(&snapshot.draft(*id)?.model_selection),
        ChatId::Session(key) => Some(&snapshot.live_session(key)?.model_selection),
    }
}

fn chat_branch<'a>(snapshot: &'a ClientSnapshot, chat: &ChatId) -> Option<&'a str> {
    match chat {
        ChatId::Draft(id) => snapshot.draft(*id)?.branch.as_deref(),
        ChatId::Session(key) => snapshot.live_session(key)?.branch.as_deref(),
    }
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

    #[test]
    fn a_loaded_list_makes_the_default_row_pickable() {
        let models = ModelsState::Loaded(vec![choice("Sonnet")]);
        let (label, enabled) = model_control_state(&ModelSelection::LaunchDefault, &models);

        assert_eq!(label, "Default model");
        assert!(enabled);
    }

    #[test]
    fn a_failed_list_keeps_the_default_label_but_offers_nothing() {
        let models = ModelsState::Failed("no gateway".to_string());
        let (label, enabled) = model_control_state(&ModelSelection::LaunchDefault, &models);

        assert_eq!(label, "Default model");
        assert!(!enabled);
    }

    #[test]
    fn the_repo_control_names_the_folder_not_the_path() {
        assert_eq!(repo_control_label(None), CHOOSE_REPO_LABEL);
        assert_eq!(repo_control_label(Some("/home/exedev/bloggy")), "bloggy");
    }
}
