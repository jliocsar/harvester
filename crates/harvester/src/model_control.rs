//! The prompt box's model control, owned once and used twice.
//!
//! The ghost chat and the live chat both load a model list, open a modal over
//! it, and take a pick — and they differ in exactly two places: the command
//! that loads the list (`list_models` against a throwaway pi, versus
//! `get_available_models` against the running one) and what a pick means (a
//! launch-line argument, versus a `set_model` the session must confirm). Those
//! two stay with their surfaces. Everything below was written twice.

use harvester_client_core::{Pending, RequestToken, RequestTokens};
use serde_json::Value;

use crate::models::{self, ModelChoice, ModelsState};

#[derive(Default)]
pub struct ModelControl {
    state: ModelsState,
    /// The list request in flight. A retargeted ghost (or a reopened chat) is
    /// waiting on a different token, so the abandoned request's answer is
    /// dropped rather than filling the new surface's list.
    request: Pending<()>,
    open: bool,
}

impl ModelControl {
    /// Start a fetch, superseding any earlier one. The modal closes: what it
    /// was showing is about to be replaced, and a pick landing in the gap
    /// would apply to a list nobody can see anymore.
    pub fn fetching(&mut self, tokens: &RequestTokens) -> RequestToken {
        self.state = ModelsState::Loading;
        self.open = false;

        self.request.ask(tokens, ())
    }

    /// Fill the list from pi's verbatim `data`. `false` means the answer was
    /// stale — a request this control has already superseded or applied.
    pub fn accept(&mut self, token: RequestToken, result: Result<Value, String>) -> bool {
        if self.request.accept(token).is_none() {
            return false;
        }

        self.state = match result {
            Ok(data) => ModelsState::Loaded(models::extract_model_choices(&data)),
            Err(detail) => ModelsState::Failed(detail),
        };

        true
    }

    /// Give up on a fetch that will never be answered — the bridge died, or
    /// the command was never sent ([`Pending::ask`] records on intent, so a
    /// caller whose send can fail has to say so). The control stays usable:
    /// a session with no list simply runs on the model it already has.
    pub fn fail(&mut self, detail: String) {
        if !self.is_loading() {
            return;
        }

        self.request.cancel();
        self.state = ModelsState::Failed(detail);
    }

    pub fn state(&self) -> &ModelsState {
        &self.state
    }

    pub fn choices(&self) -> &[ModelChoice] {
        match &self.state {
            ModelsState::Loaded(choices) => choices,
            _ => &[],
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.state, ModelsState::Loading)
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Only a loaded list can be picked from; a click while it is still
    /// loading is a click on nothing rather than an empty modal.
    pub fn open(&mut self) {
        self.open = matches!(self.state, ModelsState::Loaded(_));
    }

    pub fn close(&mut self) {
        self.open = false;
    }
}
