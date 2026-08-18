use std::fs;
use std::path::{Path, PathBuf};

const SKIPPED_DIR_NAMES: &[&str] = &["target", "bin"];

#[derive(Debug)]
struct CommentSite {
    file: PathBuf,
    line: usize,
}

fn workspace_crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent")
        .to_path_buf()
}

fn collect_rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).expect("readable source directory");

    for entry in entries {
        let entry = entry.expect("readable directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("readable file type");

        if file_type.is_dir() {
            let name = entry.file_name();
            let skipped = SKIPPED_DIR_NAMES
                .iter()
                .any(|skipped_name| name == *skipped_name);

            if !skipped {
                collect_rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn first_comment_line(source: &str) -> Option<usize> {
    let characters: Vec<char> = source.chars().collect();
    let mut line = 1;
    let mut index = 0;

    while index < characters.len() {
        let current = characters[index];

        if current == '\n' {
            line += 1;
            index += 1;
        } else if current == '/' && matches!(characters.get(index + 1), Some('/' | '*')) {
            return Some(line);
        } else if current == '"' {
            index = skip_string(&characters, index, &mut line);
        } else if current == 'r' || current == 'b' || current == 'c' {
            match raw_string_body_start(&characters, index) {
                Some(body) => index = skip_raw_string(&characters, index, body, &mut line),
                None => index += 1,
            }
        } else if current == '\'' {
            index = skip_quote(&characters, index, &mut line);
        } else {
            index += 1;
        }
    }

    None
}

fn raw_string_body_start(characters: &[char], index: usize) -> Option<usize> {
    let mut cursor = index;

    if characters.get(cursor) == Some(&'b') || characters.get(cursor) == Some(&'c') {
        cursor += 1;
    }

    if characters.get(cursor) != Some(&'r') {
        return None;
    }

    cursor += 1;

    while characters.get(cursor) == Some(&'#') {
        cursor += 1;
    }

    if characters.get(cursor) == Some(&'"') {
        Some(cursor)
    } else {
        None
    }
}

fn skip_raw_string(
    characters: &[char],
    prefix_start: usize,
    quote_index: usize,
    line: &mut usize,
) -> usize {
    let hash_count = characters[prefix_start..quote_index]
        .iter()
        .filter(|character| **character == '#')
        .count();
    let mut index = quote_index + 1;

    while index < characters.len() {
        let current = characters[index];

        if current == '\n' {
            *line += 1;
            index += 1;
        } else if current == '"' {
            let closing_hashes = characters[index + 1..]
                .iter()
                .take_while(|character| **character == '#')
                .count();

            if closing_hashes >= hash_count {
                return index + 1 + hash_count;
            }

            index += 1;
        } else {
            index += 1;
        }
    }

    index
}

fn skip_string(characters: &[char], opening_index: usize, line: &mut usize) -> usize {
    let mut index = opening_index + 1;

    while index < characters.len() {
        let current = characters[index];

        if current == '\\' {
            index += 2;
        } else if current == '\n' {
            *line += 1;
            index += 1;
        } else if current == '"' {
            return index + 1;
        } else {
            index += 1;
        }
    }

    index
}

fn skip_quote(characters: &[char], quote_index: usize, line: &mut usize) -> usize {
    let next = characters.get(quote_index + 1);

    if next == Some(&'\\') {
        let mut index = quote_index + 2;

        while index < characters.len() && characters[index] != '\'' {
            if characters[index] == '\n' {
                *line += 1;
            }

            index += 1;
        }

        index + 1
    } else if next.is_some() && characters.get(quote_index + 2) == Some(&'\'') {
        quote_index + 3
    } else {
        quote_index + 1
    }
}

#[test]
fn the_codebase_contains_no_comments() {
    let mut rust_files = Vec::new();

    collect_rust_files(&workspace_crates_dir(), &mut rust_files);
    rust_files.sort();

    assert!(
        !rust_files.is_empty(),
        "no Rust files found; the walk is broken"
    );

    let mut offenders = Vec::new();

    for file in rust_files {
        let source = fs::read_to_string(&file).expect("readable Rust source file");

        if let Some(line) = first_comment_line(&source) {
            offenders.push(CommentSite { file, line });
        }
    }

    let listing: Vec<String> = offenders
        .iter()
        .map(|site| format!("{}:{}", site.file.display(), site.line))
        .collect();

    assert!(
        offenders.is_empty(),
        "comments are banned; knowledge belongs in docs/ (see AGENTS.md). Found:\n{}",
        listing.join("\n")
    );
}

#[test]
fn the_scanner_sees_through_literals() {
    assert_eq!(
        first_comment_line("let url = \"https://example.com\";"),
        None
    );
    assert_eq!(
        first_comment_line("let raw = r#\"// not a comment\"#;"),
        None
    );
    assert_eq!(
        first_comment_line("let slash = '/'; let s: &'static str = \"\";"),
        None
    );
    assert_eq!(
        first_comment_line("let byte = b'/';\nlet ok = 1; // trailing"),
        Some(2)
    );
    assert_eq!(first_comment_line("fn a() {}\n/* block */"), Some(2));
    assert_eq!(first_comment_line("let escaped = \"\\\"//\\\"\";"), None);
    assert_eq!(first_comment_line("let quote = '\\''; // after"), Some(1));
}
