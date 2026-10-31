//! Conformance harness over the upstream bash-comparison fixtures.
//!
//! The fixture corpus in `fixtures/` was recorded from real bash by the
//! upstream TypeScript project (`packages/just-bash/src/comparison-tests/`).
//! It is reachable in this repository's own history at the fork point, and
//! replaying it here turns "how much of upstream do we match" from a prose
//! claim in `UPSTREAM_SYNC.md` into a number this test run reports.
//!
//! Each case is `{command, files, stdout, stderr, exitCode}`. A case either
//! passes, fails (a real mismatch worth fixing), or is listed in
//! `known_failures.txt` as a documented gap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use just_bash::{Bash, Options};

// ── A tiny JSON reader (the workspace is dependency-free) ───────────────────

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Number(value) => Some(*value),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn parse(text: &'a str) -> Result<Json, String> {
        let mut parser = Self {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let value = parser.parse_value()?;
        parser.skip_whitespace();
        if parser.pos != parser.bytes.len() {
            return Err(format!("trailing input at byte {}", parser.pos));
        }
        Ok(value)
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, expected: u8) -> Result<(), String> {
        self.skip_whitespace();
        if self.bytes.get(self.pos) == Some(&expected) {
            self.pos += 1;
            return Ok(());
        }
        Err(format!(
            "expected `{}` at byte {}",
            expected as char, self.pos
        ))
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_whitespace();
        match self.bytes.get(self.pos).copied() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Json::String(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Json::Bool(true)),
            Some(b'f') => self.parse_literal("false", Json::Bool(false)),
            Some(b'n') => self.parse_literal("null", Json::Null),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.parse_number(),
            other => Err(format!("unexpected {other:?} at byte {}", self.pos)),
        }
    }

    fn parse_literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(text.as_bytes()) {
            self.pos += text.len();
            return Ok(value);
        }
        Err(format!("bad literal at byte {}", self.pos))
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|byte| byte.is_ascii_digit())
        {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|e| e.to_string())?;
        text.parse::<i64>()
            .map(Json::Number)
            .map_err(|_| format!("bad number `{text}'"))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            let Some(byte) = self.bytes.get(self.pos).copied() else {
                return Err("unterminated string".to_string());
            };
            self.pos += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(escape) = self.bytes.get(self.pos).copied() else {
                        return Err("unterminated escape".to_string());
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.pos..self.pos + 4)
                                .ok_or("truncated \\u escape")?;
                            let hex = std::str::from_utf8(hex).map_err(|e| e.to_string())?;
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| format!("bad \\u escape `{hex}'"))?;
                            self.pos += 4;
                            // Surrogate pairs are not needed by the corpus.
                            if let Some(ch) = char::from_u32(code) {
                                out.push(ch);
                            }
                        }
                        other => return Err(format!("bad escape `\\{}'", other as char)),
                    }
                }
                _ => {
                    // Copy the raw byte run, validating UTF-8 as we go.
                    let start = self.pos - 1;
                    let mut end = self.pos;
                    while end < self.bytes.len()
                        && self.bytes[end] != b'"'
                        && self.bytes[end] != b'\\'
                    {
                        end += 1;
                    }
                    out.push_str(
                        std::str::from_utf8(&self.bytes[start..end]).map_err(|e| e.to_string())?,
                    );
                    self.pos = end;
                }
            }
        }
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.bytes.get(self.pos) == Some(&b']') {
                self.pos += 1;
                return Ok(Json::Array(items));
            }
            items.push(self.parse_value()?);
            self.skip_whitespace();
            if self.bytes.get(self.pos) == Some(&b',') {
                self.pos += 1;
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.eat(b'{')?;
        let mut entries = Vec::new();
        loop {
            self.skip_whitespace();
            if self.bytes.get(self.pos) == Some(&b'}') {
                self.pos += 1;
                return Ok(Json::Object(entries));
            }
            let key = self.parse_string()?;
            self.eat(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_whitespace();
            if self.bytes.get(self.pos) == Some(&b',') {
                self.pos += 1;
            }
        }
    }
}

// ── Fixtures ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Case {
    suite: String,
    id: String,
    command: String,
    files: BTreeMap<String, String>,
    stdout: String,
    stderr: String,
    exit_code: i32,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/fixtures")
}

fn known_failures_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/known_failures.txt")
}

