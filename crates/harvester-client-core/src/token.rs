use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RequestToken(u64);

#[derive(Debug, Default)]
pub(crate) struct RequestTokens {
    next: AtomicU64,
}

impl RequestTokens {
    pub(crate) fn mint(&self) -> RequestToken {
        let value = self
            .next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("request token exhausted");

        RequestToken(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_is_distinct_from_the_last() {
        let tokens = RequestTokens::default();
        let first = tokens.mint();
        let second = tokens.mint();

        assert_ne!(first, second);
    }
}
