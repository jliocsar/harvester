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

/// The name a workspace goes by on screen: its path's last component (the
/// whole path when there is no separator). Lives here because a workspace path
/// is this module's domain, and both the sidebar and the chat need to say it.
pub fn folder_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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
///
/// Written to a sibling temp file and renamed into place, the same atomic swap
/// the daemon install uses. Writing the real file directly would truncate it
/// first, so a crash mid-write leaves torn JSON — which `load` reads as an
/// empty list, and the next save then persists that emptiness over the user's
/// whole configuration.
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
    // Sibling, so the rename stays on one filesystem (where it is atomic).
    let staged = path.with_extension("json.next");

    std::fs::write(&staged, json)?;
    std::fs::rename(&staged, &path)
}
