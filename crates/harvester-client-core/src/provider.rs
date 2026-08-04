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

use serde::Deserialize;
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
        // One unreadable entry is not a reason to lose every provider: the
        // caller (machines.rs) turns a discovery error into "change nothing
        // at all", so a single dangling symlink would freeze the machine
        // list forever. Skip and log, like the non-UTF-8 arm below.
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("harvester: skipping unreadable provider directory entry: {error}");

                continue;
            }
        };
        let path = entry.path();

        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            eprintln!("harvester: ignoring non-UTF-8 provider name: {path:?}");

            continue;
        };

        if name.starts_with('.') {
            continue;
        }

        // std::fs::metadata follows symlinks, so a symlink to an executable
        // registers like the executable itself — and a *dangling* symlink is
        // just an entry we cannot classify, not a discovery failure.
        let Ok(metadata) = std::fs::metadata(&path) else {
            eprintln!("harvester: skipping unreadable provider entry {path:?}");

            continue;
        };

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

        check_info(&self.name, result)
    }

    /// `list`: enumerate machines. Entries missing required fields are
    /// skipped with a stderr note rather than failing the whole list.
    pub async fn list_machines(&self) -> Result<Vec<ProviderMachine>> {
        let result = self.invoke("list", None).await?;

        parse_machine_list(&self.name, result)
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
        provider_error(&self.name, detail)
    }
}

fn provider_error(provider: &str, detail: String) -> Error {
    Error::Provider {
        provider: provider.to_string(),
        detail,
    }
}

/// The `info` result (docs/PROVIDERS.md §info). Unknown fields are ignored on
/// purpose: an older Harvester must compose with a newer provider.
#[derive(Deserialize)]
struct InfoResult {
    protocol: u64,
    /// Required by contract. Nothing consumes the reported identity — the UI
    /// labels machines by the provider's file name — so deserializing it *is*
    /// the validation.
    #[allow(dead_code)]
    name: String,
    /// Required by contract (PROVIDERS.md §capabilities). The capability-driven
    /// UI arrives with the machine-CRUD slice; until then, same story as `name`.
    #[allow(dead_code)]
    capabilities: Vec<String>,
}

/// The `list` result. Machines stay `Value` so one malformed entry can be
/// skipped with its own serde diagnostic instead of failing the whole list.
#[derive(Deserialize)]
struct ListResult {
    machines: Vec<Value>,
}

/// One machine object (docs/PROVIDERS.md §list). `status` is required by
/// contract; `ssh` is one of two shapes, so it is decoded separately.
#[derive(Deserialize)]
struct MachineEntry {
    id: String,
    label: String,
    status: String,
    ssh: Value,
}

/// The ProxyCommand-style escape hatch.
#[derive(Deserialize)]
struct ArgvSshEntry {
    argv: Vec<String>,
}

