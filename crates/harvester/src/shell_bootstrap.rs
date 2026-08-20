use gpui::{
    actions, px, size, AnyView, App, AppContext, Bounds, KeyBinding, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};
use gpui_component::{Root, Theme, ThemeMode};

use crate::assets::HarvesterAssets;
use crate::ui;

actions!(harvester, [Quit]);

pub struct ShellWindow {
    pub title: String,
    pub width_in_pixels: f32,
    pub height_in_pixels: f32,
}

pub fn run_desktop_shell<BuildRootView>(shell_window: ShellWindow, build_root_view: BuildRootView)
where
    BuildRootView: FnOnce(&mut Window, &mut App) -> AnyView + 'static,
{
    gpui_platform::application()
        .with_assets(HarvesterAssets)
        .run(move |cx: &mut App| {
            install_widget_layer(cx);
            install_application_commands(cx);
            open_shell_window(shell_window, build_root_view, cx);
        });
}

fn open_shell_window<BuildRootView>(
    shell_window: ShellWindow,
    build_root_view: BuildRootView,
    cx: &mut App,
) where
    BuildRootView: FnOnce(&mut Window, &mut App) -> AnyView + 'static,
{
    let window_options = create_window_options(&shell_window, cx);
    let open_result = cx.open_window(window_options, move |window, cx| {
        let root_view = build_root_view(window, cx);

        cx.new(|cx| Root::new(root_view, window, cx))
    });

    match open_result {
        Ok(_) => cx.activate(true),
        Err(error) => {
            eprintln!("harvester: open window: {error}");
            cx.quit();
        }
    }
}

fn create_window_options(shell_window: &ShellWindow, cx: &mut App) -> WindowOptions {
    let window_size = size(
        px(shell_window.width_in_pixels),
        px(shell_window.height_in_pixels),
    );
    let bounds = Bounds::centered(None, window_size, cx);

    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(shell_window.title.clone().into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn install_widget_layer(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
    ui::install_graphite_theme(cx);
}

fn install_application_commands(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
