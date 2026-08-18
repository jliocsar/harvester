use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ssh::SshTarget;

#[derive(Debug, Clone)]
pub enum Host {
    Local,
    Ssh(SshTarget),
}

impl Host {
    pub(crate) async fn exec(
        &self,
        script: &str,
        stdin_data: Option<&[u8]>,
        timeout: Duration,
    ) -> std::io::Result<ExecOutput> {
        let command = match self {
            Host::Local => {
                let mut command = Command::new("sh");
                command.args(["-c", script]);

                command
            }
            Host::Ssh(target) => {
                let mut command = target.command(&[]);
                command.arg(format!("exec sh -c {}", shell_quote(script)));

                command
            }
        };

        match tokio::time::timeout(timeout, run(command, stdin_data)).await {
            Ok(result) => result,
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {timeout:?}"),
            )),
        }
    }
}

pub(crate) const HOME_SCRIPT: &str = r#"printf '%s\n' "$HOME""#;

pub(crate) fn parse_home(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|home| home.starts_with('/'))
}

pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) struct ExecOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub(crate) fn require_success(&self, context: &'static str) -> crate::Result<()> {
        if self.status.success() {
            return Ok(());
        }

        Err(crate::Error::Exec {
            context,
            detail: self.describe_failure(),
        })
    }

    pub(crate) fn describe_failure(&self) -> String {
        format!("{} — {}", self.status, self.stderr.trim())
    }
}

pub(crate) async fn run(
    mut command: Command,
    stdin_data: Option<&[u8]>,
) -> std::io::Result<ExecOutput> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = command.spawn()?;
    let feed = stdin_data.map(|data| {
        let mut stdin = child.stdin.take().expect("stdin piped above");
        let data = data.to_vec();

        tokio::spawn(async move {
            if let Err(error) = stdin.write_all(&data).await {
                eprintln!("exec: writing stdin failed: {error}");
            }
        })
    });

    let output = child.wait_with_output().await?;

    if let Some(feed) = feed {
        let _ = feed.await;
    }

    Ok(ExecOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A_MOMENT: Duration = Duration::from_secs(10);

    async fn echo(value: &str) -> String {
        Host::Local
            .exec(
                &format!("printf '%s' {}", shell_quote(value)),
                None,
                A_MOMENT,
            )
            .await
            .expect("a local sh -c always runs")
            .stdout
    }

    #[tokio::test]
    async fn a_quoted_value_reaches_the_shell_verbatim() {
        for value in [
            "plain",
            "with spaces",
            "it's",
            "''",
            "$HOME",
            "`id`",
            "$(id)",
            "back\\slash",
            "semi; colon",
            "pipe | amp & redirect > file",
            "glob*?[a-z]",
            "\"double quoted\"",
            "~/tilde",
            "üñïçødé",
        ] {
            assert_eq!(echo(value).await, value, "value {value:?}");
        }
    }

    #[tokio::test]
    async fn a_quoted_value_cannot_end_its_own_quoting() {
        let attempt = "'; touch /tmp/harvester-shell-quote-escaped; echo '";

        assert_eq!(echo(attempt).await, attempt);
        assert!(!std::path::Path::new("/tmp/harvester-shell-quote-escaped").exists());
    }

    #[tokio::test]
    async fn a_non_zero_exit_carries_the_child_s_own_diagnosis() {
        let output = Host::Local
            .exec("echo 'no such thing' >&2; exit 3", None, A_MOMENT)
            .await
            .expect("the exec itself succeeds");

        let Err(crate::Error::Exec { context, detail }) = output.require_success("open a widget")
        else {
            panic!("exit 3 must not read as success");
        };

        assert_eq!(context, "open a widget");
        assert!(detail.contains("no such thing"), "{detail}");
    }

    #[tokio::test]
    async fn stdin_is_fed_without_deadlocking_on_a_full_output_pipe() {
        let payload = vec![b'x'; 4 * 1024 * 1024];

        let output = Host::Local
            .exec("cat", Some(&payload), A_MOMENT)
            .await
            .expect("cat runs");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), payload.len());
    }

    #[tokio::test]
    async fn a_script_that_never_ends_is_stopped_by_its_deadline() {
        let Err(error) = Host::Local
            .exec("sleep 30", None, Duration::from_millis(200))
            .await
        else {
            panic!("the deadline must fire");
        };

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }
}
