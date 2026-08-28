use std::io::Write as _;

use serde::{Deserialize, Serialize};

const STAGED_EXTENSION_PREFIX: &str = "json.next";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub machine: String,
    pub path: String,
}

pub fn folder_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
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

pub fn load() -> Vec<Workspace> {
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

pub fn save(workspaces: &[Workspace]) -> std::io::Result<()> {
    let path = store_path()?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_vec_pretty(workspaces)?;
    let staged = staged_path(&path);

    let swapped = write_staged_file(&staged, &json).and_then(|()| std::fs::rename(&staged, &path));

    if let Err(error) = swapped {
        remove_staged_file(&staged);

        return Err(error);
    }

    Ok(())
}

fn staged_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension(format!("{STAGED_EXTENSION_PREFIX}.{}", std::process::id()))
}

fn write_staged_file(staged: &std::path::Path, json: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(staged)?;

    file.write_all(json)?;

    file.sync_all()
}

fn remove_staged_file(staged: &std::path::Path) {
    match std::fs::remove_file(staged) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => eprintln!("harvester: remove {}: {error}", staged.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_staged_file_is_a_sibling_named_for_the_writing_process() {
        let path = std::path::Path::new("/home/someone/.harvester/workspaces.json");
        let staged = staged_path(path);

        assert_eq!(
            staged,
            std::path::PathBuf::from(format!(
                "/home/someone/.harvester/workspaces.json.next.{}",
                std::process::id()
            )),
            "a staging name shared by two clients is not a staging file at all"
        );
        assert_eq!(
            staged.parent(),
            path.parent(),
            "the rename is only atomic while both names live on one filesystem"
        );
    }
}
