use gpui::{
    div, prelude::*, rems, AnyElement, Context, Div, Role, SharedString, Stateful, Styled, Window,
};
use gpui_component::button::{Button, ButtonGroup};
use gpui_component::input::Input;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, Selectable as _};
use harvester::ui::{
    chat_header, collapsible_section, inline_note, sidebar_row, statusbar_frame, KeyboardFocusRing,
    Tone,
};

use crate::stories::caption;
use crate::story::{KnobKind, KnobSpec, KnobValue, KnobValues, StoryScene};
use crate::Gallery;

const STAGE_MEASURE_IN_REMS: f32 = 64.;

const STORY_SIDEBAR_ID: &str = "story-sidebar";
const STAGE_CANVAS_ID: &str = "stage-canvas";
const DOCK_HEADER_ID: &str = "control-dock-header";
const KNOB_GRID_ID: &str = "knob-grid";
const STORY_SIDEBAR_LABEL: &str = "stories";
const STAGE_LABEL: &str = "stage";
const DOCK_TITLE: &str = "Controls";
const NO_KNOBS_NOTE: &str = "This story has no knobs";
const NO_PRESETS_NOTE: &str = "This story has no presets";

pub fn render_gallery(
    gallery: &mut Gallery,
    window: &mut Window,
    cx: &mut Context<Gallery>,
) -> Div {
    let knobs = gallery.selected_knob_values(cx);
    let story_sidebar = render_story_sidebar(gallery, cx);
    let stage = render_stage(gallery, &knobs, window, cx);
    let dock = render_control_dock(gallery, &knobs, cx);
    let statusbar = render_statusbar(gallery, cx);

    div()
        .size_full()
        .flex()
        .bg(cx.theme().background)
        .text_color(cx.theme().foreground)
        .text_size(cx.theme().font_size)
        .child(story_sidebar)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(stage)
                .child(dock)
                .child(statusbar),
        )
}

fn with_content_inset<E: Styled>(element: E) -> E {
    element.px_3()
}

fn with_dock_row_inset<E: Styled>(element: E) -> E {
    with_content_inset(element).py_2p5()
}

fn render_story_sidebar(gallery: &Gallery, cx: &mut Context<Gallery>) -> Stateful<Div> {
    let groups: Vec<Div> = gallery
        .groups()
        .iter()
        .enumerate()
        .map(|(index, group)| render_story_group(gallery, index, group, cx))
        .collect();

    div()
        .id(STORY_SIDEBAR_ID)
        .role(Role::List)
        .aria_label(STORY_SIDEBAR_LABEL)
        .w_64()
        .min_w_56()
        .max_w_80()
        .flex_grow_0()
        .flex_shrink_1()
        .h_full()
        .overflow_y_scroll()
        .bg(cx.theme().sidebar)
        .border_r_1()
        .border_color(cx.theme().sidebar_border)
        .flex()
        .flex_col()
        .children(groups)
}

fn render_story_group(
    gallery: &Gallery,
    group_index: usize,
    group: &'static str,
    cx: &mut Context<Gallery>,
) -> Div {
    let expanded = gallery.group_expanded(group);
    let header = collapsible_section(
        ("story-group", group_index),
        group,
        expanded,
        cx.listener(move |gallery, _, _, cx| gallery.toggle_group(group, cx)),
        cx,
    );
    let rows: Vec<_> = gallery
        .stories()
        .iter()
        .enumerate()
        .filter(|(_, story)| story.group == group)
        .map(|(index, story)| render_story_row(gallery, index, story.name, cx))
        .collect();

    div()
        .flex()
        .flex_col()
        .child(header)
        .when(expanded, |section| section.children(rows))
}

fn render_story_row(
    gallery: &Gallery,
    story_index: usize,
    name: &'static str,
    cx: &mut Context<Gallery>,
) -> impl IntoElement {
    let selected = gallery.selected_story() == story_index;

    sidebar_row(("story", story_index))
        .role(Role::ListItem)
        .aria_label(name)
        .aria_selected(selected)
        .selected(selected)
        .keyboard_focus_ring(cx)
        .child(div().overflow_hidden().text_ellipsis().child(name))
        .on_click(cx.listener(move |gallery, _, _, cx| gallery.select_story(story_index, cx)))
}

fn render_stage(
    gallery: &Gallery,
    knobs: &KnobValues,
    window: &mut Window,
    cx: &mut Context<Gallery>,
) -> Div {
    let story = &gallery.stories()[gallery.selected_story()];
    let scene = StoryScene {
        knobs,
        runtime: gallery.runtime(),
    };
    let header = chat_header(story.name.into(), story.group.into(), cx);
    let body = (story.render)(&scene, window, cx);

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(header)
        .child(render_stage_canvas(body))
}

fn render_stage_canvas(body: AnyElement) -> Stateful<Div> {
    with_content_inset(div().id(STAGE_CANVAS_ID))
        .role(Role::Group)
        .aria_label(STAGE_LABEL)
        .py_4()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .justify_center()
        .child(
            div()
                .w_full()
                .max_w(rems(STAGE_MEASURE_IN_REMS))
                .flex()
                .flex_col()
                .justify_center()
                .child(body),
        )
}

