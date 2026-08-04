//! Machine providers (docs/PROVIDERS.md): user-supplied one-shot executables
//! in `~/.harvester/providers/` answering *what machines exist and how do I
//! SSH into them*. Control plane only — session traffic never touches a
//! provider. Each operation is one local exec: args (if any) as one JSON
//! object on stdin, result as one JSON object on stdout, diagnostics on
//! stderr (logged, never parsed).
//!
//! This slice implements the mandatory core (`info`, `list`); `create` and
//! `delete` arrive with the machine-CRUD slice.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::host;
use crate::ssh::SshTarget;
use crate::{Error, Result};

/// The provider protocol version this Harvester speaks. Providers reporting
/// anything else are rejected (docs/PROVIDERS.md §info).
pub const SUPPORTED_PROVIDER_PROTOCOL: u64 = 1;

/// Contract timeout for one provider invocation; the invocation is killed
/// when it elapses. (`create` gets 120s by contract — constant arrives with
/// the machine-CRUD slice.)
const INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

/// The default provider shipped with Harvester (docs/PROVIDERS.md §Reference
/// implementation) — implemented in the same format as user providers, no
/// privileged code paths.
const BUNDLED_EXE_DEV: &str = include_str!("../../../providers/exe-dev");

/// A discovered provider executable. The path is private so every invocation
/// goes through the contract (`invoke`: timeout, exit-code rules, stderr
/// logging).
#[derive(Debug, Clone)]
pub struct Provider {
    /// The file name — the provider id, shown in the UI and the first half
    /// of every machine key.
    pub name: String,
    path: PathBuf,
}

/// How Harvester keys a machine: `<provider>/<id>` (docs/SPEC.md §Machine).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MachineKey {
    pub provider: String,
    pub id: String,
}

impl MachineKey {
    /// Splits a stored `<provider>/<id>` string. `None` when there is no
    /// separator — such strings are ad-hoc SSH destinations, not provider
    /// machines (legacy workspaces.json entries, the argv destination).
    pub fn parse(stored: &str) -> Option<MachineKey> {
        let (provider, id) = stored.split_once('/')?;

        Some(MachineKey {
            provider: provider.to_string(),
            id: id.to_string(),
        })
    }
}

impl std::fmt::Display for MachineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.provider, self.id)
    }
}

/// Provider-reported machine status. Advisory only — the UI dot is driven by
/// Harvester's own SSH probe, never this field (docs/PROVIDERS.md §list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachineStatus {
    Running,
    Stopped,
    Unknown,
}

/// One machine from a provider's `list`.
#[derive(Debug, Clone)]
pub struct ProviderMachine {
    pub id: String,
    pub label: String,
    pub status: MachineStatus,
    pub ssh: SshInfo,
}

/// A machine's SSH connection info: plain host form, or the ProxyCommand-
/// style argv escape hatch. Argv is parsed (a provider using it must still
/// list cleanly) but not yet wired into a connection.
#[derive(Debug, Clone)]
pub enum SshInfo {
    Host {
        host: String,
        user: Option<String>,
        port: Option<u16>,
    },
    Argv(Vec<String>),
}

impl SshInfo {
    /// Derives the `SshTarget` all session/workspace traffic uses.
    pub fn target(&self) -> Result<SshTarget> {
        match self {
            SshInfo::Host { host, user, port } => {
                let dest = match user {
                    Some(user) => format!("{user}@{host}"),
                    None => host.clone(),
                };

                Ok(SshTarget { dest, port: *port })
            }
            SshInfo::Argv(_) => Err(Error::Exec {
                context: "derive ssh target",
                detail: "the argv ssh form is not supported yet — \
                         use {host, user, port}"
                    .to_string(),
            }),
        }
    }
}

/// `~/.harvester/providers`.
fn providers_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".harvester/providers")),
        None => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset; provider directory unavailable",
        ))),
    }
}

/// Drops the bundled `exe-dev` provider into the providers dir when absent.
/// Absence is the only trigger, so deleting the file revives it on the next
/// launch — the supported way to disable a provider is clearing its
/// executable bit (non-executable files are ignored by discovery).
pub fn install_bundled_providers() -> Result<()> {
    let path = providers_dir()?.join("exe-dev");

    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&path, BUNDLED_EXE_DEV)?;

    // Registered by the executable bit (docs/PROVIDERS.md §Discovery).
    let mut permissions = std::fs::metadata(&path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions)?;

    Ok(())
}

/// Enumerates `~/.harvester/providers`: executable regular files register,
/// dotfiles and non-executables are ignored (docs/PROVIDERS.md §Discovery).
/// A missing directory is an empty list, not an error (first run).
pub fn discover_providers() -> Result<Vec<Provider>> {
    let dir = providers_dir()?;

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::Io(error)),
    };

    let mut providers = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            eprintln!("harvester: ignoring non-UTF-8 provider name: {path:?}");

            continue;
        };

        if name.starts_with('.') {
            continue;
        }

        // std::fs::metadata follows symlinks, so a symlink to an executable
        // registers like the executable itself.
        let metadata = std::fs::metadata(&path)?;

        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }

        providers.push(Provider {
            name: name.to_string(),
            path,
        });
    }

    providers.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(providers)
}

