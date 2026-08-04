//! Machine identity (docs/SPEC.md §Machine): the trichotomy that decides
//! what a stored machine string refers to, and therefore which host the
//! client execs on. Decided once, here — [`Host::Local`](crate::Host) is
//! already a client-core concept, and a frontend re-deriving these semantics
//! from [`MachineKey`] alone drifts from them.

use crate::provider::MachineKey;

/// The hardcoded local machine's key: not a provider machine, never
/// deletable — harvesterd runs as a detached child there, no SSH.
pub const LOCAL_MACHINE: &str = "local";

/// What a stored machine string (workspaces.json, the argv destination)
/// refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachineRef {
    Local,
    /// A `<provider>/<id>` key; the connection info comes from that
    /// provider's `list`, so such a machine is only resolvable once its
    /// provider has been listed.
    Provider(MachineKey),
    /// A plain SSH destination, used verbatim: the argv launch form and
    /// legacy keyless workspaces.json entries.
    AdHoc(String),
}

impl MachineRef {
    /// Total by construction — every string is one of the three (an
    /// unresolvable *reference* is still a well-formed one; resolving it to a
    /// host is the caller's separate question).
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
        // The split is on the *first* separator — the provider name is the
        // one part Harvester owns the format of.
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
        // The argv launch form and legacy workspaces.json entries.
        assert_eq!(
            MachineRef::parse("exedev@vm.exe.dev"),
            MachineRef::AdHoc("exedev@vm.exe.dev".to_string())
        );
    }

    #[test]
    fn local_only_wins_as_the_whole_string() {
        // A provider that happens to be named `local` is still a provider.
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
