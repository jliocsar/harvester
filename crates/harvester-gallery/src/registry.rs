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

#[cfg(test)]
mod tests {
    use super::{build_stories, group_names};

    const EXPECTED_GROUP_COUNT: usize = 6;
    const EXPECTED_STORY_COUNT: usize = 14;

    #[test]
    fn the_registry_holds_at_least_the_walk_floor_of_stories() {
        let stories = build_stories();

        assert!(
            stories.len() >= EXPECTED_STORY_COUNT,
            "expected at least {EXPECTED_STORY_COUNT} stories, found {}",
            stories.len()
        );
    }

    #[test]
    fn the_registry_holds_at_least_the_walk_floor_of_groups() {
        let stories = build_stories();
        let groups = group_names(&stories);

        assert!(
            groups.len() >= EXPECTED_GROUP_COUNT,
            "expected at least {EXPECTED_GROUP_COUNT} story groups, found {}",
            groups.len()
        );
    }
}
