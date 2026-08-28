use crate::stories::{core, dialog, note, prompt_strip, shell, sidebar, transcript};
use crate::story::Story;

pub fn build_stories() -> Vec<Story> {
    [
        core::stories(),
        note::stories(),
        shell::stories(),
        sidebar::stories(),
        prompt_strip::stories(),
        transcript::stories(),
        dialog::stories(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn story_index(stories: &[Story], name: &str) -> Option<usize> {
    stories.iter().position(|story| story.name == name)
}

pub fn group_names(stories: &[Story]) -> Vec<&'static str> {
    let mut groups: Vec<&'static str> = Vec::new();

    for story in stories {
        if !groups.contains(&story.group) {
            groups.push(story.group);
        }
    }

    groups
}
