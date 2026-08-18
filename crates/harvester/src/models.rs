use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelChoice {
    pub name: String,
    pub provider: String,
    pub id: String,
}

pub fn model_choice_from_value(model: &Value) -> Option<ModelChoice> {
    let id = model.get("id").and_then(Value::as_str)?;
    let provider = model.get("provider").and_then(Value::as_str)?;
    let name = model.get("name").and_then(Value::as_str).unwrap_or(id);

    Some(ModelChoice {
        name: name.to_string(),
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

pub fn extract_model_choices(data: &Value) -> Vec<ModelChoice> {
    let Some(models) = data.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };

    models.iter().filter_map(model_choice_from_value).collect()
}
