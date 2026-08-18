use gpui::{Context, EventEmitter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalOwner {
    PiDialogs,
    AddRepo,
    ForgetMachine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModalLease {
    owner: ModalOwner,
    generation: u64,
}

pub(crate) struct ModalAvailable;

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
