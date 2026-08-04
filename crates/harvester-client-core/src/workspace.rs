//! Workspace browsing (docs/SPEC.md §Workspace): directory listings for the
//! folder picker and git validation. These are client business, never the
//! daemon's (docs/DAEMON.md keeps path work out of the envelope) — one-shot
//! execs against [`Host`], so the local machine and SSH machines browse with
//! identical semantics (remote execs multiplex over the ControlMaster
//! connection: a per-click listing costs about one network round trip).

use std::time::Duration;

use crate::host::{shell_quote, Host};
use crate::{Error, Result};

/// Ceiling on one picker exec — multiplexed listings take tens of ms; a
/// wedged transport must not hang the picker forever.
const WORKSPACE_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// Prints the machine's home directory and nothing else. `printf` rather than
/// `echo` for the same reason the bootstrap probe uses it: `echo`'s handling
/// of backslashes is shell-dependent.
const HOME_SCRIPT: &str = r#"printf '%s\n' "$HOME""#;

/// One entry from a directory listing.
#[derive(Debug, Clone)]
pub struct RemoteDirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// The machine's `$HOME` — where the folder picker starts its walk
/// (SPEC §Workspace). One exec, never a bootstrap: path work is client
/// business, so the picker must not wait on a daemon it does not use.
pub async fn resolve_home(host: &Host) -> Result<String> {
    let output = host.exec(HOME_SCRIPT, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("resolve home")?;

    let home = output.stdout.lines().next().unwrap_or_default().to_string();

    // An empty or relative `$HOME` would send the picker walking from
    // nowhere; fail with the exec's own diagnosis instead.
    if !home.starts_with('/') {
        return Err(Error::Exec {
            context: "resolve home",
            detail: output.describe_failure(),
        });
    }

    Ok(home)
}

/// Lists `path` on the machine via `ls -1Ap`: one name per line, hidden
/// entries included (dotdirs are legitimate workspace parents), a `/` suffix
/// marking directories. Known limit: symlinked directories are listed
/// undereferenced, so they read as plain entries — acceptable for a picker.
pub async fn list_directory(host: &Host, path: &str) -> Result<Vec<RemoteDirEntry>> {
    let script = format!("ls -1Ap -- {}", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("list directory")?;

    Ok(parse_ls_entries(&output.stdout))
}

fn parse_ls_entries(stdout: &str) -> Vec<RemoteDirEntry> {
    stdout
        .lines()
        .map(|line| match line.strip_suffix('/') {
            Some(name) => RemoteDirEntry {
                name: name.to_string(),
                is_dir: true,
            },
            None => RemoteDirEntry {
                name: line.to_string(),
                is_dir: false,
            },
        })
        .collect()
}

/// Checks that `path` is a git repository — the definition of a workspace
/// (SPEC §Workspace). The error carries git's own explanation for the UI.
pub async fn validate_git_workspace(host: &Host, path: &str) -> Result<()> {
    let script = format!("git -C {} rev-parse --git-dir", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("validate workspace")
}
