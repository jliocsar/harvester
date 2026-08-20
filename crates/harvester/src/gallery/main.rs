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
use crate::stories::transcript::PLAYING_KNOB;
use crate::story::{KnobKind, KnobValue, KnobValues, Story};

const GALLERY_WINDOW_TITLE: &str = "Harvester Gallery";
const GALLERY_WINDOW_WIDTH: f32 = 1100.;
const GALLERY_WINDOW_HEIGHT: f32 = 720.;

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
    subscriptions: Vec<Subscription>,
}

impl Gallery {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let stories = registry::build_stories();
        let knobs = stories
            .iter()
            .map(|story| KnobValues::from_specs(&story.knobs))
            .collect();
        let groups = registry::group_names(&stories);
        let replay_story = registry::story_index(&stories, registry::STREAMING_TRANSCRIPT_STORY);
        let runtime = cx.new(|cx| StoryRuntime::new(window, cx));
        let runtime_subscription = cx.observe(&runtime, |_, _, cx| cx.notify());

        Self::schedule_replay(cx);

        Self {
            stories,
            knobs,
            groups,
            collapsed_groups: Vec::new(),
            selected_story: 0,
            dock_expanded: true,
            runtime,
            text_inputs: HashMap::new(),
            replay_story,
            subscriptions: vec![runtime_subscription],
        }
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

    fn sync_text_knob_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let story_index = self.selected_story;
        let missing: Vec<(&'static str, String)> = self.stories[story_index]
            .knobs
            .iter()
            .filter(|spec| matches!(spec.kind, KnobKind::Text))
            .filter(|spec| !self.text_inputs.contains_key(&(story_index, spec.name)))
            .map(|spec| {
                (
                    spec.name,
                    self.knobs[story_index].text(spec.name).to_string(),
                )
            })
            .collect();

        for (name, value) in missing {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
            let subscription = cx.observe(&input, |_, _, cx| cx.notify());

            self.text_inputs.insert((story_index, name), input);
            self.subscriptions.push(subscription);
        }
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

        self.knobs[story_index].choice(crate::stories::transcript::SPEED_KNOB)
    }
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_text_knob_inputs(window, cx);

        chrome::render_gallery(self, window, cx)
    }
}
