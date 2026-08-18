//! The cohesive GPUI desktop shell around Harvester's workbench.
//!
//! This module owns application-wide UI policy and window construction. The
//! workbench remains responsible for product layout and user intent; callers
//! only provide launch inputs that were resolved before GPUI starts.

use gpui::{
    actions, px, size, App, AppContext, Bounds, KeyBinding, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions,
};
use gpui_component::{Root, Theme, ThemeMode};

use crate::{assets::HarvesterAssets, ui, workbench::Workbench};

actions!(harvester, [Quit]);

/// Width of the centered main window.
const MAIN_WINDOW_WIDTH: f32 = 1100.;
/// Height of the centered main window.
const MAIN_WINDOW_HEIGHT: f32 = 720.;

/// Launch the GPUI application around one workbench.
pub(crate) fn run(argv_machine: Option<String>, daemon_binary: Vec<u8>) {
    let desktop_shell = DesktopShell::new(argv_machine, daemon_binary);

    desktop_shell.run();
}

/// Inputs and policy needed to construct Harvester's application shell.
struct DesktopShell {
    argv_machine: Option<String>,
    daemon_binary: Vec<u8>,
}

impl DesktopShell {
    fn new(argv_machine: Option<String>, daemon_binary: Vec<u8>) -> Self {
        Self {
            argv_machine,
            daemon_binary,
        }
    }

    fn run(self) {
        install_system_mono_font_family();

        gpui_platform::application()
            .with_assets(HarvesterAssets)
            .run(move |cx: &mut App| self.setup_application(cx));
    }

    fn setup_application(self, cx: &mut App) {
        install_widget_layer(cx);
        install_application_commands(cx);
        self.open_main_window(cx);
    }

    fn open_main_window(self, cx: &mut App) {
        let window_options = self.main_window_options(cx);
        let open_result = cx.open_window(window_options, move |window, cx| {
            let workbench =
                cx.new(|cx| Workbench::new(self.argv_machine, self.daemon_binary, window, cx));

            cx.new(|cx| Root::new(workbench, window, cx))
        });

        match open_result {
            Ok(_) => cx.activate(true),
            Err(error) => {
                eprintln!("harvester: open window: {error}");
                cx.quit();
            }
        }
    }

    fn main_window_options(&self, cx: &mut App) -> WindowOptions {
        let title = self.main_window_title();
        let bounds = Bounds::centered(
            None,
            size(px(MAIN_WINDOW_WIDTH), px(MAIN_WINDOW_HEIGHT)),
            cx,
        );

        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn main_window_title(&self) -> String {
        match &self.argv_machine {
            Some(machine) => format!("Harvester — {machine}"),
            None => "Harvester".to_string(),
        }
    }
}

/// Ask fontconfig for the system monospace family once, before anything
/// renders. The application shell owns this subprocess because rendering code
/// must not shell out. `None` leaves `ui` on its own fallback.
fn install_system_mono_font_family() {
    let Some(family) = resolve_mono_font_family() else {
        return;
    };

    ui::use_mono_font_family(family);
}

fn resolve_mono_font_family() -> Option<SharedString> {
    let fontconfig_match = std::process::Command::new("fc-match")
        .args(["--format", "%{family}", "monospace"])
        .output()
        .ok()?;

    if !fontconfig_match.status.success() || fontconfig_match.stdout.is_empty() {
        return None;
    }

    Some(
        String::from_utf8_lossy(&fontconfig_match.stdout)
            .trim()
            .to_string()
            .into(),
    )
}

/// gpui-component's one-time setup. `init` must precede every other call into
/// the library; it also hardcodes light mode, so dark is chosen explicitly
/// right after. `Theme::sync_system_appearance` is deliberately never called:
/// nothing observes the desktop appearance, keeping Harvester dark on a
/// light-mode system.
fn install_widget_layer(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
}

/// Install the app-level quit action and its cross-platform keybindings.
/// gpui-component owns the Input, Table, List, and Menu context bindings.
fn install_application_commands(cx: &mut App) {
    // Both modifier spellings so one build behaves natively on Linux (ctrl)
    // and macOS (cmd).
    cx.bind_keys([
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}
