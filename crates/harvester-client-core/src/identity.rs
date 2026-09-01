use harvester_proto::is_valid_session_id;
use serde::{Deserialize, Deserializer, Serialize};

pub(crate) const LOCAL_MACHINE: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct MachineId(String);

impl MachineId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();

        (!value.is_empty()).then_some(Self(value))
    }

    pub fn local() -> Self {
        Self(LOCAL_MACHINE.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MachineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<MachineId> for String {
    fn from(value: MachineId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for MachineId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;

        Self::new(value).ok_or_else(|| serde::de::Error::custom("machine id must not be empty"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();

        is_valid_session_id(&value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;

        Self::new(value).ok_or_else(|| serde::de::Error::custom("invalid session id"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionKey {
    machine: MachineId,
    session: SessionId,
}

impl SessionKey {
    pub const fn new(machine: MachineId, session: SessionId) -> Self {
        Self { machine, session }
    }

    pub const fn machine(&self) -> &MachineId {
        &self.machine
    }

    pub const fn session(&self) -> &SessionId {
        &self.session
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DraftId(pub(crate) u64);

impl DraftId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceBrowserId(pub(crate) u64);

impl WorkspaceBrowserId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubmissionId(pub(crate) u64);

impl SubmissionId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ChatId {
    Draft(DraftId),
    Session(SessionKey),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_values_enforce_their_wire_invariants() {
        assert!(MachineId::new("").is_none());
        assert!(SessionId::new("").is_none());
        assert!(SessionId::new("../escape").is_none());
        assert!(serde_json::from_str::<MachineId>(r#"""#).is_err());
        assert!(serde_json::from_str::<SessionId>(r#""../escape""#).is_err());
    }

    #[test]
    fn wire_identities_remain_transparent_strings() {
        let machine = MachineId::new("exe-dev/bloggy").expect("non-empty");
        let session = SessionId::new("0199-session").expect("non-empty");

        assert_eq!(
            serde_json::to_string(&machine).expect("serializes"),
            r#""exe-dev/bloggy""#
        );
        assert_eq!(
            serde_json::to_string(&session).expect("serializes"),
            r#""0199-session""#
        );
    }
}
