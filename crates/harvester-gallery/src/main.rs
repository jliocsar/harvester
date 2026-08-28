mod chrome;
mod fixtures;
mod registry;
mod runtime;
mod stories;
mod story;

use std::collections::HashMap;
use std::time::Duration;

use gpui::{prelude::*, App, AppContext, Context, Entity, Subscription, Window};
use gpui_component::input::InputState;
use harvester::shell_bootstrap::{run_desktop_shell, ShellWindow};

use crate::runtime::{replay_frame_interval, replay_idle_poll_interval, StoryRuntime};
use crate::stories::transcript::{PLAYING_KNOB, SPEED_KNOB, STREAMING_TRANSCRIPT_STORY};
use crate::story::{KnobKind, KnobValue, KnobValues, Story};

const GALLERY_WINDOW_TITLE: &str = "Harvester Gallery";
const GALLERY_WINDOW_WIDTH: f32 = 1100.;
const GALLERY_WINDOW_HEIGHT: f32 = 720.;
const FIRST_STORY_INDEX: usize = 0;

type TextKnobKey = (usize, &'static str);

fn main() {
    let shell_window = ShellWindow {
        title: GALLERY_WINDOW_TITLE.to_string(),
        width_in_pixels: GALLERY_WINDOW_WIDTH,
        height_in_pixels: GALLERY_WINDOW_HEIGHT,
    };

    run_desktop_shell(shell_window, |window, cx: &mut App| {
        gpui_mcp_bridge::install(cx);

        cx.new(|cx| Gallery::new(window, cx)).into()
    });
}

pub struct Gallery {
    stories: Vec<Story>,
    knobs: Vec<KnobValues>,
    groups: Vec<&'static str>,
    collapsed_groups: Vec<&'static str>,
    selected_story: usize,
    dock_expanded: bool,
    runtime: Entity<StoryRuntime>,
    text_inputs: HashMap<TextKnobKey, Entity<InputState>>,
    replay_story: Option<usize>,
    _subscriptions: Vec<Subscription>,
}

impl Gallery {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let stories = registry::build_stories();
        let knobs: Vec<KnobValues> = stories
            .iter()
            .map(|story| KnobValues::from_specs(&story.knobs))
            .collect();
        let groups = registry::group_names(&stories);
        let replay_story = registry::story_index(&stories, STREAMING_TRANSCRIPT_STORY);
        let runtime = cx.new(|cx| StoryRuntime::new(window, cx));
        let mut subscriptions = vec![cx.observe(&runtime, |_, _, cx| cx.notify())];
        let text_inputs =
            Self::create_text_knob_inputs(&stories, &knobs, &mut subscriptions, window, cx);

        Self::schedule_replay(cx);

        Self {
            stories,
            knobs,
            groups,
            collapsed_groups: Vec::new(),
            selected_story: FIRST_STORY_INDEX,
            dock_expanded: true,
            runtime,
            text_inputs,
            replay_story,
            _subscriptions: subscriptions,
        }
    }

    fn create_text_knob_inputs(
        stories: &[Story],
        knobs: &[KnobValues],
        subscriptions: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<TextKnobKey, Entity<InputState>> {
        let mut inputs = HashMap::new();

        for (story_index, story) in stories.iter().enumerate() {
            let text_knob_names = story
                .knobs
                .iter()
                .filter(|spec| matches!(spec.kind, KnobKind::Text))
                .map(|spec| spec.name);

            for name in text_knob_names {
                let value = knobs[story_index].text(name).to_string();
                let input = cx.new(|cx| InputState::new(window, cx).default_value(value));

                subscriptions.push(cx.observe(&input, |_, _, cx| cx.notify()));
                inputs.insert((story_index, name), input);
            }
        }

        inputs
    }

    pub(crate) fn stories(&self) -> &[Story] {
        &self.stories
    }

    pub(crate) fn groups(&self) -> &[&'static str] {
        &self.groups
    }

    pub(crate) fn selected_story(&self) -> usize {
        self.selected_story
    }

    pub(crate) fn dock_expanded(&self) -> bool {
        self.dock_expanded
    }

    pub(crate) fn runtime(&self) -> &Entity<StoryRuntime> {
        &self.runtime
    }

    pub(crate) fn group_expanded(&self, group: &'static str) -> bool {
        !self.collapsed_groups.contains(&group)
    }

    pub(crate) fn text_knob_input(&self, name: &'static str) -> Option<&Entity<InputState>> {
        self.text_inputs.get(&(self.selected_story, name))
    }

    pub(crate) fn select_story(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.selected_story == index {
            return;
        }

        self.selected_story = index;
        cx.notify();
    }

    pub(crate) fn toggle_group(&mut self, group: &'static str, cx: &mut Context<Self>) {
        match self.collapsed_groups.iter().position(|name| *name == group) {
            Some(position) => {
                self.collapsed_groups.remove(position);
            }
            None => self.collapsed_groups.push(group),
        }

        cx.notify();
    }

    pub(crate) fn toggle_dock(&mut self, cx: &mut Context<Self>) {
        self.dock_expanded = !self.dock_expanded;
        cx.notify();
    }

    pub(crate) fn set_knob(
        &mut self,
        name: &'static str,
        value: KnobValue,
        cx: &mut Context<Self>,
    ) {
        let story_index = self.selected_story;

        self.knobs[story_index].set(name, value);
        cx.notify();
    }

    pub(crate) fn apply_preset(
        &mut self,
        preset_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let story_index = self.selected_story;
        let Some(preset) = self.stories[story_index].presets.get(preset_index) else {
            return;
        };
        let values = preset.values.clone();

        for (name, value) in values {
            self.apply_preset_value(story_index, name, value, window, cx);
        }

        cx.notify();
    }

    pub(crate) fn selected_knob_values(&self, cx: &App) -> KnobValues {
        let story_index = self.selected_story;
        let mut values = self.knobs[story_index].clone();

        for spec in self.stories[story_index].knobs.iter() {
            if !matches!(spec.kind, KnobKind::Text) {
                continue;
            }

            let Some(input) = self.text_inputs.get(&(story_index, spec.name)) else {
                continue;
            };

            values.set(
                spec.name,
                KnobValue::Text(input.read(cx).value().to_string()),
            );
        }

        values
    }

    fn apply_preset_value(
        &mut self,
        story_index: usize,
        name: &'static str,
        value: KnobValue,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let KnobValue::Text(text) = &value {
            let input = self.text_inputs.get(&(story_index, name)).cloned();

            if let Some(input) = input {
                input.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
            }
        }

        self.knobs[story_index].set(name, value);
    }

    fn schedule_replay(cx: &mut Context<Self>) {
        cx.spawn(async move |gallery, cx| loop {
            let Ok(delay) = gallery.update(cx, |gallery, cx| gallery.replay_tick_delay(cx)) else {
                return;
            };

            cx.background_executor().timer(delay).await;

            if gallery
                .update(cx, |gallery, cx| gallery.advance_replay(cx))
                .is_err()
            {
                return;
            }
        })
        .detach();
    }

    fn replay_tick_delay(&self, cx: &App) -> Duration {
        if !self.replay_is_playing(cx) {
            return replay_idle_poll_interval();
        }

        replay_frame_interval(self.replay_speed_index())
    }

    fn advance_replay(&mut self, cx: &mut Context<Self>) {
        if !self.replay_is_playing(cx) {
            return;
        }

        self.runtime.update(cx, |runtime, cx| {
            if runtime.replay.advance() {
                cx.notify();
            }
        });
    }

    fn replay_is_playing(&self, cx: &App) -> bool {
        let Some(story_index) = self.replay_story else {
            return false;
        };

        if self.selected_story != story_index {
            return false;
        }

        if !self.knobs[story_index].flag(PLAYING_KNOB) {
            return false;
        }

        !self.runtime.read(cx).replay.finished()
    }

    fn replay_speed_index(&self) -> usize {
        let Some(story_index) = self.replay_story else {
            return 0;
        };

        self.knobs[story_index].choice(SPEED_KNOB)
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        chrome::render_gallery(self, window, cx)
    }
}
