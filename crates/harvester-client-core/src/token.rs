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

/// The outstanding half of one request/reply pair: the token whose answer is
/// still wanted, plus whatever the asker needs in order to apply it (`T` — the
/// path a listing walked, the sid a model list belongs to; `()` when the answer
/// speaks for itself).
///
/// Minting a token is only half the mechanism; this is the other half, and it
/// was hand-rolled three different ways in the frontend. All three want the
/// same three rules: a re-ask supersedes the request before it, an answer is
/// applied only while its token is the outstanding one, and everything else is
/// dropped on the floor.
#[derive(Debug)]
pub struct Pending<T> {
    outstanding: Option<(RequestToken, T)>,
}

impl<T> Default for Pending<T> {
    /// Nothing asked yet. Manual because `T` need not be `Default` — the
    /// derive would demand it.
    fn default() -> Self {
        Pending { outstanding: None }
    }
}

impl<T> Pending<T> {
    /// Mints the token for a new request and supersedes any previous one: a
    /// re-ask makes the earlier answer stale by definition, so its context is
    /// dropped here rather than lingering to be applied out of order.
    pub fn ask(&mut self, tokens: &RequestTokens, context: T) -> RequestToken {
        let token = tokens.mint();

        self.outstanding = Some((token, context));

        token
    }

    /// The context for `token`, taken, iff it is still the answer being waited
    /// for. `None` for a late answer, an answer to a superseded request, or an
    /// answer to a request already applied — the three cases that used to need
    /// a bespoke staleness guard per command.
    pub fn accept(&mut self, token: RequestToken) -> Option<T> {
        if !self.is_outstanding(token) {
            return None;
        }

        self.outstanding.take().map(|(_token, context)| context)
    }

    /// Whether `token` is the outstanding one, without consuming it — for
    /// answers that arrive in installments (a stream, a progress event).
    pub fn is_outstanding(&self, token: RequestToken) -> bool {
        matches!(&self.outstanding, Some((outstanding, _)) if *outstanding == token)
    }

    /// Stop waiting without an answer: the surface that asked went away, so
    /// whatever arrives later has nowhere to land.
    pub fn cancel(&mut self) {
        self.outstanding = None;
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

    #[test]
    fn an_answer_is_applied_exactly_once() {
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();
        let token = pending.ask(&tokens, "/work/repo");

        assert_eq!(pending.accept(token), Some("/work/repo"));
        assert_eq!(
            pending.accept(token),
            None,
            "a duplicate answer has nothing left to apply"
        );
    }

    #[test]
    fn a_re_ask_supersedes_the_request_before_it() {
        // Two in-flight requests with the same key — the case an echoed `cwd`
        // or `path` cannot tell apart at all.
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();
        let first = pending.ask(&tokens, "/work/repo");
        let second = pending.ask(&tokens, "/work/repo");

        assert_eq!(pending.accept(first), None, "the slow answer is stale");
        assert_eq!(pending.accept(second), Some("/work/repo"));
    }

    #[test]
    fn a_token_from_another_surface_is_never_accepted() {
        let tokens = RequestTokens::default();
        let mut mine = Pending::default();
        let theirs = tokens.mint();

        mine.ask(&tokens, ());

        assert_eq!(mine.accept(theirs), None);
    }

    #[test]
    fn an_installment_answer_can_peek_without_consuming() {
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();
        let token = pending.ask(&tokens, ());

        assert!(pending.is_outstanding(token));
        assert!(pending.is_outstanding(token), "peeking does not consume");
        assert_eq!(pending.accept(token), Some(()));
        assert!(!pending.is_outstanding(token));
    }

    #[test]
    fn a_cancelled_request_lands_nowhere() {
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();
        let token = pending.ask(&tokens, "/work/repo");

        pending.cancel();

        assert_eq!(pending.accept(token), None);
    }
}
