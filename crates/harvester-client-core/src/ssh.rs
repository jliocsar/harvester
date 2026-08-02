//! Plain-SSH plumbing shared by bootstrap (one-shot execs) and connection
//! (the long-lived socket forward).

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// How to reach a machine. `dest` is handed to ssh verbatim, so both
/// `user@host` and ssh-config aliases work.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub dest: String,
    pub port: Option<u16>,
}

/// Non-interactive by contract: the client must fail fast, never hang on a
/// password or host-key prompt. `accept-new` trusts a first-seen host key
/// without a prompt (fresh VMs would otherwise be unreachable under
/// BatchMode) but still hard-fails if a known key *changes*.
const SSH_BASE_ARGS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

impl SshTarget {
    /// An ssh invocation against this target; `extra_args` land before the
    /// destination (ssh requires options first).
    pub(crate) fn command(&self, extra_args: &[&str]) -> Command {
        let mut command = Command::new("ssh");
        command.args(SSH_BASE_ARGS);
        command.args(extra_args);

        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }

        command.arg(&self.dest);
        command
    }
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

/// Runs `script` through the remote login shell, optionally piping
/// `stdin_data` into it, and waits for completion.
pub(crate) async fn exec(
    target: &SshTarget,
    script: &str,
    stdin_data: Option<&[u8]>,
) -> std::io::Result<ExecOutput> {
    let mut command = target.command(&[]);
    command.arg(script);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    command.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = command.spawn()?;

    if let Some(data) = stdin_data {
        let mut stdin = child.stdin.take().expect("stdin piped above");
        stdin.write_all(data).await?;
        stdin.shutdown().await?;

        // EOF is only delivered when the handle is dropped, not on shutdown()
        // (the same fact that bit harvesterd's stop path).
        drop(stdin);
    }

    let output = child.wait_with_output().await?;

    Ok(ExecOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
