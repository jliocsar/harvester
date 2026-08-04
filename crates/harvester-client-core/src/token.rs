//! Reply correlation for the client's workers (docs/SPEC.md §Decisions, C2).
//!
//! A frontend fires a command at a worker and gets the answer back as an
//! event, a round trip later. Which request an answer belongs to used to be
//! inferred from an echoed key — the `cwd` a model list was asked for, the
//! `path` a listing walked — which needs a bespoke staleness guard per
//! command and still cannot tell two in-flight requests with the same key
//! apart. A monotonic token is the whole mechanism instead: mint one per
//! request, echo it on the answer, apply the answer only when it matches the
//! token still recorded as outstanding.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Identifies one request/reply pair. Opaque on purpose — nothing may derive
/// meaning from the number, only compare it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestToken(u64);

/// Mints request tokens. Cloneable and shared by every surface that talks to
/// a worker (the workbench and each chat hold the same source), so a token
/// identifies a request across the whole client rather than within one
/// surface — two surfaces asking the same worker the same question at the
/// same moment stay distinguishable.
#[derive(Debug, Clone, Default)]
pub struct RequestTokens {
    next: Arc<AtomicU64>,
}

impl RequestTokens {
    /// The next token. Monotonic; wrapping is not a concern (a u64 at one
    /// token per nanosecond takes centuries).
    pub fn mint(&self) -> RequestToken {
        RequestToken(self.next.fetch_add(1, Ordering::Relaxed))
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

    #[test]
    fn a_cloned_source_keeps_minting_from_the_same_counter() {
        // The chat holds a clone of the workbench's source; two surfaces must
        // never mint the same token.
        let tokens = RequestTokens::default();
        let clone = tokens.clone();

        assert_ne!(tokens.mint(), clone.mint());
    }
}
