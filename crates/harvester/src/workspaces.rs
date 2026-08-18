use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub machine: String,
    pub path: String,
}

pub fn folder_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn store_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".harvester/workspaces.json"))
}

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
    let staged = path.with_extension("json.next");

    std::fs::write(&staged, json)?;
    std::fs::rename(&staged, &path)
}
