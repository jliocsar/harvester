use crate::fixtures::TRANSCRIPT_SOURCE_LABELS;
use crate::runtime::REPLAY_SPEED_LABELS;
use crate::stories::TONE_LABELS;
use crate::stories::{core, dialog, note, prompt, prompt_strip, shell, sidebar, transcript};
use crate::story::{KnobSpec, KnobValue, Preset, Story};

pub const CORE_GROUP: &str = "Core";
pub const NOTE_GROUP: &str = "Note";
pub const SHELL_GROUP: &str = "Shell";
pub const SIDEBAR_GROUP: &str = "Sidebar";
pub const PROMPT_GROUP: &str = "Prompt";
pub const FLAGSHIP_GROUP: &str = "Flagship";
pub const DIALOG_GROUP: &str = "Dialog";

pub const STREAMING_TRANSCRIPT_STORY: &str = "Transcript — streaming";

const LIVE_TONE_INDEX: usize = 4;
const FAILED_TONE_INDEX: usize = 5;
const DEFAULT_SPEED_INDEX: usize = 1;

pub fn build_stories() -> Vec<Story> {
    vec![
        Story::new(
            CORE_GROUP,
            "Tone swatches",
            Vec::new(),
            Vec::new(),
            core::tone_swatches,
        ),
        Story::new(
            CORE_GROUP,
            "Dot",
            vec![KnobSpec::choice(
                core::TONE_KNOB,
                TONE_LABELS,
                LIVE_TONE_INDEX,
            )],
            vec![
                Preset::new(
                    "running session",
                    vec![(core::TONE_KNOB, KnobValue::Choice(LIVE_TONE_INDEX))],
                ),
                Preset::new(
                    "unreachable machine",
                    vec![(core::TONE_KNOB, KnobValue::Choice(FAILED_TONE_INDEX))],
                ),
            ],
            core::dot,
        ),
        Story::new(
            NOTE_GROUP,
            "inline_note",
            vec![
                KnobSpec::choice(note::TONE_KNOB, TONE_LABELS, 0),
                KnobSpec::text(note::TEXT_KNOB, "Starting session…"),
            ],
            vec![
                Preset::new(
                    "progress",
                    vec![
                        (note::TONE_KNOB, KnobValue::Choice(0)),
                        (note::TEXT_KNOB, KnobValue::Text("Starting session…".into())),
                    ],
                ),
                Preset::new(
                    "failure",
                    vec![
                        (note::TONE_KNOB, KnobValue::Choice(FAILED_TONE_INDEX)),
                        (
                            note::TEXT_KNOB,
                            KnobValue::Text("Session exited (code 137)".into()),
                        ),
                    ],
                ),
            ],
            note::inline,
        ),
        Story::new(
            NOTE_GROUP,
            "empty_pane_hint",
            vec![
                KnobSpec::choice(note::TONE_KNOB, TONE_LABELS, 0),
                KnobSpec::text(note::TEXT_KNOB, "No session selected"),
            ],
            Vec::new(),
            note::empty_pane,
        ),
        Story::new(
            SHELL_GROUP,
            "chat_header",
            vec![
                KnobSpec::text(shell::TITLE_KNOB, "harvester on exe-vm-7fa2"),
                KnobSpec::text(shell::SUBTITLE_KNOB, "backend-hardening · pi 0.83.0"),
            ],
            Vec::new(),
            shell::header,
        ),
        Story::new(
            SHELL_GROUP,
            "collapsible_section",
            vec![KnobSpec::choice(
                shell::SECTION_TITLE_KNOB,
                shell::SECTION_TITLES,
                0,
            )],
            Vec::new(),
            shell::section,
        ),
        Story::new(
            SHELL_GROUP,
            "statusbar_frame",
            vec![
                KnobSpec::text(shell::STATUS_TEXT_KNOB, "2 machines · 3 sessions"),
                KnobSpec::choice(shell::STATUS_TONE_KNOB, TONE_LABELS, LIVE_TONE_INDEX),
            ],
            Vec::new(),
            shell::statusbar,
        ),
        Story::new(
            SIDEBAR_GROUP,
            "sidebar_row",
            vec![
                KnobSpec::text(sidebar::LABEL_KNOB, "fix the dialog room"),
                KnobSpec::flag(sidebar::SELECTED_KNOB, false),
                KnobSpec::flag(sidebar::DISABLED_KNOB, false),
            ],
            vec![
                Preset::new(
                    "idle",
                    vec![
                        (sidebar::SELECTED_KNOB, KnobValue::Bool(false)),
                        (sidebar::DISABLED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
                Preset::new(
                    "selected",
                    vec![
                        (sidebar::SELECTED_KNOB, KnobValue::Bool(true)),
                        (sidebar::DISABLED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
                Preset::new(
                    "disabled",
                    vec![
                        (sidebar::SELECTED_KNOB, KnobValue::Bool(false)),
                        (sidebar::DISABLED_KNOB, KnobValue::Bool(true)),
                    ],
                ),
            ],
            sidebar::row,
        ),
        Story::new(
            SIDEBAR_GROUP,
            "sidebar_stacked_row",
            vec![
                KnobSpec::choice(sidebar::TONE_KNOB, TONE_LABELS, LIVE_TONE_INDEX),
                KnobSpec::text(sidebar::TITLE_KNOB, "fix the dialog room"),
                KnobSpec::text(sidebar::DETAIL_KNOB, "harvester · exe-vm-7fa2"),
                KnobSpec::text(sidebar::NOTE_KNOB, "opening…"),
                KnobSpec::flag(sidebar::SELECTED_KNOB, false),
            ],
            vec![
                Preset::new(
                    "running session",
                    vec![
                        (sidebar::TONE_KNOB, KnobValue::Choice(LIVE_TONE_INDEX)),
                        (sidebar::NOTE_KNOB, KnobValue::Text(String::new())),
                        (sidebar::SELECTED_KNOB, KnobValue::Bool(true)),
                    ],
                ),
                Preset::new(
                    "opening session",
                    vec![
                        (sidebar::TONE_KNOB, KnobValue::Choice(1)),
                        (sidebar::NOTE_KNOB, KnobValue::Text("opening…".into())),
                        (sidebar::SELECTED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
            ],
            sidebar::stacked_row,
        ),
        Story::new(
            SIDEBAR_GROUP,
            "sidebar_empty_note",
            vec![KnobSpec::choice(
                sidebar::EMPTY_TEXT_KNOB,
                sidebar::EMPTY_NOTE_TEXTS,
                0,
            )],
            Vec::new(),
            sidebar::empty_note,
        ),
        Story::new(
            PROMPT_GROUP,
            "prompt_box_control",
            vec![
                KnobSpec::text(prompt::LABEL_KNOB, "claude-sonnet-4-6"),
                KnobSpec::flag(prompt::ENABLED_KNOB, true),
            ],
            vec![
                Preset::new(
                    "enabled",
                    vec![
                        (
                            prompt::LABEL_KNOB,
                            KnobValue::Text("claude-sonnet-4-6".into()),
                        ),
                        (prompt::ENABLED_KNOB, KnobValue::Bool(true)),
                    ],
                ),
                Preset::new(
                    "disabled",
                    vec![
                        (
                            prompt::LABEL_KNOB,
                            KnobValue::Text("Loading models…".into()),
                        ),
                        (prompt::ENABLED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
            ],
            prompt::control,
        ),
        Story::new(
            PROMPT_GROUP,
            "prompt_box_send",
            vec![KnobSpec::flag(prompt::ENABLED_KNOB, true)],
            Vec::new(),
            prompt::send,
        ),
        Story::new(
            PROMPT_GROUP,
            "prompt_box_dropdown",
            Vec::new(),
            Vec::new(),
            prompt::dropdown,
        ),
        Story::new(
            FLAGSHIP_GROUP,
            "Prompt strip",
            prompt_strip_knobs(),
            prompt_strip::presets(),
            prompt_strip::strip,
        ),
        Story::new(
            FLAGSHIP_GROUP,
            "Transcript — rendered",
            vec![KnobSpec::choice(
                transcript::SOURCE_KNOB,
                TRANSCRIPT_SOURCE_LABELS,
                0,
            )],
            transcript_source_presets(),
            transcript::rendered,
        ),
        Story::new(
            FLAGSHIP_GROUP,
            STREAMING_TRANSCRIPT_STORY,
            vec![
                KnobSpec::flag(transcript::PLAYING_KNOB, true),
                KnobSpec::choice(
                    transcript::SPEED_KNOB,
                    REPLAY_SPEED_LABELS,
                    DEFAULT_SPEED_INDEX,
                ),
            ],
            vec![
                Preset::new(
                    "play",
                    vec![
                        (transcript::PLAYING_KNOB, KnobValue::Bool(true)),
                        (
                            transcript::SPEED_KNOB,
                            KnobValue::Choice(DEFAULT_SPEED_INDEX),
                        ),
                    ],
                ),
                Preset::new(
                    "pause",
                    vec![(transcript::PLAYING_KNOB, KnobValue::Bool(false))],
                ),
                Preset::new(
                    "fast forward",
                    vec![
                        (transcript::PLAYING_KNOB, KnobValue::Bool(true)),
                        (transcript::SPEED_KNOB, KnobValue::Choice(2)),
                    ],
                ),
            ],
            transcript::streaming,
        ),
        Story::new(
            DIALOG_GROUP,
            "Dialog chrome",
            dialog_knobs(),
            dialog_presets(),
            dialog::chrome,
        ),
    ]
}

pub fn story_index(stories: &[Story], name: &str) -> Option<usize> {
    stories.iter().position(|story| story.name == name)
}

pub fn group_names(stories: &[Story]) -> Vec<&'static str> {
    let mut groups: Vec<&'static str> = Vec::new();

    for story in stories {
        if !groups.contains(&story.group) {
            groups.push(story.group);
        }
    }

    groups
}

fn prompt_strip_knobs() -> Vec<KnobSpec> {
    vec![
        KnobSpec::choice(prompt_strip::NOTICE_KNOB, prompt_strip::NOTICE_LABELS, 0),
        KnobSpec::text(
            prompt_strip::EXIT_NOTICE_KNOB,
            prompt_strip::READY_EXIT_NOTICE,
        ),
        KnobSpec::text(prompt_strip::MODEL_KNOB, prompt_strip::READY_MODEL),
        KnobSpec::flag(prompt_strip::MODEL_ENABLED_KNOB, true),
        KnobSpec::text(prompt_strip::MACHINE_KNOB, prompt_strip::READY_MACHINE),
        KnobSpec::flag(prompt_strip::MACHINE_ENABLED_KNOB, true),
        KnobSpec::text(prompt_strip::REPO_KNOB, prompt_strip::READY_REPO),
        KnobSpec::flag(prompt_strip::REPO_ENABLED_KNOB, true),
        KnobSpec::text(prompt_strip::BRANCH_KNOB, prompt_strip::READY_BRANCH),
        KnobSpec::flag(prompt_strip::CAN_SEND_KNOB, true),
        KnobSpec::flag(prompt_strip::FOCUSED_KNOB, true),
    ]
}

fn transcript_source_presets() -> Vec<Preset> {
    TRANSCRIPT_SOURCE_LABELS
        .iter()
        .enumerate()
        .map(|(index, label)| {
            Preset::new(
                label,
                vec![(transcript::SOURCE_KNOB, KnobValue::Choice(index))],
            )
        })
        .collect()
}

fn dialog_knobs() -> Vec<KnobSpec> {
    vec![
        KnobSpec::choice(dialog::ACCENT_KNOB, dialog::ACCENT_LABELS, 0),
        KnobSpec::text(dialog::TITLE_KNOB, "Start a session on exe-vm-7fa2?"),
        KnobSpec::text(
            dialog::MESSAGE_KNOB,
            "pi will run in ~/harvester on the branch backend-hardening.",
        ),
        KnobSpec::text(dialog::CANCEL_LABEL_KNOB, "Cancel"),
        KnobSpec::text(dialog::ACTION_LABEL_KNOB, "Start"),
    ]
}

fn dialog_presets() -> Vec<Preset> {
    vec![
        Preset::new(
            "confirm",
            vec![
                (dialog::ACCENT_KNOB, KnobValue::Choice(0)),
                (
                    dialog::TITLE_KNOB,
                    KnobValue::Text("Start a session on exe-vm-7fa2?".into()),
                ),
                (
                    dialog::MESSAGE_KNOB,
                    KnobValue::Text(
                        "pi will run in ~/harvester on the branch backend-hardening.".into(),
                    ),
                ),
                (dialog::ACTION_LABEL_KNOB, KnobValue::Text("Start".into())),
            ],
        ),
        Preset::new(
            "destructive",
            vec![
                (dialog::ACCENT_KNOB, KnobValue::Choice(1)),
                (
                    dialog::TITLE_KNOB,
                    KnobValue::Text("Forget exe-vm-0c19?".into()),
                ),
                (
                    dialog::MESSAGE_KNOB,
                    KnobValue::Text(
                        "The machine is no longer listed by its provider. Sessions on its disk are untouched.".into(),
                    ),
                ),
                (dialog::ACTION_LABEL_KNOB, KnobValue::Text("Forget".into())),
            ],
        ),
        Preset::new(
            "untitled",
            vec![(dialog::TITLE_KNOB, KnobValue::Text(String::new()))],
        ),
    ]
}
