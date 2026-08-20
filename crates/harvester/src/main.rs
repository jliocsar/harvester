mod catalog;
mod chat;
mod desktop_shell;
mod dialogs;
mod modal;
mod models;
mod session_state;
mod store;
mod workbench;
mod workspaces;

pub use harvester::{assets, ui};

const EMBEDDED_DAEMON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harvesterd"));
const DEV_DAEMON_PATH: &str = "target/x86_64-unknown-linux-musl/release/harvesterd";

fn main() {
    let argv_machine = std::env::args().nth(1);
    let daemon_binary = match daemon_binary() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("harvester: {error}");
            std::process::exit(1);
        }
    };

    desktop_shell::run(argv_machine, daemon_binary);
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
