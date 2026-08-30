use harvester_proto::replace_atomically;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Workspace {
    pub machine: String,
    pub path: String,
}

fn store_path() -> std::io::Result<std::path::PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset",
        ));
    };

    Ok(std::path::PathBuf::from(home).join(".harvester/workspaces.json"))
}

pub(crate) fn load() -> Vec<Workspace> {
    let path = match store_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("harvester: {error}; workspace list unavailable");

            return Vec::new();
        }
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

pub(crate) fn save(workspaces: &[Workspace]) -> std::io::Result<()> {
    let path = store_path()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    replace_atomically(&path, &serde_json::to_vec_pretty(workspaces)?, None)
}
