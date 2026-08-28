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
    vec![Story::new(
        FLAGSHIP_GROUP,
        PROMPT_STRIP_STORY,
        knobs(),
        presets(),
        strip,
    )]
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

fn presets() -> Vec<Preset> {
    vec![
        preset(
            "ready",
            NOTICE_NONE,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            true,
            false,
        ),
        preset(
            "turn running",
            NOTICE_NONE,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            true,
            true,
        ),
        preset(
            "loading models",
            NOTICE_NONE,
            (LOADING_MODEL_LABEL, false),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
            false,
        ),
        preset(
            "models failed",
            NOTICE_NONE,
            (FAILED_MODEL_LABEL, false),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
            false,
        ),
        preset(
            "no machines",
            NOTICE_NONE,
            (READY_MODEL, true),
            (NO_MACHINE_LABEL, false),
            (NO_REPO_LABEL, false),
            "",
            false,
            false,
        ),
        preset(
            "starting session",
            NOTICE_STARTING,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
            false,
        ),
        preset(
            "exited",
            NOTICE_EXITED,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
            false,
        ),
    ]
}

fn preset(
    name: &'static str,
    notice_index: usize,
    model: (&str, bool),
    machine: (&str, bool),
    repo: (&str, bool),
    branch: &str,
    can_send: bool,
    can_stop: bool,
) -> Preset {
    Preset::new(
        name,
        vec![
            (NOTICE_KNOB, KnobValue::Choice(notice_index)),
            (EXIT_NOTICE_KNOB, KnobValue::Text(READY_EXIT_NOTICE.into())),
            (MODEL_KNOB, KnobValue::Text(model.0.into())),
            (MODEL_ENABLED_KNOB, KnobValue::Bool(model.1)),
            (MACHINE_KNOB, KnobValue::Text(machine.0.into())),
            (MACHINE_ENABLED_KNOB, KnobValue::Bool(machine.1)),
            (REPO_KNOB, KnobValue::Text(repo.0.into())),
            (REPO_ENABLED_KNOB, KnobValue::Bool(repo.1)),
            (BRANCH_KNOB, KnobValue::Text(branch.into())),
            (CAN_SEND_KNOB, KnobValue::Bool(can_send)),
            (CAN_STOP_KNOB, KnobValue::Bool(can_stop)),
            (FOCUSED_KNOB, KnobValue::Bool(true)),
        ],
    )
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
