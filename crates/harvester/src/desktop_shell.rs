use gpui::AppContext;
use harvester::shell_bootstrap::{run_desktop_shell, ShellWindow};

use crate::workbench::Workbench;

const MAIN_WINDOW_WIDTH: f32 = 1100.;
const MAIN_WINDOW_HEIGHT: f32 = 720.;

pub(crate) fn run(argv_machine: Option<String>, daemon_binary: Vec<u8>) {
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
