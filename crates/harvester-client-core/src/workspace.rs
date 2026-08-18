use std::time::Duration;

use crate::host::{parse_home, shell_quote, Host, HOME_SCRIPT};
use crate::{Error, Result};

const WORKSPACE_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RemoteDirEntry {
    pub name: String,
    pub is_dir: bool,
}

pub async fn resolve_home(host: &Host) -> Result<String> {
    let output = host.exec(HOME_SCRIPT, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("resolve home")?;

    parse_home(&output.stdout)
        .map(str::to_string)
        .ok_or_else(|| Error::Exec {
            context: "resolve home",
            detail: output.describe_failure(),
        })
}

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

pub async fn validate_git_workspace(host: &Host, path: &str) -> Result<()> {
    let script = format!("git -C {} rev-parse --git-dir", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("validate workspace")
}

pub async fn resolve_git_branch(host: &Host, path: &str) -> Result<String> {
    let script = format!("git -C {} rev-parse --abbrev-ref HEAD", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("resolve branch")?;

    let branch = output.stdout.trim();

    if branch.is_empty() {
        return Err(Error::Exec {
            context: "resolve branch",
            detail: output.describe_failure(),
        });
    }

    Ok(branch.to_string())
}
