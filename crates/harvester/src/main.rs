mod catalog;
mod chat;
mod dialogs;
mod models;
mod session_state;
mod store;
mod workbench;
mod workspaces;

pub use harvester::{assets, ui};

use gpui::AppContext;
use harvester::shell_bootstrap::{run_desktop_shell, ShellWindow};
use harvester_proto::DEV_DAEMON_PATH;

use crate::workbench::Workbench;

const EMBEDDED_DAEMON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harvesterd"));

const MAIN_WINDOW_WIDTH: f32 = 1100.;
const MAIN_WINDOW_HEIGHT: f32 = 720.;

fn main() {
    let argv_machine = std::env::args().nth(1);
    let daemon_binary = match daemon_binary() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("harvester: {error}");
            std::process::exit(1);
        }
    };

    run(argv_machine, daemon_binary);
}

fn run(argv_machine: Option<String>, daemon_binary: Vec<u8>) {
    let shell_window = ShellWindow {
        title: main_window_title(argv_machine.as_deref()),
        width_in_pixels: MAIN_WINDOW_WIDTH,
        height_in_pixels: MAIN_WINDOW_HEIGHT,
    };

    run_desktop_shell(shell_window, move |window, cx| {
        cx.new(|cx| Workbench::new(argv_machine, daemon_binary, window, cx))
            .into()
    });
}

fn main_window_title(argv_machine: Option<&str>) -> String {
    match argv_machine {
        Some(machine) => format!("Harvester — {machine}"),
        None => "Harvester".to_string(),
    }
}

fn daemon_binary() -> Result<Vec<u8>, String> {
    if !EMBEDDED_DAEMON.is_empty() {
        return Ok(EMBEDDED_DAEMON.to_vec());
    }

    std::fs::read(DEV_DAEMON_PATH).map_err(|error| {
        format!(
            "no embedded harvesterd and reading {DEV_DAEMON_PATH} failed: {error}\n\
             build it: cargo build --profile daemon-release --target x86_64-unknown-linux-musl -p harvesterd"
        )
    })
}