/// The plain host form; `user`/`port` default to the current user and 22.
#[derive(Deserialize)]
struct HostSshEntry {
    host: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

/// The protocol gate: the version must match, and the contract's other
/// required `info` fields must be present (serde reports which one is not).
fn check_info(provider: &str, result: Value) -> Result<()> {
    let info: InfoResult = serde_json::from_value(result)
        .map_err(|error| provider_error(provider, format!("info: {error}")))?;

    if info.protocol != SUPPORTED_PROVIDER_PROTOCOL {
        return Err(provider_error(
            provider,
            format!(
                "speaks provider protocol {}; \
                 this Harvester supports {SUPPORTED_PROVIDER_PROTOCOL}",
                info.protocol
            ),
        ));
    }

    Ok(())
}

/// Decodes a `list` result. A malformed *entry* is skipped with serde's own
/// diagnostic ("missing field `label`"); only a malformed *result* fails.
fn parse_machine_list(provider: &str, result: Value) -> Result<Vec<ProviderMachine>> {
    let list: ListResult = serde_json::from_value(result)
        .map_err(|error| provider_error(provider, format!("list: {error}")))?;
    let mut machines = Vec::new();

    for entry in list.machines {
        match parse_machine(entry) {
            Ok(machine) => machines.push(machine),
            Err(error) => {
                eprintln!("harvester: provider {provider}: skipping machine entry: {error}")
            }
        }
    }

    Ok(machines)
}

fn parse_machine(entry: Value) -> serde_json::Result<ProviderMachine> {
    let entry: MachineEntry = serde_json::from_value(entry)?;
    let ssh = parse_ssh_info(entry.ssh)?;

    Ok(ProviderMachine {
        id: entry.id,
        label: entry.label,
        status: parse_status(&entry.status),
        ssh,
    })
}

/// Unrecognized status strings degrade to Unknown (forward compat) — the
/// field is advisory either way.
fn parse_status(status: &str) -> MachineStatus {
    match status {
        "running" => MachineStatus::Running,
        "stopped" => MachineStatus::Stopped,
        _ => MachineStatus::Unknown,
    }
}

/// The two `ssh` shapes, tried in the contract's precedence order: `argv`
/// first, the host form otherwise. Deliberately *not* an untagged enum —
/// untagged reports "data did not match any variant", which throws away the
/// diagnostic ("missing field `host`") that the caller logs.
fn parse_ssh_info(ssh: Value) -> serde_json::Result<SshInfo> {
    if ssh.get("argv").is_some() {
        let argv: ArgvSshEntry = serde_json::from_value(ssh)?;

        return Ok(SshInfo::Argv(argv.argv));
    }

    let host: HostSshEntry = serde_json::from_value(ssh)?;

    Ok(SshInfo::Host {
        host: host.host,
        user: host.user,
        port: host.port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// A `list` result carrying `entries` verbatim.
    fn list_result(entries: Value) -> Value {
        json!({ "machines": entries })
    }

    fn host_machine(id: &str) -> Value {
        json!({
            "id": id,
            "label": id,
            "status": "running",
            "ssh": {"host": format!("{id}.exe.xyz"), "user": "exedev", "port": 22},
        })
    }

    #[test]
    fn parses_a_well_formed_machine_list() {
        let machines = parse_machine_list("exe-dev", list_result(json!([host_machine("bloggy")])))
            .expect("a well-formed list must parse");

        assert_eq!(machines.len(), 1);
        assert_eq!(machines[0].id, "bloggy");
        assert_eq!(machines[0].label, "bloggy");
        assert_eq!(machines[0].status, MachineStatus::Running);

        let SshInfo::Host { host, user, port } = &machines[0].ssh else {
            panic!("the host form must decode as SshInfo::Host");
        };

        assert_eq!(host, "bloggy.exe.xyz");
        assert_eq!(user.as_deref(), Some("exedev"));
        assert_eq!(*port, Some(22));
    }

    #[test]
    fn host_form_defaults_user_and_port() {
        let entry = json!({
            "id": "vps",
            "label": "vps",
            "status": "stopped",
            "ssh": {"host": "vps.exe.xyz"},
        });
        let machines =
            parse_machine_list("exe-dev", list_result(json!([entry]))).expect("must parse");

        assert_eq!(machines[0].status, MachineStatus::Stopped);

        let SshInfo::Host { user, port, .. } = &machines[0].ssh else {
            panic!("the host form must decode as SshInfo::Host");
        };

        assert_eq!(*user, None);
        assert_eq!(*port, None);
    }

    #[test]
    fn parses_the_argv_ssh_form() {
        let entry = json!({
            "id": "myapp",
            "label": "myapp",
            "status": "unknown",
            "ssh": {"argv": ["fly", "ssh", "console", "-a", "myapp", "-C"]},
        });
        let machines = parse_machine_list("fly", list_result(json!([entry]))).expect("must parse");

        assert_eq!(machines[0].status, MachineStatus::Unknown);

        let SshInfo::Argv(argv) = &machines[0].ssh else {
            panic!("the argv form must decode as SshInfo::Argv");
        };

        assert_eq!(argv.len(), 6);
        assert_eq!(argv[0], "fly");
    }

    /// An unrecognized status is advisory-only, so it degrades rather than
    /// dropping the machine (docs/PROVIDERS.md §list).
    #[test]
    fn unknown_status_string_degrades_to_unknown() {
        let mut entry = host_machine("bloggy");
        entry["status"] = json!("provisioning");

        let machines =
            parse_machine_list("exe-dev", list_result(json!([entry]))).expect("must parse");

        assert_eq!(machines[0].status, MachineStatus::Unknown);
    }

    /// The point of per-entry decoding: one bad machine costs one machine.
    #[test]
    fn a_machine_missing_a_required_field_skips_only_that_entry() {
        let entries = json!([
            host_machine("first"),
            json!({"id": "no-label", "status": "running", "ssh": {"host": "x"}}),
            json!({"id": "no-status", "label": "no-status", "ssh": {"host": "x"}}),
            json!({"id": "bad-ssh", "label": "bad-ssh", "status": "running", "ssh": {}}),
            host_machine("last"),
        ]);
        let machines =
            parse_machine_list("exe-dev", list_result(entries)).expect("the list itself is valid");

        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].id, "first");
        assert_eq!(machines[1].id, "last");
    }

    #[test]
    fn a_list_without_machines_is_an_error() {
        let error = parse_machine_list("exe-dev", json!({}))
            .expect_err("`machines` is required by contract");

        assert!(error.to_string().contains("machines"), "{error}");
    }

    #[test]
    fn info_accepts_the_supported_protocol() {
        let info = json!({
            "protocol": SUPPORTED_PROVIDER_PROTOCOL,
            "name": "exe.dev",
            "capabilities": ["create", "delete"],
        });

        check_info("exe-dev", info).expect("a contract-compliant info must pass");
    }

    #[test]
    fn info_rejects_an_unsupported_protocol_version() {
        let info = json!({"protocol": 2, "name": "exe.dev", "capabilities": []});
        let error = check_info("exe-dev", info).expect_err("protocol 2 is not supported");

        assert!(error.to_string().contains("protocol 2"), "{error}");
    }

    /// `capabilities` is required by PROVIDERS.md §info and nothing used to
    /// check it.
    #[test]
    fn info_rejects_missing_required_fields() {
        let missing_capabilities = json!({"protocol": 1, "name": "exe.dev"});
        let error =
            check_info("exe-dev", missing_capabilities).expect_err("capabilities is required");

        assert!(error.to_string().contains("capabilities"), "{error}");

        let missing_name = json!({"protocol": 1, "capabilities": []});
        let error = check_info("exe-dev", missing_name).expect_err("name is required");

        assert!(error.to_string().contains("name"), "{error}");
    }
}
