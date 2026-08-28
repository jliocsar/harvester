use std::time::Duration;

use crate::host::{parse_home, shell_quote, Host, HOME_SCRIPT};
use crate::{Error, Result};

const WORKSPACE_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RemoteDirEntry {
    pub name: String,
    pub is_dir: bool,
}

pub(crate) async fn resolve_home(host: &Host) -> Result<String> {
    let output = host.exec(HOME_SCRIPT, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("resolve home")?;

    parse_home(&output.stdout)
        .map(str::to_string)
        .ok_or_else(|| Error::Exec {
            context: "resolve home",
            detail: output.describe_failure(),
        })
}

pub(crate) async fn list_directory(host: &Host, path: &str) -> Result<Vec<RemoteDirEntry>> {
    let script = format!("ls -1Ap -- {}", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("list directory")?;

    Ok(parse_ls_entries(&output.stdout))
}

fn parse_ls_entries(stdout: &str) -> Vec<RemoteDirEntry> {
    stdout.lines().filter_map(parse_ls_entry).collect()
}

fn parse_ls_entry(line: &str) -> Option<RemoteDirEntry> {
    let (name, is_dir) = match line.strip_suffix('/') {
        Some(directory) => (directory, true),
        None => (line, false),
    };

    if name.is_empty() || name.chars().any(char::is_control) {
        return None;
    }

    Some(RemoteDirEntry {
        name: name.to_string(),
        is_dir,
    })
}

pub(crate) async fn validate_git_workspace(host: &Host, path: &str) -> Result<()> {
    let script = format!("git -C {} rev-parse --git-dir", shell_quote(path));
    let output = host.exec(&script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success("validate workspace")
}

pub(crate) async fn resolve_git_branch(host: &Host, path: &str) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(stdout: &str) -> Vec<(String, bool)> {
        parse_ls_entries(stdout)
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect()
    }

    #[test]
    fn a_trailing_slash_is_what_marks_a_directory() {
        assert_eq!(
            names("src/\nCargo.toml\ntarget/\n"),
            vec![
                ("src".to_string(), true),
                ("Cargo.toml".to_string(), false),
                ("target".to_string(), true),
            ]
        );
    }

    #[test]
    fn a_name_carrying_control_characters_is_dropped_rather_than_rendered() {
        assert_eq!(
            names("good.txt\nbell\u{7}name\nescape\u{1b}[31m\n"),
            vec![("good.txt".to_string(), false)],
            "`ls -1` cannot escape these, and a picker row is a path we later quote into a shell"
        );
    }

    #[test]
    fn a_blank_line_is_not_a_file() {
        assert_eq!(names("\n\nreal\n\n"), vec![("real".to_string(), false)]);
    }

    #[test]
    fn a_carriage_return_inside_a_name_is_dropped_but_a_crlf_line_ending_is_not() {
        assert_eq!(names("win\rdows.txt\n"), Vec::new());
        assert_eq!(
            names("windows.txt\r\n"),
            vec![("windows.txt".to_string(), false)],
            "a CRLF transport must not cost the listing every entry"
        );
    }
}
