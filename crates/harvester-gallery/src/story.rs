use gpui::{AnyElement, App, Entity, SharedString, Window};

use crate::runtime::StoryRuntime;

#[derive(Clone, Debug, PartialEq)]
pub enum KnobValue {
    Bool(bool),
    Choice(usize),
    Text(String),
}

pub enum KnobKind {
    Bool,
    Choice(Vec<SharedString>),
    Text,
}

pub struct KnobSpec {
    pub name: &'static str,
    pub default: KnobValue,
    pub kind: KnobKind,
}

impl KnobSpec {
    pub fn flag(name: &'static str, default: bool) -> Self {
        Self {
            name,
            default: KnobValue::Bool(default),
            kind: KnobKind::Bool,
        }
    }

    pub fn choice(name: &'static str, labels: &[&'static str], default: usize) -> Self {
        let labels: Vec<SharedString> = labels.iter().map(|label| (*label).into()).collect();

        Self {
            name,
            default: KnobValue::Choice(default),
            kind: KnobKind::Choice(labels),
        }
    }

    pub fn text(name: &'static str, default: &str) -> Self {
        Self {
            name,
            default: KnobValue::Text(default.to_string()),
            kind: KnobKind::Text,
        }
    }
}

pub struct Preset {
    pub name: &'static str,
    pub values: Vec<(&'static str, KnobValue)>,
}

impl Preset {
    pub fn new(name: &'static str, values: Vec<(&'static str, KnobValue)>) -> Self {
        Self { name, values }
    }
}

#[derive(Clone, Default)]
pub struct KnobValues {
    entries: Vec<(&'static str, KnobValue)>,
}

impl KnobValues {
    pub fn from_specs(specs: &[KnobSpec]) -> Self {
        let entries = specs
            .iter()
            .map(|spec| (spec.name, spec.default.clone()))
            .collect();

        Self { entries }
    }

    pub fn set(&mut self, name: &'static str, value: KnobValue) {
        for entry in self.entries.iter_mut() {
            if entry.0 == name {
                entry.1 = value;

                return;
            }
        }

        self.entries.push((name, value));
    }

    pub fn value(&self, name: &str) -> Option<&KnobValue> {
        self.entries
            .iter()
            .find(|entry| entry.0 == name)
            .map(|entry| &entry.1)
    }

    pub fn flag(&self, name: &str) -> bool {
        match self.value(name) {
            Some(KnobValue::Bool(flag)) => *flag,
            _ => false,
        }
    }

    pub fn choice(&self, name: &str) -> usize {
        match self.value(name) {
            Some(KnobValue::Choice(index)) => *index,
            _ => 0,
        }
    }

    pub fn text(&self, name: &str) -> &str {
        match self.value(name) {
            Some(KnobValue::Text(text)) => text.as_str(),
            _ => "",
        }
    }

    pub fn optional_text(&self, name: &str) -> Option<SharedString> {
        let text = self.text(name);

        if text.is_empty() {
            return None;
        }

        Some(text.to_string().into())
    }
}

pub struct StoryScene<'a> {
    pub knobs: &'a KnobValues,
    pub runtime: &'a Entity<StoryRuntime>,
}

pub type RenderStory = fn(&StoryScene, &mut Window, &mut App) -> AnyElement;

pub struct Story {
    pub name: &'static str,
    pub group: &'static str,
    pub knobs: Vec<KnobSpec>,
    pub presets: Vec<Preset>,
    pub render: RenderStory,
}

impl Story {
    pub fn new(
        group: &'static str,
        name: &'static str,
        knobs: Vec<KnobSpec>,
        presets: Vec<Preset>,
        render: RenderStory,
    ) -> Self {
        Self {
            name,
            group,
            knobs,
            presets,
            render,
        }
    }
}
