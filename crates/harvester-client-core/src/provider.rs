use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::host;
use crate::ssh::SshTarget;
use crate::{Error, Result};

pub const SUPPORTED_PROVIDER_PROTOCOL: u64 = 1;

const INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

const BUNDLED_EXE_DEV: &str = include_str!("../../../providers/exe-dev");

const BUNDLED_EXE_DEV_NAME: &str = "exe-dev";

const BUNDLED_PROVIDER_MODE: u32 = 0o755;

#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MachineKey {
    pub provider: String,
    pub id: String,
}

impl MachineKey {
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

#[derive(Debug, Clone)]
pub struct ProviderMachine {
    pub id: String,
    pub label: String,
    pub ssh: SshInfo,
}

#[derive(Debug, Clone)]
pub enum SshInfo {
    Host {
        host: String,
        user: Option<String>,
        port: Option<u16>,
    },
    Argv(#[allow(dead_code)] Vec<String>),
}

impl SshInfo {
    pub fn target(&self) -> Option<SshTarget> {
        let SshInfo::Host { host, user, port } = self else {
            return None;
        };
        let dest = match user {
            Some(user) => format!("{user}@{host}"),
            None => host.clone(),
        };

        Some(SshTarget { dest, port: *port })
    }
}

fn providers_dir() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) => Ok(PathBuf::from(home).join(".harvester/providers")),
        None => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "$HOME unset; provider directory unavailable",
        ))),
    }
}

pub fn install_bundled_providers() -> Result<()> {
    let path = providers_dir()?.join(BUNDLED_EXE_DEV_NAME);

    if installed_matches_bundle(&path) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let staged = path.with_extension(format!("next.{}", std::process::id()));

    if let Err(error) = write_bundled_provider(&staged, &path) {
        let _ = std::fs::remove_file(&staged);

        return Err(error);
    }

    Ok(())
}

fn installed_matches_bundle(path: &std::path::Path) -> bool {
    matches!(std::fs::read(path), Ok(installed) if installed == BUNDLED_EXE_DEV.as_bytes())
}

fn write_bundled_provider(staged: &std::path::Path, path: &std::path::Path) -> Result<()> {
    std::fs::write(staged, BUNDLED_EXE_DEV)?;

    let mut permissions = std::fs::metadata(staged)?.permissions();
    permissions.set_mode(BUNDLED_PROVIDER_MODE);
    std::fs::set_permissions(staged, permissions)?;
    std::fs::rename(staged, path)?;

    Ok(())
}

pub fn discover_providers() -> Result<Vec<Provider>> {
    let dir = providers_dir()?;

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::Io(error)),
    };

    let mut providers = Vec::new();

    for entry in entries {
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
    pub async fn check_protocol(&self) -> Result<()> {
        let result = self.invoke("info", None).await?;

        check_info(&self.name, result)
    }

    pub async fn list_machines(&self) -> Result<Vec<ProviderMachine>> {
        let result = self.invoke("list", None).await?;

        parse_machine_list(&self.name, result)
    }

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

        match serde_json::from_str::<ErrorResult>(&output.stdout) {
            Ok(reported) => Err(self.error(reported.error)),
            Err(_not_the_contract_shape) => Err(self.error(format!(
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

#[derive(Deserialize)]
struct ErrorResult {
    error: String,
}

#[derive(Deserialize)]
struct InfoResult {
    protocol: u64,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
struct ListResult {
    machines: Vec<Value>,
}

#[derive(Deserialize)]
struct MachineEntry {
    id: String,
    label: String,
    #[allow(dead_code)]
    status: String,
    ssh: Value,
}

#[derive(Deserialize)]
struct ArgvSshEntry {
    argv: Vec<String>,
}

#[derive(Deserialize)]
struct HostSshEntry {
    host: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

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
        ssh,
    })
}

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

        let SshInfo::Argv(argv) = &machines[0].ssh else {
            panic!("the argv form must decode as SshInfo::Argv");
        };

        assert_eq!(argv.len(), 6);
        assert_eq!(argv[0], "fly");
    }

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
    fn only_a_byte_identical_install_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("harvester-bundle-{}", std::process::id()));

        std::fs::create_dir_all(&dir).expect("test dir");

        let path = dir.join(BUNDLED_EXE_DEV_NAME);

        assert!(
            !installed_matches_bundle(&path),
            "nothing installed is not a match"
        );

        std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write");

        assert!(
            !installed_matches_bundle(&path),
            "a shipped fix must reach an install that predates it"
        );

        std::fs::write(&path, BUNDLED_EXE_DEV).expect("write");

        assert!(installed_matches_bundle(&path));

        std::fs::remove_dir_all(&dir).expect("test dir cleanup");
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
