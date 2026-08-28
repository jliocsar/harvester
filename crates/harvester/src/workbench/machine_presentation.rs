use harvester_client_core::MachineState;

use crate::ui::Tone;

pub(super) struct MachinePresentation {
    pub tone: Tone,
    pub pane_message: Option<String>,
    pub statusbar_link: Option<String>,
}

pub(super) fn present_machine(state: MachineState<'_>, machine: &str) -> MachinePresentation {
    match state {
        MachineState::Connecting => MachinePresentation {
            tone: Tone::Pending,
            pane_message: Some(format!("connecting to {machine}…")),
            statusbar_link: Some("connecting…".to_string()),
        },
        MachineState::Connected => MachinePresentation {
            tone: Tone::Live,
            pane_message: Some("select a session".to_string()),
            statusbar_link: Some(machine.to_string()),
        },
        MachineState::ConnectFailed { detail } => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some(detail.to_string()),
            statusbar_link: Some("connect failed".to_string()),
        },
        MachineState::ConnectionLost => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some("connection lost".to_string()),
            statusbar_link: Some("connection lost".to_string()),
        },
        MachineState::Missing => MachinePresentation {
            tone: Tone::Failed,
            pane_message: Some(format!("{machine} is no longer listed by its provider")),
            statusbar_link: Some("no longer listed".to_string()),
        },
        MachineState::Unreachable => MachinePresentation {
            tone: Tone::Failed,
            pane_message: None,
            statusbar_link: None,
        },
        MachineState::Reachable => MachinePresentation {
            tone: Tone::Muted,
            pane_message: None,
            statusbar_link: None,
        },
        MachineState::Unknown => MachinePresentation {
            tone: Tone::Idle,
            pane_message: None,
            statusbar_link: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Dot;

    #[test]
    fn unknown_is_an_absence_not_a_reachability_verdict() {
        let presentation = present_machine(MachineState::Unknown, "vm");

        assert_eq!(presentation.tone, Tone::Idle);
        assert_eq!(presentation.tone.dot(), Dot::Hollow(Tone::Idle));
        assert_eq!(presentation.pane_message, None);
        assert_eq!(presentation.statusbar_link, None);
    }

    #[test]
    fn missing_is_failed_and_names_the_provider_loss() {
        let presentation = present_machine(MachineState::Missing, "provider/vm");

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
        let presentation = present_machine(MachineState::Connected, "vm");

        assert_eq!(presentation.tone, Tone::Live);
        assert_eq!(
            presentation.pane_message.as_deref(),
            Some("select a session")
        );
        assert_eq!(presentation.statusbar_link.as_deref(), Some("vm"));
    }

    #[test]
    fn a_reachable_machine_is_a_verdict_and_an_unprobed_one_is_not() {
        let reachable = present_machine(MachineState::Reachable, "vm");
        let unknown = present_machine(MachineState::Unknown, "vm");

        assert_eq!(reachable.tone.dot(), Dot::Solid(Tone::Muted));
        assert_eq!(unknown.tone.dot(), Dot::Hollow(Tone::Idle));
        assert_ne!(reachable.tone.dot(), unknown.tone.dot());
    }
}