impl Provider {
    /// `info`: the protocol gate. Nothing consumes the reported identity —
    /// the UI labels machines by the provider's file name (PROVIDERS.md
    /// §Discovery) — so this checks the contract and returns nothing. A typed
    /// result belongs to the slice that first needs `capabilities` /
    /// `create_fields`, which is also the slice that defines its shape.
    pub async fn check_protocol(&self) -> Result<()> {
        let result = self.invoke("info", None).await?;

        let Some(protocol) = result.get("protocol").and_then(Value::as_u64) else {
            return Err(self.error("info: missing required field \"protocol\"".to_string()));
        };

        if protocol != SUPPORTED_PROVIDER_PROTOCOL {
            return Err(self.error(format!(
                "speaks provider protocol {protocol}; \
                 this Harvester supports {SUPPORTED_PROVIDER_PROTOCOL}"
            )));
        }

        if result.get("name").and_then(Value::as_str).is_none() {
            return Err(self.error("info: missing required field \"name\"".to_string()));
        }

        Ok(())
    }

    /// `list`: enumerate machines. Entries missing required fields are
    /// skipped with a stderr note rather than failing the whole list.
    pub async fn list_machines(&self) -> Result<Vec<ProviderMachine>> {
        let result = self.invoke("list", None).await?;

        let Some(entries) = result.get("machines").and_then(Value::as_array) else {
            return Err(self.error("list: missing required field \"machines\"".to_string()));
        };

        let mut machines = Vec::new();

        for entry in entries {
            match parse_machine(entry) {
                Some(machine) => machines.push(machine),
                None => eprintln!(
                    "harvester: provider {}: skipping malformed machine entry: {entry}",
                    self.name
                ),
            }
        }

        Ok(machines)
    }

    /// One exec per operation, per contract: exit 0 = success and stdout
    /// parses as the result object; non-zero = failure with an optional
    /// `{"error"}` object surfaced verbatim. Killed at [`INVOKE_TIMEOUT`]
    /// (`kill_on_drop` — the timeout drops the child future).
    /// Spawning, piping and decoding are [`host::run`]'s job — the crate's one
    /// exec helper — so this keeps only what is provider-specific: the
    /// deadline and the contract's result classification. `stdin_data` is
    /// carried through for the operations that take a JSON object on stdin.
    async fn invoke(&self, operation: &'static str, stdin_data: Option<&[u8]>) -> Result<Value> {
        let mut command = tokio::process::Command::new(&self.path);
        command.arg(operation);

        let output =
            match tokio::time::timeout(INVOKE_TIMEOUT, host::run(command, stdin_data)).await {
                Ok(io_result) => io_result?,
                Err(_elapsed) => {
                    return Err(self.error(format!(
                        "`{operation}` timed out after {}s",
                        INVOKE_TIMEOUT.as_secs()
                    )))
                }
            };

        // Diagnostics are logged, never parsed (docs/PROVIDERS.md §Transport).
        let stderr = output.stderr.trim();

        if !stderr.is_empty() {
            eprintln!("harvester: provider {} `{operation}`: {stderr}", self.name);
        }

        if output.status.success() {
            return serde_json::from_str(&output.stdout).map_err(|parse_error| {
                self.error(format!(
                    "`{operation}` returned invalid JSON: {parse_error}"
                ))
            });
        }

        let reported: Option<String> = serde_json::from_str::<Value>(&output.stdout)
            .ok()
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string);

        match reported {
            Some(message) => Err(self.error(message)),
            None => Err(self.error(format!(
                "`{operation}` failed ({}) — {stderr}",
                output.status
            ))),
        }
    }

    fn error(&self, detail: String) -> Error {
        Error::Provider {
            provider: self.name.clone(),
            detail,
        }
    }
}

/// One machine object (docs/PROVIDERS.md §list). `None` on any missing or
/// malformed required field; the caller logs and skips.
fn parse_machine(entry: &Value) -> Option<ProviderMachine> {
    let id = entry.get("id")?.as_str()?.to_string();
    let label = entry.get("label")?.as_str()?.to_string();

    // Unrecognized status strings degrade to Unknown (forward compat) — the
    // field is advisory either way.
    let status = match entry.get("status")?.as_str()? {
        "running" => MachineStatus::Running,
        "stopped" => MachineStatus::Stopped,
        _ => MachineStatus::Unknown,
    };

    let ssh = parse_ssh_info(entry.get("ssh")?)?;

    Some(ProviderMachine {
        id,
        label,
        status,
        ssh,
    })
}

fn parse_ssh_info(ssh: &Value) -> Option<SshInfo> {
    if let Some(argv) = ssh.get("argv").and_then(Value::as_array) {
        let argv = argv
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()?;

        return Some(SshInfo::Argv(argv));
    }

    let host = ssh.get("host")?.as_str()?.to_string();
    let user = match ssh.get("user") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_str()?.to_string()),
    };
    let port = match ssh.get("port") {
        None | Some(Value::Null) => None,
        Some(value) => Some(u16::try_from(value.as_u64()?).ok()?),
    };

    Some(SshInfo::Host { host, user, port })
}
