use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Div, ElementId, Role, SharedString, Stateful,
    Window,
};
use gpui_component::input::Input;
use gpui_component::ActiveTheme as _;
use harvester::ui::{
    chat_header, collapsible_section, inline_note, sidebar_row, statusbar_frame, Tone,
    BASE_TEXT_SIZE,
};

use crate::stories::caption;
use crate::story::{KnobKind, KnobSpec, KnobValue, KnobValues, StoryScene};
use crate::Gallery;

const STORY_SIDEBAR_WIDTH: f32 = 240.;
const STAGE_MAX_WIDTH: f32 = 900.;
const DOCK_KNOBS_MAX_HEIGHT: f32 = 200.;
const TEXT_KNOB_WIDTH: f32 = 220.;
const BOOL_ON_SLOT: usize = 0;
const BOOL_OFF_SLOT: usize = 1;

const STORY_SIDEBAR_ID: &str = "story-sidebar";
const STAGE_CANVAS_ID: &str = "stage-canvas";
const DOCK_HEADER_ID: &str = "control-dock-header";
const KNOB_GRID_ID: &str = "knob-grid";
const DOCK_TITLE: &str = "controls";
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
        .text_size(px(BASE_TEXT_SIZE))
        .child(story_sidebar)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .child(stage)
                .child(dock)
                .child(statusbar),
        )
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
        .aria_label("stories")
        .w(px(STORY_SIDEBAR_WIDTH))
        .h_full()
        .flex_none()
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
    )
    .role(Role::Button)
    .aria_label(group)
    .aria_expanded(expanded);
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
        .min_h(px(0.))
        .flex()
        .flex_col()
        .child(header)
        .child(
            div()
                .id(STAGE_CANVAS_ID)
                .role(Role::Group)
                .aria_label("stage")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .p_6()
                .flex()
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(px(STAGE_MAX_WIDTH))
                        .flex()
                        .flex_col()
                        .justify_center()
                        .child(body),
                ),
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
    )
    .role(Role::Button)
    .aria_label(DOCK_TITLE)
    .aria_expanded(expanded);
    let presets = render_preset_chips(gallery, cx);
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

fn render_preset_chips(gallery: &Gallery, cx: &mut Context<Gallery>) -> Div {
    let presets = &gallery.stories()[gallery.selected_story()].presets;

    if presets.is_empty() {
        return div().px_3().py_1p5().child(caption(NO_PRESETS_NOTE, cx));
    }

    div()
        .px_3()
        .py_1p5()
        .flex()
        .flex_wrap()
        .gap_2()
        .children(presets.iter().enumerate().map(|(index, preset)| {
            chip(("preset", index), preset.name, false, cx).on_click(
                cx.listener(move |gallery, _, window, cx| gallery.apply_preset(index, window, cx)),
            )
        }))
}

fn render_knob_grid(
    gallery: &Gallery,
    knobs: &KnobValues,
    cx: &mut Context<Gallery>,
) -> AnyElement {
    let specs = &gallery.stories()[gallery.selected_story()].knobs;

    if specs.is_empty() {
        return div()
            .px_3()
            .py_1p5()
            .child(caption(NO_KNOBS_NOTE, cx))
            .into_any_element();
    }

    let controls: Vec<Stateful<Div>> = specs
        .iter()
        .map(|spec| render_knob(gallery, spec, knobs, cx))
        .collect();

    div()
        .id(KNOB_GRID_ID)
        .max_h(px(DOCK_KNOBS_MAX_HEIGHT))
        .overflow_y_scroll()
        .px_3()
        .py_1p5()
        .flex()
        .flex_wrap()
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
    let control = match &spec.kind {
        KnobKind::Bool => render_bool_knob(spec.name, knobs.flag(spec.name), cx),
        KnobKind::Choice(labels) => {
            render_choice_knob(spec.name, labels, knobs.choice(spec.name), cx)
        }
        KnobKind::Text => render_text_knob(gallery, spec.name),
    };

    div()
        .id(SharedString::from(format!("knob-{}", spec.name)))
        .role(Role::Group)
        .aria_label(spec.name)
        .flex()
        .flex_col()
        .gap_1()
        .child(caption(spec.name, cx))
        .child(control)
}

fn render_bool_knob(name: &'static str, value: bool, cx: &mut Context<Gallery>) -> Div {
    let on = chip((SharedString::from(name), BOOL_ON_SLOT), "on", value, cx).on_click(
        cx.listener(move |gallery, _, _, cx| gallery.set_knob(name, KnobValue::Bool(true), cx)),
    );
    let off = chip((SharedString::from(name), BOOL_OFF_SLOT), "off", !value, cx).on_click(
        cx.listener(move |gallery, _, _, cx| gallery.set_knob(name, KnobValue::Bool(false), cx)),
    );

    div().flex().gap_1().child(on).child(off)
}

fn render_choice_knob(
    name: &'static str,
    labels: &[SharedString],
    selected: usize,
    cx: &mut Context<Gallery>,
) -> Div {
    let chips: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            chip(
                (SharedString::from(name), index),
                label.clone(),
                index == selected,
                cx,
            )
            .on_click(cx.listener(move |gallery, _, _, cx| {
                gallery.set_knob(name, KnobValue::Choice(index), cx)
            }))
        })
        .collect();

    div().flex().flex_wrap().gap_1().children(chips)
}

fn render_text_knob(gallery: &Gallery, name: &'static str) -> Div {
    let Some(input) = gallery.text_knob_input(name) else {
        return div();
    };

    div().w(px(TEXT_KNOB_WIDTH)).child(Input::new(input))
}

fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let label = label.into();

    let base = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_selected(active)
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .text_xs()
        .cursor_pointer()
        .child(label);

    if active {
        return base
            .bg(cx.theme().primary)
            .border_color(cx.theme().primary)
            .text_color(cx.theme().primary_foreground)
            .active(|style| style.bg(cx.theme().primary_active));
    }

    base.bg(cx.theme().secondary)
        .border_color(cx.theme().border)
        .text_color(cx.theme().muted_foreground)
        .hover(|style| {
            style
                .bg(cx.theme().secondary_hover)
                .text_color(cx.theme().foreground)
        })
        .active(|style| style.bg(cx.theme().secondary_active))
}

fn render_statusbar(gallery: &Gallery, cx: &mut Context<Gallery>) -> Div {
    let story_count = gallery.stories().len();
    let story = &gallery.stories()[gallery.selected_story()];
    let fixture_failure = gallery.runtime().read(cx).fixture_failure();

    statusbar_frame(cx)
        .child(div().child(format!("{story_count} stories")))
        .child(inline_note(
            Tone::Muted,
            format!("{} · {}", story.group, story.name),
            cx,
        ))
        .child(div().flex_1())
        .children(
            fixture_failure.map(|failure| inline_note(Tone::Failed, failure, cx).overflow_hidden()),
        )
}
