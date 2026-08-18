use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestToken(u64);

#[derive(Debug, Clone, Default)]
pub struct RequestTokens {
    next: Arc<AtomicU64>,
}

impl RequestTokens {
    pub fn mint(&self) -> RequestToken {
        RequestToken(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

#[derive(Debug)]
pub struct Pending<T> {
    outstanding: Option<(RequestToken, T)>,
}

impl<T> Default for Pending<T> {
    fn default() -> Self {
        Pending { outstanding: None }
    }
}

impl<T> Pending<T> {
    pub fn ask(&mut self, tokens: &RequestTokens, context: T) -> RequestToken {
        let token = tokens.mint();

        self.outstanding = Some((token, context));

        token
    }

    pub fn accept(&mut self, token: RequestToken) -> Option<T> {
        if !self.is_outstanding(token) {
            return None;
        }

        self.outstanding.take().map(|(_token, context)| context)
    }

    pub fn is_outstanding(&self, token: RequestToken) -> bool {
        matches!(&self.outstanding, Some((outstanding, _)) if *outstanding == token)
    }

    pub fn is_waiting(&self) -> bool {
        self.outstanding.is_some()
    }

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

    #[test]
    fn waiting_spans_exactly_the_time_an_answer_is_wanted() {
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();

        assert!(!pending.is_waiting());

        let token = pending.ask(&tokens, "/work/repo");

        assert!(pending.is_waiting());
        assert_eq!(pending.accept(token), Some("/work/repo"));
        assert!(!pending.is_waiting());
    }

    #[test]
    fn a_cancelled_request_stops_being_waited_for() {
        let tokens = RequestTokens::default();
        let mut pending = Pending::default();

        pending.ask(&tokens, ());
        pending.cancel();

        assert!(!pending.is_waiting());
    }
}
