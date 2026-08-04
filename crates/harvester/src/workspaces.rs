//! The client-side workspace store: `~/.harvester/workspaces.json`, plain
//! JSON `[{machine, path}]` (docs/SPEC.md §Workspace). This is user
//! configuration, not session state — the VM's disk stays the only authority
//! on sessions.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// Machine key `<provider>/<id>`, resolved to ssh info through the
    /// provider list at use time. A string without the separator is an
    /// ad-hoc ssh destination used verbatim (legacy entries, argv machines).
    pub machine: String,
    pub path: String,
}

/// `~/.harvester/workspaces.json`; `None` when `$HOME` is unset.
fn store_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".harvester/workspaces.json"))
}

/// Missing file → empty list (first run). Unreadable or corrupt → empty list
/// with the reason on stderr; the next save rewrites the file whole.
pub fn load() -> Vec<Workspace> {
    let Some(path) = store_path() else {
        eprintln!("harvester: $HOME unset; workspace list unavailable");

        return Vec::new();
    };

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            eprintln!("harvester: read {}: {error}", path.display());

            return Vec::new();
        }
    };

    match serde_json::from_slice(&bytes) {
        Ok(workspaces) => workspaces,
        Err(error) => {
            eprintln!(
                "harvester: parse {}: {error} — treating as empty",
                path.display()
            );

            Vec::new()
        }
    }
}

/// Whole-list rewrite (the list is tiny). Errors go to the caller — the
/// statusbar is the right place for them, not stderr.
pub fn save(workspaces: &[Workspace]) -> std::io::Result<()> {
    let Some(path) = store_path() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset",
        ));
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_vec_pretty(workspaces)?;

    std::fs::write(path, json)
}
