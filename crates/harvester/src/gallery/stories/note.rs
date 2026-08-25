use gpui::{prelude::*, AnyElement, App, Window};
use harvester::ui::{empty_pane_action, empty_pane_hint, inline_note};

use super::{caption, panel, story_column, tone_at};
use crate::story::StoryScene;

pub const TONE_KNOB: &str = "tone";
pub const TEXT_KNOB: &str = "text";

const HINT_ACTION_ID: &str = "story-empty-pane-action";
const HINT_ACTION_LABEL: &str = "New session";
const HINT_ACTION_CAPTION: &str = "The trailing slot carries the one action the empty state names";

pub fn inline(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    story_column()
        .child(inline_note(tone, text, cx))
        .into_any_element()
}

pub fn empty_pane(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    story_column()
        .child(
            panel(cx).h_48().child(
                empty_pane_hint(tone, text, cx)
                    .child(empty_pane_action(HINT_ACTION_ID, HINT_ACTION_LABEL)),
            ),
        )
        .child(caption(HINT_ACTION_CAPTION, cx))
        .into_any_element()
}
