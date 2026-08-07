//! Model-domain types shared by the unstarted chat's picker (pre-session)
//! and the live chat's (mid-session `set_model`): only what the UI renders
//! and what a launch line or `set_model` call needs, extracted from pi's
//! verbatim Model objects — the payloads themselves are never normalized.

use gpui::SharedString;
use serde_json::Value;

/// One entry the model picker can offer.
#[derive(Clone)]
pub struct ModelChoice {
    /// Display string (pi's `name`, falling back to `id`).
    pub name: SharedString,
    pub provider: String,
    pub id: String,
}

#[derive(Default)]
pub enum ModelsState {
    #[default]
    Loading,
    Loaded(Vec<ModelChoice>),
    /// The fetch failed; the chat stays usable — pi keeps running on its
    /// current (or default) model.
    Failed(String),
}

/// One pi Model object → a picker entry. Objects without an id or provider
/// are dropped — they could never go on a launch line or a `set_model` call.
pub fn model_choice_from_value(model: &Value) -> Option<ModelChoice> {
    let id = model.get("id").and_then(Value::as_str)?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);

    Some(ModelChoice {
        name: name.to_string().into(),
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

/// The picker's entries from pi's verbatim `get_available_models` data.
pub fn extract_model_choices(data: &Value) -> Vec<ModelChoice> {
    let Some(models) = data.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };

    models.iter().filter_map(model_choice_from_value).collect()
}
