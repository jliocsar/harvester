use gpui::{prelude::*, AnyElement, App, Window};
use harvester::ui::{
    prompt_strip, DropdownRow, PromptStrip, PromptStripControl, PromptStripNotice,
};

use super::{caption, panel, story_column};
use crate::story::{KnobValue, KnobValues, Preset, StoryScene};

pub const NOTICE_KNOB: &str = "notice";
pub const EXIT_NOTICE_KNOB: &str = "exit notice";
pub const MODEL_KNOB: &str = "model";
pub const MODEL_ENABLED_KNOB: &str = "model enabled";
pub const MACHINE_KNOB: &str = "machine";
pub const MACHINE_ENABLED_KNOB: &str = "machine enabled";
pub const REPO_KNOB: &str = "repo";
pub const REPO_ENABLED_KNOB: &str = "repo enabled";
pub const BRANCH_KNOB: &str = "branch";
pub const CAN_SEND_KNOB: &str = "can send";
pub const FOCUSED_KNOB: &str = "focused";

pub const NOTICE_LABELS: &[&str] = &["none", "starting session", "session exited"];

pub const READY_MODEL: &str = "claude-sonnet-4-6";
pub const READY_MACHINE: &str = "exe-vm-7fa2";
pub const READY_REPO: &str = "harvester";
pub const READY_BRANCH: &str = "backend-hardening";
pub const READY_EXIT_NOTICE: &str = "Session exited (code 137) — start a new one";

const NOTICE_NONE: usize = 0;
const NOTICE_STARTING: usize = 1;
const NOTICE_EXITED: usize = 2;
const MODEL_ROW_LABELS: &[&str] = &["claude-sonnet-4-6", "claude-opus-4-1", "gpt-5-codex"];
const MACHINE_ROW_LABELS: &[&str] = &["local", "exe-vm-7fa2", "exe-vm-0c19"];
const REPO_ROW_LABELS: &[&str] = &["harvester", "taskline", "Add a folder…"];
const REPO_DIVIDER_INDEX: usize = 2;
const STRIP_WIDTH: f32 = 680.;
const STRIP_CAPTION: &str = "The input is a real InputState — type in it";

pub fn strip(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let input = scene.runtime.read(cx).prompt_input.clone();
    let props = strip_props(scene.knobs);

    story_column()
        .w(gpui::px(STRIP_WIDTH))
        .child(
            panel(cx)
                .p_0()
                .child(prompt_strip(props, |_, _| {}, &input, cx)),
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

pub fn preset(
    name: &'static str,
    notice_index: usize,
    model: (&str, bool),
    machine: (&str, bool),
    repo: (&str, bool),
    branch: &str,
    can_send: bool,
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
            (FOCUSED_KNOB, KnobValue::Bool(true)),
        ],
    )
}

pub fn presets() -> Vec<Preset> {
    vec![
        preset(
            "ready",
            NOTICE_NONE,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            true,
        ),
        preset(
            "loading models",
            NOTICE_NONE,
            ("Loading models…", false),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
        ),
        preset(
            "models failed",
            NOTICE_NONE,
            ("Models unavailable", false),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
        ),
        preset(
            "no machines",
            NOTICE_NONE,
            (READY_MODEL, true),
            ("No machines", false),
            ("Pick a machine first", false),
            "",
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
        ),
        preset(
            "exited",
            NOTICE_EXITED,
            (READY_MODEL, true),
            (READY_MACHINE, true),
            (READY_REPO, true),
            READY_BRANCH,
            false,
        ),
    ]
}
