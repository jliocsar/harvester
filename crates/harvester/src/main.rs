//! Harvester — the desktop client (docs/SPEC.md).
//!
//! `harvester` is the normal launch (machines come from providers and the
//! workspace list); `harvester <ssh-destination>` adds that destination as an
//! ad-hoc machine (the dev workflow). Argument parsing stays manual — clap
//! lives in harvester-cli only, and one positional argument does not earn it
//! here either.

mod chat;
mod choices;
mod model_control;
mod models;
mod store;
mod ui;
mod workbench;
mod workspaces;

use gpui::{
    actions, px, size, App, AppContext, Bounds, KeyBinding, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions,
};
use gpui_component::{Root, Theme, ThemeMode};

use crate::workbench::Workbench;

actions!(harvester, [Quit]);

/// The musl harvesterd baked in by build.rs; empty in dev builds made without
/// the musl artifact (then `daemon_binary` falls back to the target dir).
const EMBEDDED_DAEMON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harvesterd"));
/// Dev fallback, relative to the repo-root cwd of `cargo run -p harvester`.
const DEV_DAEMON_PATH: &str = "target/x86_64-unknown-linux-musl/release/harvesterd";

/// Ask fontconfig for the system monospace family, once, before anything
/// renders. Here rather than in `ui` because it spawns a subprocess, and a
/// rendering module that shells out cannot be lifted into a component library
/// — nor run anywhere a subprocess is unwelcome. `None` leaves `ui` on its own
/// fallback.
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

fn main() {
    let argv_machine = std::env::args().nth(1);

    if let Some(family) = resolve_mono_font_family() {
        ui::use_mono_font_family(family);
    }

    let daemon_binary = match daemon_binary() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("harvester: {error}");
            std::process::exit(1);
        }
    };

    gpui_platform::application()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            install_widget_layer(cx);
            bind_keys(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());

            let title = match &argv_machine {
                Some(machine) => format!("Harvester — {machine}"),
                None => "Harvester".to_string(),
            };
            let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
            let open_result = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let workbench =
                        cx.new(|cx| Workbench::new(argv_machine, daemon_binary, window, cx));

                    cx.new(|cx| Root::new(workbench, window, cx))
                },
            );

            match open_result {
                Ok(_) => cx.activate(true),
                Err(error) => {
                    eprintln!("harvester: open window: {error}");
                    cx.quit();
                }
            }
        });
}

/// gpui-component's one-time setup. `init` must precede
/// every other call into the library; it also hardcodes light mode, so dark is
/// chosen explicitly right after. `Theme::sync_system_appearance` is
/// deliberately never called — nothing else observes the desktop's appearance,
/// which is what keeps Harvester dark on a light-mode system.
fn install_widget_layer(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
}

fn daemon_binary() -> Result<Vec<u8>, String> {
    if !EMBEDDED_DAEMON.is_empty() {
        return Ok(EMBEDDED_DAEMON.to_vec());
    }

    std::fs::read(DEV_DAEMON_PATH).map_err(|error| {
        format!(
            "no embedded harvesterd and reading {DEV_DAEMON_PATH} failed: {error}\n\
             build it: cargo build --release --target x86_64-unknown-linux-musl -p harvesterd"
        )
    })
}

/// Only the app-level bindings survive: `gpui_component::init` binds the
/// Input, Table, List and Menu contexts itself, which is what replaced the
/// vendored text input's seventeen.
fn bind_keys(cx: &mut App) {
    // Both modifier spellings so one build behaves natively on Linux (ctrl)
    // and macOS (cmd).
    cx.bind_keys([
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}
