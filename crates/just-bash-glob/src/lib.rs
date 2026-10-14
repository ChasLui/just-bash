//! Glob pattern matching and pathname expansion.
//!
//! The runtime has no real filesystem, so pathname expansion walks the
//! [`InMemoryFs`] entry table rather than calling `readdir`. Matching uses the
//! classic iterative star-backtracking algorithm, so a pattern like
//! `*a*a*a*a*b` cannot blow up exponentially on a hostile input.

use just_bash_fs::{InMemoryFs, normalize_absolute};

/// A single compiled pattern element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Element {
    /// A literal character. `escaped` marks `\x`, which also lets a leading `.`
    /// satisfy the hidden-file rule.
    Literal { ch: char, escaped: bool },
    /// `?` — exactly one character.
    Any,
    /// `*` — zero or more characters.
    Star,
    /// `[...]` — a bracket expression, possibly negated.
    Class {
        items: Vec<ClassItem>,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

/// Compile a glob pattern into elements.
///
/// An unterminated `[` is treated as a literal `[`, matching bash/GNU behavior.
fn compile(pattern: &str) -> Vec<Element> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut elements = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        match chars[index] {
            '\\' if index + 1 < chars.len() => {
                elements.push(Element::Literal {
                    ch: chars[index + 1],
                    escaped: true,
                });
                index += 2;
            }
            '?' => {
                elements.push(Element::Any);
                index += 1;
            }
            '*' => {
                // Collapse runs of `*` — they are equivalent and this keeps the
                // backtracking loop honest about progress.
                if !matches!(elements.last(), Some(Element::Star)) {
                    elements.push(Element::Star);
                }
                index += 1;
            }
            '[' => match compile_class(&chars, index) {
                Some((class, next)) => {
                    elements.push(class);
                    index = next;
                }
                None => {
                    elements.push(Element::Literal {
                        ch: '[',
                        escaped: false,
                    });
                    index += 1;
                }
            },
            ch => {
                elements.push(Element::Literal { ch, escaped: false });
                index += 1;
            }
        }
    }
    elements
}

fn compile_class(chars: &[char], start: usize) -> Option<(Element, usize)> {
    let mut index = start + 1;
    let mut negated = false;
    if matches!(chars.get(index), Some('!') | Some('^')) {
        negated = true;
        index += 1;
    }
    let mut items = Vec::new();
    // A `]` immediately after the (possibly negated) opening bracket is literal.
    if chars.get(index) == Some(&']') {
        items.push(ClassItem::Char(']'));
        index += 1;
    }
    while index < chars.len() {
        let ch = chars[index];
        if ch == ']' {
            if items.is_empty() {
                return None;
            }
            return Some((Element::Class { items, negated }, index + 1));
        }
        if ch == '\\' && index + 1 < chars.len() {
            items.push(ClassItem::Char(chars[index + 1]));
            index += 2;
            continue;
        }
        // `a-z` is a range; a trailing `-` before `]` is literal.
        if index + 2 < chars.len() && chars[index + 1] == '-' && chars[index + 2] != ']' {
            items.push(ClassItem::Range(ch, chars[index + 2]));
            index += 3;
            continue;
        }
        items.push(ClassItem::Char(ch));
        index += 1;
    }
    None
}

fn class_matches(items: &[ClassItem], negated: bool, ch: char) -> bool {
    let hit = items.iter().any(|item| match item {
        ClassItem::Char(candidate) => *candidate == ch,
        ClassItem::Range(low, high) => *low <= ch && ch <= *high,
    });
    hit != negated
}

/// Match `text` against `pattern` in "string" mode, where `*` and `?` may match
/// any character including `/`. Used by `case` patterns and `[[ == ]]`.
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    let elements = compile(pattern);
    let chars: Vec<char> = text.chars().collect();
    matches_elements(&elements, &chars)
}

/// Match a single path component. A leading `.` in `name` is only matched when
/// the pattern itself starts with a literal `.` (bash's hidden-file rule).
pub fn component_matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') {
        let starts_literal_dot = matches!(
            compile(pattern).first(),
            Some(Element::Literal { ch: '.', .. })
        );
        if !starts_literal_dot {
            return false;
        }
    }
    let elements = compile(pattern);
    let chars: Vec<char> = name.chars().collect();
    matches_elements(&elements, &chars)
}