fn load_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixture directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let suite = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .trim_end_matches(".comparison.fixtures.json")
            .to_string();
        let text = std::fs::read_to_string(&path).expect("read fixture");
        let json = JsonParser::parse(&text).expect("parse fixture");
        let Json::Object(entries) = json else {
            panic!("fixture root must be an object: {}", path.display());
        };
        for (id, value) in entries {
            let files = match value.get("files") {
                Some(Json::Object(entries)) => entries
                    .iter()
                    .filter_map(|(name, content)| {
                        Some((name.clone(), content.as_str()?.to_string()))
                    })
                    .collect(),
                _ => BTreeMap::new(),
            };
            cases.push(Case {
                suite: suite.clone(),
                id,
                command: value
                    .get("command")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                files,
                stdout: value
                    .get("stdout")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                stderr: value
                    .get("stderr")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
                exit_code: value.get("exitCode").and_then(Json::as_i64).unwrap_or(0) as i32,
            });
        }
    }
    cases
}

/// Cases that are known not to match, one `suite/id` per line. Keeping the
/// list in a file makes every outstanding gap visible in review.
fn known_failures() -> std::collections::BTreeSet<String> {
    let path = known_failures_path();
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::BTreeSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Strip the `PREFIX: line N: ` label bash adds to diagnostics when running a
/// `-c` script. The fixtures were recorded that way; the label depends on the
/// bash binary's path and on line numbering, so comparing it would measure the
/// recorder rather than the shell.
fn normalize_stderr(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        normalized.push_str(&normalize_stderr_line(line));
    }
    normalized
}

/// Strip `PATH: line N: ` from one diagnostic line, if present. The runtime
/// itself prefixes diagnostics with just `bash: `, so both spellings collapse
/// to the bare message.
fn normalize_stderr_line(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("bash: ") {
        return rest.to_string();
    }
    let Some(prefix_end) = line.find(": line ") else {
        return line.to_string();
    };
    // The prefix must look like a program path, not an ordinary message.
    if !line[..prefix_end].contains('/') {
        return line.to_string();
    }
    let rest = &line[prefix_end + ": line ".len()..];
    let Some(message_at) = rest.find(": ") else {
        return line.to_string();
    };
    rest[message_at + 2..].to_string()
}

fn run_case(case: &Case) -> (String, String, i32) {
    let mut files = BTreeMap::new();
    for (name, contents) in &case.files {
        files.insert(format!("/home/user/{name}"), contents.clone());
    }
    let mut bash = Bash::new(Options {
        files,
        cwd: Some("/home/user".to_string()),
        ..Options::default()
    })
    .expect("initialize shell");
    let result = bash.exec(&case.command);
    (
        String::from_utf8_lossy(&result.stdout).into_owned(),
        String::from_utf8_lossy(&result.stderr).into_owned(),
        result.exit_code,
    )
}

#[test]
fn conformance_against_upstream_bash_fixtures() {
    let cases = load_cases();
    assert!(
        cases.len() > 400,
        "expected the full upstream corpus, found {}",
        cases.len()
    );

    let known = known_failures();
    let mut seen_known = std::collections::BTreeSet::new();
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut expected_failures = 0usize;
    let mut suites: BTreeMap<String, (usize, usize)> = BTreeMap::new();

    for case in &cases {
        let key = format!("{}/{}", case.suite, case.id);
        let (stdout, stderr, code) = run_case(case);
        let matches = stdout == case.stdout
            && normalize_stderr(&stderr) == normalize_stderr(&case.stderr)
            && code == case.exit_code;
        let stats = suites.entry(case.suite.clone()).or_default();
        if matches {
            passed += 1;
            stats.0 += 1;
            if known.contains(&key) {
                // A stale entry: the case passes now, so the list should shrink.
                failures.push(format!(
                    "{key} is listed in known_failures.txt but now passes"
                ));
            }
        } else if known.contains(&key) {
            expected_failures += 1;
            stats.1 += 1;
            seen_known.insert(key);
        } else {
            stats.1 += 1;
            failures.push(format!(
                "{key}\n    command: {:?}\n    expected: stdout={:?} stderr={:?} code={}\n    actual:   stdout={:?} stderr={:?} code={}",
                case.command, case.stdout, case.stderr, case.exit_code, stdout, stderr, code
            ));
        }
    }

    // Report a per-suite pass rate so progress is measurable.
    println!("\nconformance: {passed}/{} match bash", cases.len());
    for (suite, (ok, bad)) in &suites {
        println!("  {suite:24} {ok:3} pass {bad:3} fail");
    }
    println!("  ({expected_failures} of the failures are documented in known_failures.txt)");

    assert!(
        failures.is_empty(),
        "\n{} fixture(s) mismatch:\n{}\n",
        failures.len(),
        failures.join("\n")
    );
}
