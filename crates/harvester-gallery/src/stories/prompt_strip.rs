use gpui::{prelude::*, AnyElement, App, Window};
use harvester::ui::{
    prompt_strip, DropdownRow, PromptStrip, PromptStripControl, PromptStripNotice, ADD_REPO_ROW,
    DEFAULT_MODEL_ROW,
};

use super::{caption, panel, story_column, FLAGSHIP_GROUP};
use crate::story::{KnobSpec, KnobValue, KnobValues, Preset, Story, StoryScene};

const NOTICE_KNOB: &str = "notice";
const EXIT_NOTICE_KNOB: &str = "exit notice";
const MODEL_KNOB: &str = "model";
const MODEL_ENABLED_KNOB: &str = "model enabled";
const MACHINE_KNOB: &str = "machine";
const MACHINE_ENABLED_KNOB: &str = "machine enabled";
const REPO_KNOB: &str = "repo";
const REPO_ENABLED_KNOB: &str = "repo enabled";
const BRANCH_KNOB: &str = "branch";
const CAN_SEND_KNOB: &str = "can send";
const CAN_STOP_KNOB: &str = "turn running";
const FOCUSED_KNOB: &str = "focused";

const PROMPT_STRIP_STORY: &str = "Prompt strip";
const NOTICE_LABELS: &[&str] = &["none", "starting session", "session exited"];

const READY_MODEL: &str = "claude-sonnet-4-6";
const READY_MACHINE: &str = "exe-vm-7fa2";
const READY_REPO: &str = "harvester";
const READY_BRANCH: &str = "backend-hardening";
const READY_EXIT_NOTICE: &str = "Session exited (code 137) — start a new one";
const LOADING_MODEL_LABEL: &str = "Loading models…";
const FAILED_MODEL_LABEL: &str = "Models unavailable";
const NO_MACHINE_LABEL: &str = "No machines";
const NO_REPO_LABEL: &str = "Pick a machine first";

const NOTICE_NONE: usize = 0;
const NOTICE_STARTING: usize = 1;
const NOTICE_EXITED: usize = 2;
const MODEL_ROW_LABELS: &[&str] = &[
    DEFAULT_MODEL_ROW,
    READY_MODEL,
    "claude-opus-4-1",
    "gpt-5-codex",
];
const MACHINE_ROW_LABELS: &[&str] = &["local", READY_MACHINE, "exe-vm-0c19"];
const REPO_ROW_LABELS: &[&str] = &[READY_REPO, "taskline", ADD_REPO_ROW];
const REPO_DIVIDER_INDEX: usize = 2;
const STRIP_WIDTH: f32 = 680.;
const STRIP_CAPTION: &str = "The input is a real InputState — type in it";

pub fn stories() -> Vec<Story> {
    vec![Story {
        group: FLAGSHIP_GROUP,
        name: PROMPT_STRIP_STORY,
        knobs: knobs(),
        presets: presets(),
        render: strip,
    }]
}

fn knobs() -> Vec<KnobSpec> {
    vec![
        KnobSpec::choice(NOTICE_KNOB, NOTICE_LABELS, NOTICE_NONE),
        KnobSpec::text(EXIT_NOTICE_KNOB, READY_EXIT_NOTICE),
        KnobSpec::text(MODEL_KNOB, READY_MODEL),
        KnobSpec::flag(MODEL_ENABLED_KNOB, true),
        KnobSpec::text(MACHINE_KNOB, READY_MACHINE),
        KnobSpec::flag(MACHINE_ENABLED_KNOB, true),
        KnobSpec::text(REPO_KNOB, READY_REPO),
        KnobSpec::flag(REPO_ENABLED_KNOB, true),
        KnobSpec::text(BRANCH_KNOB, READY_BRANCH),
        KnobSpec::flag(CAN_SEND_KNOB, true),
        KnobSpec::flag(CAN_STOP_KNOB, false),
        KnobSpec::flag(FOCUSED_KNOB, true),
    ]
}

struct StripPreset {
    name: &'static str,
    notice_index: usize,
    model_label: &'static str,
    model_enabled: bool,
    machine_label: &'static str,
    machine_enabled: bool,
    repo_label: &'static str,
    repo_enabled: bool,
    branch: &'static str,
    can_send: bool,
    can_stop: bool,
}

