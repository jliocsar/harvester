use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

const SKIPPED_DIR_NAMES: &[&str] = &["target"];

const SPIKE_DIR_NAME: &str = "bin";
const SPIKE_PARENT_DIR_NAME: &str = "src";

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
            if !is_skipped_directory(&path) {
                collect_rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn is_skipped_directory(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };

    if SKIPPED_DIR_NAMES.contains(&name) {
        return true;
    }

    let sits_directly_under_a_crate_src = path
        .parent()
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .is_some_and(|parent_name| parent_name == SPIKE_PARENT_DIR_NAME);

    name == SPIKE_DIR_NAME && sits_directly_under_a_crate_src
}

struct Scanner {
    characters: Vec<char>,
    index: usize,
    line: usize,
}

impl Scanner {
    fn new(source: &str) -> Self {
        Scanner {
            characters: source.chars().collect(),
            index: 0,
            line: 1,
        }
    }

    fn current(&self) -> Option<char> {
        self.characters.get(self.index).copied()
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.characters.get(self.index + ahead).copied()
    }

    fn advance(&mut self) {
        if self.current() == Some('\n') {
            self.line += 1;
        }

        self.index += 1;
    }

    fn advance_by(&mut self, count: usize) {
        for _ in 0..count {
            self.advance();
        }
    }

    fn skip_string(&mut self) {
        self.advance();

        while let Some(current) = self.current() {
            match current {
                '\\' => self.advance_by(2),
                '"' => {
                    self.advance();

                    return;
                }
                _ => self.advance(),
            }
        }
    }

    fn raw_string_quote_offset(&self) -> Option<usize> {
        let mut offset = 0;

        if matches!(self.peek(offset), Some('b' | 'c')) {
            offset += 1;
        }

        if self.peek(offset) != Some('r') {
            return None;
        }

        offset += 1;

        while self.peek(offset) == Some('#') {
            offset += 1;
        }

        (self.peek(offset) == Some('"')).then_some(offset)
    }

    fn skip_raw_string_or_advance(&mut self) {
        let Some(quote_offset) = self.raw_string_quote_offset() else {
            self.advance();

            return;
        };
        let hash_count = (1..quote_offset)
            .filter(|offset| self.peek(*offset) == Some('#'))
            .count();

        self.advance_by(quote_offset + 1);

        while let Some(current) = self.current() {
            if current == '"' && self.closing_hash_count() >= hash_count {
                self.advance_by(1 + hash_count);

                return;
            }

            self.advance();
        }
    }

    fn closing_hash_count(&self) -> usize {
        (1..)
            .take_while(|offset| self.peek(*offset) == Some('#'))
            .count()
    }

    fn skip_quote(&mut self) {
        if self.peek(1) == Some('\\') {
            self.advance_by(2);
            self.skip_to_closing_quote();
        } else if self.peek(1).is_some() && self.peek(2) == Some('\'') {
            self.advance_by(3);
        } else {
            self.advance();
        }
    }

    fn skip_to_closing_quote(&mut self) {
        while let Some(current) = self.current() {
            self.advance();

            if current == '\'' {
                return;
            }
        }
    }
}

fn first_comment_line(source: &str) -> Option<usize> {
    let mut scanner = Scanner::new(source);

    while let Some(current) = scanner.current() {
        if current == '/' && matches!(scanner.peek(1), Some('/' | '*')) {
            return Some(scanner.line);
        }

        match current {
            '"' => scanner.skip_string(),
            'r' | 'b' | 'c' => scanner.skip_raw_string_or_advance(),
            '\'' => scanner.skip_quote(),
            _ => scanner.advance(),
        }
    }

    None
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

#[test]
fn a_line_continuation_inside_a_string_still_counts_its_newline() {
    let source = "let s = \"first \\\n    second\";\nlet t = 1; // here";

    assert_eq!(
        first_comment_line(source),
        Some(3),
        "a backslash-newline continuation is still a newline; missing it drifts \
         every offender line number after it"
    );
}

#[test]
fn only_a_crates_own_src_bin_is_exempt_from_the_walk() {
    for exempt in [
        "/w/crates/harvester/src/bin",
        "/w/crates/harvesterd/src/bin",
        "/w/crates/harvester/target",
        "/w/crates/harvester/src/target",
    ] {
        assert!(is_skipped_directory(Path::new(exempt)), "{exempt}");
    }

    for walked in [
        "/w/crates/harvester-gallery/src/stories/bin",
        "/w/crates/harvester/bin",
        "/w/crates/bin",
        "/w/crates/harvester/src/binary",
        "/w/crates/harvester/src",
    ] {
        assert!(!is_skipped_directory(Path::new(walked)), "{walked}");
    }
}