fn render_control_dock(gallery: &Gallery, knobs: &KnobValues, cx: &mut Context<Gallery>) -> Div {
    let expanded = gallery.dock_expanded();
    let header = collapsible_section(
        DOCK_HEADER_ID,
        DOCK_TITLE,
        expanded,
        cx.listener(|gallery, _, _, cx| gallery.toggle_dock(cx)),
        cx,
    );
    let presets = render_preset_buttons(gallery, cx);
    let knob_grid = render_knob_grid(gallery, knobs, cx);

    div()
        .flex_none()
        .border_t_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().sidebar)
        .flex()
        .flex_col()
        .child(header)
        .when(expanded, |dock| dock.child(presets).child(knob_grid))
}

fn render_preset_buttons(gallery: &Gallery, cx: &mut Context<Gallery>) -> Div {
    let presets = &gallery.stories()[gallery.selected_story()].presets;

    if presets.is_empty() {
        return with_dock_row_inset(div()).child(caption(NO_PRESETS_NOTE, cx));
    }

    with_dock_row_inset(div())
        .flex()
        .flex_wrap()
        .gap_2()
        .children(
            presets
                .iter()
                .enumerate()
                .map(|(index, preset)| render_preset_button(index, preset.name, cx)),
        )
}

fn render_preset_button(
    preset_index: usize,
    name: &'static str,
    cx: &mut Context<Gallery>,
) -> Button {
    Button::new(("preset", preset_index))
        .outline()
        .label(name)
        .on_click(
            cx.listener(move |gallery, _, window, cx| {
                gallery.apply_preset(preset_index, window, cx)
            }),
        )
}

fn render_knob_grid(
    gallery: &Gallery,
    knobs: &KnobValues,
    cx: &mut Context<Gallery>,
) -> AnyElement {
    let specs = &gallery.stories()[gallery.selected_story()].knobs;

    if specs.is_empty() {
        return with_dock_row_inset(div())
            .child(caption(NO_KNOBS_NOTE, cx))
            .into_any_element();
    }

    let controls: Vec<Stateful<Div>> = specs
        .iter()
        .map(|spec| render_knob(gallery, spec, knobs, cx))
        .collect();

    with_dock_row_inset(div().id(KNOB_GRID_ID))
        .max_h_56()
        .overflow_y_scroll()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_3()
        .children(controls)
        .into_any_element()
}

fn render_knob(
    gallery: &Gallery,
    spec: &KnobSpec,
    knobs: &KnobValues,
    cx: &mut Context<Gallery>,
) -> Stateful<Div> {
    let control_carries_its_own_label = matches!(spec.kind, KnobKind::Bool);
    let control = match &spec.kind {
        KnobKind::Bool => render_bool_knob(spec.name, knobs.flag(spec.name), cx),
        KnobKind::Choice(labels) => {
            render_choice_knob(spec.name, labels, knobs.choice(spec.name), cx)
        }
        KnobKind::Text => render_text_knob(gallery, spec.name),
    };
    let knob_label = (!control_carries_its_own_label).then(|| caption(spec.name, cx));

    div()
        .id(SharedString::from(format!("knob-{}", spec.name)))
        .role(Role::Group)
        .aria_label(spec.name)
        .flex()
        .flex_col()
        .gap_1p5()
        .children(knob_label)
        .child(control)
}

fn render_bool_knob(name: &'static str, value: bool, cx: &mut Context<Gallery>) -> AnyElement {
    Switch::new(SharedString::from(format!("knob-switch-{name}")))
        .label(name)
        .checked(value)
        .on_click(cx.listener(move |gallery, checked: &bool, _, cx| {
            gallery.set_knob(name, KnobValue::Bool(*checked), cx)
        }))
        .into_any_element()
}

fn render_choice_knob(
    name: &'static str,
    labels: &[SharedString],
    selected: usize,
    cx: &mut Context<Gallery>,
) -> AnyElement {
    let segments = labels.iter().enumerate().map(|(index, label)| {
        Button::new(index)
            .label(label.clone())
            .selected(index == selected)
    });

    ButtonGroup::new(SharedString::from(format!("knob-choice-{name}")))
        .outline()
        .children(segments)
        .on_click(cx.listener(move |gallery, picked: &Vec<usize>, _, cx| {
            let Some(index) = picked.first() else {
                return;
            };

            gallery.set_knob(name, KnobValue::Choice(*index), cx);
        }))
        .into_any_element()
}

fn render_text_knob(gallery: &Gallery, name: &'static str) -> AnyElement {
    let Some(input) = gallery.text_knob_input(name) else {
        return div().into_any_element();
    };

    div()
        .w_56()
        .child(Input::new(input).aria_label(name))
        .into_any_element()
}

fn render_statusbar(gallery: &Gallery, cx: &mut Context<Gallery>) -> Div {
    let story_count = gallery.stories().len();
    let fixture_failure = gallery.runtime().read(cx).fixture_failure();

    statusbar_frame(cx)
        .child(div().child(format!("{story_count} stories")))
        .child(div().flex_1())
        .children(
            fixture_failure.map(|failure| inline_note(Tone::Failed, failure, cx).overflow_hidden()),
        )
}
