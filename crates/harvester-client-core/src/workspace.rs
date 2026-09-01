use std::path::PathBuf;

use harvester_proto::replace_atomically;
use serde::{Deserialize, Deserializer, Serialize};

use crate::MachineId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct WorkspaceId {
    machine: MachineId,
    path: String,
}

impl WorkspaceId {
    pub fn new(machine: MachineId, path: impl Into<String>) -> Option<Self> {
        let path = path.into();

        path.starts_with('/').then_some(Self { machine, path })
    }

    pub const fn machine(&self) -> &MachineId {
        &self.machine
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredWorkspace {
            machine: MachineId,
            path: String,
        }

        let stored = StoredWorkspace::deserialize(deserializer)?;

        Self::new(stored.machine, stored.path)
            .ok_or_else(|| serde::de::Error::custom("workspace path must be absolute"))
    }
}

pub(crate) struct WorkspaceStore {
    path: std::io::Result<PathBuf>,
}

impl WorkspaceStore {
    pub(crate) fn discover() -> Self {
        Self { path: store_path() }
    }

    #[cfg(test)]
    pub(crate) const fn at(path: PathBuf) -> Self {
        Self { path: Ok(path) }
    }

    pub(crate) fn load(&self) -> Vec<WorkspaceId> {
        let path = match &self.path {
            Ok(path) => path,
            Err(error) => {
                eprintln!("harvester: {error}; workspace list unavailable");

                return Vec::new();
            }
        };
        let bytes = match std::fs::read(path) {
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

    pub(crate) fn save(&self, workspaces: &[WorkspaceId]) -> std::io::Result<()> {
        let path = self
            .path
            .as_ref()
            .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        replace_atomically(path, &serde_json::to_vec_pretty(workspaces)?, None)
    }
}

fn store_path() -> std::io::Result<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset",
        ));
    };

    Ok(PathBuf::from(home).join(".harvester/workspaces.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::ScratchDir;

    #[test]
    fn workspace_identity_preserves_the_existing_configuration_shape() {
        let workspace = WorkspaceId::new(
            MachineId::new("exe-dev/bloggy").expect("non-empty"),
            "/work/repo",
        )
        .expect("non-empty path");
        let value = serde_json::to_value(workspace).expect("serializes");

        assert_eq!(
            value,
            serde_json::json!({"machine": "exe-dev/bloggy", "path": "/work/repo"})
        );
    }

    #[test]
    fn workspace_identity_requires_an_absolute_path() {
        assert!(WorkspaceId::new(MachineId::local(), "").is_none());
        assert!(WorkspaceId::new(MachineId::local(), "relative/repo").is_none());
        assert!(serde_json::from_value::<WorkspaceId>(serde_json::json!({
            "machine": "local",
            "path": "relative/repo"
        }))
        .is_err());
    }

    #[test]
    fn workspace_store_round_trips_and_replaces_the_whole_list() {
        let scratch = ScratchDir::create("workspace-store");
        let store = WorkspaceStore::at(scratch.join("workspaces.json"));
        let first = WorkspaceId::new(MachineId::local(), "/first").expect("workspace");
        let second = WorkspaceId::new(MachineId::local(), "/second").expect("workspace");

        store.save(&[first.clone(), second]).expect("first save");
        store
            .save(std::slice::from_ref(&first))
            .expect("replacement save");

        assert_eq!(store.load(), vec![first]);
    }
}
