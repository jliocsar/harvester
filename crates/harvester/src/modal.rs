//! Harvester's narrow arbitration layer over gpui-component's global Root
//! dialog stack.
//!
//! Root stores every dialog in one vector and `close_dialog` always pops its
//! top. Harvester therefore permits exactly one of its modal flows to own that
//! stack at a time: Pi's FIFO obligations, the repo picker, or the
//! forget-machine confirmation. A generation tagged lease makes delayed
//! callbacks from a completed flow harmless.

use gpui::{Context, EventEmitter};

/// The only Harvester flows that place dialogs on Root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalOwner {
    PiDialogs,
    AddRepo,
    ForgetMachine,
}

/// Proof that one flow currently owns Root's dialog stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModalLease {
    owner: ModalOwner,
    generation: u64,
}

/// Emitted after a valid lease releases Root for another flow.
pub(crate) struct ModalAvailable;

/// Window-free ownership state. It deliberately knows nothing about dialog
/// bodies, queues, or callbacks; those remain with the flows themselves.
#[derive(Debug, Default)]
struct ModalArbiter {
    active: Option<ModalLease>,
    next_generation: u64,
}

impl ModalArbiter {
    fn acquire(&mut self, owner: ModalOwner) -> Option<ModalLease> {
        if self.active.is_some() {
            return None;
        }

        let lease = ModalLease {
            owner,
            generation: self.next_generation,
        };
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("modal lease generation exhausted");
        self.active = Some(lease);

        Some(lease)
    }

    fn release(&mut self, lease: ModalLease) -> bool {
        if self.active != Some(lease) {
            return false;
        }

        self.active = None;

        true
    }

    fn owns(&self, lease: ModalLease) -> bool {
        self.active == Some(lease)
    }
}

/// GPUI entity shared by the Pi-dialog coordinator and the user-initiated
/// flows. `ModalAvailable` is only a wake-up; each flow still has to acquire a
/// fresh lease before it may touch Root.
#[derive(Default)]
pub(crate) struct ModalCoordinator {
    arbiter: ModalArbiter,
}

impl EventEmitter<ModalAvailable> for ModalCoordinator {}

impl ModalCoordinator {
    pub(crate) fn acquire(&mut self, owner: ModalOwner) -> Option<ModalLease> {
        self.arbiter.acquire(owner)
    }

    pub(crate) fn release(&mut self, lease: ModalLease, cx: &mut Context<Self>) {
        if !self.arbiter.release(lease) {
            return;
        }

        // Pi is the only waiting flow: every other flow starts from a user
        // click, which Root's focus trap prevents while Pi owns a dialog.
        // Avoiding a Pi-release event also avoids synchronously re-entering the
        // Pi coordinator that initiated that release.
        if lease.owner != ModalOwner::PiDialogs {
            cx.emit(ModalAvailable);
        }
    }

    pub(crate) fn owns(&self, lease: ModalLease) -> bool {
        self.arbiter.owns(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_harvester_flow_owns_root_at_a_time() {
        let mut arbiter = ModalArbiter::default();
        let repo = arbiter.acquire(ModalOwner::AddRepo).expect("repo lease");

        assert!(arbiter.acquire(ModalOwner::PiDialogs).is_none());
        assert!(arbiter.owns(repo));
    }

    #[test]
    fn releasing_the_owner_allows_the_waiting_flow_to_acquire() {
        let mut arbiter = ModalArbiter::default();
        let repo = arbiter.acquire(ModalOwner::AddRepo).expect("repo lease");

        assert!(arbiter.release(repo));

        let pi = arbiter
            .acquire(ModalOwner::PiDialogs)
            .expect("pi lease after release");
        assert!(arbiter.owns(pi));
    }

    #[test]
    fn a_stale_release_cannot_free_another_flows_modal() {
        let mut arbiter = ModalArbiter::default();
        let first = arbiter.acquire(ModalOwner::AddRepo).expect("first lease");
        assert!(arbiter.release(first));
        let second = arbiter
            .acquire(ModalOwner::PiDialogs)
            .expect("second lease");

        assert!(!arbiter.release(first));
        assert!(arbiter.owns(second));
    }
}
