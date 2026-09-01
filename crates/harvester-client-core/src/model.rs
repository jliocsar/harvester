use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    pub name: String,
    pub provider: String,
    pub id: String,
}

pub(crate) fn model_choice_from_value(model: &Value) -> Option<ModelChoice> {
    let id = model.get("id").and_then(Value::as_str)?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);

    Some(ModelChoice {
        name: name.to_string(),
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

pub(crate) fn extract_model_choices(data: &Value) -> Vec<ModelChoice> {
    let Some(models) = data.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };

    models.iter().filter_map(model_choice_from_value).collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ModelsState {
    #[default]
    NotRequested,
    Loading,
    Loaded(Vec<ModelChoice>),
    Failed(String),
}

impl ModelsState {
    pub fn choices(&self) -> &[ModelChoice] {
        match self {
            Self::Loaded(choices) => choices,
            Self::NotRequested | Self::Loading | Self::Failed(_) => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelection {
    LaunchDefault,
    Launch(ModelChoice),
    LivePending,
    Live(ModelChoice),
    LiveUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_model_payloads_are_only_projected_when_actionable() {
        let data = serde_json::json!({"models": [
            {"id": "one", "provider": "test", "name": "One", "future": {"x": 1}},
            {"id": "missing-provider"},
            "future-shape"
        ]});
        let choices = extract_model_choices(&data);

        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].id, "one");
        assert_eq!(choices[0].name, "One");
    }
}
