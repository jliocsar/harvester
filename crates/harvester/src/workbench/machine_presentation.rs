use harvester_client_core::MachineStatus;

use crate::ui::Tone;

pub(super) const MISSING_MACHINE_DETAIL: &str = "no longer listed";

const CONNECTING_PANE_MESSAGE_PREFIX: &str = "connecting to ";
const CONNECTING_STATUSBAR_LINK: &str = "connecting…";
const CONNECTED_PANE_MESSAGE: &str = "select a session";
const CONNECT_FAILED_STATUSBAR_LINK: &str = "connect failed";
const CONNECTION_LOST_MESSAGE: &str = "connection lost";
const MISSING_MACHINE_PANE_MESSAGE_SUFFIX: &str = " is no longer listed by its provider";

pub(super) struct MachinePresentation {
    pub tone: Tone,
    pub pane_message: Option<String>,
    pub statusbar_link: Option<String>,
}

pub(super) fn present_machine(state: &MachineStatus, machine: &str) -> MachinePresentation {
    match state {
        MachineStatus::Connecting => MachinePresentation {
            tone: Tone::Pending,
            pane_message: Some(format!("{CONNECTING_PANE_MESSAGE_PREFIX}{machine}…")),
            statusbar_link: Some(CONNECTING_STATUSBAR_LINK.to_string()),
        },
        MachineStatus::Connected => MachinePresentation {
            tone: Tone::Live,
            pane_message: Some(CONNECTED_PANE_MESSAGE.to_string()),
            statusbar_link: Some(machine.to_string()),
        },
        MachineStatus::ConnectFailed(detail) => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some(detail.clone()),
            statusbar_link: Some(CONNECT_FAILED_STATUSBAR_LINK.to_string()),
        },
        MachineStatus::ConnectionLost => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some(CONNECTION_LOST_MESSAGE.to_string()),
            statusbar_link: Some(CONNECTION_LOST_MESSAGE.to_string()),
        },
        MachineStatus::Missing => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some(format!("{machine}{MISSING_MACHINE_PANE_MESSAGE_SUFFIX}")),
            statusbar_link: Some(MISSING_MACHINE_DETAIL.to_string()),
        },
        MachineStatus::Unreachable => MachinePresentation {
            tone: Tone::Failed,
            pane_message: None,
            statusbar_link: None,
        },
        MachineStatus::Reachable => MachinePresentation {
            tone: Tone::Muted,
            pane_message: None,
            statusbar_link: None,
        },
        MachineStatus::Unknown => MachinePresentation {
            tone: Tone::Idle,
            pane_message: None,
            statusbar_link: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_an_absence_not_a_reachability_verdict() {
        let presentation = present_machine(&MachineStatus::Unknown, "vm");

        assert_eq!(presentation.tone, Tone::Idle);
        assert!(presentation.tone.is_hollow());
        assert_eq!(presentation.pane_message, None);
        assert_eq!(presentation.statusbar_link, None);
    }

    #[test]
    fn missing_is_failed_and_names_the_provider_loss() {
        let presentation = present_machine(&MachineStatus::Missing, "provider/vm");

        assert_eq!(presentation.tone, Tone::Failed);
        assert_eq!(
            presentation.pane_message.as_deref(),
            Some("provider/vm is no longer listed by its provider")
        );
        assert_eq!(
            presentation.statusbar_link.as_deref(),
            Some("no longer listed")
        );
    }

    #[test]
    fn a_live_bridge_is_named_by_its_machine_alone() {
        let presentation = present_machine(&MachineStatus::Connected, "vm");

        assert_eq!(presentation.tone, Tone::Live);
        assert_eq!(
            presentation.pane_message.as_deref(),
            Some("select a session")
        );
        assert_eq!(presentation.statusbar_link.as_deref(), Some("vm"));
    }

    #[test]
    fn a_reachable_machine_is_a_verdict_and_an_unprobed_one_is_not() {
        let reachable = present_machine(&MachineStatus::Reachable, "vm");
        let unknown = present_machine(&MachineStatus::Unknown, "vm");

        assert_eq!(reachable.tone, Tone::Muted);
        assert!(!reachable.tone.is_hollow());
        assert_eq!(unknown.tone, Tone::Idle);
        assert!(unknown.tone.is_hollow());
    }
}