fn matches_elements(elements: &[Element], text: &[char]) -> bool {
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star: Option<(usize, usize)> = None;

    while ti < text.len() {
        if pi < elements.len() {
            match &elements[pi] {
                Element::Star => {
                    star = Some((pi, ti));
                    pi += 1;
                    continue;
                }
                Element::Any => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                Element::Literal { ch, .. } if *ch == text[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                Element::Class { items, negated } if class_matches(items, *negated, text[ti]) => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        // Mismatch: retry the last star with one more character consumed.
        match star {
            Some((star_pi, star_ti)) => {
                pi = star_pi + 1;
                ti = star_ti + 1;
                star = Some((star_pi, star_ti + 1));
            }
            None => return false,
        }
    }

    while pi < elements.len() && matches!(elements[pi], Element::Star) {
        pi += 1;
    }
    pi == elements.len()
}

/// True when a pattern contains an unquoted/unescaped glob metacharacter.
pub fn has_meta(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// Expand a glob pattern against the in-memory filesystem.
///
/// * A relative pattern is resolved against `cwd`; an absolute one against `/`.
/// * Every produced path must exist; a trailing `/` restricts the result to
///   directories.
/// * Results are sorted and de-duplicated, so expansion is deterministic.
/// * No match yields an empty vector: the caller keeps the original word, as
///   bash does without `nullglob`.
pub fn expand(fs: &InMemoryFs, pattern: &str, cwd: &str) -> Vec<String> {
    let require_dir = pattern.len() > 1 && pattern.ends_with('/');
    let components: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        return Vec::new();
    }

    let absolute = pattern.starts_with('/');
    let mut candidates: Vec<String> = vec![if absolute {
        "/".to_string()
    } else {
        cwd.to_string()
    }];

    for (index, component) in components.iter().enumerate() {
        let mut next: Vec<String> = Vec::new();
        if has_meta(component) {
            for directory in &candidates {
                // Candidates keep their lexical shape, so list through the
                // normalized path (e.g. `/home/user/.`).
                let Ok(names) = fs.list_dir(&normalize_absolute(directory)) else {
                    continue;
                };
                for name in names {
                    if component_matches(component, &name) {
                        next.push(join(directory, &name));
                    }
                }
            }
        } else {
            for directory in &candidates {
                next.push(join(directory, component));
            }
        }
        if next.is_empty() {
            return Vec::new();
        }
        candidates = next;
        // Intermediate components must be directories to be descended into.
        if index + 1 < components.len() {
            candidates.retain(|candidate| fs.is_dir(&normalize_absolute(candidate)));
            if candidates.is_empty() {
                return Vec::new();
            }
        }
    }

    // Candidates keep the lexical shape of the pattern; only existence is
    // checked against the normalized path, so `./a.rs` prints as `./a.rs`.
    let mut results: Vec<String> = candidates
        .into_iter()
        .filter(|candidate| {
            let resolved = normalize_absolute(candidate);
            if require_dir {
                fs.is_dir(&resolved)
            } else {
                fs.exists(&resolved)
            }
        })
        .collect();
    results.sort();
    results.dedup();
    // Bash reports matches with the shape the pattern had: a relative pattern
    // prints relative names even though it was resolved against `cwd`.
    if !absolute {
        let prefix = if cwd == "/" {
            "/".to_string()
        } else {
            format!("{cwd}/")
        };
        results = results
            .into_iter()
            .map(|path| {
                path.strip_prefix(&prefix)
                    .map(str::to_string)
                    .unwrap_or(path)
            })
            .collect();
    }
    // A trailing `/` in the pattern is preserved in the match, as bash does.
    if require_dir {
        results = results.into_iter().map(|path| format!("{path}/")).collect();
    }
    results
}

fn join(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use just_bash_fs::parent_dir;

    fn fs_with(entries: &[(&str, &str)]) -> InMemoryFs {
        let mut fs = InMemoryFs::new();
        for (path, contents) in entries {
            fs.create_dir_all(&parent_dir(path)).unwrap();
            fs.write_file(path, contents).unwrap();
        }
        fs
    }

    #[test]
    fn literals_and_stars_match() {
        assert!(pattern_matches("abc", "abc"));
        assert!(!pattern_matches("abc", "abd"));
        assert!(pattern_matches("a*c", "ac"));
        assert!(pattern_matches("a*c", "abbbc"));
        assert!(pattern_matches("*", ""));
        assert!(pattern_matches("*", "anything/with/slashes"));
        assert!(pattern_matches("a*b*c", "aXbYc"));
        assert!(!pattern_matches("a*b*c", "aXbY"));
    }

    #[test]
    fn question_and_classes_match() {
        assert!(pattern_matches("a?c", "abc"));
        assert!(!pattern_matches("a?c", "ac"));
        assert!(pattern_matches("[abc]", "b"));
        assert!(!pattern_matches("[abc]", "d"));
        assert!(pattern_matches("[a-z]", "q"));
        assert!(!pattern_matches("[a-z]", "Q"));
        assert!(pattern_matches("[!a-z]", "Q"));
        assert!(pattern_matches("[^0-9]", "x"));
        assert!(pattern_matches("[]]", "]"));
        assert!(pattern_matches("[a-]", "-"));
    }

    #[test]
    fn unterminated_bracket_is_literal() {
        assert!(pattern_matches("[abc", "[abc"));
        assert!(pattern_matches("a[", "a["));
    }

    #[test]
    fn escapes_are_literal() {
        assert!(pattern_matches("a\\*b", "a*b"));
        assert!(!pattern_matches("a\\*b", "axb"));
        assert!(!has_meta("a\\*b"));
        assert!(has_meta("a*b"));
        assert!(has_meta("a[bc]"));
        assert!(!has_meta("plain"));
    }

    #[test]
    fn hidden_files_need_an_explicit_dot() {
        assert!(!component_matches("*", ".hidden"));
        assert!(component_matches(".*", ".hidden"));
        assert!(component_matches(".h*", ".hidden"));
        assert!(component_matches("*", "visible"));
        // An escaped dot also counts as explicit.
        assert!(component_matches("\\*", "*"));
        assert!(!component_matches("\\*", "x"));
    }

    #[test]
    fn star_backtracking_is_not_exponential() {
        // The pathological case for naive recursion.
        let pattern = "*a*a*a*a*a*a*a*a*b";
        let text = "a".repeat(64);
        // Must terminate quickly and report no match.
        assert!(!pattern_matches(pattern, &text));
    }

    #[test]
    fn expands_relative_patterns_in_sorted_order() {
        let fs = fs_with(&[
            ("/home/user/src/main.rs", ""),
            ("/home/user/src/lib.rs", ""),
            ("/home/user/README.md", ""),
        ]);
        // A relative pattern reports relative matches, as bash does.
        let results = expand(&fs, "*.rs", "/home/user/src");
        assert_eq!(results, vec!["lib.rs", "main.rs"]);
        let results = expand(&fs, "*", "/home/user");
        assert_eq!(results, vec!["README.md", "src"]);
    }

    #[test]
    fn expands_absolute_and_nested_patterns() {
        let fs = fs_with(&[("/data/a/one.txt", ""), ("/data/b/two.txt", "")]);
        let results = expand(&fs, "/data/*/*.txt", "/");
        assert_eq!(results, vec!["/data/a/one.txt", "/data/b/two.txt"]);
        let results = expand(&fs, "/data/a/*.md", "/");
        assert!(results.is_empty(), "no match keeps the word literal");
    }

    #[test]
    fn hidden_files_are_skipped_by_star() {
        let fs = fs_with(&[("/home/user/.profile", ""), ("/home/user/notes", "")]);
        assert_eq!(expand(&fs, "*", "/home/user"), vec!["notes"]);
        assert_eq!(expand(&fs, ".*", "/home/user"), vec![".profile"]);
    }

    #[test]
    fn trailing_slash_requires_directories() {
        let fs = fs_with(&[("/home/user/dir/file", ""), ("/home/user/file", "")]);
        // The trailing slash is kept on directory matches.
        assert_eq!(expand(&fs, "*/", "/home/user"), vec!["dir/"]);
        assert_eq!(expand(&fs, "*", "/home/user"), vec!["dir", "file"]);
    }

    #[test]
    fn intermediate_components_must_be_directories() {
        let fs = fs_with(&[("/home/user/file", "")]);
        assert!(expand(&fs, "file/*", "/home/user").is_empty());
    }

    #[test]
    fn expansion_is_deduplicated() {
        let fs = fs_with(&[("/home/user/a", "")]);
        let results = expand(&fs, "a", "/home/user");
        assert_eq!(results, vec!["a"]);
        // `./a` normalizes to the same path.
        let results = expand(&fs, "./a", "/home/user");
        assert_eq!(results, vec!["./a"]);
    }
}