impl StripPreset {
    fn into_preset(self) -> Preset {
        Preset::new(
            self.name,
            vec![
                (NOTICE_KNOB, KnobValue::Choice(self.notice_index)),
                (EXIT_NOTICE_KNOB, KnobValue::Text(READY_EXIT_NOTICE.into())),
                (MODEL_KNOB, KnobValue::Text(self.model_label.into())),
                (MODEL_ENABLED_KNOB, KnobValue::Bool(self.model_enabled)),
                (MACHINE_KNOB, KnobValue::Text(self.machine_label.into())),
                (MACHINE_ENABLED_KNOB, KnobValue::Bool(self.machine_enabled)),
                (REPO_KNOB, KnobValue::Text(self.repo_label.into())),
                (REPO_ENABLED_KNOB, KnobValue::Bool(self.repo_enabled)),
                (BRANCH_KNOB, KnobValue::Text(self.branch.into())),
                (CAN_SEND_KNOB, KnobValue::Bool(self.can_send)),
                (CAN_STOP_KNOB, KnobValue::Bool(self.can_stop)),
                (FOCUSED_KNOB, KnobValue::Bool(true)),
            ],
        )
    }
}

fn presets() -> Vec<Preset> {
    [
        StripPreset {
            name: "ready",
            notice_index: NOTICE_NONE,
            model_label: READY_MODEL,
            model_enabled: true,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: true,
            can_stop: false,
        },
        StripPreset {
            name: "turn running",
            notice_index: NOTICE_NONE,
            model_label: READY_MODEL,
            model_enabled: true,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: true,
            can_stop: true,
        },
        StripPreset {
            name: "loading models",
            notice_index: NOTICE_NONE,
            model_label: LOADING_MODEL_LABEL,
            model_enabled: false,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: false,
            can_stop: false,
        },
        StripPreset {
            name: "models failed",
            notice_index: NOTICE_NONE,
            model_label: FAILED_MODEL_LABEL,
            model_enabled: false,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: false,
            can_stop: false,
        },
        StripPreset {
            name: "no machines",
            notice_index: NOTICE_NONE,
            model_label: READY_MODEL,
            model_enabled: true,
            machine_label: NO_MACHINE_LABEL,
            machine_enabled: false,
            repo_label: NO_REPO_LABEL,
            repo_enabled: false,
            branch: "",
            can_send: false,
            can_stop: false,
        },
        StripPreset {
            name: "starting session",
            notice_index: NOTICE_STARTING,
            model_label: READY_MODEL,
            model_enabled: true,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: false,
            can_stop: false,
        },
        StripPreset {
            name: "exited",
            notice_index: NOTICE_EXITED,
            model_label: READY_MODEL,
            model_enabled: true,
            machine_label: READY_MACHINE,
            machine_enabled: true,
            repo_label: READY_REPO,
            repo_enabled: true,
            branch: READY_BRANCH,
            can_send: false,
            can_stop: false,
        },
    ]
    .into_iter()
    .map(StripPreset::into_preset)
    .collect()
}

fn strip(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let input = scene.runtime.read(cx).prompt_input.clone();
    let props = strip_props(scene.knobs);

    story_column()
        .w(gpui::px(STRIP_WIDTH))
        .child(
            panel(cx)
                .p_0()
                .child(prompt_strip(props, |_, _| {}, |_, _| {}, &input, cx)),
        )
        .child(caption(STRIP_CAPTION, cx))
        .into_any_element()
}

fn strip_props(knobs: &KnobValues) -> PromptStrip {
    PromptStrip {
        notice: notice(knobs),
        model: control(
            knobs.text(MODEL_KNOB),
            knobs.flag(MODEL_ENABLED_KNOB),
            MODEL_ROW_LABELS,
            None,
        ),
        machine: control(
            knobs.text(MACHINE_KNOB),
            knobs.flag(MACHINE_ENABLED_KNOB),
            MACHINE_ROW_LABELS,
            None,
        ),
        repo: control(
            knobs.text(REPO_KNOB),
            knobs.flag(REPO_ENABLED_KNOB),
            REPO_ROW_LABELS,
            Some(REPO_DIVIDER_INDEX),
        ),
        branch: knobs.optional_text(BRANCH_KNOB),
        can_send: knobs.flag(CAN_SEND_KNOB),
        can_stop: knobs.flag(CAN_STOP_KNOB),
        focused: knobs.flag(FOCUSED_KNOB),
    }
}

fn notice(knobs: &KnobValues) -> Option<PromptStripNotice> {
    match knobs.choice(NOTICE_KNOB) {
        NOTICE_STARTING => Some(PromptStripNotice::SessionStarting),
        NOTICE_EXITED => Some(PromptStripNotice::SessionExited(
            knobs.text(EXIT_NOTICE_KNOB).to_string().into(),
        )),
        _ => None,
    }
}

fn control(
    label: &str,
    enabled: bool,
    row_labels: &[&'static str],
    divider_index: Option<usize>,
) -> PromptStripControl {
    let rows = row_labels
        .iter()
        .enumerate()
        .map(|(index, row_label)| DropdownRow {
            label: (*row_label).into(),
            selected: *row_label == label,
            divider_above: divider_index == Some(index),
        })
        .collect();

    PromptStripControl {
        label: label.to_string().into(),
        enabled,
        rows,
    }
}
