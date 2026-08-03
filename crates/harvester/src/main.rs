//! Harvester — the desktop client (docs/SPEC.md). Slice 4: connect to one
//! machine, list its sessions, attach, render one streaming turn.
//!
//! `harvester <ssh-destination>`, or `harvester kitchen` for the design-system
//! catalog. The machine list UI (providers, add-machine)
//! is a later slice; until then the destination comes from argv, like
//! harvester-cli. Argument parsing stays manual — clap lives in harvester-cli
//! only, and one positional argument does not earn it here either.

mod bridge;
mod chat;
mod input;
mod kitchen;
mod theme;
mod ui;
mod workbench;
mod workspaces;

use gpui::{
    actions, px, size, App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions,
    WindowBounds, WindowOptions,
};

use crate::kitchen::Kitchen;
use crate::workbench::Workbench;

actions!(harvester, [Quit]);

/// The musl harvesterd baked in by build.rs; empty in dev builds made without
/// the musl artifact (then `daemon_binary` falls back to the target dir).
const EMBEDDED_DAEMON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harvesterd"));
/// Dev fallback, relative to the repo-root cwd of `cargo run -p harvester`.
const DEV_DAEMON_PATH: &str = "target/x86_64-unknown-linux-musl/release/harvesterd";

fn main() {
    let Some(dest) = std::env::args().nth(1) else {
        eprintln!("usage: harvester <ssh-destination> | harvester kitchen");
        std::process::exit(2);
    };

    // The design-system catalog: pure rendering, no bridge, no daemon.
    if dest == "kitchen" {
        run_kitchen();
        return;
    }

    let daemon_binary = match daemon_binary() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("harvester: {error}");
            std::process::exit(1);
        }
    };

    let bridge = match bridge::start(dest.clone(), daemon_binary) {
        Ok(bridge) => bridge,
        Err(error) => {
            eprintln!("harvester: start bridge thread: {error}");
            std::process::exit(1);
        }
    };

    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
        let open_result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(format!("Harvester — {dest}").into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| Workbench::new(dest.clone(), bridge, cx)),
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

fn run_kitchen() {
    Application::new().run(|cx: &mut App| {
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(820.), px(720.)), cx);
        let open_result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Harvester — kitchen".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(Kitchen::new),
        );

        match open_result {
            Ok(_) => cx.activate(true),
            Err(error) => {
                eprintln!("harvester: open kitchen window: {error}");
                cx.quit();
            }
        }
    });
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

fn bind_keys(cx: &mut App) {
    use crate::input::{
        Backspace, Copy, Cut, Delete, End, Home, Left, Paste, Right, SelectAll, SelectLeft,
        SelectRight, Submit,
    };

    // Both modifier spellings so one build behaves natively on Linux (ctrl)
    // and macOS (cmd).
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Submit, Some("TextInput")),
        KeyBinding::new("ctrl-a", SelectAll, Some("TextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        KeyBinding::new("ctrl-c", Copy, Some("TextInput")),
        KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        KeyBinding::new("ctrl-x", Cut, Some("TextInput")),
        KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        KeyBinding::new("ctrl-v", Paste, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}
