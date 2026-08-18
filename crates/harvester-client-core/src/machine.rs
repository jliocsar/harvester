use crate::provider::MachineKey;

pub const LOCAL_MACHINE: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineRef {
    Local,
    Provider(MachineKey),
    AdHoc(String),
}

impl MachineRef {
    pub fn parse(stored: &str) -> MachineRef {
        if stored == LOCAL_MACHINE {
            return MachineRef::Local;
        }

        match MachineKey::parse(stored) {
            Some(key) => MachineRef::Provider(key),
            None => MachineRef::AdHoc(stored.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_machine_is_its_own_kind() {
        assert_eq!(MachineRef::parse(LOCAL_MACHINE), MachineRef::Local);
    }

    #[test]
    fn a_slash_separated_key_is_a_provider_machine() {
        assert_eq!(
            MachineRef::parse("exe-dev/bloggy"),
            MachineRef::Provider(MachineKey {
                provider: "exe-dev".to_string(),
                id: "bloggy".to_string(),
            })
        );
    }

    #[test]
    fn an_id_may_itself_contain_slashes() {
        assert_eq!(
            MachineRef::parse("exe-dev/team/bloggy"),
            MachineRef::Provider(MachineKey {
                provider: "exe-dev".to_string(),
                id: "team/bloggy".to_string(),
            })
        );
    }

    #[test]
    fn a_bare_destination_is_ad_hoc() {
        assert_eq!(
            MachineRef::parse("exedev@vm.exe.dev"),
            MachineRef::AdHoc("exedev@vm.exe.dev".to_string())
        );
    }

    #[test]
    fn local_only_wins_as_the_whole_string() {
        assert_eq!(
            MachineRef::parse("local/vm"),
            MachineRef::Provider(MachineKey {
                provider: "local".to_string(),
                id: "vm".to_string(),
            })
        );
        assert_eq!(
            MachineRef::parse("localhost"),
            MachineRef::AdHoc("localhost".to_string())
        );
    }
}
