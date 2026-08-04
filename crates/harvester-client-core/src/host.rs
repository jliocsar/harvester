//! The one exec boundary for both machine kinds (docs/SPEC.md §Machine).
//! Every control-plane script runs through `sh` either way — `sh -c` on this
//! machine, `exec sh -c` on the far side of ssh — so bootstrap and workspace
//! browsing are written once against [`Host::exec`] and cannot drift between
//! the two, nor depend on the remote user's login shell. Only the socket
//! connect differs (forward vs direct), and that split lives in
//! `connection.rs`.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ssh::SshTarget;

/// Where to run control-plane commands: this machine, or an SSH destination.
#[derive(Debug, Clone)]
pub enum Host {
    Local,
    Ssh(SshTarget),
}

impl Host {
    /// Runs `script` through the host's POSIX shell, optionally piping
    /// `stdin_data` into it, and waits for completion — at most `timeout`.
    /// Every remote op carries a deadline: ssh's ConnectTimeout only bounds
    /// the TCP connect, so without one a wedged transport (or an unbounded
    /// remote loop) would hang its bridge forever.
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
                // sshd runs the command through the user's *login* shell, which
                // is only POSIX by luck — fish and csh reject the arithmetic,
                // async subshells and `&&` chains our scripts are written in.
                // Re-exec through `sh` so both arms really do get the POSIX
                // shell this module promises.
                let mut command = target.command(&[]);
                command.arg(format!("exec sh -c {}", shell_quote(script)));

                command
            }
        };

        match tokio::time::timeout(timeout, run(command, stdin_data)).await {
            Ok(result) => result,
            // The timed-out child was dropped mid-wait — `kill_on_drop`
            // reaps it; nothing lingers.
            Err(_elapsed) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {timeout:?}"),
            )),
        }
    }
}

/// Single-quotes `value` for the POSIX shell (the `'\''` dance), so arbitrary
/// characters survive the command-line round trip. Lives next to the exec
/// boundary because that is what it protects: both the scripts sent through
/// [`Host::exec`] and the paths interpolated into them.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

pub(crate) struct ExecOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    /// One-line failure description for error messages.
    pub(crate) fn describe_failure(&self) -> String {
        format!("{} — {}", self.status, self.stderr.trim())
    }
}

/// Spawn `command`, feed it `stdin_data` (EOF by dropping the handle — a mere
/// `shutdown()` never closes a pipe fd, the same fact that bit harvesterd's
/// stop path), and wait for completion. Callers own the deadline; the child
/// is `kill_on_drop`, so cancelling this future kills it.
///
/// The stdin feed runs as its own task so writing and draining the child's
/// output overlap. Doing them in sequence deadlocks whenever the child fills
/// its output pipe before consuming all of stdin: it blocks writing, we block
/// writing, and nobody reads — the multi-megabyte daemon upload is exactly the
/// shape that hits it.
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
            // A broken pipe here is normal (the child exited early); its exit
            // status is the real diagnosis, so only log the surprising part.
            if let Err(error) = stdin.write_all(&data).await {
                eprintln!("exec: writing stdin failed: {error}");
            }
            // Dropping the handle is what delivers EOF.
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
