use std::time::Duration;

use gpui::{App, AppContext, Entity, ScrollHandle, Window};
use gpui_component::input::InputState;
use harvester::ui::dialog::{create_multi_line_input, create_single_line_input};
use harvester::ui::TranscriptPresentation;
use harvester_client_core::transcript::Transcript;
use serde_json::Value;

use crate::fixtures::{self, FixtureLibrary, TranscriptSource};

pub const REPLAY_SPEED_LABELS: &[&str] = &["0.5x", "1x", "4x"];
pub const REPLAY_SPEED_MULTIPLIERS: &[f32] = &[0.5, 1., 4.];
pub const REPLAY_FRAME_INTERVAL_MS: u64 = 260;
pub const REPLAY_IDLE_POLL_INTERVAL_MS: u64 = 200;

const PROMPT_INPUT_PLACEHOLDER: &str = "Message pi — Enter to send";
const PROMPT_INPUT_ROWS: (usize, usize) = (1, 8);
const DIALOG_SINGLE_LINE_PLACEHOLDER: &str = "Answer the agent";
const DIALOG_MULTI_LINE_PREFILL: &str = "The agent prefilled this editor\nwith two lines";

pub fn replay_frame_interval(speed_index: usize) -> Duration {
    let multiplier = REPLAY_SPEED_MULTIPLIERS
        .get(speed_index)
        .copied()
        .unwrap_or(1.);

    Duration::from_millis((REPLAY_FRAME_INTERVAL_MS as f32 / multiplier) as u64)
}

pub fn replay_idle_poll_interval() -> Duration {
    Duration::from_millis(REPLAY_IDLE_POLL_INTERVAL_MS)
}

pub struct RenderedTranscript {
    pub presentation: TranscriptPresentation,
    pub scroll: ScrollHandle,
    source_index: Option<usize>,
    transcript: Result<Transcript, String>,
}

impl RenderedTranscript {
    pub fn transcript(&self) -> Result<&Transcript, String> {
        match &self.transcript {
            Ok(transcript) => Ok(transcript),
            Err(failure) => Err(failure.clone()),
        }
    }
}

pub struct ReplayTranscript {
    pub presentation: TranscriptPresentation,
    pub scroll: ScrollHandle,
    events: Result<Vec<Value>, String>,
    cursor: usize,
    transcript: Transcript,
}

impl ReplayTranscript {
    fn new(events: Result<Vec<Value>, String>) -> Self {
        Self {
            presentation: TranscriptPresentation::default(),
            scroll: ScrollHandle::new(),
            events,
            cursor: 0,
            transcript: Transcript::new(&[]),
        }
    }

    pub fn transcript(&self) -> Result<&Transcript, String> {
        match &self.events {
            Ok(_) => Ok(&self.transcript),
            Err(failure) => Err(failure.clone()),
        }
    }

    pub fn progress(&self) -> (usize, usize) {
        let total = match &self.events {
            Ok(events) => events.len(),
            Err(_) => 0,
        };

        (self.cursor.min(total), total)
    }

    pub fn finished(&self) -> bool {
        let (applied, total) = self.progress();

        applied >= total
    }

    pub fn restart(&mut self) {
        self.cursor = 0;
        self.transcript = Transcript::new(&[]);
        self.presentation = TranscriptPresentation::default();
    }

    pub fn advance(&mut self) -> bool {
        let Ok(events) = &self.events else {
            return false;
        };
        let Some(event) = events.get(self.cursor) else {
            return false;
        };

        fixtures::apply_event(&mut self.transcript, event);
        self.cursor += 1;

        true
    }
}

pub struct StoryRuntime {
    pub section_expanded: bool,
    pub prompt_input: Entity<InputState>,
    pub dialog_single_line_input: Entity<InputState>,
    pub dialog_multi_line_input: Entity<InputState>,
    pub rendered: RenderedTranscript,
    pub replay: ReplayTranscript,
    fixtures: FixtureLibrary,
}

impl StoryRuntime {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let fixtures = FixtureLibrary::load();
        let replay_events = fixtures.streaming_events().map(<[Value]>::to_vec);
        let prompt_input = create_prompt_input(window, cx);

        Self {
            section_expanded: true,
            prompt_input,
            dialog_single_line_input: create_single_line_input(
                DIALOG_SINGLE_LINE_PLACEHOLDER.to_string(),
                window,
                cx,
            ),
            dialog_multi_line_input: create_multi_line_input(
                DIALOG_MULTI_LINE_PREFILL.to_string(),
                window,
                cx,
            ),
            rendered: RenderedTranscript {
                presentation: TranscriptPresentation::default(),
                scroll: ScrollHandle::new(),
                source_index: None,
                transcript: Err(String::new()),
            },
            replay: ReplayTranscript::new(replay_events),
            fixtures,
        }
    }

    pub fn fixture_failure(&self) -> Option<String> {
        self.fixtures.failure_summary()
    }

    pub fn ensure_rendered_source(&mut self, source_index: usize) {
        if self.rendered.source_index == Some(source_index) {
            return;
        }

        self.rendered.source_index = Some(source_index);
        self.rendered.presentation = TranscriptPresentation::default();
        self.rendered.transcript = self.fixtures.transcript(TranscriptSource::at(source_index));
    }
}

fn create_prompt_input(window: &mut Window, cx: &mut App) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(PROMPT_INPUT_PLACEHOLDER)
            .multi_line(true)
            .auto_grow(PROMPT_INPUT_ROWS.0, PROMPT_INPUT_ROWS.1)
            .submit_on_enter(true)
    })
}
