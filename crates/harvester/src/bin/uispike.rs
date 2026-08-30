//! Throwaway probe: what can a gpui app learn about its own UI from inside?
//!
//! Not part of the product. Renders a small representative screen, then dumps
//! `Window::debug_a11y_tree_json` on ctrl-d so the tree can be compared with
//! accessibility inactive vs active.

use gpui::{
    actions, div, prelude::*, px, rgb, size, text, App, Bounds, Context, FocusHandle, KeyBinding,
    Role, SharedString, Window, WindowBounds, WindowOptions,
};

actions!(uispike, [Dump]);

struct Spike {
    focus_handle: FocusHandle,
    count: i32,
}

impl Spike {
    fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        window.focus(&focus_handle, cx);

        Self {
            focus_handle,
            count: 0,
        }
    }

    /// Print what the app can see of itself, to stdout.
    fn dump(&mut self, _: &Dump, window: &mut Window, _cx: &mut Context<'_, Self>) {
        let tree = window.debug_a11y_tree_json();

        println!("=== a11y_active={} ===", window.is_a11y_active());

        match tree {
            Some(json) => println!("{json}"),
            None => println!("<no tree>"),
        }
    }
}

impl Render for Spike {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let rows = ["alpha", "beta", "gamma"];

        div()
            .id("root")
            .role(Role::Application)
            .aria_label("Spike")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::dump))
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                div()
                    .id("heading")
                    .role(Role::Heading)
                    .aria_level(1)
                    .aria_label("Spike")
                    .text_xl()
                    .child(text!("Spike")),
            )
            .child(
                div()
                    .id("counter")
                    .role(Role::Button)
                    .focusable()
                    .tab_stop(true)
                    .aria_label(SharedString::from(format!("Count: {}", self.count)))
                    .px_3()
                    .py_1()
                    .bg(rgb(0x313244))
                    .rounded_md()
                    .on_click(cx.listener(|spike, _, _, cx| {
                        spike.count += 1;
                        cx.notify();
                    }))
                    .child(text!("increment")),
            )
            .child(
                div()
                    .id("rows")
                    .role(Role::List)
                    .flex()
                    .flex_col()
                    .children(rows.iter().enumerate().map(|(index, row)| {
                        div()
                            .id(index)
                            .role(Role::ListItem)
                            .aria_label(SharedString::from(row.to_string()))
                            .child(text!(*row))
                    })),
            )
            // Deliberately unlabelled: the control case for "what does an
            // ordinary div look like in the tree" (it should be absent).
            .child(div().child(text!("plain unlabelled text")))
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.bind_keys([KeyBinding::new("ctrl-d", Dump, None)]);

        let bounds = Bounds::centered(None, size(px(600.), px(400.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };

        cx.open_window(options, |window, cx| cx.new(|cx| Spike::new(window, cx)))
            .expect("open spike window");

        cx.activate(true);
    });
}
