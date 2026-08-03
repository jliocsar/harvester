//! Workspace browsing over plain SSH (docs/SPEC.md §Workspace): directory
//! listings for the folder picker and git validation. These are client
//! business, never the daemon's (docs/DAEMON.md keeps path work out of the
//! envelope) — one-shot execs, multiplexed over the ControlMaster connection
//! so a per-click listing skips the handshake and costs about one network
//! round trip.

use crate::ssh::{exec, SshTarget};
use crate::{Error, Result};

/// One entry from a remote directory listing.
#[derive(Debug, Clone)]
pub struct RemoteDirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Lists `path` on the machine via `ls -1Ap`: one name per line, hidden
/// entries included (dotdirs are legitimate workspace parents), a `/` suffix
/// marking directories. Known limit: symlinked directories are listed
/// undereferenced, so they read as plain entries — acceptable for a picker.
pub async fn list_directory(target: &SshTarget, path: &str) -> Result<Vec<RemoteDirEntry>> {
    let script = format!("ls -1Ap -- {}", shell_quote(path));
    let output = exec(target, &script, None).await?;

    if output.status.success() {
        let entries = output
            .stdout
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
            .collect();

        Ok(entries)
    } else {
        Err(Error::Ssh {
            context: "list directory",
            detail: output.describe_failure(),
        })
    }
}

/// Checks that `path` is a git repository — the definition of a workspace
/// (SPEC §Workspace). The error carries git's own explanation for the UI.
pub async fn validate_git_workspace(target: &SshTarget, path: &str) -> Result<()> {
    let script = format!("git -C {} rev-parse --git-dir", shell_quote(path));
    let output = exec(target, &script, None).await?;

    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Ssh {
            context: "validate workspace",
            detail: output.describe_failure(),
        })
    }
}

/// Single-quotes `value` for the remote POSIX shell (the `'\''` dance), so
/// arbitrary path characters survive the ssh command-line round trip.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}
