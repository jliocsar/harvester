use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub dest: String,
    pub port: Option<u16>,
}

const SSH_BASE_ARGS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ControlMaster=auto",
    "-o",
    "ControlPersist=60",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "StreamLocalBindUnlink=yes",
];

const CONTROL_SOCKET_TEMPLATE: &str = "harvester-ssh-%C";

pub(crate) fn client_socket_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();

    DIR.get_or_init(|| match try_client_socket_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("~/.harvester/run unavailable ({error}); falling back to the temp dir");

            std::env::temp_dir()
        }
    })
}

fn try_client_socket_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$HOME unset"))?;
    let dir = PathBuf::from(home).join(".harvester/run");

    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;

    Ok(dir)
}

impl SshTarget {
    pub(crate) fn command(&self, extra_args: &[&str]) -> tokio::process::Command {
        let control_path = client_socket_dir().join(CONTROL_SOCKET_TEMPLATE);
        let mut command = tokio::process::Command::new("ssh");
        command.args(SSH_BASE_ARGS);
        command
            .arg("-o")
            .arg(format!("ControlPath={}", control_path.display()));
        command.args(extra_args);

        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }

        command.arg(&self.dest);
        command
    }
}
