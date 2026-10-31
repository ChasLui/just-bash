//! A small, dependency-free `jq` implementation.
//!
//! The `just-bash` runtime is an in-memory, deterministic shell whose threat
//! model treats scripts as untrusted input. This module is therefore written to
//! three rules:
//!
//! * **Bounded work.** JSON nesting, filter nesting, the number of emitted
//!   values and the number of accumulated output bytes are all capped (see
//!   [Limits](#limits).
//! * **No panics.** Every failure is a `Result`/`Option`; there is no `unsafe`
//!   and no arithmetic can overflow on adversarial input.
//! * **Fail closed.** Anything outside the documented subset is a *compile*
//!   error carrying a position, never a silent no-op.
//!
//! # Command line
//!
//! | Flag | Meaning |
//! |------|---------|
//! | `-r`, `--raw-output` | strings are written without quotes or escaping |
//! | `-j`, `--join-output` | like `-r`, and no trailing newline is printed |
//! | `-c`, `--compact-output` | one compact JSON value per line |
//! | `-n`, `--null-input` | the filter runs once with `null` as its input |
//! | `-s`, `--slurp` | all input documents are read into a single array |
//! | `-e`, `--exit-status` | exit status reflects the last output value |
//! | `-a`, `--ascii-output` | non-ASCII characters are escaped as `\uXXXX` |
//! | `--arg NAME VALUE` | binds `$NAME` to the string `VALUE` (repeatable) |
//! | `--argjson NAME JSON` | binds `$NAME` to a parsed JSON `VALUE` (repeatable) |
//! | `--tab` | indent with one tab per level |
//! | `--indent N` | indent with `N` spaces per level (`0`..=`7`) |
//! | `--` | ends option processing |
//!
//! The first non-option operand is the filter; every later operand is an input
//! file. With no file operands the input is standard input. `-n` takes
//! precedence over `-s`; file operands are still opened (so a missing file is
//! still reported but their contents are not read when `-n` is given.
//!
//! Input is a sequence of JSON documents, so concatenated, whitespace-separated
//! and newline-delimited documents all work.
//!
//! # Filter language subset
//!
//! * identity `.`; field access `.foo`, `.foo.bar`, `."quoted key"`,
//!   `.["key"]`; optional forms `.foo?`, `.["key"]?`
//! * index `.[0]`, `.[-1]`; slice `.[start:end]` (either side optional,
//!   negatives allowed; iteration `.[]`, `.[]?`
//! * pipe `|`, comma `,`, grouping `( … )`
//! * literals: numbers, strings (with escapes), `true`, `false`, `null`
//! * array construction `[ … ]` (collects a stream) and object construction
//!   `{a: f, "b": g, k, $v, (expr): value}`; a shorthand `{k}` means
//!   `{k: .k}` and `{$v}` means `{"v": $v}`
//! * arithmetic `+ - * / %` with jq's semantics: `+` adds numbers, concatenates
//!   strings and arrays and merges objects (right wins), `-` subtracts numbers
//!   and removes array elements, `*` multiplies numbers, repeats strings and
//!   deep-merges objects, `/` divides numbers and splits strings, `%` is the
//!   integer remainder
//! * comparisons `== != < <= > >=` using jq's total order
//!   (`null < false < true < numbers < strings < arrays < objects`), plus
//!   `and`, `or` and the `not` builtin
//! * alternative `//` (the right side runs when the left yields no truthy
//!   value
//! * `if C then A elif D then B else E end`, `if C then A end` (implicit
//!   `else .`, `try EXP catch HANDLER`, `EXP?`
//! * `EXP as $x | BODY` bindings and `$name` variables
//! * `reduce EXP as $x (INIT; UPDATE)` and
//!   `foreach EXP as $x (INIT; UPDATE)` / `(INIT; UPDATE; EXTRACT)`. The
//!   two-argument `foreach` emits the updated state after each iteration,
//!   which is `…; EXTRACT` with `EXTRACT` set to `.`
//! * `#` comments run to the end of the line
//!
//! A call's arguments are separated by `;`, and a comma inside one argument is
//! the comma operator, so `del(.a, .b)` deletes two paths and `map(.a, .b)`
//! maps a stream.
//!
//! # Builtins
//!
//! `length`, `utf8bytelength`, `keys`, `keys_unsorted`, `values`, `has(k)`,
//! `in(obj)`, `type`, `tostring`, `tonumber`, `tojson`, `fromjson`,
//! `ascii_downcase`, `ascii_upcase`, `explode`, `implode`, `add`, `any`,
//! `any(f)`, `all`, `all(f)`, `min`, `max`, `min_by(f)`, `max_by(f)`, `floor`,
//! `ceil`, `round`, `sqrt`, `fabs`, `sort`, `sort_by(f)`, `group_by(f)`,
//! `unique`, `unique_by(f)`, `reverse`, `first`, `first(f)`, `last`,
//! `last(f)`, `nth(n)`, `nth(n; f)`, `limit(n; f)`, `select(f)`, `map(f)`,
//! `map_values(f)`, `paths`, `paths(f)`, `leaf_paths`, `del(f)`, `getpath(p)`,
//! `setpath(p; v)`, `to_entries`, `from_entries`, `with_entries(f)`, `join(s)`,
//! `split(s)`, `splits(re)`, `ltrimstr(s)`, `rtrimstr(s)`, `startswith(s)`,
//! `endswith(s)`, `test(re)`, `match(re)`, `capture(re)`, `sub(re; s)`,
//! `gsub(re; s)`, `empty`, `error`, `error(msg)`, `not`.
//!
//! `paths` and `leaf_paths` report raw path components, so `{"a b": 1}` has the
//! path `["a b"]`. `del(f)` reads `f` as a *location* rather than a value: it
//! accepts `.`, field/index/iterate/slice steps, `?`, `select(f)`, `empty`, `|`
//! and `,`, and reports jq's `Invalid path expression` error for anything else.
//! `add` and `any`/`all` iterate objects as well as arrays, exactly like `.[]`.
//!
//! The regex-taking builtins accept a pattern string, an object
//! `{re: "…", flags: "…"}` or an array `["…", "…"]`, and every one of them also
//! accepts a trailing flags argument: `test(re; flags)`, `match(re; flags)`,
//! `capture(re; flags)`, `splits(re; flags)`, `sub(re; s; flags)` and
//! `gsub(re; s; flags)`. Supported flags are `i` (ASCII case folding) and `g`
//! (all matches, which `match` and `sub` otherwise ignore). `capture`, `match`
//! and the replacement filter of `sub`/`gsub` understand named groups written
//! `(?<name>…)` or `(?P<name>…)`; the names are stripped before the pattern
//! reaches [`Regex`]. `match` reports `captures` as the capture groups only,
//! with `offset`, `length` and `string` describing the whole match; a
//! `sub`/`gsub` replacement sees the same object with one extra field per named
//! group, so `.x` refers to `(?<x>…)`:
//!
//! ```text
//! $ jq -r '"a-b" | sub("(?<x>[a-z]+)-"; "\(.x)!")'   # => a!b
//! ```
//!
//! `split(s)` splits on a literal separator, exactly like jq; `splits(re)`
//! splits on a regular expression. For symmetry `split({re: "…"})` also splits
//! on a regular expression.
//!
//! # Deviations from jq 1.8
//!
//! Two deliberate differences, both narrower than they look:
//!
//! * **Numbers are normalized.** This implementation keeps an `f64` and prints
//!   the shortest plain decimal form, so the filter `1e3` prints `1000` and
//!   `1.0` prints `1`. jq 1.8 preserves the literal text it read (`1E+3`,
//!   `1.0`) for values that pass through unchanged, and prints `1000` for the
//!   same numbers once any arithmetic touches them. The two agree on every
//!   computed value and on JSON round-tripping; only cosmetic literal echo
//!   differs.
//! * **The regex object/array forms are accepted everywhere.** jq 1.8 takes
//!   `{re, flags}` only in some regex builtins and reports
//!   `object not a string or array` elsewhere. This implementation accepts the
//!   object and array forms uniformly.
//!
//! ## Deliberately unsupported
//!
//! Every one of these is a compile error (exit status 3) naming the construct:
//! `def`, `label`/`break`, the assignment operators `=`, `|=`, `+=`, `-=`,
//! `*=`, `/=`, `%=`, `//=`, recursive descent `..`, format strings `@base64`,
//! `env`/`$ENV`, `input`/`inputs`, `debug`, `walk`, `path`, `range`, `recurse`,
//! `ascii` and any other unknown function. Destructuring bindings (`as [$a]`)
//! are rejected too. String interpolation `"\(…)"` **is** supported, in string
//! values, object keys and `sub`/`gsub` replacements.
//!
//! # Limits
//!
//! | Limit | Value | Enforced in |
//! |-------|-------|-------------|
//! | [`MAX_DEPTH`] | 64 | JSON parsing, serialization, path construction, value merging |
//! | [`MAX_OUTPUT_BYTES`] | 1_000_000 | accumulated standard output |
//! | `MAX_VALUES` | 100_000 | documents read, values produced by one expression |
//! | [`MAX_EVAL_DEPTH`] | 64 | filter evaluation recursion |
//! | [`MAX_FILTER_NESTING`] | 64 | filter parsing recursion |
//! | [`MAX_FILTER_TOKENS`] | 2048 | filter size, which bounds the depth of the expression tree |
//! | `MAX_STRING_BYTES` | 8_388_608 | string repetition and `implode` |
//!
//! The two filter limits exist to keep a hostile filter from exhausting the
//! stack: they are enforced while parsing, before any tree is walked or
//! dropped, so an over-limit filter is always a clean exit-3 error.
//!
//! # Errors and exit codes
//!
//! | Status | Cause | Message shape |
//! |--------|-------|---------------|
//! | 2 | usage error, or a file operand that cannot be opened | `jq: error: Could not open NAME` |
//! | 3 | the filter does not compile | `jq: error: SYNTAX at line L, column C` |
//! | 5 | runtime error (bad JSON input, wrong type, division by zero, a limit) | `jq: error (at <stdin>): MESSAGE` |
//! | 4 | `-e` and the last output was `false` or `null` | – |
//! | 1 | `-e` and the filter produced no output | – |
//! | 0 | otherwise | – |
//!
//! A runtime error keeps whatever was already written to standard output, which
//! is what `jq` does.

use just_bash_regex::Regex;

/// Maximum JSON nesting depth accepted by the parser and the serializer, and
/// the maximum depth a constructed value may reach.
///
/// A document nested more deeply than this is a clean error: parsing reports
/// `value exceeds maximum nesting depth` and the evaluator reports the same
/// before a value is built that would overflow the stack.
pub const MAX_DEPTH: usize = 64;

/// Maximum number of bytes accumulated on standard output by [`run`].
///
/// When the limit is reached the run stops, a message is written to standard
/// error and the exit status is 5.
pub const MAX_OUTPUT_BYTES: usize = 1_000_000;

/// Maximum number of input documents, and of values produced by one filter
/// expression, before the run fails with a runtime error.
const MAX_VALUES: usize = 100_000;

/// Maximum recursion depth of the filter evaluator.
///
/// Every nesting level costs a whole precedence chain of stack frames while
/// parsing and one `eval` frame per operator while evaluating, so the budget is
/// deliberately small: a filter at this depth must not be able to exhaust a
/// default 2 MiB thread stack. Adversarial `((((…))))`, `a | b | …` and
/// `1 + 1 + …` chains are rejected with a clean error well before that point.
const MAX_EVAL_DEPTH: usize = 64;

/// Maximum length of a string built by string repetition or `implode`.
const MAX_STRING_BYTES: usize = 8 * 1024 * 1024;

/// Largest filter nesting accepted by the parser.
///
/// Kept in step with [`MAX_EVAL_DEPTH`]: the parser is the first place a deeply
/// nested filter would overflow the stack, and one nesting level expands into
/// the full `parse_pipe` → … → `parse_postfix` chain.
const MAX_FILTER_NESTING: usize = 64;

/// Largest number of tokens accepted in one filter.
///
/// Bounding tokens bounds the size of the expression tree, and therefore the
/// recursion depth of evaluating it *and* of dropping it. Without this, a flat
/// chain such as `1 + 1 + …` (or a very long array literal) parses iteratively
/// into a left-nested tree whose recursive `Drop` would overflow the stack.
const MAX_FILTER_TOKENS: usize = 2048;

/// Result of one `jq` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JqRun {
    /// Everything the filter wrote to standard output.
    pub stdout: String,
    /// Everything the filter wrote to standard error, including the trailing
    /// newline that `jq` prints after a message.
    pub stderr: String,
    /// Process exit status; see the module documentation for the meaning of
    /// each value.
    pub exit_code: i32,
}

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number. JSON numbers are held as `f64`; non-finite values cannot be
    /// parsed and are written back as `null`.
    Number(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Json>),
    /// Object entries. Key order is preserved as written.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Parse one JSON document (trailing whitespace allowed).
    ///
    /// # Errors
    ///
    /// Returns a human-readable message with a line and column for malformed
    /// input, trailing characters, or nesting deeper than [`MAX_DEPTH`].
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut parser = DocParser::new(text);
        parser.skip_whitespace();
        let value = parser.parse_value(0)?;
        parser.skip_whitespace();
        if parser.peek().is_some() {
            return Err(parser.error("unexpected trailing characters"));
        }
        Ok(value)
    }

    /// Serialize with `indent` spaces per level (0 = compact).
    ///
    /// Values produced by [`Json::parse`] and by [`run`] are never nested
    /// deeper than [`MAX_DEPTH`], so this cannot fail for them. A value built
    /// by hand that is nested too deeply is rendered as the marker string
    /// `"<depth-limit-exceeded>"` instead of overflowing the stack.
    pub fn to_string_pretty(&self, indent: usize) -> String {
        match self.render(indent, false) {
            Ok(text) => text,
            Err(_) => String::from("\"<depth-limit-exceeded>\""),
        }
    }

    /// jq's `type` name: "null" | "boolean" | "number" | "string" | "array" | "object".
    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Number(_) => "number",
            Json::String(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }

    /// jq truthiness: false and null are falsey, everything else truthy.
    pub fn is_truthy(&self) -> bool {
        !matches!(self, Json::Null | Json::Bool(false))
    }

    /// Render this value as JSON text, escaping non-ASCII when `ascii` is set.
    fn render(&self, indent: usize, ascii: bool) -> Result<String, String> {
        let mut out = String::new();
        write_value(&mut out, self, RenderOpts { indent, ascii }, 0)?;
        Ok(out)
    }

    /// Render compactly, as `tostring` and `tojson` need.
    fn compact(&self) -> String {
        self.render(0, false)
            .unwrap_or_else(|_| String::from("\"<depth-limit-exceeded>\""))
    }

    /// Fail when this value is nested more deeply than [`MAX_DEPTH`].
    fn check_depth(&self) -> Result<(), String> {
        if value_depth_within(self, MAX_DEPTH) {
            Ok(())
        } else {
            Err(format!("value exceeds maximum nesting depth ({MAX_DEPTH})"))
        }
    }
}

/// Options for one serialization pass.
#[derive(Debug, Clone, Copy)]
struct RenderOpts {
    /// Spaces per level; 0 means compact.
    indent: usize,
    /// Escape every non-ASCII character.
    ascii: bool,
}

/// Append one JSON value, failing when the nesting exceeds [`MAX_DEPTH`].
fn write_value(
    out: &mut String,
    value: &Json,
    opts: RenderOpts,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("value exceeds maximum nesting depth ({MAX_DEPTH})"));
    }
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(number) => out.push_str(&number_text(*number)),
        Json::String(text) => {
            out.push('"');
            escape_into(out, text, opts.ascii);
            out.push('"');
        }
        Json::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(out, opts, depth + 1);
                write_value(out, item, opts, depth + 1)?;
            }
            newline_indent(out, opts, depth);
            out.push(']');
        }
        Json::Object(entries) => {
            if entries.is_empty() {
                out.push_str("{}");
                return Ok(());
            }
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline_indent(out, opts, depth + 1);
                out.push('"');
                escape_into(out, key, opts.ascii);
                out.push('"');
                out.push(':');
                if opts.indent > 0 {
                    out.push(' ');
                }
                write_value(out, item, opts, depth + 1)?;
            }
            newline_indent(out, opts, depth);
            out.push('}');
        }
    }
    Ok(())
}

/// Emit the line break and indentation that precedes a nested value.
fn newline_indent(out: &mut String, opts: RenderOpts, depth: usize) {
    if opts.indent == 0 {
        return;
    }
    out.push('\n');
    for _ in 0..depth * opts.indent {
        out.push(' ');
    }
}

/// Format an `f64` the way jq prints numbers.
///
/// Integral values inside the exactly-representable range lose their decimal
/// point (`1` rather than `1.0`); values whose plain decimal form would be
/// unwieldy fall back to exponent notation. Non-finite values print as `null`,
/// which keeps the output valid JSON.
fn number_text(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e17 {
        return (value as i64).to_string();
    }
    let plain = format!("{value}");
    if plain.len() <= 24 {
        plain
    } else {
        format!("{value:e}")
    }
}

/// Escape the contents of a JSON string (without the surrounding quotes).
fn escape_into(out: &mut String, text: &str, ascii: bool) {
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other if ascii && !other.is_ascii() => {
                let code = other as u32;
                if code > 0xffff {
                    let offset = code - 0x1_0000;
                    let high = 0xd800 + (offset >> 10);
                    let low = 0xdc00 + (offset & 0x3ff);
                    out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
                } else {
                    out.push_str(&format!("\\u{code:04x}"));
                }
            }
            other => out.push(other),
        }
    }
}

/// True when `value` nests no deeper than `limit` containers.
///
/// Iterative, so a hand-built value cannot overflow the stack here.
fn value_depth_within(value: &Json, limit: usize) -> bool {
    let mut stack: Vec<(&Json, usize)> = vec![(value, 0)];
    while let Some((current, depth)) = stack.pop() {
        if depth > limit {
            return false;
        }
        match current {
            Json::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            Json::Object(entries) => {
                for (_, item) in entries {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    true
}

/// Order two values with jq's total order, failing past [`MAX_DEPTH`].
fn compare_values(left: &Json, right: &Json, depth: usize) -> Result<std::cmp::Ordering, String> {
    use std::cmp::Ordering;
    if depth > MAX_DEPTH {
        return Err(format!("value exceeds maximum nesting depth ({MAX_DEPTH})"));
    }
    let left_rank = type_rank(left);
    let right_rank = type_rank(right);
    if left_rank != right_rank {
        return Ok(left_rank.cmp(&right_rank));
    }
    match (left, right) {
        (Json::Null, Json::Null) => Ok(Ordering::Equal),
        (Json::Bool(a), Json::Bool(b)) => Ok(a.cmp(b)),
        (Json::Number(a), Json::Number(b)) => Ok(a.partial_cmp(b).unwrap_or(Ordering::Equal)),
        (Json::String(a), Json::String(b)) => Ok(a.cmp(b)),
        (Json::Array(a), Json::Array(b)) => {
            for (left_item, right_item) in a.iter().zip(b.iter()) {
                let ordering = compare_values(left_item, right_item, depth + 1)?;
                if ordering != Ordering::Equal {
                    return Ok(ordering);
                }
            }
            Ok(a.len().cmp(&b.len()))
        }
        (Json::Object(a), Json::Object(b)) => {
            let left_entries = sorted_entries(a);
            let right_entries = sorted_entries(b);
            let left_keys: Vec<&str> = left_entries.iter().map(|(key, _)| *key).collect();
            let right_keys: Vec<&str> = right_entries.iter().map(|(key, _)| *key).collect();
            let keys = left_keys.cmp(&right_keys);
            if keys != Ordering::Equal {
                return Ok(keys);
            }
            for ((_, left_value), (_, right_value)) in left_entries.iter().zip(right_entries.iter())
            {
                let ordering = compare_values(left_value, right_value, depth + 1)?;
                if ordering != Ordering::Equal {
                    return Ok(ordering);
                }
            }
            Ok(Ordering::Equal)
        }
        _ => Ok(Ordering::Equal),
    }
}

/// jq's type ordering rank: null < false < true < numbers < strings < arrays
/// < objects. Booleans rank together; within booleans `false < true`.
fn type_rank(value: &Json) -> u8 {
    match value {
        Json::Null => 0,
        Json::Bool(_) => 1,
        Json::Number(_) => 2,
        Json::String(_) => 3,
        Json::Array(_) => 4,
        Json::Object(_) => 5,
    }
}

/// Object entries sorted by key, which is the order jq compares objects in.
fn sorted_entries(entries: &[(String, Json)]) -> Vec<(&str, &Json)> {
    let mut sorted: Vec<(&str, &Json)> = entries
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted
}

/// Structural equality with jq's rules: objects compare as unordered maps.
fn equal_values(left: &Json, right: &Json, depth: usize) -> Result<bool, String> {
    if depth > MAX_DEPTH {
        return Err(format!("value exceeds maximum nesting depth ({MAX_DEPTH})"));
    }
    Ok(match (left, right) {
        (Json::Null, Json::Null) => true,
        (Json::Bool(a), Json::Bool(b)) => a == b,
        (Json::Number(a), Json::Number(b)) => a == b,
        (Json::String(a), Json::String(b)) => a == b,
        (Json::Array(a), Json::Array(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            for (left_item, right_item) in a.iter().zip(b.iter()) {
                if !equal_values(left_item, right_item, depth + 1)? {
                    return Ok(false);
                }
            }
            true
        }
        (Json::Object(a), Json::Object(b)) => {
            if a.len() != b.len() {
                return Ok(false);
            }
            let right_entries = sorted_entries(b);
            for (key, value) in a {
                let found = right_entries
                    .binary_search_by(|(other, _)| (*other).cmp(key.as_str()))
                    .ok()
                    .map(|index| right_entries[index].1);
                match found {
                    Some(other) => {
                        if !equal_values(value, other, depth + 1)? {
                            return Ok(false);
                        }
                    }
                    None => return Ok(false),
                }
            }
            true
        }
        _ => false,
    })
}

/// Parser for JSON documents, used for whole documents and for streams.
struct DocParser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
}

impl DocParser {
    /// Start parsing `text`.
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
            line: 1,
            column: 1,
        }
    }

    /// The character under the cursor, if any.
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// The character `offset` positions ahead, if any.
    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Consume one character, tracking the line and column.
    fn bump(&mut self) -> Option<char> {
        let character = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if character == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(character)
    }

    /// Build an error message pointing at the cursor.
    fn error(&self, message: &str) -> String {
        format!("{message} at line {}, column {}", self.line, self.column)
    }

    /// Skip JSON whitespace.
    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.bump();
        }
    }

    /// Parse the next document of a stream, or `None` at end of input.
    fn next_document(&mut self) -> Result<Option<Json>, String> {
        self.skip_whitespace();
        if self.peek().is_none() {
            return Ok(None);
        }
        self.parse_value(0).map(Some)
    }

    /// Parse one value; `depth` counts the containers already open.
    fn parse_value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.error(&format!(
                "value exceeds maximum nesting depth ({MAX_DEPTH})"
            )));
        }
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some('{') => self.parse_object(depth),
            Some('[') => self.parse_array(depth),
            Some('"') => Ok(Json::String(self.parse_string()?)),
            Some('t') => self.parse_keyword("true", Json::Bool(true)),
            Some('f') => self.parse_keyword("false", Json::Bool(false)),
            Some('n') => self.parse_keyword("null", Json::Null),
            Some(character) if character == '-' || character.is_ascii_digit() => {
                self.parse_number()
            }
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    /// Parse a bare keyword such as `true`.
    fn parse_keyword(&mut self, word: &str, value: Json) -> Result<Json, String> {
        for expected in word.chars() {
            match self.bump() {
                Some(actual) if actual == expected => {}
                _ => return Err(self.error(&format!("expected `{word}`"))),
            }
        }
        Ok(value)
    }

    /// Parse an object; duplicate keys keep the position of their first
    /// occurrence and the value of the last.
    fn parse_object(&mut self, depth: usize) -> Result<Json, String> {
        self.bump();
        let mut entries: Vec<(String, Json)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Json::Object(entries));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some('"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek() != Some(':') {
                return Err(self.error("expected `:`"));
            }
            self.bump();
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            match entries.iter_mut().find(|(existing, _)| *existing == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    return Ok(Json::Object(entries));
                }
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    /// Parse an array.
    fn parse_array(&mut self, depth: usize) -> Result<Json, String> {
        self.bump();
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    /// Parse a string literal, decoding escapes and surrogate pairs.
    fn parse_string(&mut self) -> Result<String, String> {
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some('"') => return Ok(out),
                Some('\\') => match self.bump() {
                    None => return Err(self.error("unterminated escape")),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('u') => {
                        let unit = self.parse_hex4()?;
                        if (0xd800..0xdc00).contains(&unit) {
                            // High surrogate: pair it when the next escape is a
                            // low surrogate, otherwise emit the replacement
                            // character, which is what jq does.
                            if self.peek() == Some('\\') && self.peek_at(1) == Some('u') {
                                let mark = (self.pos, self.line, self.column);
                                self.bump();
                                self.bump();
                                match self.parse_hex4() {
                                    Ok(low) if (0xdc00..0xe000).contains(&low) => {
                                        let code =
                                            0x1_0000 + ((unit - 0xd800) << 10) + (low - 0xdc00);
                                        out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                    }
                                    _ => {
                                        self.pos = mark.0;
                                        self.line = mark.1;
                                        self.column = mark.2;
                                        out.push('\u{fffd}');
                                    }
                                }
                            } else {
                                out.push('\u{fffd}');
                            }
                        } else if (0xdc00..0xe000).contains(&unit) {
                            out.push('\u{fffd}');
                        } else {
                            out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                        }
                    }
                    Some(_) => return Err(self.error("invalid escape sequence")),
                },
                Some(control) if (control as u32) < 0x20 => {
                    return Err(self.error("unescaped control character in string"));
                }
                Some(other) => out.push(other),
            }
        }
    }

    /// Parse the four hex digits of a `\uXXXX` escape.
    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            match self.bump() {
                Some(digit) if digit.is_ascii_hexdigit() => {
                    value = value * 16 + digit.to_digit(16).unwrap_or(0);
                }
                _ => return Err(self.error("invalid unicode escape")),
            }
        }
        Ok(value)
    }

    /// Parse a number; leading zeros and out-of-range values are rejected.
    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.bump();
        }
        match self.peek() {
            Some('0') => {
                self.bump();
                if matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                    return Err(self.error("invalid numeric literal"));
                }
            }
            Some(next) if next.is_ascii_digit() => {
                while matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                    self.bump();
                }
            }
            _ => return Err(self.error("invalid numeric literal")),
        }
        if self.peek() == Some('.') {
            self.bump();
            if !matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                return Err(self.error("invalid numeric literal"));
            }
            while matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                return Err(self.error("invalid numeric literal"));
            }
            while matches!(self.peek(), Some(next) if next.is_ascii_digit()) {
                self.bump();
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        match text.parse::<f64>() {
            Ok(number) if number.is_finite() => Ok(Json::Number(number)),
            Ok(_) => Err(self.error("number out of range")),
            Err(_) => Err(self.error("invalid numeric literal")),
        }
    }
}

/// A compile error: a message and the 1-based position it was found at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompileError {
    /// Human-readable description of the problem.
    message: String,
    /// 1-based line of the offending token.
    line: usize,
    /// 1-based column of the offending token.
    column: usize,
}

impl CompileError {
    /// Render as `jq: error: MESSAGE at line L, column C`.
    fn render(&self) -> String {
        format!(
            "jq: error: {} at line {}, column {}",
            self.message, self.line, self.column
        )
    }
}

/// One segment of a string literal.
#[derive(Debug, Clone, PartialEq)]
enum RawStrPart {
    /// Literal text, already unescaped.
    Text(String),
    /// The source of an interpolation, with the line it started at.
    Interp(String, usize),
}

/// One lexical token of the filter language.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// `.`
    Dot,
    /// `..`
    Recurse,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `|`
    Pipe,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `;`
    Semi,
    /// `?`
    Question,
    /// `//`
    Alt,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `and`
    And,
    /// `or`
    Or,
    /// `not`
    Not,
    /// `if`
    If,
    /// `then`
    Then,
    /// `elif`
    Elif,
    /// `else`
    Else,
    /// `end`
    End,
    /// `as`
    As,
    /// `reduce`
    Reduce,
    /// `foreach`
    Foreach,
    /// `try`
    Try,
    /// `catch`
    Catch,
    /// `$name`
    Var(String),
    /// `NAME`, or `true` / `false` / `null`
    Ident(String),
    /// a string literal: its segments
    Str(Vec<RawStrPart>),
    /// a numeric literal
    Num(f64),
    /// end of input
    Eof,
}

impl Tok {
    /// How this token is named in error messages.
    fn describe(&self) -> String {
        match self {
            Tok::Dot => "`.`".to_string(),
            Tok::Recurse => "`..`".to_string(),
            Tok::LBracket => "`[`".to_string(),
            Tok::RBracket => "`]`".to_string(),
            Tok::LBrace => "`{`".to_string(),
            Tok::RBrace => "`}`".to_string(),
            Tok::LParen => "`(`".to_string(),
            Tok::RParen => "`)`".to_string(),
            Tok::Pipe => "`|`".to_string(),
            Tok::Comma => "`,`".to_string(),
            Tok::Colon => "`:`".to_string(),
            Tok::Semi => "`;`".to_string(),
            Tok::Question => "`?`".to_string(),
            Tok::Alt => "`//`".to_string(),
            Tok::Add => "`+`".to_string(),
            Tok::Sub => "`-`".to_string(),
            Tok::Mul => "`*`".to_string(),
            Tok::Div => "`/`".to_string(),
            Tok::Rem => "`%`".to_string(),
            Tok::Eq => "`==`".to_string(),
            Tok::Ne => "`!=`".to_string(),
            Tok::Lt => "`<`".to_string(),
            Tok::Le => "`<=`".to_string(),
            Tok::Gt => "`>`".to_string(),
            Tok::Ge => "`>=`".to_string(),
            Tok::And => "`and`".to_string(),
            Tok::Or => "`or`".to_string(),
            Tok::Not => "`not`".to_string(),
            Tok::If => "`if`".to_string(),
            Tok::Then => "`then`".to_string(),
            Tok::Elif => "`elif`".to_string(),
            Tok::Else => "`else`".to_string(),
            Tok::End => "`end`".to_string(),
            Tok::As => "`as`".to_string(),
            Tok::Reduce => "`reduce`".to_string(),
            Tok::Foreach => "`foreach`".to_string(),
            Tok::Try => "`try`".to_string(),
            Tok::Catch => "`catch`".to_string(),
            Tok::Var(name) => format!("`${name}`"),
            Tok::Ident(name) => format!("`{name}`"),
            Tok::Str(_) => "a string".to_string(),
            Tok::Num(value) => format!("`{}`", number_text(*value)),
            Tok::Eof => "end of input".to_string(),
        }
    }
}

/// A token together with the position it started at.
#[derive(Debug, Clone)]
struct Token {
    /// The token itself.
    tok: Tok,
    /// 1-based line the token starts on.
    line: usize,
    /// 1-based column the token starts at.
    column: usize,
}

/// Words that may not be used as function names, because the lexer gives them
/// a dedicated token.
const RESERVED: &[&str] = &[
    "and", "or", "not", "if", "then", "elif", "else", "end", "as", "reduce", "foreach", "try",
    "catch", "def", "label", "import", "include", "module", "break",
];

/// Whether the last token emitted can end an expression.
///
/// A `-` right after a term is the binary subtraction operator; anywhere else it
/// may start a negative numeric literal (`1-1` is `1 - 1`, `[-1]` is a negative
/// index).
fn previous_ends_expression(tokens: &[Token]) -> bool {
    match tokens.last() {
        None => false,
        Some(token) => matches!(
            token.tok,
            Tok::Num(_)
                | Tok::Str(_)
                | Tok::Var(_)
                | Tok::Ident(_)
                | Tok::RParen
                | Tok::RBracket
                | Tok::RBrace
        ),
    }
}

/// Turn filter text into tokens, splitting string interpolations out.
fn lex(source: &str) -> Result<Vec<Token>, CompileError> {
    let chars: Vec<char> = source.chars().collect();
    let mut pos = 0usize;
    let mut line = 1usize;
    let mut column = 1usize;
    let mut tokens = Vec::new();

    while pos < chars.len() {
        let character = chars[pos];
        if character == '\n' {
            pos += 1;
            line += 1;
            column = 1;
            continue;
        }
        if character.is_whitespace() {
            pos += 1;
            column += 1;
            continue;
        }
        if character == '#' {
            while pos < chars.len() && chars[pos] != '\n' {
                pos += 1;
                column += 1;
            }
            continue;
        }
        let start_line = line;
        let start_column = column;
        match character {
            '.' => {
                if chars.get(pos + 1) == Some(&'.') {
                    tokens.push(Token {
                        tok: Tok::Recurse,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else if matches!(chars.get(pos + 1), Some(next) if next.is_ascii_digit()) {
                    let (tok, end, next_column) =
                        lex_number(&chars, pos, column, start_line, start_column)?;
                    tokens.push(Token {
                        tok,
                        line: start_line,
                        column: start_column,
                    });
                    pos = end;
                    column = next_column;
                } else {
                    tokens.push(Token {
                        tok: Tok::Dot,
                        line,
                        column,
                    });
                    pos += 1;
                    column += 1;
                }
            }
            '-' => {
                let after_term = previous_ends_expression(&tokens);
                if !after_term && matches!(chars.get(pos + 1), Some(next) if next.is_ascii_digit())
                {
                    let (tok, end, next_column) =
                        lex_number(&chars, pos, column, start_line, start_column)?;
                    tokens.push(Token {
                        tok,
                        line: start_line,
                        column: start_column,
                    });
                    pos = end;
                    column = next_column;
                } else if chars.get(pos + 1) == Some(&'=') {
                    tokens.push(Token {
                        tok: Tok::Ne,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else {
                    tokens.push(Token {
                        tok: Tok::Sub,
                        line,
                        column,
                    });
                    pos += 1;
                    column += 1;
                }
            }
            '0'..='9' => {
                let (tok, end, next_column) =
                    lex_number(&chars, pos, column, start_line, start_column)?;
                tokens.push(Token {
                    tok,
                    line: start_line,
                    column: start_column,
                });
                pos = end;
                column = next_column;
            }
            '[' => {
                let at = column;
                push_single(&mut tokens, Tok::LBracket, &mut pos, &mut column, line, at)?;
            }
            ']' => {
                let at = column;
                push_single(&mut tokens, Tok::RBracket, &mut pos, &mut column, line, at)?;
            }
            '{' => {
                let at = column;
                push_single(&mut tokens, Tok::LBrace, &mut pos, &mut column, line, at)?;
            }
            '}' => {
                let at = column;
                push_single(&mut tokens, Tok::RBrace, &mut pos, &mut column, line, at)?;
            }
            '(' => {
                let at = column;
                push_single(&mut tokens, Tok::LParen, &mut pos, &mut column, line, at)?;
            }
            ')' => {
                let at = column;
                push_single(&mut tokens, Tok::RParen, &mut pos, &mut column, line, at)?;
            }
            ',' => {
                let at = column;
                push_single(&mut tokens, Tok::Comma, &mut pos, &mut column, line, at)?;
            }
            ':' => {
                let at = column;
                push_single(&mut tokens, Tok::Colon, &mut pos, &mut column, line, at)?;
            }
            ';' => {
                let at = column;
                push_single(&mut tokens, Tok::Semi, &mut pos, &mut column, line, at)?;
            }
            '?' => {
                let at = column;
                push_single(&mut tokens, Tok::Question, &mut pos, &mut column, line, at)?;
            }
            '+' => {
                if chars.get(pos + 1) == Some(&'=') {
                    return Err(CompileError {
                        message: "unsupported operator `+=`; write `. = . + …` instead".to_string(),
                        line,
                        column,
                    });
                }
                push_one(
                    &mut tokens,
                    Tok::Add,
                    &mut pos,
                    &mut column,
                    start_line,
                    start_column,
                );
            }
            '*' => {
                if chars.get(pos + 1) == Some(&'=') {
                    return Err(CompileError {
                        message: "unsupported operator `*=`; write `. = . * …` instead".to_string(),
                        line,
                        column,
                    });
                }
                push_one(
                    &mut tokens,
                    Tok::Mul,
                    &mut pos,
                    &mut column,
                    start_line,
                    start_column,
                );
            }
            '%' => {
                if chars.get(pos + 1) == Some(&'=') {
                    return Err(CompileError {
                        message: "unsupported operator `%=`; write `. = . % …` instead".to_string(),
                        line,
                        column,
                    });
                }
                push_one(
                    &mut tokens,
                    Tok::Rem,
                    &mut pos,
                    &mut column,
                    start_line,
                    start_column,
                );
            }
            '|' => {
                if chars.get(pos + 1) == Some(&'=') {
                    return Err(CompileError {
                        message: "unsupported operator `|=`; write `. = …` instead".to_string(),
                        line,
                        column,
                    });
                }
                push_one(
                    &mut tokens,
                    Tok::Pipe,
                    &mut pos,
                    &mut column,
                    start_line,
                    start_column,
                );
            }
            '/' => {
                if chars.get(pos + 1) == Some(&'/') {
                    if chars.get(pos + 2) == Some(&'=') {
                        return Err(CompileError {
                            message: "unsupported operator `//=`; write `. = …` instead"
                                .to_string(),
                            line,
                            column,
                        });
                    }
                    tokens.push(Token {
                        tok: Tok::Alt,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else if chars.get(pos + 1) == Some(&'=') {
                    return Err(CompileError {
                        message: "unsupported operator `/=`; write `. = . / …` instead".to_string(),
                        line,
                        column,
                    });
                } else {
                    push_one(
                        &mut tokens,
                        Tok::Div,
                        &mut pos,
                        &mut column,
                        start_line,
                        start_column,
                    );
                }
            }
            '=' => {
                if chars.get(pos + 1) == Some(&'=') {
                    tokens.push(Token {
                        tok: Tok::Eq,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else {
                    return Err(CompileError {
                        message: "unsupported assignment `=`; this subset has no assignments"
                            .to_string(),
                        line,
                        column,
                    });
                }
            }
            '!' => {
                if chars.get(pos + 1) == Some(&'=') {
                    tokens.push(Token {
                        tok: Tok::Ne,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else {
                    return Err(CompileError {
                        message: "unexpected character `!`".to_string(),
                        line,
                        column,
                    });
                }
            }
            '<' => {
                if chars.get(pos + 1) == Some(&'=') {
                    tokens.push(Token {
                        tok: Tok::Le,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else {
                    push_one(
                        &mut tokens,
                        Tok::Lt,
                        &mut pos,
                        &mut column,
                        start_line,
                        start_column,
                    );
                }
            }
            '>' => {
                if chars.get(pos + 1) == Some(&'=') {
                    tokens.push(Token {
                        tok: Tok::Ge,
                        line,
                        column,
                    });
                    pos += 2;
                    column += 2;
                } else {
                    push_one(
                        &mut tokens,
                        Tok::Gt,
                        &mut pos,
                        &mut column,
                        start_line,
                        start_column,
                    );
                }
            }
            '"' => {
                let lexed = lex_string(&chars, pos, line, column)?;
                tokens.push(Token {
                    tok: Tok::Str(lexed.parts),
                    line: start_line,
                    column: start_column,
                });
                pos = lexed.end;
                line = lexed.line;
                column = lexed.column;
            }
            '$' => {
                let mut end = pos + 1;
                let mut name = String::new();
                while let Some(next) = chars.get(end) {
                    if next.is_alphanumeric() || *next == '_' {
                        name.push(*next);
                        end += 1;
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    return Err(CompileError {
                        message: "expected a variable name after `$`".to_string(),
                        line,
                        column,
                    });
                }
                column += end - pos;
                tokens.push(Token {
                    tok: Tok::Var(name),
                    line: start_line,
                    column: start_column,
                });
                pos = end;
            }
            '@' => {
                return Err(CompileError {
                    message: "unsupported format string `@…`".to_string(),
                    line,
                    column,
                });
            }
            other if other.is_alphabetic() || other == '_' => {
                let mut end = pos;
                let mut name = String::new();
                while let Some(next) = chars.get(end) {
                    if next.is_alphanumeric() || *next == '_' {
                        name.push(*next);
                        end += 1;
                    } else {
                        break;
                    }
                }
                column += end - pos;
                pos = end;
                let tok = match name.as_str() {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "not" => Tok::Not,
                    "if" => Tok::If,
                    "then" => Tok::Then,
                    "elif" => Tok::Elif,
                    "else" => Tok::Else,
                    "end" => Tok::End,
                    "as" => Tok::As,
                    "reduce" => Tok::Reduce,
                    "foreach" => Tok::Foreach,
                    "try" => Tok::Try,
                    "catch" => Tok::Catch,
                    "true" | "false" | "null" => Tok::Ident(name.clone()),
                    other => {
                        if RESERVED.contains(&other) {
                            return Err(CompileError {
                                message: format!("unsupported construct `{other}` in this subset"),
                                line: start_line,
                                column: start_column,
                            });
                        }
                        Tok::Ident(other.to_string())
                    }
                };
                tokens.push(Token {
                    tok,
                    line: start_line,
                    column: start_column,
                });
            }
            other => {
                return Err(CompileError {
                    message: format!("unexpected character `{other}`"),
                    line,
                    column,
                });
            }
        }
    }

    tokens.push(Token {
        tok: Tok::Eof,
        line,
        column,
    });
    Ok(tokens)
}

/// Push a one-character token and advance the cursor.
fn push_one(
    tokens: &mut Vec<Token>,
    tok: Tok,
    pos: &mut usize,
    column: &mut usize,
    line: usize,
    at: usize,
) {
    tokens.push(Token {
        tok,
        line,
        column: at,
    });
    *pos += 1;
    *column += 1;
}

/// Push a single-character token and advance the cursor (the punctuation path
/// of the lexer, which uses an infallible helper).
fn push_single(
    tokens: &mut Vec<Token>,
    tok: Tok,
    pos: &mut usize,
    column: &mut usize,
    line: usize,
    at: usize,
) -> Result<(), CompileError> {
    push_one(tokens, tok, pos, column, line, at);
    Ok(())
}

/// Scan a numeric literal starting at `pos`.
fn lex_number(
    chars: &[char],
    pos: usize,
    column: usize,
    line: usize,
    start_column: usize,
) -> Result<(Tok, usize, usize), CompileError> {
    let mut end = pos;
    let mut text = String::new();
    if chars.get(end) == Some(&'-') {
        text.push('-');
        end += 1;
    }
    while matches!(chars.get(end), Some(next) if next.is_ascii_digit()) {
        text.push(chars[end]);
        end += 1;
    }
    if chars.get(end) == Some(&'.') {
        text.push('.');
        end += 1;
        while matches!(chars.get(end), Some(next) if next.is_ascii_digit()) {
            text.push(chars[end]);
            end += 1;
        }
    }
    if matches!(chars.get(end), Some('e' | 'E')) {
        text.push('e');
        end += 1;
        if matches!(chars.get(end), Some('+' | '-')) {
            text.push(chars[end]);
            end += 1;
        }
        while matches!(chars.get(end), Some(next) if next.is_ascii_digit()) {
            text.push(chars[end]);
            end += 1;
        }
        if !text.ends_with(|character: char| character.is_ascii_digit()) {
            return Err(CompileError {
                message: "invalid numeric literal".to_string(),
                line,
                column: start_column,
            });
        }
    }
    match text.parse::<f64>() {
        Ok(number) => Ok((Tok::Num(number), end, column + (end - pos))),
        Err(_) => Err(CompileError {
            message: format!("invalid numeric literal `{text}`"),
            line,
            column: start_column,
        }),
    }
}

/// The result of scanning a string literal.
struct LexedString {
    /// The string's segments.
    parts: Vec<RawStrPart>,
    /// Index just past the closing quote.
    end: usize,
    /// Line the string ends on.
    line: usize,
    /// Column just past the closing quote.
    column: usize,
}

/// Scan a string literal, decoding escapes and splitting out interpolations.
fn lex_string(
    chars: &[char],
    pos: usize,
    line: usize,
    column: usize,
) -> Result<LexedString, CompileError> {
    let mut end = pos + 1;
    let current_line = line;
    let mut current_column = column + 1;
    let mut parts: Vec<RawStrPart> = Vec::new();
    let mut text = String::new();
    loop {
        let character = match chars.get(end) {
            Some(character) => *character,
            None => {
                return Err(CompileError {
                    message: "unterminated string".to_string(),
                    line,
                    column,
                });
            }
        };
        match character {
            '"' => {
                if !text.is_empty() {
                    parts.push(RawStrPart::Text(text));
                }
                return Ok(LexedString {
                    parts,
                    end: end + 1,
                    line: current_line,
                    column: current_column + 1,
                });
            }
            '\n' => {
                return Err(CompileError {
                    message: "unterminated string (a string may not contain a raw newline)"
                        .to_string(),
                    line: current_line,
                    column: current_column,
                });
            }
            '\\' => {
                end += 1;
                current_column += 1;
                let escape = match chars.get(end) {
                    Some(escape) => *escape,
                    None => {
                        return Err(CompileError {
                            message: "unterminated string".to_string(),
                            line,
                            column,
                        });
                    }
                };
                if escape == '(' {
                    if !text.is_empty() {
                        parts.push(RawStrPart::Text(std::mem::take(&mut text)));
                    }
                    let start_line = current_line;
                    end += 1;
                    current_column += 1;
                    let start = end;
                    let mut nesting = 1usize;
                    while nesting > 0 {
                        match chars.get(end) {
                            None => {
                                return Err(CompileError {
                                    message: "unterminated string interpolation".to_string(),
                                    line: current_line,
                                    column: current_column,
                                });
                            }
                            Some('(') => {
                                nesting += 1;
                            }
                            Some(')') => {
                                nesting -= 1;
                            }
                            Some('"') => {
                                end += 1;
                                current_column += 1;
                                loop {
                                    match chars.get(end) {
                                        None => {
                                            return Err(CompileError {
                                                message: "unterminated string interpolation"
                                                    .to_string(),
                                                line: current_line,
                                                column: current_column,
                                            });
                                        }
                                        Some('"') => {
                                            break;
                                        }
                                        Some('\\') => {
                                            end += 1;
                                            current_column += 1;
                                        }
                                        Some(_) => {}
                                    }
                                    end += 1;
                                    current_column += 1;
                                }
                            }
                            Some(_) => {}
                        }
                        end += 1;
                        current_column += 1;
                    }
                    let source: String = chars[start..end - 1].iter().collect();
                    parts.push(RawStrPart::Interp(source, start_line));
                } else {
                    end += 1;
                    current_column += 1;
                    match escape {
                        '"' => text.push('"'),
                        '\\' => text.push('\\'),
                        '/' => text.push('/'),
                        'b' => text.push('\u{8}'),
                        'f' => text.push('\u{c}'),
                        'n' => text.push('\n'),
                        'r' => text.push('\r'),
                        't' => text.push('\t'),
                        'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let digit = match chars.get(end) {
                                    Some(digit) if digit.is_ascii_hexdigit() => *digit,
                                    _ => {
                                        return Err(CompileError {
                                            message: "invalid unicode escape".to_string(),
                                            line: current_line,
                                            column: current_column,
                                        });
                                    }
                                };
                                code = code * 16 + digit.to_digit(16).unwrap_or(0);
                                end += 1;
                                current_column += 1;
                            }
                            if (0xd800..0xe000).contains(&code) {
                                return Err(CompileError {
                                    message: "surrogate escapes are not supported".to_string(),
                                    line: current_line,
                                    column: current_column,
                                });
                            }
                            text.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => {
                            return Err(CompileError {
                                message: format!("invalid escape sequence `\\{other}`"),
                                line: current_line,
                                column: current_column,
                            });
                        }
                    }
                }
            }
            other => {
                text.push(other);
                end += 1;
                current_column += 1;
            }
        }
    }
}

/// Require a string literal with no interpolation, as keys need.
fn literal_key(parts: Vec<RawStrPart>, line: usize, column: usize) -> Result<String, CompileError> {
    let mut text = String::new();
    for part in parts {
        match part {
            RawStrPart::Text(literal) => text.push_str(&literal),
            RawStrPart::Interp(_, _) => {
                return Err(CompileError {
                    message: "string interpolation is not supported in a key; use `(expr)`"
                        .to_string(),
                    line,
                    column,
                });
            }
        }
    }
    Ok(text)
}

/// A compiled segment of a string literal.
#[derive(Debug, Clone)]
enum SPart {
    /// Literal text.
    Text(String),
    /// A nested filter, rendered with `tostring`.
    Interp(Box<Expr>),
}

/// One step of an object-construction key: either a literal name or an
/// expression to evaluate.
#[derive(Debug, Clone)]
enum KeyStep {
    /// A literal key name.
    Name(String),
    /// A computed key, from `{(expr): value}`.
    Index(Box<Expr>),
}

/// A parsed filter expression.
#[derive(Debug, Clone)]
enum Expr {
    /// `.`
    Identity,
    /// `… | …`
    Pipe(Box<Expr>, Box<Expr>),
    /// `… , …`
    Comma(Box<Expr>, Box<Expr>),
    /// `… as $name | body`
    Bind {
        /// The expression whose outputs are bound.
        source: Box<Expr>,
        /// The variable name, without the `$`.
        name: String,
        /// The body, evaluated once per bound value.
        body: Box<Expr>,
    },
    /// `reduce source as $name (init; update)`
    Reduce {
        /// The expression whose outputs are iterated.
        source: Box<Expr>,
        /// The accumulator variable, without the `$`.
        name: String,
        /// The starting accumulator.
        init: Box<Expr>,
        /// The update expression.
        update: Box<Expr>,
    },
    /// `foreach source as $name (init; update; extract)`
    Foreach {
        /// The expression whose outputs are iterated.
        source: Box<Expr>,
        /// The state variable, without the `$`.
        name: String,
        /// The starting state.
        init: Box<Expr>,
        /// The update expression.
        update: Box<Expr>,
        /// The expression emitted after each update.
        extract: Box<Expr>,
    },
    /// `if cond then yes else no end`
    If {
        /// The condition.
        cond: Box<Expr>,
        /// Run when the condition is truthy.
        yes: Box<Expr>,
        /// Run otherwise; `None` means identity.
        no: Option<Box<Expr>>,
    },
    /// `try body catch handler`
    Try {
        /// The expression to evaluate.
        body: Box<Expr>,
        /// Run when `body` fails; `None` means "yield nothing".
        handler: Option<Box<Expr>>,
    },
    /// `body?`, which is `try body`.
    Optional(Box<Expr>),
    /// `… // …`
    Alt(Box<Expr>, Box<Expr>),
    /// `… and …`
    And(Box<Expr>, Box<Expr>),
    /// `… or …`
    Or(Box<Expr>, Box<Expr>),
    /// A binary arithmetic or comparison operator.
    Binary {
        /// The operator.
        op: BinOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// Unary minus.
    Neg(Box<Expr>),
    /// A literal value.
    Literal(Json),
    /// A string literal with zero or more interpolations.
    Str(Vec<SPart>),
    /// `$name`
    Var(String),
    /// A base expression followed by a path, for example `.a.b[0]`.
    Access {
        /// The base expression the path is applied to.
        base: Box<Expr>,
        /// The steps, applied left to right.
        steps: Vec<Step>,
    },
    /// `.[]` with no base expression.
    Iterate,
    /// `f(args)`, or a bare constant such as `length`.
    Call {
        /// The function name.
        name: String,
        /// The argument expressions.
        args: Vec<Expr>,
    },
    /// `[ … ]`, collecting the stream into one array.
    Array(Box<Expr>),
    /// `{ … }`
    Object(Vec<(Vec<KeyStep>, Expr)>),
}

/// One binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// One access step in a path expression.
#[derive(Debug, Clone)]
struct Step {
    /// What to do with the base value.
    kind: StepKind,
    /// `?` after the step makes type mismatches yield nothing instead of
    /// failing.
    optional: bool,
    /// 1-based line of the token that introduced the step.
    line: usize,
    /// 1-based column of that token.
    column: usize,
}

/// The kinds of access step.
#[derive(Debug, Clone)]
enum StepKind {
    /// `.foo` or `.["foo"]`
    Field(String),
    /// `.[0]`; negative values index from the end.
    Index(f64),
    /// `.[expr]`, where the key is computed.
    IndexValue(Box<Expr>),
    /// `.[]`
    Iterate,
    /// `.[start:end]`
    Slice {
        /// Inclusive lower bound; `None` means the start of the value.
        start: Option<Box<Expr>>,
        /// Exclusive upper bound; `None` means the end of the value.
        end: Option<Box<Expr>>,
    },
}

/// Names that parse as function calls but are not implemented, with the reason.
const UNSUPPORTED_FUNCTIONS: &[(&str, &str)] = &[
    ("env", "the environment is not available in this runtime"),
    ("ENV", "the environment is not available in this runtime"),
    ("$ENV", "the environment is not available in this runtime"),
    ("input", "there is no additional input"),
    ("inputs", "there is no additional input"),
    ("debug", "debugging output is not supported"),
    ("walk", "`walk` is not supported"),
    ("path", "`path` is not supported"),
    ("recurse", "`recurse` is not supported"),
    ("ascii", "`ascii` is not supported; use `explode`"),
    ("input_filename", "there are no input filenames to report"),
    ("halt", "`halt` is not supported"),
    ("halt_error", "`halt_error` is not supported"),
    ("builtins", "`builtins` is not supported"),
    ("now", "the runtime is deterministic and has no clock"),
    ("gmtime", "dates are not supported"),
    ("localtime", "dates are not supported"),
    ("strftime", "dates are not supported"),
    ("mktime", "dates are not supported"),
    ("todate", "dates are not supported"),
    ("fromdate", "dates are not supported"),
    ("getpath_at", "`getpath_at` is not supported"),
    ("limit_by", "`limit_by` is not supported"),
];

/// Names this module implements, used to reject unknown functions at compile
/// time rather than at run time.
const BUILTIN_NAMES: &[&str] = &[
    "range",
    "length",
    "utf8bytelength",
    "keys",
    "keys_unsorted",
    "values",
    "has",
    "in",
    "type",
    "tostring",
    "tonumber",
    "tojson",
    "fromjson",
    "ascii_downcase",
    "ascii_upcase",
    "explode",
    "implode",
    "add",
    "any",
    "all",
    "min",
    "max",
    "min_by",
    "max_by",
    "floor",
    "ceil",
    "round",
    "sqrt",
    "fabs",
    "sort",
    "sort_by",
    "group_by",
    "unique",
    "unique_by",
    "reverse",
    "first",
    "last",
    "nth",
    "limit",
    "select",
    "map",
    "map_values",
    "paths",
    "leaf_paths",
    "del",
    "getpath",
    "setpath",
    "to_entries",
    "from_entries",
    "with_entries",
    "join",
    "split",
    "splits",
    "ltrimstr",
    "rtrimstr",
    "startswith",
    "endswith",
    "test",
    "match",
    "capture",
    "sub",
    "gsub",
    "empty",
    "error",
    "not",
];

/// Recursive-descent parser for the filter language.
struct FilterParser {
    tokens: Vec<Token>,
    pos: usize,
    /// Current recursion depth.
    depth: usize,
    /// When set, `as` bindings are not consumed; used while parsing the source
    /// of `reduce` and `foreach`.
    bind_locked: bool,
}

impl FilterParser {
    /// Parse a whole filter program.
    fn parse(source: &str) -> Result<Expr, CompileError> {
        let tokens = lex(source)?;
        if tokens.len() > MAX_FILTER_TOKENS {
            let offending = &tokens[MAX_FILTER_TOKENS];
            return Err(CompileError {
                message: format!("filter exceeds the maximum of {MAX_FILTER_TOKENS} tokens"),
                line: offending.line,
                column: offending.column,
            });
        }
        let mut parser = Self {
            tokens,
            pos: 0,
            depth: 0,
            bind_locked: false,
        };
        let expr = parser.parse_pipe(true)?;
        parser.expect_eof()?;
        Ok(expr)
    }

    /// Consume one level of nesting budget.
    fn enter(&mut self, line: usize, column: usize) -> Result<(), CompileError> {
        if self.depth >= MAX_FILTER_NESTING {
            return Err(CompileError {
                message: format!("filter exceeds maximum nesting depth ({MAX_FILTER_NESTING})"),
                line,
                column,
            });
        }
        self.depth += 1;
        Ok(())
    }

    /// The token under the cursor.
    fn peek(&self) -> &Tok {
        self.tokens
            .get(self.pos)
            .map(|token| &token.tok)
            .unwrap_or(&Tok::Eof)
    }

    /// The token `offset` positions ahead.
    fn peek_at(&self, offset: usize) -> &Tok {
        self.tokens
            .get(self.pos + offset)
            .map(|token| &token.tok)
            .unwrap_or(&Tok::Eof)
    }

    /// The position of the token under the cursor.
    fn position(&self) -> (usize, usize) {
        self.tokens
            .get(self.pos)
            .map(|token| (token.line, token.column))
            .unwrap_or((0, 0))
    }

    /// Consume the token under the cursor.
    fn advance(&mut self) -> Tok {
        let tok = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    /// Build an error at the cursor.
    fn error(&self, message: String) -> CompileError {
        let (line, column) = self.position();
        CompileError {
            message,
            line,
            column,
        }
    }

    /// Consume the expected token or fail.
    fn expect(&mut self, expected: &Tok, context: &str) -> Result<(), CompileError> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(self.error(format!(
                "expected {} {context}, found {}",
                expected.describe(),
                self.peek().describe()
            )))
        }
    }

    /// Fail unless the whole filter has been consumed.
    fn expect_eof(&mut self) -> Result<(), CompileError> {
        match self.peek() {
            Tok::Eof => Ok(()),
            other => Err(self.error(format!(
                "unexpected {} after the end of the filter",
                other.describe()
            ))),
        }
    }

    /// Parse `… , …` (comma binds looser than `|`), guarding the recursion
    /// depth.
    fn parse_pipe(&mut self, allow_comma: bool) -> Result<Expr, CompileError> {
        let (line, column) = self.position();
        self.enter(line, column)?;
        let result = self.parse_pipe_inner(allow_comma);
        self.depth -= 1;
        result
    }

    /// Parse `… , …` (comma binds looser than `|`).
    fn parse_pipe_inner(&mut self, allow_comma: bool) -> Result<Expr, CompileError> {
        let mut expr = self.parse_alt()?;
        loop {
            match self.peek() {
                Tok::Pipe => {
                    self.advance();
                    let right = self.parse_alt()?;
                    expr = Expr::Pipe(Box::new(expr), Box::new(right));
                }
                Tok::Comma if allow_comma => {
                    self.advance();
                    let right = self.parse_alt()?;
                    expr = Expr::Comma(Box::new(expr), Box::new(right));
                }
                _ => return Ok(expr),
            }
        }
    }

    /// Parse `//`.
    fn parse_alt(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_bind()?;
        while matches!(self.peek(), Tok::Alt) {
            self.advance();
            let right = self.parse_bind()?;
            expr = Expr::Alt(Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    /// Parse `EXPR as $x | BODY`.
    fn parse_bind(&mut self) -> Result<Expr, CompileError> {
        let source = self.parse_or()?;
        if self.bind_locked || !matches!(self.peek(), Tok::As) {
            return Ok(source);
        }
        let (line, column) = self.position();
        self.advance();
        let name = match self.advance() {
            Tok::Var(name) => name,
            other => {
                return Err(self.error(format!(
                    "expected a variable after `as`, found {}",
                    other.describe()
                )));
            }
        };
        match self.peek() {
            Tok::Pipe => {
                self.advance();
            }
            _ => {
                return Err(CompileError {
                    message: "expected `|` after an `as` binding".to_string(),
                    line,
                    column,
                });
            }
        }
        let body = self.parse_pipe(true)?;
        Ok(Expr::Bind {
            source: Box::new(source),
            name,
            body: Box::new(body),
        })
    }

    /// Parse `or`.
    fn parse_or(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_and()?;
        while matches!(self.peek(), Tok::Or) {
            self.advance();
            let right = self.parse_and()?;
            expr = Expr::Or(Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    /// Parse `and`.
    fn parse_and(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_compare()?;
        while matches!(self.peek(), Tok::And) {
            self.advance();
            let right = self.parse_compare()?;
            expr = Expr::And(Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    /// Parse the six comparison operators (non-associative).
    fn parse_compare(&mut self) -> Result<Expr, CompileError> {
        let left = self.parse_additive()?;
        let op = match self.peek() {
            Tok::Eq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_additive()?;
        if matches!(
            self.peek(),
            Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
        ) {
            return Err(
                self.error("comparison operators are non-associative; add parentheses".to_string())
            );
        }
        Ok(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    /// Parse `+` and `-`.
    fn parse_additive(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Add => BinOp::Add,
                Tok::Sub => BinOp::Sub,
                _ => return Ok(expr),
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            expr = Expr::Binary {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
    }

    /// Parse `*`, `/` and `%`.
    fn parse_multiplicative(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Mul => BinOp::Mul,
                Tok::Div => BinOp::Div,
                Tok::Rem => BinOp::Rem,
                _ => return Ok(expr),
            };
            self.advance();
            let right = self.parse_unary()?;
            expr = Expr::Binary {
                op,
                left: Box::new(expr),
                right: Box::new(right),
            };
        }
    }

    /// Parse a prefix `-` and any trailing `?` operators.
    fn parse_unary(&mut self) -> Result<Expr, CompileError> {
        if matches!(self.peek(), Tok::Sub) {
            self.advance();
            if matches!(self.peek(), Tok::Sub) {
                return Err(self
                    .error("unsupported operator `--`; write parentheses explicitly".to_string()));
            }
            let inner = self.parse_unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        let mut expr = self.parse_postfix()?;
        loop {
            match self.peek() {
                Tok::Question => {
                    self.advance();
                    expr = Expr::Optional(Box::new(expr));
                }
                Tok::Recurse => {
                    return Err(self.error(
                        "recursive descent `..` is not supported; use `paths`".to_string(),
                    ));
                }
                _ => return Ok(expr),
            }
        }
    }

    /// Parse a primary expression with its access path.
    fn parse_postfix(&mut self) -> Result<Expr, CompileError> {
        let (line, column) = self.position();
        match self.peek().clone() {
            Tok::Recurse => Err(CompileError {
                message: "recursive descent `..` is not supported; use `paths`".to_string(),
                line,
                column,
            }),
            Tok::Dot => {
                self.advance();
                let mut steps = Vec::new();
                self.parse_path(&mut steps, false)?;
                Ok(self.apply_steps(Expr::Identity, steps))
            }
            Tok::Not => {
                self.advance();
                Ok(Expr::Call {
                    name: "not".to_string(),
                    args: Vec::new(),
                })
            }
            Tok::Num(value) => {
                self.advance();
                let mut steps = Vec::new();
                self.parse_path(&mut steps, true)?;
                Ok(self.apply_steps(Expr::Literal(Json::Number(value)), steps))
            }
            // In expression position `[…]` always builds an array: `[0]` is
            // the one-element array, only `.[0]` indexes. A `[` after a term is
            // handled as a step by `parse_path`.
            Tok::LBracket => {
                let base = self.parse_array_constructor()?;
                self.parse_trailing_steps(base)
            }
            Tok::LParen => {
                self.advance();
                let inner = self.parse_pipe(true)?;
                self.expect(&Tok::RParen, "to close a group")?;
                let mut steps = Vec::new();
                self.parse_path(&mut steps, true)?;
                Ok(self.apply_steps(inner, steps))
            }
            Tok::LBrace => {
                let base = self.parse_object()?;
                self.parse_trailing_steps(base)
            }
            Tok::Str(parts) => {
                self.advance();
                let literal = self.compile_string(parts, line, column)?;
                let mut steps = Vec::new();
                self.parse_path(&mut steps, true)?;
                Ok(self.apply_steps(literal, steps))
            }
            Tok::Var(name) => {
                self.advance();
                let mut steps = Vec::new();
                self.parse_path(&mut steps, true)?;
                Ok(self.apply_steps(Expr::Var(name), steps))
            }
            Tok::If => {
                let base = self.parse_if()?;
                self.parse_trailing_steps(base)
            }
            Tok::Try => {
                self.advance();
                let body = self.parse_postfix()?;
                let handler = if matches!(self.peek(), Tok::Catch) {
                    self.advance();
                    Some(Box::new(self.parse_postfix()?))
                } else {
                    None
                };
                let base = Expr::Try {
                    body: Box::new(body),
                    handler,
                };
                self.parse_trailing_steps(base)
            }
            Tok::Reduce => {
                let base = self.parse_reduce()?;
                self.parse_trailing_steps(base)
            }
            Tok::Foreach => {
                let base = self.parse_foreach()?;
                self.parse_trailing_steps(base)
            }
            Tok::Ident(name) => {
                self.advance();
                match name.as_str() {
                    "true" => return Ok(Expr::Literal(Json::Bool(true))),
                    "false" => return Ok(Expr::Literal(Json::Bool(false))),
                    "null" => return Ok(Expr::Literal(Json::Null)),
                    _ => {}
                }
                if let Some((_, reason)) = UNSUPPORTED_FUNCTIONS
                    .iter()
                    .find(|(unsupported, _)| *unsupported == name)
                {
                    return Err(CompileError {
                        message: format!("unsupported function `{name}`: {reason}"),
                        line,
                        column,
                    });
                }
                if !BUILTIN_NAMES.contains(&name.as_str()) {
                    return Err(CompileError {
                        message: format!("unsupported function `{name}`"),
                        line,
                        column,
                    });
                }
                let args = self.parse_arguments(&name)?;
                let mut steps = Vec::new();
                self.parse_path(&mut steps, true)?;
                Ok(self.apply_steps(Expr::Call { name, args }, steps))
            }
            other => Err(CompileError {
                message: format!("unexpected {} in a filter", other.describe()),
                line,
                column,
            }),
        }
    }

    /// Compile a string literal with interpolations into an expression.
    fn compile_string(
        &self,
        parts: Vec<RawStrPart>,
        line: usize,
        column: usize,
    ) -> Result<Expr, CompileError> {
        let mut compiled = Vec::new();
        for part in parts {
            match part {
                RawStrPart::Text(text) => compiled.push(SPart::Text(text)),
                RawStrPart::Interp(source, at) => {
                    let inner = FilterParser::parse(&source).map_err(|mut error| {
                        error.message = format!("in a string interpolation: {}", error.message);
                        error
                    })?;
                    let _ = at;
                    compiled.push(SPart::Interp(Box::new(inner)));
                }
            }
        }
        match compiled.len() {
            0 => Ok(Expr::Literal(Json::String(String::new()))),
            1 => match compiled.into_iter().next() {
                Some(SPart::Text(text)) => Ok(Expr::Literal(Json::String(text))),
                Some(part) => Ok(Expr::Str(vec![part])),
                None => Ok(Expr::Literal(Json::String(String::new()))),
            },
            _ => Ok(Expr::Str(compiled)),
        }
        .map_err(|error: CompileError| CompileError {
            line,
            column,
            ..error
        })
    }

    /// Parse the argument list of a function call, with `(` under the cursor.
    fn parse_arguments(&mut self, name: &str) -> Result<Vec<Expr>, CompileError> {
        if !matches!(self.peek(), Tok::LParen) {
            return Ok(Vec::new());
        }
        self.advance();
        let mut args = Vec::new();
        if matches!(self.peek(), Tok::RParen) {
            self.advance();
            return Ok(args);
        }
        loop {
            // Arguments are separated by `;`, so a comma inside one argument
            // is the comma operator: `del(.a, .b)`, `map(.a, .b)`.
            args.push(self.parse_pipe(true)?);
            match self.peek() {
                Tok::Semi => {
                    self.advance();
                }
                Tok::RParen => {
                    self.advance();
                    break;
                }
                other => {
                    return Err(self.error(format!(
                        "expected `;` or `)` in the arguments of `{name}`, found {}",
                        other.describe()
                    )));
                }
            }
        }
        Ok(args)
    }

    /// Attach a parsed path to a base expression.
    fn apply_steps(&self, base: Expr, steps: Vec<Step>) -> Expr {
        if steps.is_empty() {
            return base;
        }
        if matches!(base, Expr::Identity)
            && steps.len() == 1
            && matches!(steps[0].kind, StepKind::Iterate)
            && !steps[0].optional
        {
            return Expr::Iterate;
        }
        Expr::Access {
            base: Box::new(base),
            steps,
        }
    }

    /// Continue a finished term with the access steps that may follow it.
    ///
    /// Array and object constructors end in a bracket, so `[1,2][0]` and
    /// `{a:1}.a` are a term followed by steps rather than a bare postfix.
    fn parse_trailing_steps(&mut self, base: Expr) -> Result<Expr, CompileError> {
        let mut steps = Vec::new();
        self.parse_path(&mut steps, true)?;
        Ok(self.apply_steps(base, steps))
    }

    /// Parse the `.foo`, `[0]` and `[]` steps that may follow a term.
    fn parse_path(&mut self, steps: &mut Vec<Step>, inside_term: bool) -> Result<(), CompileError> {
        // A leading `.` is followed directly by the first step (`.foo`, `."k"`,
        // `.[0]`); only later steps repeat the `.`.
        // After a term (`1`, `"a"`, `$x`, `)`) the next token cannot be a bare
        // field name, so only a leading `.` consumes one: `1-1` is subtraction
        // and not `1` indexed by `-1`. `Tok::Eof` never matches an arm below.
        let leading = if inside_term {
            Tok::Eof
        } else {
            self.peek().clone()
        };
        match leading {
            Tok::Ident(name) => {
                let (line, column) = self.position();
                self.advance();
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Field(name),
                    optional,
                    line,
                    column,
                });
            }
            Tok::Str(parts) => {
                let (line, column) = self.position();
                self.advance();
                let name = literal_key(parts, line, column)?;
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Field(name),
                    optional,
                    line,
                    column,
                });
            }
            Tok::Num(value) => {
                // `.0` is accepted as `.[0]`.
                let (line, column) = self.position();
                self.advance();
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Index(value),
                    optional,
                    line,
                    column,
                });
            }
            _ => {}
        }
        loop {
            match self.peek() {
                Tok::Dot => {
                    let (line, column) = self.position();
                    let next = self.peek_at(1).clone();
                    match next {
                        Tok::Ident(name) => {
                            self.advance();
                            self.advance();
                            let optional = self.take_question();
                            steps.push(Step {
                                kind: StepKind::Field(name),
                                optional,
                                line,
                                column,
                            });
                        }
                        Tok::Str(parts) => {
                            self.advance();
                            self.advance();
                            let name = literal_key(parts, line, column)?;
                            let optional = self.take_question();
                            steps.push(Step {
                                kind: StepKind::Field(name),
                                optional,
                                line,
                                column,
                            });
                        }
                        Tok::LBracket => {
                            self.advance();
                            self.parse_bracket_step(steps)?;
                        }
                        Tok::Num(value) => {
                            // `.0` is accepted as `. [0]`.
                            self.advance();
                            self.advance();
                            let optional = self.take_question();
                            steps.push(Step {
                                kind: StepKind::Index(value),
                                optional,
                                line,
                                column,
                            });
                        }
                        _ => {
                            if inside_term {
                                return Err(CompileError {
                                    message: "expected a field name after `.`".to_string(),
                                    line,
                                    column,
                                });
                            }
                            return Ok(());
                        }
                    }
                }
                // A `[…]` step always continues a path: `.tags[0]`, `.[0]`.
                Tok::LBracket => self.parse_bracket_step(steps)?,
                _ => return Ok(()),
            }
        }
    }

    /// Parse `[ filter ]` as array construction.
    fn parse_array_constructor(&mut self) -> Result<Expr, CompileError> {
        self.expect(&Tok::LBracket, "to open an array")?;
        if matches!(self.peek(), Tok::RBracket) {
            // `[]` is the empty array; `.[]` is the iteration.
            self.advance();
            return Ok(Expr::Array(Box::new(Expr::Call {
                name: String::from("empty"),
                args: Vec::new(),
            })));
        }
        let inner = self.parse_pipe(true)?;
        self.expect(&Tok::RBracket, "to close an array")?;
        Ok(Expr::Array(Box::new(inner)))
    }

    /// Parse one `[…]` step, with the `[` under the cursor.
    fn parse_bracket_step(&mut self, steps: &mut Vec<Step>) -> Result<(), CompileError> {
        let (line, column) = self.position();
        self.expect(&Tok::LBracket, "to open an index")?;
        match self.peek().clone() {
            Tok::RBracket => {
                self.advance();
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Iterate,
                    optional,
                    line,
                    column,
                });
            }
            Tok::Str(parts) => {
                self.advance();
                self.expect(&Tok::RBracket, "to close an index")?;
                let name = literal_key(parts, line, column)?;
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Field(name),
                    optional,
                    line,
                    column,
                });
            }
            Tok::Num(value) => {
                self.advance();
                if matches!(self.peek(), Tok::Colon) {
                    self.advance();
                    let end = self.parse_slice_bound()?;
                    self.expect(&Tok::RBracket, "to close a slice")?;
                    let optional = self.take_question();
                    steps.push(Step {
                        kind: StepKind::Slice {
                            start: Some(Box::new(Expr::Literal(Json::Number(value)))),
                            end,
                        },
                        optional,
                        line,
                        column,
                    });
                } else {
                    self.expect(&Tok::RBracket, "to close an index")?;
                    let optional = self.take_question();
                    steps.push(Step {
                        kind: StepKind::Index(value),
                        optional,
                        line,
                        column,
                    });
                }
            }
            Tok::Sub => {
                self.advance();
                let value = match self.advance() {
                    Tok::Num(value) => value,
                    other => {
                        return Err(self.error(format!(
                            "expected a number after `-` in an index, found {}",
                            other.describe()
                        )));
                    }
                };
                if matches!(self.peek(), Tok::Colon) {
                    self.advance();
                    let end = self.parse_slice_bound()?;
                    self.expect(&Tok::RBracket, "to close a slice")?;
                    let optional = self.take_question();
                    steps.push(Step {
                        kind: StepKind::Slice {
                            start: Some(Box::new(Expr::Literal(Json::Number(-value)))),
                            end,
                        },
                        optional,
                        line,
                        column,
                    });
                } else {
                    self.expect(&Tok::RBracket, "to close an index")?;
                    let optional = self.take_question();
                    steps.push(Step {
                        kind: StepKind::Index(-value),
                        optional,
                        line,
                        column,
                    });
                }
            }
            Tok::Colon => {
                self.advance();
                let end = self.parse_slice_bound()?;
                self.expect(&Tok::RBracket, "to close a slice")?;
                let optional = self.take_question();
                steps.push(Step {
                    kind: StepKind::Slice { start: None, end },
                    optional,
                    line,
                    column,
                });
            }
            _ => {
                let start = self.parse_pipe(false)?;
                match self.peek() {
                    Tok::Colon => {
                        self.advance();
                        let end = self.parse_slice_bound()?;
                        self.expect(&Tok::RBracket, "to close a slice")?;
                        let optional = self.take_question();
                        steps.push(Step {
                            kind: StepKind::Slice {
                                start: Some(Box::new(start)),
                                end,
                            },
                            optional,
                            line,
                            column,
                        });
                    }
                    Tok::RBracket => {
                        self.advance();
                        let optional = self.take_question();
                        steps.push(Step {
                            kind: StepKind::IndexValue(Box::new(start)),
                            optional,
                            line,
                            column,
                        });
                    }
                    other => {
                        return Err(self.error(format!(
                            "expected `]` or `:` in an index, found {}",
                            other.describe()
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Parse the optional upper bound of a slice.
    fn parse_slice_bound(&mut self) -> Result<Option<Box<Expr>>, CompileError> {
        if matches!(self.peek(), Tok::RBracket) {
            return Ok(None);
        }
        let expr = self.parse_pipe(false)?;
        Ok(Some(Box::new(expr)))
    }

    /// Parse an expression with `as` bindings disabled, as the source of a
    /// `reduce` or `foreach` needs.
    fn with_bind_locked(&mut self) -> Result<Expr, CompileError> {
        let saved = self.bind_locked;
        self.bind_locked = true;
        let result = self.parse_pipe(false);
        self.bind_locked = saved;
        result
    }

    /// Consume a `?` when one follows the current position.
    fn take_question(&mut self) -> bool {
        if matches!(self.peek(), Tok::Question) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Parse `if C then A elif D then B else E end`.
    fn parse_if(&mut self) -> Result<Expr, CompileError> {
        let (line, column) = self.position();
        self.expect(&Tok::If, "to start a conditional")?;
        let cond = self.parse_pipe(true)?;
        self.expect(&Tok::Then, "after an `if` condition")?;
        let yes = self.parse_pipe(true)?;
        let no = if matches!(self.peek(), Tok::Elif) {
            Some(Box::new(self.parse_if()?))
        } else if matches!(self.peek(), Tok::Else) {
            self.advance();
            Some(Box::new(self.parse_pipe(true)?))
        } else if matches!(self.peek(), Tok::End) {
            None
        } else {
            return Err(CompileError {
                message: format!(
                    "expected `elif`, `else` or `end` in a conditional, found {}",
                    self.peek().describe()
                ),
                line,
                column,
            });
        };
        self.expect(&Tok::End, "to close a conditional")?;
        Ok(Expr::If {
            cond: Box::new(cond),
            yes: Box::new(yes),
            no,
        })
    }

    /// Parse `reduce EXP as $x (INIT; UPDATE)`.
    fn parse_reduce(&mut self) -> Result<Expr, CompileError> {
        self.expect(&Tok::Reduce, "to start a reduction")?;
        let source = self.with_bind_locked()?;
        self.expect(&Tok::As, "after the `reduce` source")?;
        let name = match self.advance() {
            Tok::Var(name) => name,
            other => {
                return Err(self.error(format!(
                    "expected a variable after `reduce … as`, found {}",
                    other.describe()
                )));
            }
        };
        self.expect(&Tok::LParen, "to open the `reduce` state")?;
        let init = self.parse_pipe(false)?;
        self.expect(&Tok::Semi, "between the `reduce` state and its update")?;
        let update = self.parse_pipe(false)?;
        self.expect(&Tok::RParen, "to close a `reduce`")?;
        Ok(Expr::Reduce {
            source: Box::new(source),
            name,
            init: Box::new(init),
            update: Box::new(update),
        })
    }

    /// Parse `foreach EXP as $x (INIT; UPDATE)` or `(INIT; UPDATE; EXTRACT)`.
    fn parse_foreach(&mut self) -> Result<Expr, CompileError> {
        self.expect(&Tok::Foreach, "to start a `foreach`")?;
        let source = self.with_bind_locked()?;
        self.expect(&Tok::As, "after the `foreach` source")?;
        let name = match self.advance() {
            Tok::Var(name) => name,
            other => {
                return Err(self.error(format!(
                    "expected a variable after `foreach … as`, found {}",
                    other.describe()
                )));
            }
        };
        self.expect(&Tok::LParen, "to open the `foreach` state")?;
        let init = self.parse_pipe(false)?;
        self.expect(&Tok::Semi, "between the `foreach` state and its update")?;
        let update = self.parse_pipe(false)?;
        let extract = if matches!(self.peek(), Tok::Semi) {
            self.advance();
            self.parse_pipe(false)?
        } else {
            // jq's two-argument `foreach` emits the updated state itself.
            Expr::Identity
        };
        self.expect(&Tok::RParen, "to close a `foreach`")?;
        Ok(Expr::Foreach {
            source: Box::new(source),
            name,
            init: Box::new(init),
            update: Box::new(update),
            extract: Box::new(extract),
        })
    }

    /// Parse `{a: f, "b": g, $v, (expr): x}`.
    fn parse_object(&mut self) -> Result<Expr, CompileError> {
        self.expect(&Tok::LBrace, "to start an object")?;
        let mut entries = Vec::new();
        if matches!(self.peek(), Tok::RBrace) {
            self.advance();
            return Ok(Expr::Object(entries));
        }
        loop {
            let (line, column) = self.position();
            let keys = match self.peek().clone() {
                Tok::Ident(name) => {
                    self.advance();
                    vec![KeyStep::Name(name)]
                }
                Tok::Str(parts) => {
                    self.advance();
                    vec![KeyStep::Name(literal_key(parts, line, column)?)]
                }
                Tok::Num(value) => {
                    self.advance();
                    vec![KeyStep::Name(number_text(value))]
                }
                Tok::Var(name) => {
                    self.advance();
                    if matches!(self.peek(), Tok::Colon) {
                        vec![KeyStep::Index(Box::new(Expr::Var(name)))]
                    } else {
                        // `{$v}` is `{"v": $v}`.
                        entries.push((vec![KeyStep::Name(name.clone())], Expr::Var(name)));
                        if self.object_separator()? {
                            return Ok(Expr::Object(entries));
                        }
                        continue;
                    }
                }
                Tok::LParen => {
                    self.advance();
                    let key = self.parse_pipe(true)?;
                    self.expect(&Tok::RParen, "to close an object key")?;
                    vec![KeyStep::Index(Box::new(key))]
                }
                other => {
                    return Err(self.error(format!(
                        "expected an object key, found {}",
                        other.describe()
                    )));
                }
            };
            if !matches!(self.peek(), Tok::Colon) {
                // Shorthand `{a}` means `{a: .a}`.
                if keys.len() == 1
                    && let KeyStep::Name(name) = &keys[0]
                {
                    entries.push((
                        keys.clone(),
                        Expr::Access {
                            base: Box::new(Expr::Identity),
                            steps: vec![Step {
                                kind: StepKind::Field(name.clone()),
                                optional: false,
                                line,
                                column,
                            }],
                        },
                    ));
                    if self.object_separator()? {
                        return Ok(Expr::Object(entries));
                    }
                    continue;
                }
                return Err(self.error("expected `:` in an object entry".to_string()));
            }
            self.advance();
            let value = self.parse_object_value()?;
            entries.push((keys, value));
            if self.object_separator()? {
                return Ok(Expr::Object(entries));
            }
        }
    }

    /// Consume the separator after an object entry; true when `}` followed.
    fn object_separator(&mut self) -> Result<bool, CompileError> {
        match self.peek() {
            Tok::Comma => {
                self.advance();
                Ok(false)
            }
            Tok::RBrace => {
                self.advance();
                Ok(true)
            }
            other => Err(self.error(format!(
                "expected `,` or `}}` in an object, found {}",
                other.describe()
            ))),
        }
    }

    /// Parse an object entry value; `,` and `}` end it.
    fn parse_object_value(&mut self) -> Result<Expr, CompileError> {
        let mut expr = self.parse_alt()?;
        loop {
            match self.peek() {
                Tok::Pipe => {
                    self.advance();
                    let right = self.parse_alt()?;
                    expr = Expr::Pipe(Box::new(expr), Box::new(right));
                }
                _ => return Ok(expr),
            }
        }
    }
}

/// A runtime failure, with the position of the expression that raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EvalErr {
    /// Human-readable message; `error(msg)` passes its argument through.
    message: String,
    /// 1-based line of the raising expression.
    line: usize,
    /// 1-based column of the raising expression.
    column: usize,
    /// Fatal errors (a limit or an internal failure) are not caught by `try`,
    /// because retrying them would not terminate.
    fatal: bool,
}

impl EvalErr {
    /// A catchable runtime error.
    fn runtime(message: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            message: message.into(),
            line,
            column,
            fatal: false,
        }
    }

    /// A runtime error that `try` must not swallow.
    fn fatal(message: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            message: message.into(),
            line,
            column,
            fatal: true,
        }
    }
}

/// A sink an expression writes its values into.
type Sink<'s> = &'s mut dyn FnMut(Json) -> Result<(), EvalErr>;

/// The evaluator: one input value, a variable scope and an optional loop
/// budget.
#[derive(Clone, Copy)]
struct Evaluator<'a> {
    /// Variables in scope, innermost last.
    env: &'a [(String, Json)],
    /// Remaining iterations for the body of the enclosing loop, when inside
    /// one.
    limit: Option<usize>,
}

impl<'a> Evaluator<'a> {
    /// An evaluator with no loop budget.
    fn new(env: &'a [(String, Json)]) -> Self {
        Self { env, limit: None }
    }

    /// Look up a variable.
    fn lookup(&self, name: &str) -> Option<&Json> {
        self.env
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// Fail when the evaluator recurses too deeply.
    fn check_depth(&self, depth: usize) -> Result<(), EvalErr> {
        if depth > MAX_EVAL_DEPTH {
            return Err(EvalErr::fatal(
                format!("filter evaluation exceeds maximum depth ({MAX_EVAL_DEPTH})"),
                0,
                0,
            ));
        }
        Ok(())
    }

    /// Evaluate `expr`, writing every value into `sink` and returning either
    /// the whole stream or the single value a budgeted loop produced.
    fn eval(
        &self,
        expr: &Expr,
        input: &Json,
        depth: usize,
        sink: Sink,
    ) -> Result<Vec<Json>, EvalErr> {
        self.check_depth(depth)?;
        match expr {
            Expr::Identity => {
                sink(input.clone())?;
                Ok(vec![input.clone()])
            }
            Expr::Literal(value) => {
                sink(value.clone())?;
                Ok(vec![value.clone()])
            }
            Expr::Str(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        SPart::Text(literal) => text.push_str(literal),
                        SPart::Interp(inner) => match self.eval_single(inner, input, depth + 1)? {
                            Some(Json::String(inner_text)) => text.push_str(&inner_text),
                            Some(other) => text.push_str(&other.compact()),
                            None => {}
                        },
                    }
                }
                let value = Json::String(text);
                sink(value.clone())?;
                Ok(vec![value])
            }
            Expr::Var(name) => {
                let value = match self.lookup(name) {
                    Some(value) => value.clone(),
                    None => {
                        return Err(EvalErr::runtime(format!("${name} is not defined"), 0, 0));
                    }
                };
                sink(value.clone())?;
                Ok(vec![value])
            }
            Expr::Pipe(left, right) => {
                let produced = {
                    let mut collected = Vec::new();
                    let mut inner = |value: Json| -> Result<(), EvalErr> {
                        collected.push(value);
                        Ok(())
                    };
                    self.eval(left, input, depth + 1, &mut inner)?;
                    collected
                };
                let mut values = Vec::new();
                for value in produced {
                    values.extend(self.eval(right, &value, depth + 1, sink)?);
                }
                Ok(values)
            }
            Expr::Comma(left, right) => {
                let mut values = self.eval(left, input, depth + 1, sink)?;
                values.extend(self.eval(right, input, depth + 1, sink)?);
                Ok(values)
            }
            Expr::Bind { source, name, body } => {
                let mut scope = self.env.to_vec();
                scope.push((name.clone(), Json::Null));
                let produced = {
                    let mut collected = Vec::new();
                    let mut inner = |value: Json| -> Result<(), EvalErr> {
                        collected.push(value);
                        Ok(())
                    };
                    self.eval(source, input, depth + 1, &mut inner)?;
                    collected
                };
                let mut values = Vec::new();
                for value in produced {
                    if let Some(slot) = scope.last_mut() {
                        slot.1 = value;
                    }
                    let bound = Evaluator {
                        env: &scope,
                        limit: self.limit,
                    };
                    values.extend(bound.eval(body, input, depth + 1, sink)?);
                }
                Ok(values)
            }
            Expr::Reduce {
                source,
                name,
                init,
                update,
            } => {
                let state = match self.eval_single(init, input, depth + 1)? {
                    Some(state) => state,
                    None => return Ok(Vec::new()),
                };
                let mut state = state;
                let items = self.eval_values(source, input, depth + 1)?;
                let mut scope = self.env.to_vec();
                scope.push((name.clone(), Json::Null));
                for item in items {
                    if let Some(slot) = scope.last_mut() {
                        slot.1 = item;
                    }
                    let bound = Evaluator {
                        env: &scope,
                        limit: self.limit,
                    };
                    match bound.eval_single(update, &state, depth + 1)? {
                        Some(next) => state = next,
                        None => return Ok(Vec::new()),
                    }
                }
                sink(state.clone())?;
                Ok(vec![state])
            }
            Expr::Foreach {
                source,
                name,
                init,
                update,
                extract,
            } => {
                let mut produced = Vec::new();
                if let Some(mut state) = self.eval_single(init, input, depth + 1)? {
                    let items = self.eval_values(source, input, depth + 1)?;
                    let mut scope = self.env.to_vec();
                    scope.push((name.clone(), Json::Null));
                    for item in items {
                        if let Some(slot) = scope.last_mut() {
                            slot.1 = item;
                        }
                        let bound = Evaluator {
                            env: &scope,
                            limit: self.limit,
                        };
                        match bound.eval_single(update, &state, depth + 1)? {
                            Some(next) => state = next,
                            None => break,
                        }
                        bound.eval(extract, &state, depth + 1, sink)?;
                        produced.push(state.clone());
                    }
                }
                Ok(produced)
            }
            Expr::If { cond, yes, no } => {
                let truthy = {
                    let mut found = false;
                    let mut inner = |value: Json| -> Result<(), EvalErr> {
                        if value.is_truthy() {
                            found = true;
                        }
                        Ok(())
                    };
                    self.eval(cond, input, depth + 1, &mut inner)?;
                    found
                };
                if truthy {
                    self.eval(yes, input, depth + 1, sink)
                } else {
                    match no {
                        Some(otherwise) => self.eval(otherwise, input, depth + 1, sink),
                        None => {
                            sink(input.clone())?;
                            Ok(vec![input.clone()])
                        }
                    }
                }
            }
            Expr::Try { body, handler } => {
                let outcome = self.eval(body, input, depth + 1, sink);
                match outcome {
                    Ok(result) => Ok(result),
                    Err(error) if error.fatal => Err(error),
                    Err(error) => match handler {
                        Some(handler) => {
                            let error_value = Json::String(error.message.clone());
                            self.eval(handler, &error_value, depth + 1, sink)
                        }
                        None => Ok(Vec::new()),
                    },
                }
            }
            Expr::Optional(body) => match self.eval(body, input, depth + 1, sink) {
                Ok(result) => Ok(result),
                Err(error) if error.fatal => Err(error),
                Err(_) => Ok(Vec::new()),
            },
            Expr::Alt(left, right) => {
                let values = {
                    let mut found: Vec<Json> = Vec::new();
                    let mut inner = |value: Json| -> Result<(), EvalErr> {
                        if value.is_truthy() {
                            found.push(value);
                        }
                        Ok(())
                    };
                    self.eval(left, input, depth + 1, &mut inner)?;
                    found
                };
                if values.is_empty() {
                    self.eval(right, input, depth + 1, sink)
                } else {
                    for value in &values {
                        sink(value.clone())?;
                    }
                    Ok(values)
                }
            }
            Expr::And(left, right) => {
                let value = if self
                    .eval_single(left, input, depth + 1)?
                    .map(|value| value.is_truthy())
                    .unwrap_or(false)
                {
                    Json::Bool(
                        self.eval_single(right, input, depth + 1)?
                            .map(|value| value.is_truthy())
                            .unwrap_or(false),
                    )
                } else {
                    Json::Bool(false)
                };
                sink(value.clone())?;
                Ok(vec![value])
            }
            Expr::Or(left, right) => {
                let value = if self
                    .eval_single(left, input, depth + 1)?
                    .map(|value| value.is_truthy())
                    .unwrap_or(false)
                {
                    Json::Bool(true)
                } else {
                    Json::Bool(
                        self.eval_single(right, input, depth + 1)?
                            .map(|value| value.is_truthy())
                            .unwrap_or(false),
                    )
                };
                sink(value.clone())?;
                Ok(vec![value])
            }
            Expr::Binary { op, left, right } => {
                let left_value = self.eval_single(left, input, depth + 1)?;
                let right_values = self.eval_values(right, input, depth + 1)?;
                let mut out = Vec::new();
                if let Some(left_value) = left_value {
                    for right_value in &right_values {
                        let value = binary_op(*op, &left_value, right_value, 0, 0)?;
                        sink(value.clone())?;
                        out.push(value);
                    }
                }
                Ok(out)
            }
            Expr::Neg(inner) => {
                let mut out = Vec::new();
                if let Some(value) = self.eval_single(inner, input, depth + 1)? {
                    let negated = Json::Number(-as_number(&value, 0, 0)?);
                    sink(negated.clone())?;
                    out.push(negated);
                }
                Ok(out)
            }
            Expr::Iterate => {
                let mut out = Vec::new();
                iterate_value(input, 0, 0, &mut |value| {
                    sink(value.clone())?;
                    out.push(value);
                    Ok(())
                })?;
                Ok(out)
            }
            Expr::Access { base, steps } => {
                let mut current = self.eval_values(base, input, depth + 1)?;
                for step in steps {
                    if current.len() > MAX_VALUES {
                        return Err(EvalErr::fatal(
                            format!("too many values (limit {MAX_VALUES})"),
                            step.line,
                            step.column,
                        ));
                    }
                    let mut next = Vec::new();
                    for value in &current {
                        let mut inner = |produced: Json| -> Result<(), EvalErr> {
                            next.push(produced);
                            Ok(())
                        };
                        self.apply_step(step, value, depth + 1, &mut inner)?;
                    }
                    current = next;
                }
                for value in &current {
                    sink(value.clone())?;
                }
                Ok(current)
            }
            Expr::Call { name, args } => self.eval_call(name, args, input, depth, sink),
            Expr::Array(inner) => {
                let items = self.eval_values(inner, input, depth + 1)?;
                let value = Json::Array(items);
                check_container_depth(&value, (0, 0))?;
                sink(value.clone())?;
                Ok(vec![value])
            }
            Expr::Object(entries) => {
                let mut pairs: Vec<(String, Json)> = Vec::new();
                for (keys, value_expr) in entries {
                    let key = self.object_key(keys, input, depth + 1)?;
                    let values = self.eval_values(value_expr, input, depth + 1)?;
                    for item in values {
                        match pairs.iter_mut().find(|(existing, _)| *existing == key) {
                            Some(slot) => slot.1 = item,
                            None => pairs.push((key.clone(), item)),
                        }
                    }
                }
                let value = Json::Object(pairs);
                check_container_depth(&value, (0, 0))?;
                sink(value.clone())?;
                Ok(vec![value])
            }
        }
    }

    /// Evaluate one object-construction key.
    fn object_key(&self, keys: &[KeyStep], input: &Json, depth: usize) -> Result<String, EvalErr> {
        let mut name = String::new();
        for key in keys {
            match key {
                KeyStep::Name(text) => name.push_str(text),
                KeyStep::Index(expr) => match self.eval_single(expr, input, depth + 1)? {
                    Some(Json::String(text)) => name.push_str(&text),
                    Some(Json::Number(number)) => name.push_str(&number_text(number)),
                    Some(Json::Null) | None => name.push_str("null"),
                    Some(Json::Bool(true)) => name.push_str("true"),
                    Some(Json::Bool(false)) => name.push_str("false"),
                    Some(_) => {
                        return Err(EvalErr::runtime(
                            "object keys must be strings, numbers or null",
                            0,
                            0,
                        ));
                    }
                },
            }
        }
        Ok(name)
    }

    /// Apply one access step to one value.
    fn apply_step(
        &self,
        step: &Step,
        value: &Json,
        depth: usize,
        sink: Sink,
    ) -> Result<(), EvalErr> {
        self.check_depth(depth)?;
        let position = (step.line, step.column);
        match &step.kind {
            StepKind::Field(name) => match value {
                Json::Object(entries) => {
                    match entries.iter().find(|(key, _)| key == name) {
                        Some((_, item)) => sink(item.clone())?,
                        None => sink(Json::Null)?,
                    }
                    Ok(())
                }
                Json::Null => {
                    sink(Json::Null)?;
                    Ok(())
                }
                other => {
                    if step.optional {
                        return Ok(());
                    }
                    Err(EvalErr::runtime(
                        format!("Cannot index {} with \"{name}\"", other.type_name()),
                        position.0,
                        position.1,
                    ))
                }
            },
            StepKind::Index(number) => {
                let index = index_from_number(*number, position)?;
                index_by_number(value, index, position, step.optional, sink)
            }
            StepKind::IndexValue(expr) => {
                for key in self.eval_values(expr, value, depth + 1)? {
                    match key {
                        Json::String(name) => {
                            let field = Step {
                                kind: StepKind::Field(name),
                                optional: step.optional,
                                line: step.line,
                                column: step.column,
                            };
                            self.apply_step(&field, value, depth + 1, sink)?;
                        }
                        Json::Number(number) => {
                            let index = index_from_number(number, position)?;
                            index_by_number(value, index, position, step.optional, sink)?;
                        }
                        _ => {
                            if !step.optional {
                                return Err(EvalErr::runtime(
                                    "object keys must be strings, numbers or null",
                                    position.0,
                                    position.1,
                                ));
                            }
                        }
                    }
                }
                Ok(())
            }
            StepKind::Iterate => match iterate_value(value, position.0, position.1, &mut |item| {
                sink(item.clone())
            }) {
                Ok(()) => Ok(()),
                Err(error) if step.optional && !error.fatal => Ok(()),
                Err(error) => Err(error),
            },
            StepKind::Slice { start, end } => {
                let start = self.slice_bound(start.as_deref(), value, depth + 1, position)?;
                let end = self.slice_bound(end.as_deref(), value, depth + 1, position)?;
                match value {
                    Json::Null => sink(Json::Null),
                    Json::Array(items) => {
                        let (from, to) = slice_bounds(items.len(), start, end);
                        let sliced: Vec<Json> = items
                            .get(from..to)
                            .map(|slice| slice.to_vec())
                            .unwrap_or_default();
                        sink(Json::Array(sliced))
                    }
                    Json::String(text) => {
                        let characters: Vec<char> = text.chars().collect();
                        let (from, to) = slice_bounds(characters.len(), start, end);
                        let sliced: String = characters
                            .get(from..to)
                            .map(|slice| slice.iter().collect())
                            .unwrap_or_default();
                        sink(Json::String(sliced))
                    }
                    other => {
                        if step.optional {
                            return Ok(());
                        }
                        Err(EvalErr::runtime(
                            format!("Cannot index {} with an object", other.type_name()),
                            position.0,
                            position.1,
                        ))
                    }
                }
            }
        }
    }

    /// Evaluate one slice bound.
    fn slice_bound(
        &self,
        expr: Option<&Expr>,
        value: &Json,
        depth: usize,
        position: (usize, usize),
    ) -> Result<Option<f64>, EvalErr> {
        let expr = match expr {
            Some(expr) => expr,
            None => return Ok(None),
        };
        match self.eval_single(expr, value, depth + 1)? {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Number(number)) => Ok(Some(number)),
            Some(other) => Err(EvalErr::runtime(
                format!("slice bounds must be numbers, found {}", other.type_name()),
                position.0,
                position.1,
            )),
        }
    }

    /// Evaluate an expression and collect its stream.
    fn eval_values(&self, expr: &Expr, input: &Json, depth: usize) -> Result<Vec<Json>, EvalErr> {
        let mut values = Vec::new();
        {
            let mut sink = |value: Json| -> Result<(), EvalErr> {
                if values.len() >= MAX_VALUES {
                    return Err(EvalErr::fatal(
                        format!("too many values (limit {MAX_VALUES})"),
                        0,
                        0,
                    ));
                }
                values.push(value);
                Ok(())
            };
            self.eval(expr, input, depth, &mut sink)?;
        }
        Ok(values)
    }

    /// Evaluate an expression and keep at most its first value.
    fn eval_single(
        &self,
        expr: &Expr,
        input: &Json,
        depth: usize,
    ) -> Result<Option<Json>, EvalErr> {
        let mut value = None;
        {
            let mut sink = |produced: Json| -> Result<(), EvalErr> {
                if value.is_none() {
                    value = Some(produced);
                }
                Ok(())
            };
            self.eval(expr, input, depth, &mut sink)?;
        }
        Ok(value)
    }
}

/// Fail when a freshly built container would nest too deeply.
///
/// Only container children are examined: a scalar child cannot add depth, and
/// checking it would make the common `.[…] + [x]` idiom quadratic.
fn check_container_depth(value: &Json, position: (usize, usize)) -> Result<(), EvalErr> {
    let children: Vec<&Json> = match value {
        Json::Array(items) => items.iter().collect(),
        Json::Object(entries) => entries.iter().map(|(_, item)| item).collect(),
        _ => return Ok(()),
    };
    for child in children {
        if matches!(child, Json::Array(_) | Json::Object(_))
            && !value_depth_within(child, MAX_DEPTH - 1)
        {
            return Err(EvalErr::runtime(
                format!("value exceeds maximum nesting depth ({MAX_DEPTH})"),
                position.0,
                position.1,
            ));
        }
    }
    Ok(())
}

/// Convert a JSON number into an array index, rejecting fractional values.
fn index_from_number(number: f64, position: (usize, usize)) -> Result<i64, EvalErr> {
    if !number.is_finite() || number.fract() != 0.0 || number.abs() > 9.007_199_254_740_992e15 {
        return Err(EvalErr::runtime(
            "array index must be an integer",
            position.0,
            position.1,
        ));
    }
    Ok(number as i64)
}

/// Index an array or an object with a numeric key.
fn index_by_number(
    value: &Json,
    index: i64,
    position: (usize, usize),
    optional: bool,
    sink: Sink,
) -> Result<(), EvalErr> {
    match value {
        Json::Array(items) => {
            match resolve_index(items.len(), index).and_then(|at| items.get(at)) {
                Some(item) => sink(item.clone()),
                None => sink(Json::Null),
            }
        }
        Json::Null => sink(Json::Null),
        other => {
            if optional {
                return Ok(());
            }
            Err(EvalErr::runtime(
                format!("Cannot index {} with a number", other.type_name()),
                position.0,
                position.1,
            ))
        }
    }
}

/// Resolve a possibly negative index against a length.
fn resolve_index(length: usize, index: i64) -> Option<usize> {
    let length = length as i64;
    let resolved = if index < 0 { length + index } else { index };
    if resolved < 0 || resolved >= length {
        None
    } else {
        Some(resolved as usize)
    }
}

/// Compute the concrete `[start, end)` bounds of a slice.
fn slice_bounds(length: usize, start: Option<f64>, end: Option<f64>) -> (usize, usize) {
    let length_i = length as i64;
    let clamp = |value: i64| -> usize {
        if value < 0 {
            0
        } else if value > length_i {
            length
        } else {
            value as usize
        }
    };
    let resolve = |bound: Option<f64>, default: i64| -> i64 {
        match bound {
            Some(number) if number.is_finite() => {
                let rounded = number.trunc();
                if rounded < -9.007_199_254_740_992e15 {
                    i64::MIN
                } else if rounded > 9.007_199_254_740_992e15 {
                    i64::MAX
                } else if rounded < 0.0 {
                    length_i.saturating_add(rounded as i64)
                } else {
                    rounded as i64
                }
            }
            _ => default,
        }
    };
    let from = clamp(resolve(start, 0));
    let to = clamp(resolve(end, length_i));
    if to < from { (from, from) } else { (from, to) }
}

/// Emit every element of an array, or every value of an object.
fn iterate_value(value: &Json, line: usize, column: usize, sink: Sink) -> Result<(), EvalErr> {
    match value {
        Json::Array(items) => {
            for item in items {
                sink(item.clone())?;
            }
            Ok(())
        }
        Json::Object(entries) => {
            for (_, item) in entries {
                sink(item.clone())?;
            }
            Ok(())
        }
        other => Err(EvalErr::runtime(
            format!("Cannot iterate over {}", other.type_name()),
            line,
            column,
        )),
    }
}

/// Collect the values `.[]` would yield, in order.
///
/// Objects iterate their values, exactly like `.[]`; anything that is neither
/// an array nor an object is the same "Cannot iterate over" error.
fn iterate_all(value: &Json, line: usize, column: usize) -> Result<Vec<Json>, EvalErr> {
    let mut out = Vec::new();
    iterate_value(value, line, column, &mut |item| {
        out.push(item);
        Ok(())
    })?;
    Ok(out)
}

/// `has`: does `container` hold `key`?
///
/// Objects take string keys and arrays take numeric ones; `null` holds nothing.
/// Any other pairing is jq's "Cannot check whether … has a … key" error, which
/// `in` reports with the container and key roles swapped.
fn container_has(container: &Json, key: &Json) -> Result<bool, EvalErr> {
    match (container, key) {
        (Json::Object(entries), Json::String(text)) => {
            Ok(entries.iter().any(|(existing, _)| existing == text))
        }
        (Json::Array(items), Json::Number(number)) => {
            let index = index_from_number(*number, (0, 0))?;
            Ok(resolve_index(items.len(), index).is_some())
        }
        (Json::Null, _) => Ok(false),
        (a, b) => Err(EvalErr::runtime(
            format!(
                "Cannot check whether {} has a {} key",
                a.type_name(),
                b.type_name()
            ),
            0,
            0,
        )),
    }
}

/// The `(key, value)` pairs `.[]` yields, as location segments.
fn iterate_locations(value: &Json, position: (usize, usize)) -> Result<Vec<(Json, Json)>, EvalErr> {
    match value {
        Json::Array(items) => Ok(items
            .iter()
            .enumerate()
            .map(|(index, item)| (Json::Number(index as f64), item.clone()))
            .collect()),
        Json::Object(entries) => Ok(entries
            .iter()
            .map(|(key, item)| (Json::String(key.clone()), item.clone()))
            .collect()),
        other => Err(EvalErr::runtime(
            format!("Cannot iterate over {}", other.type_name()),
            position.0,
            position.1,
        )),
    }
}

/// `.name` as a location: one string segment, or the "Cannot index" error.
fn location_field(
    value: &Json,
    name: &str,
    position: (usize, usize),
) -> Result<Vec<(Json, Json)>, EvalErr> {
    match value {
        Json::Object(entries) => Ok(entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(key, item)| vec![(Json::String(key.clone()), item.clone())])
            .unwrap_or_default()),
        // Indexing `null` yields `null`, so the location exists but is empty.
        Json::Null => Ok(vec![(Json::String(name.to_string()), Json::Null)]),
        other => Err(EvalErr::runtime(
            format!("Cannot index {} with \"{name}\"", other.type_name()),
            position.0,
            position.1,
        )),
    }
}

/// `.[index]` as a location, resolving negative indices against `value`.
fn location_index(
    value: &Json,
    number: f64,
    position: (usize, usize),
) -> Result<Vec<(Json, Json)>, EvalErr> {
    match value {
        Json::Array(items) => {
            let index = index_from_number(number, position)?;
            let Some(at) = resolve_index(items.len(), index) else {
                // Reading past the end yields `null`, and deleting it is a
                // no-op, so the location exists but holds nothing to remove.
                return Ok(vec![(Json::Number(number), Json::Null)]);
            };
            Ok(vec![(Json::Number(at as f64), items[at].clone())])
        }
        Json::Null => Ok(vec![(Json::Number(number), Json::Null)]),
        other => Err(EvalErr::runtime(
            format!("Cannot index {} with a number", other.type_name()),
            position.0,
            position.1,
        )),
    }
}

/// Require a number.
fn as_number(value: &Json, line: usize, column: usize) -> Result<f64, EvalErr> {
    match value {
        Json::Number(number) => Ok(*number),
        other => Err(EvalErr::runtime(
            format!("{} cannot be used as a number", other.type_name()),
            line,
            column,
        )),
    }
}

/// Require a string.
fn as_string(value: &Json, line: usize, column: usize) -> Result<String, EvalErr> {
    match value {
        Json::String(text) => Ok(text.clone()),
        other => Err(EvalErr::runtime(
            format!("{} cannot be used as a string", other.type_name()),
            line,
            column,
        )),
    }
}

/// Require an array.
fn as_array(value: &Json, line: usize, column: usize) -> Result<Vec<Json>, EvalErr> {
    match value {
        Json::Array(items) => Ok(items.clone()),
        other => Err(EvalErr::runtime(
            format!("{} cannot be used as an array", other.type_name()),
            line,
            column,
        )),
    }
}

/// Compare two values with jq's total order.
fn order(
    left: &Json,
    right: &Json,
    line: usize,
    column: usize,
) -> Result<std::cmp::Ordering, EvalErr> {
    compare_values(left, right, 0).map_err(|message| EvalErr::runtime(message, line, column))
}

/// Structural equality, with jq's rules.
fn values_equal(left: &Json, right: &Json, line: usize, column: usize) -> Result<bool, EvalErr> {
    equal_values(left, right, 0).map_err(|message| EvalErr::runtime(message, line, column))
}

/// Apply one binary operator.
fn binary_op(
    op: BinOp,
    left: &Json,
    right: &Json,
    line: usize,
    column: usize,
) -> Result<Json, EvalErr> {
    use std::cmp::Ordering;
    match op {
        BinOp::Add => add_values(left, right, line, column),
        BinOp::Sub => subtract_values(left, right, line, column),
        BinOp::Mul => multiply_values(left, right, line, column),
        BinOp::Div => divide_values(left, right, line, column),
        BinOp::Rem => remainder_values(left, right, line, column),
        BinOp::Eq => Ok(Json::Bool(values_equal(left, right, line, column)?)),
        BinOp::Ne => Ok(Json::Bool(!values_equal(left, right, line, column)?)),
        BinOp::Lt => Ok(Json::Bool(
            order(left, right, line, column)? == Ordering::Less,
        )),
        BinOp::Le => Ok(Json::Bool(
            order(left, right, line, column)? != Ordering::Greater,
        )),
        BinOp::Gt => Ok(Json::Bool(
            order(left, right, line, column)? == Ordering::Greater,
        )),
        BinOp::Ge => Ok(Json::Bool(
            order(left, right, line, column)? != Ordering::Less,
        )),
    }
}

/// jq's `+`: numbers add, strings and arrays concatenate, objects merge.
fn add_values(left: &Json, right: &Json, line: usize, column: usize) -> Result<Json, EvalErr> {
    match (left, right) {
        (Json::Null, other) => Ok(other.clone()),
        (other, Json::Null) => Ok(other.clone()),
        (Json::Number(a), Json::Number(b)) => Ok(Json::Number(a + b)),
        (Json::String(a), Json::String(b)) => Ok(Json::String(format!("{a}{b}"))),
        (Json::Array(a), Json::Array(b)) => {
            let mut items = a.clone();
            items.extend(b.iter().cloned());
            let value = Json::Array(items);
            check_container_depth(&value, (line, column))?;
            Ok(value)
        }
        (Json::Object(a), Json::Object(b)) => {
            let mut entries = a.clone();
            for (key, value) in b {
                match entries.iter_mut().find(|(existing, _)| existing == key) {
                    Some(slot) => slot.1 = value.clone(),
                    None => entries.push((key.clone(), value.clone())),
                }
            }
            let value = Json::Object(entries);
            check_container_depth(&value, (line, column))?;
            Ok(value)
        }
        (a, b) => Err(EvalErr::runtime(
            format!("{} and {} cannot be added", a.type_name(), b.type_name()),
            line,
            column,
        )),
    }
}

/// jq's `-`: numbers subtract, arrays lose the elements of the right side.
fn subtract_values(left: &Json, right: &Json, line: usize, column: usize) -> Result<Json, EvalErr> {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => Ok(Json::Number(a - b)),
        (Json::Array(a), Json::Array(b)) => {
            let mut items = Vec::new();
            for item in a {
                let mut removed = false;
                for other in b {
                    if values_equal(item, other, line, column)? {
                        removed = true;
                        break;
                    }
                }
                if !removed {
                    items.push(item.clone());
                }
            }
            Ok(Json::Array(items))
        }
        (a, b) => Err(EvalErr::runtime(
            format!(
                "{} and {} cannot be subtracted",
                a.type_name(),
                b.type_name()
            ),
            line,
            column,
        )),
    }
}

/// jq's `*`: numbers multiply, strings repeat, objects deep-merge.
fn multiply_values(left: &Json, right: &Json, line: usize, column: usize) -> Result<Json, EvalErr> {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => Ok(Json::Number(a * b)),
        (Json::String(text), Json::Number(count)) => repeat_string(text, *count, line, column),
        (Json::Object(a), Json::Object(b)) => deep_merge(a, b, 0, line, column),
        (a, b) => Err(EvalErr::runtime(
            format!(
                "{} and {} cannot be multiplied",
                a.type_name(),
                b.type_name()
            ),
            line,
            column,
        )),
    }
}

/// Repeat a string `count` times, refusing absurd results.
fn repeat_string(text: &str, count: f64, line: usize, column: usize) -> Result<Json, EvalErr> {
    if !count.is_finite() || count < 0.0 {
        return Err(EvalErr::runtime(
            "string repetition count must be a non-negative number",
            line,
            column,
        ));
    }
    let count = count.trunc();
    if count == 0.0 {
        return Ok(Json::String(String::new()));
    }
    let total = text.len() as f64 * count;
    if !total.is_finite() || total > MAX_STRING_BYTES as f64 {
        return Err(EvalErr::runtime(
            format!("string repetition would exceed {MAX_STRING_BYTES} bytes"),
            line,
            column,
        ));
    }
    let mut out = String::with_capacity(total as usize);
    for _ in 0..(count as usize) {
        out.push_str(text);
    }
    Ok(Json::String(out))
}

/// jq's object `*`: merge recursively, right wins.
fn deep_merge(
    left: &[(String, Json)],
    right: &[(String, Json)],
    depth: usize,
    line: usize,
    column: usize,
) -> Result<Json, EvalErr> {
    if depth > MAX_DEPTH {
        return Err(EvalErr::runtime(
            format!("value exceeds maximum nesting depth ({MAX_DEPTH})"),
            line,
            column,
        ));
    }
    let mut entries = left.to_vec();
    for (key, value) in right {
        match entries.iter().position(|(existing, _)| existing == key) {
            Some(index) => {
                let merged = match (&entries[index].1, value) {
                    (Json::Object(a), Json::Object(b)) => {
                        deep_merge(a, b, depth + 1, line, column)?
                    }
                    _ => value.clone(),
                };
                entries[index].1 = merged;
            }
            None => entries.push((key.clone(), value.clone())),
        }
    }
    Ok(Json::Object(entries))
}

/// jq's `/`: numbers divide, strings split on a literal separator.
fn divide_values(left: &Json, right: &Json, line: usize, column: usize) -> Result<Json, EvalErr> {
    match (left, right) {
        (Json::Number(a), Json::Number(b)) => {
            if *b == 0.0 {
                return Err(EvalErr::runtime(
                    format!(
                        "number ({a}) and number (0) cannot be divided because the divisor is zero"
                    ),
                    line,
                    column,
                ));
            }
            Ok(Json::Number(a / b))
        }
        (Json::String(text), Json::String(separator)) => {
            if separator.is_empty() {
                return Err(EvalErr::runtime(
                    "string cannot be split by an empty separator",
                    line,
                    column,
                ));
            }
            let parts = text
                .split(separator.as_str())
                .map(|part| Json::String(part.to_string()))
                .collect();
            Ok(Json::Array(parts))
        }
        (a, b) => Err(EvalErr::runtime(
            format!("{} and {} cannot be divided", a.type_name(), b.type_name()),
            line,
            column,
        )),
    }
}

/// jq's `%`: integer remainder with jq's sign convention.
fn remainder_values(
    left: &Json,
    right: &Json,
    line: usize,
    column: usize,
) -> Result<Json, EvalErr> {
    let a = as_number(left, line, column)?;
    let b = as_number(right, line, column)?;
    if !a.is_finite() || !b.is_finite() {
        return Err(EvalErr::runtime(
            "remainder requires finite numbers",
            line,
            column,
        ));
    }
    let a = a.trunc();
    let b = b.trunc();
    if b == 0.0 {
        return Err(EvalErr::runtime(
            format!(
                "number ({a}) and number (0) cannot be divided (remainder) because the divisor is zero"
            ),
            line,
            column,
        ));
    }
    if a.abs() > 9.007_199_254_740_992e15 || b.abs() > 9.007_199_254_740_992e15 {
        return Err(EvalErr::runtime(
            "remainder requires integers that fit in 53 bits",
            line,
            column,
        ));
    }
    let a = a as i64;
    let b = b as i64;
    let mut remainder = a % b;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        remainder += b;
    }
    Ok(Json::Number(remainder as f64))
}

impl<'a> Evaluator<'a> {
    /// Evaluate a function call.
    fn eval_call(
        &self,
        name: &str,
        args: &[Expr],
        input: &Json,
        depth: usize,
        sink: Sink,
    ) -> Result<Vec<Json>, EvalErr> {
        self.check_depth(depth)?;
        let values = self.call_values(name, args, input, depth)?;
        for value in &values {
            sink(value.clone())?;
        }
        Ok(values)
    }

    /// The values a builtin produces for one input.
    #[allow(clippy::too_many_lines)]
    fn call_values(
        &self,
        name: &str,
        args: &[Expr],
        input: &Json,
        depth: usize,
    ) -> Result<Vec<Json>, EvalErr> {
        let _wrong_arity = |expected: &str| -> EvalErr {
            EvalErr::runtime(
                format!("`{name}` expects {expected}, found {}", args.len()),
                0,
                0,
            )
        };
        match name {
            "empty" => Ok(Vec::new()),
            "error" => {
                if args.is_empty() {
                    Err(EvalErr::runtime(describe_value(input), 0, 0))
                } else {
                    let message = match self.argument(args, 0, input, depth)? {
                        Json::String(text) => text,
                        other => describe_value(&other),
                    };
                    Err(EvalErr::runtime(message, 0, 0))
                }
            }
            "not" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Bool(!input.is_truthy())])
            }
            "length" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(match input {
                    Json::Null => 0.0,
                    Json::String(text) => text.chars().count() as f64,
                    Json::Array(items) => items.len() as f64,
                    Json::Object(entries) => entries.len() as f64,
                    Json::Number(number) => number.abs(),
                    other => {
                        return Err(EvalErr::runtime(
                            format!("{} has no length", other.type_name()),
                            0,
                            0,
                        ));
                    }
                })])
            }
            "utf8bytelength" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(as_string(input, 0, 0)?.len() as f64)])
            }
            "keys" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Array(match input {
                    Json::Array(items) => (0..items.len())
                        .map(|index| Json::Number(index as f64))
                        .collect(),
                    Json::Object(entries) => {
                        let mut keys: Vec<String> =
                            entries.iter().map(|(key, _)| key.clone()).collect();
                        keys.sort();
                        keys.into_iter().map(Json::String).collect()
                    }
                    other => {
                        return Err(EvalErr::runtime(
                            format!("{} has no keys", other.type_name()),
                            0,
                            0,
                        ));
                    }
                })])
            }
            "keys_unsorted" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Array(match input {
                    Json::Array(items) => (0..items.len())
                        .map(|index| Json::Number(index as f64))
                        .collect(),
                    Json::Object(entries) => entries
                        .iter()
                        .map(|(key, _)| Json::String(key.clone()))
                        .collect(),
                    other => {
                        return Err(EvalErr::runtime(
                            format!("{} has no keys", other.type_name()),
                            0,
                            0,
                        ));
                    }
                })])
            }
            "values" => {
                self.expect_args(args, 0, name)?;
                if matches!(input, Json::Null) {
                    Ok(Vec::new())
                } else {
                    Ok(vec![input.clone()])
                }
            }
            "type" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::String(input.type_name().to_string())])
            }
            "tostring" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::String(match input {
                    Json::String(text) => text.clone(),
                    other => other.compact(),
                })])
            }
            "tojson" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::String(input.compact())])
            }
            "fromjson" => {
                self.expect_args(args, 0, name)?;
                let text = as_string(input, 0, 0)?;
                match Json::parse(&text) {
                    Ok(value) => Ok(vec![value]),
                    Err(message) => Err(EvalErr::runtime(
                        format!("Invalid JSON text: {message}"),
                        0,
                        0,
                    )),
                }
            }
            "tonumber" => {
                self.expect_args(args, 0, name)?;
                match input {
                    Json::Number(_) => Ok(vec![input.clone()]),
                    Json::String(text) => match Json::parse(text) {
                        Ok(Json::Number(number)) => Ok(vec![Json::Number(number)]),
                        _ => Err(EvalErr::runtime(
                            format!("{text} is not a valid number"),
                            0,
                            0,
                        )),
                    },
                    other => Err(EvalErr::runtime(
                        format!("{} cannot be parsed as a number", other.type_name()),
                        0,
                        0,
                    )),
                }
            }
            "ascii_downcase" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::String(
                    as_string(input, 0, 0)?
                        .chars()
                        .map(|character| character.to_ascii_lowercase())
                        .collect(),
                )])
            }
            "ascii_upcase" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::String(
                    as_string(input, 0, 0)?
                        .chars()
                        .map(|character| character.to_ascii_uppercase())
                        .collect(),
                )])
            }
            "explode" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Array(
                    as_string(input, 0, 0)?
                        .chars()
                        .map(|character| Json::Number(character as u32 as f64))
                        .collect(),
                )])
            }
            "implode" => {
                self.expect_args(args, 0, name)?;
                let items = as_array(input, 0, 0)?;
                let mut text = String::new();
                for item in items {
                    let number = as_number(&item, 0, 0)?;
                    if !number.is_finite() || number.fract() != 0.0 || number < 0.0 {
                        return Err(EvalErr::runtime(
                            "implode requires an array of codepoints",
                            0,
                            0,
                        ));
                    }
                    text.push(char::from_u32(number as u32).unwrap_or('\u{fffd}'));
                    if text.len() > MAX_STRING_BYTES {
                        return Err(EvalErr::runtime(
                            format!("implode would exceed {MAX_STRING_BYTES} bytes"),
                            0,
                            0,
                        ));
                    }
                }
                Ok(vec![Json::String(text)])
            }
            "add" => {
                self.expect_args(args, 0, name)?;
                // `add` is `reduce .[] as $x (null; . + $x)`, so an object adds
                // its values and a non-iterable input is the same error `.[]`
                // would raise.
                let mut accumulator = Json::Null;
                for item in iterate_all(input, 0, 0)? {
                    accumulator = add_values(&accumulator, &item, 0, 0)?;
                }
                Ok(vec![accumulator])
            }
            "any" | "all" => {
                self.expect_args(args, 1, name)?;
                let want_all = name == "all";
                let mut result = want_all;
                for item in iterate_all(input, 0, 0)? {
                    let candidate = if args.is_empty() {
                        item
                    } else {
                        match self.eval_single(&args[0], &item, depth + 1)? {
                            Some(value) => value,
                            None => continue,
                        }
                    };
                    if want_all && !candidate.is_truthy() {
                        result = false;
                        break;
                    }
                    if !want_all && candidate.is_truthy() {
                        result = true;
                        break;
                    }
                }
                Ok(vec![Json::Bool(result)])
            }
            "min" | "max" => {
                self.expect_args(args, 0, name)?;
                let mut best: Option<Json> = None;
                for item in as_array(input, 0, 0)? {
                    match &best {
                        None => best = Some(item),
                        Some(current) => {
                            let ordering = order(&item, current, 0, 0)?;
                            if (name == "min" && ordering == std::cmp::Ordering::Less)
                                || (name == "max" && ordering == std::cmp::Ordering::Greater)
                            {
                                best = Some(item);
                            }
                        }
                    }
                }
                Ok(vec![best.unwrap_or(Json::Null)])
            }
            "min_by" | "max_by" => {
                self.expect_args(args, 1, name)?;
                let mut best: Option<(Json, Json)> = None;
                for item in as_array(input, 0, 0)? {
                    let key = self.key_of(args, &item, depth + 1)?;
                    match &best {
                        None => best = Some((item, key)),
                        Some((_, current_key)) => {
                            let ordering = order(&key, current_key, 0, 0)?;
                            if (name == "min_by" && ordering == std::cmp::Ordering::Less)
                                || (name == "max_by" && ordering == std::cmp::Ordering::Greater)
                            {
                                best = Some((item, key));
                            }
                        }
                    }
                }
                Ok(vec![best.map(|(item, _)| item).unwrap_or(Json::Null)])
            }
            "floor" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(as_number(input, 0, 0)?.floor())])
            }
            "ceil" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(as_number(input, 0, 0)?.ceil())])
            }
            "round" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(as_number(input, 0, 0)?.round())])
            }
            "sqrt" => {
                self.expect_args(args, 0, name)?;
                let number = as_number(input, 0, 0)?;
                if number < 0.0 {
                    return Err(EvalErr::runtime("sqrt of a negative number", 0, 0));
                }
                Ok(vec![Json::Number(number.sqrt())])
            }
            "fabs" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Number(as_number(input, 0, 0)?.abs())])
            }
            "sort" => {
                self.expect_args(args, 0, name)?;
                let mut keyed = Vec::new();
                for item in as_array(input, 0, 0)? {
                    keyed.push((vec![item.clone()], item));
                }
                Ok(vec![Json::Array(sort_keyed(keyed)?)])
            }
            "sort_by" => {
                self.expect_args(args, 1, name)?;
                let mut keyed = Vec::new();
                for item in as_array(input, 0, 0)? {
                    keyed.push((vec![self.key_of(args, &item, depth + 1)?], item));
                }
                Ok(vec![Json::Array(sort_keyed(keyed)?)])
            }
            "group_by" => {
                self.expect_args(args, 1, name)?;
                let mut keyed = Vec::new();
                for item in as_array(input, 0, 0)? {
                    keyed.push((vec![self.key_of(args, &item, depth + 1)?], item));
                }
                let sorted = sort_pairs(keyed)?;
                let mut groups: Vec<Json> = Vec::new();
                let mut current: Vec<Json> = Vec::new();
                let mut current_key: Option<Json> = None;
                for (key, item) in sorted {
                    let key = key.into_iter().next().unwrap_or(Json::Null);
                    let same = match &current_key {
                        Some(previous) => values_equal(previous, &key, 0, 0)?,
                        None => false,
                    };
                    if current_key.is_none() || same {
                        current.push(item);
                    } else {
                        groups.push(Json::Array(std::mem::take(&mut current)));
                        current.push(item);
                    }
                    current_key = Some(key);
                }
                if !current.is_empty() {
                    groups.push(Json::Array(current));
                }
                Ok(vec![Json::Array(groups)])
            }
            "unique" => {
                self.expect_args(args, 0, name)?;
                let mut keyed = Vec::new();
                for item in as_array(input, 0, 0)? {
                    keyed.push((vec![item.clone()], item));
                }
                Ok(vec![Json::Array(unique_keyed(sort_pairs(keyed)?)?)])
            }
            "unique_by" => {
                self.expect_args(args, 1, name)?;
                let mut keyed = Vec::new();
                for item in as_array(input, 0, 0)? {
                    keyed.push((vec![self.key_of(args, &item, depth + 1)?], item));
                }
                Ok(vec![Json::Array(unique_keyed(sort_pairs(keyed)?)?)])
            }
            "reverse" => {
                self.expect_args(args, 0, name)?;
                match input {
                    Json::Array(items) => {
                        let mut items = items.clone();
                        items.reverse();
                        Ok(vec![Json::Array(items)])
                    }
                    Json::String(text) => Ok(vec![Json::String(text.chars().rev().collect())]),
                    Json::Null => Ok(vec![Json::Null]),
                    other => Err(EvalErr::runtime(
                        format!("{} cannot be reversed", other.type_name()),
                        0,
                        0,
                    )),
                }
            }
            "first" => {
                self.expect_args(args, 1, name)?;
                if args.is_empty() {
                    return Ok(first_value(input, 0, 0)?.into_iter().collect());
                }
                let values = self.eval_values(&args[0], input, depth + 1)?;
                Ok(values.into_iter().next().into_iter().collect())
            }
            "last" => {
                self.expect_args(args, 1, name)?;
                if args.is_empty() {
                    return Ok(last_value(input, 0, 0)?.into_iter().collect());
                }
                let values = self.eval_values(&args[0], input, depth + 1)?;
                Ok(values.into_iter().last().into_iter().collect())
            }
            "nth" => {
                self.expect_args(args, 2, name)?;
                let index = as_number(&self.argument(args, 0, input, depth)?, 0, 0)?;
                let index = index_from_number(index, (0, 0))?;
                let values = if args.len() > 1 {
                    self.eval_values(&args[1], input, depth + 1)?
                } else {
                    as_array(input, 0, 0)?
                };
                Ok(resolve_index(values.len(), index)
                    .and_then(|at| values.get(at).cloned())
                    .into_iter()
                    .collect())
            }
            // `range(n)`, `range(from; to)` and `range(from; to; step)`.
            "range" => {
                if args.is_empty() || args.len() > 3 {
                    return Err(EvalErr::runtime("range expects 1 to 3 arguments", 0, 0));
                }
                let mut bounds = Vec::new();
                for index in 0..args.len() {
                    bounds.push(as_number(&self.argument(args, index, input, depth)?, 0, 0)?);
                }
                let (start, end, step) = match bounds.len() {
                    1 => (0.0, bounds[0], 1.0),
                    2 => (bounds[0], bounds[1], 1.0),
                    _ => (bounds[0], bounds[1], bounds[2]),
                };
                if step == 0.0 {
                    return Err(EvalErr::runtime("range step cannot be zero", 0, 0));
                }
                let mut values = Vec::new();
                let mut current = start;
                // A hard cap keeps a hostile filter from allocating forever.
                while (step > 0.0 && current < end) || (step < 0.0 && current > end) {
                    values.push(Json::Number(current));
                    if values.len() > 1_000_000 {
                        return Err(EvalErr::runtime("range produces too many values", 0, 0));
                    }
                    current += step;
                }
                Ok(values)
            }
            "limit" => {
                self.expect_args(args, 2, name)?;
                let count = as_number(&self.argument(args, 0, input, depth)?, 0, 0)?;
                if !count.is_finite() || count < 0.0 {
                    return Err(EvalErr::runtime(
                        "limit requires a non-negative count",
                        0,
                        0,
                    ));
                }
                let values = self.eval_values(&args[1], input, depth + 1)?;
                Ok(values.into_iter().take(count as usize).collect())
            }
            "select" => {
                self.expect_args(args, 1, name)?;
                let mut selected = false;
                for value in self.eval_values(&args[0], input, depth + 1)? {
                    if value.is_truthy() {
                        selected = true;
                        break;
                    }
                }
                if selected {
                    Ok(vec![input.clone()])
                } else {
                    Ok(Vec::new())
                }
            }
            "map" => {
                self.expect_args(args, 1, name)?;
                let mut out = Vec::new();
                for item in as_array(input, 0, 0)? {
                    out.extend(self.eval_values(&args[0], &item, depth + 1)?);
                }
                Ok(vec![Json::Array(out)])
            }
            "map_values" => {
                self.expect_args(args, 1, name)?;
                match input {
                    Json::Array(items) => {
                        let mut out = Vec::new();
                        for item in items {
                            out.extend(self.eval_values(&args[0], item, depth + 1)?);
                        }
                        Ok(vec![Json::Array(out)])
                    }
                    Json::Object(entries) => {
                        let mut out: Vec<(String, Json)> = Vec::new();
                        for (key, item) in entries {
                            for value in self.eval_values(&args[0], item, depth + 1)? {
                                match out.iter_mut().find(|(existing, _)| existing == key) {
                                    Some(slot) => slot.1 = value,
                                    None => out.push((key.clone(), value)),
                                }
                            }
                        }
                        Ok(vec![Json::Object(out)])
                    }
                    Json::Null => Ok(vec![Json::Null]),
                    other => Err(EvalErr::runtime(
                        format!("{} has no values to map", other.type_name()),
                        0,
                        0,
                    )),
                }
            }
            "paths" | "leaf_paths" => {
                self.expect_args(args, 1, name)?;
                let leaves_only = name == "leaf_paths";
                let predicate = args.first();
                let mut all: Vec<Vec<Json>> = Vec::new();
                collect_paths(input, &mut Vec::new(), &mut all, 0)?;
                let mut out = Vec::new();
                for path in all {
                    let value = get_path(input, &path, 0, 0)?;
                    if leaves_only && matches!(value, Json::Array(_) | Json::Object(_)) {
                        continue;
                    }
                    if let Some(predicate) = predicate {
                        let mut keep = false;
                        for value in self.eval_values(predicate, &value, depth + 1)? {
                            if value.is_truthy() {
                                keep = true;
                                break;
                            }
                        }
                        if !keep {
                            continue;
                        }
                    }
                    out.push(Json::Array(path));
                }
                Ok(out)
            }
            "del" => {
                self.expect_args(args, 1, name)?;
                // `del(f)` is `delpaths([path(f)])`, so the argument is read as
                // a location rather than as a value.
                let mut locations = Vec::new();
                let at: Vec<Json> = Vec::new();
                self.collect_locations(&args[0], input, &at, &mut locations, depth + 1)?;
                let mut paths: Vec<Vec<Json>> =
                    locations.into_iter().map(|(path, _)| path).collect();
                // Delete the deepest and right-most paths first so earlier
                // indices stay valid.
                paths.sort_by(|a, b| compare_paths(b, a));
                let mut current = input.clone();
                for path in paths {
                    current = delete_path(&current, &path)?;
                }
                Ok(vec![current])
            }
            "getpath" => {
                self.expect_args(args, 1, name)?;
                let path = as_path(&self.argument(args, 0, input, depth)?, 0, 0)?;
                Ok(vec![get_path(input, &path, 0, 0)?])
            }
            "setpath" => {
                self.expect_args(args, 2, name)?;
                let path = as_path(&self.argument(args, 0, input, depth)?, 0, 0)?;
                let value = self.argument(args, 1, input, depth)?;
                Ok(vec![set_path(input, &path, &value, 0)?])
            }
            "to_entries" => {
                self.expect_args(args, 0, name)?;
                Ok(vec![Json::Array(match input {
                    Json::Object(entries) => entries
                        .iter()
                        .map(|(key, value)| {
                            Json::Object(vec![
                                (String::from("key"), Json::String(key.clone())),
                                (String::from("value"), value.clone()),
                            ])
                        })
                        .collect(),
                    Json::Array(items) => items
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            Json::Object(vec![
                                (String::from("key"), Json::Number(index as f64)),
                                (String::from("value"), value.clone()),
                            ])
                        })
                        .collect(),
                    other => {
                        return Err(EvalErr::runtime(
                            format!("{} cannot be converted to entries", other.type_name()),
                            0,
                            0,
                        ));
                    }
                })])
            }
            "from_entries" => {
                self.expect_args(args, 0, name)?;
                let items = as_array(input, 0, 0)?;
                let mut entries: Vec<(String, Json)> = Vec::new();
                for item in items {
                    let object = match &item {
                        Json::Object(entries) => entries,
                        other => {
                            return Err(EvalErr::runtime(
                                format!("Cannot use {} as an object entry", other.type_name()),
                                0,
                                0,
                            ));
                        }
                    };
                    let find = |names: &[&str]| -> Option<&Json> {
                        object
                            .iter()
                            .find(|(key, _)| names.contains(&key.as_str()))
                            .map(|(_, value)| value)
                    };
                    let key = find(&["key", "k", "name", "Name"]);
                    let value = find(&["value", "v"]);
                    let key = match key {
                        Some(Json::String(text)) => text.clone(),
                        Some(Json::Number(number)) => number_text(*number),
                        Some(Json::Null) | None => {
                            return Err(EvalErr::runtime("Cannot use null as an object key", 0, 0));
                        }
                        Some(other) => {
                            return Err(EvalErr::runtime(
                                format!("Cannot use {} as an object key", other.type_name()),
                                0,
                                0,
                            ));
                        }
                    };
                    let value = value.cloned().unwrap_or(Json::Null);
                    match entries.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(slot) => slot.1 = value,
                        None => entries.push((key, value)),
                    }
                }
                Ok(vec![Json::Object(entries)])
            }
            "with_entries" => {
                self.expect_args(args, 1, name)?;
                let entries = self.call_values("to_entries", &[], input, depth)?;
                let entries = entries.into_iter().next().unwrap_or(Json::Null);
                let mut mapped = Vec::new();
                for item in as_array(&entries, 0, 0)? {
                    mapped.extend(self.eval_values(&args[0], &item, depth + 1)?);
                }
                self.call_values("from_entries", &[], &Json::Array(mapped), depth)
            }
            "join" => {
                self.expect_args(args, 1, name)?;
                let separator = as_string(&self.argument(args, 0, input, depth)?, 0, 0)?;
                let items = as_array(input, 0, 0)?;
                let mut parts = Vec::new();
                for item in items {
                    parts.push(match item {
                        Json::String(text) => text,
                        Json::Number(number) => number_text(number),
                        Json::Bool(true) => "true".to_string(),
                        Json::Bool(false) => "false".to_string(),
                        Json::Null => String::new(),
                        other => {
                            return Err(EvalErr::runtime(
                                format!("{} cannot be joined", other.type_name()),
                                0,
                                0,
                            ));
                        }
                    });
                }
                Ok(vec![Json::String(parts.join(&separator))])
            }
            "split" => {
                self.expect_args(args, 1, name)?;
                let text = as_string(input, 0, 0)?;
                let separator = self.argument(args, 0, input, depth)?;
                if let Some(regex) = try_regex_spec(&separator, false)? {
                    let pieces = regex.regex.split(&text);
                    return Ok(vec![Json::Array(
                        pieces.into_iter().map(Json::String).collect(),
                    )]);
                }
                let separator = as_string(&separator, 0, 0)?;
                if separator.is_empty() {
                    return Ok(vec![Json::Array(
                        text.chars().map(|c| Json::String(c.to_string())).collect(),
                    )]);
                }
                Ok(vec![Json::Array(
                    text.split(separator.as_str())
                        .map(|piece| Json::String(piece.to_string()))
                        .collect(),
                )])
            }
            "splits" => {
                self.expect_args(args, 2, name)?;
                let text = as_string(input, 0, 0)?;
                let regex = self.regex_argument(args, input, depth)?;
                Ok(regex
                    .regex
                    .split(&text)
                    .into_iter()
                    .map(Json::String)
                    .collect())
            }
            "ltrimstr" | "rtrimstr" => {
                self.expect_args(args, 1, name)?;
                let text = as_string(input, 0, 0)?;
                let affix = as_string(&self.argument(args, 0, input, depth)?, 0, 0)?;
                let trimmed = if name == "ltrimstr" {
                    text.strip_prefix(affix.as_str()).map(str::to_string)
                } else {
                    text.strip_suffix(affix.as_str()).map(str::to_string)
                };
                Ok(vec![Json::String(trimmed.unwrap_or(text))])
            }
            "startswith" | "endswith" => {
                self.expect_args(args, 1, name)?;
                let other = self.argument(args, 0, input, depth)?;
                let result = match (input, &other) {
                    (Json::String(text), Json::String(affix)) => {
                        if name == "startswith" {
                            text.starts_with(affix.as_str())
                        } else {
                            text.ends_with(affix.as_str())
                        }
                    }
                    (Json::Array(items), Json::Array(prefix)) => {
                        if prefix.len() > items.len() {
                            false
                        } else {
                            let slice = if name == "startswith" {
                                &items[..prefix.len()]
                            } else {
                                &items[items.len() - prefix.len()..]
                            };
                            let mut same = true;
                            for (a, b) in slice.iter().zip(prefix.iter()) {
                                if !values_equal(a, b, 0, 0)? {
                                    same = false;
                                    break;
                                }
                            }
                            same
                        }
                    }
                    (a, b) => {
                        return Err(EvalErr::runtime(
                            format!(
                                "{} and {} cannot be compared with {name}",
                                a.type_name(),
                                b.type_name()
                            ),
                            0,
                            0,
                        ));
                    }
                };
                Ok(vec![Json::Bool(result)])
            }
            "has" => {
                self.expect_args(args, 1, name)?;
                let key = self.argument(args, 0, input, depth)?;
                Ok(vec![Json::Bool(container_has(input, &key)?)])
            }
            "in" => {
                self.expect_args(args, 1, name)?;
                // `in(xs)` is `xs | has(.)`: the input is the key and the
                // argument is the container.
                let container = self.argument(args, 0, input, depth)?;
                Ok(vec![Json::Bool(container_has(&container, input)?)])
            }
            "test" => {
                self.expect_args(args, 2, name)?;
                let text = as_string(input, 0, 0)?;
                let regex = self.regex_argument(args, input, depth)?;
                Ok(vec![Json::Bool(regex.regex.is_match(&text))])
            }
            "match" => {
                self.expect_args(args, 2, name)?;
                let text = as_string(input, 0, 0)?;
                let regex = self.regex_argument(args, input, depth)?;
                let matches = find_matches(&regex, &text, regex.global);
                let mut out = Vec::new();
                for captures in matches {
                    out.push(match_object(&regex, &text, &captures));
                }
                Ok(out)
            }
            "capture" => {
                self.expect_args(args, 2, name)?;
                let text = as_string(input, 0, 0)?;
                let regex = self.regex_argument(args, input, depth)?;
                let captures = match regex.regex.captures(&text) {
                    Some(captures) => captures,
                    None => return Ok(Vec::new()),
                };
                let mut entries: Vec<(String, Json)> = Vec::new();
                for (index, name) in regex.names.iter().enumerate() {
                    let name = match name {
                        Some(name) => name.clone(),
                        None => continue,
                    };
                    let span = captures.get(index + 1).copied().flatten();
                    let value = match span {
                        Some((start, end)) => {
                            Json::String(text.get(start..end).unwrap_or("").to_string())
                        }
                        None => Json::Null,
                    };
                    entries.push((name, value));
                }
                Ok(vec![Json::Object(entries)])
            }
            "sub" | "gsub" => {
                self.expect_args(args, 3, name)?;
                let text = as_string(input, 0, 0)?;
                let regex = compile_regex(&self.argument(args, 0, input, depth)?, false)?;
                let regex = match args.get(2) {
                    // `sub(re; replacement; flags)`
                    Some(_) => {
                        let flags = as_string(&self.argument(args, 2, input, depth)?, 0, 0)?;
                        let mut regex = regex;
                        apply_flags(&mut regex, &flags)?;
                        regex
                    }
                    None => regex,
                };
                let global = name == "gsub" || regex.global;
                let matches = find_matches(&regex, &text, global);
                let mut out = String::new();
                let mut last = 0usize;
                for captures in matches {
                    let (start, end) = match captures.first().copied().flatten() {
                        Some(span) => span,
                        None => continue,
                    };
                    out.push_str(text.get(last..start).unwrap_or(""));
                    let match_value = replacement_context(&regex, &text, &captures);
                    let replacement = match self.eval_single(&args[1], &match_value, depth + 1)? {
                        Some(Json::String(replacement)) => replacement,
                        Some(other) => other.compact(),
                        None => String::new(),
                    };
                    out.push_str(&replacement);
                    last = end;
                }
                out.push_str(text.get(last..).unwrap_or(""));
                Ok(vec![Json::String(out)])
            }
            other => Err(EvalErr::runtime(
                format!("unsupported function `{other}`"),
                0,
                0,
            )),
        }
    }

    /// Resolve `expr` to the locations it denotes.
    ///
    /// `del` needs the paths a filter points at rather than the values it
    /// produces, so this walks the location-shaped part of the language: `.`,
    /// field/index/iterate/slice steps, `?`, `select`, `empty`, `|` and `,`.
    /// `at` is the path already walked to reach `value` within the document;
    /// each result pairs a full path with the value it currently holds.
    fn collect_locations(
        &self,
        expr: &Expr,
        value: &Json,
        at: &[Json],
        out: &mut Vec<(Vec<Json>, Json)>,
        depth: usize,
    ) -> Result<(), EvalErr> {
        if depth > MAX_EVAL_DEPTH {
            return Err(EvalErr::fatal(
                format!("filter exceeds maximum evaluation depth ({MAX_EVAL_DEPTH})"),
                0,
                0,
            ));
        }
        if out.len() > MAX_VALUES {
            return Err(EvalErr::fatal(
                format!("too many values (limit {MAX_VALUES})"),
                0,
                0,
            ));
        }
        match expr {
            Expr::Identity => {
                out.push((at.to_vec(), value.clone()));
                Ok(())
            }
            Expr::Iterate => {
                for (segment, child) in iterate_locations(value, (0, 0))? {
                    let mut path = at.to_vec();
                    path.push(segment);
                    out.push((path, child));
                }
                Ok(())
            }
            Expr::Access { base, steps } => {
                let mut bases: Vec<(Vec<Json>, Json)> = Vec::new();
                self.collect_locations(base, value, at, &mut bases, depth + 1)?;
                for (base_path, base_value) in bases {
                    let mut pending = vec![(base_path, base_value)];
                    for step in steps {
                        let mut next: Vec<(Vec<Json>, Json)> = Vec::new();
                        for (path, current) in pending {
                            let stepped = match self.location_steps(&current, step, depth + 1) {
                                Ok(stepped) => stepped,
                                Err(error) if step.optional && !error.fatal => continue,
                                Err(error) => return Err(error),
                            };
                            for (segment, child) in stepped {
                                let mut full = path.clone();
                                full.push(segment);
                                next.push((full, child));
                            }
                        }
                        pending = next;
                    }
                    out.extend(pending);
                }
                Ok(())
            }
            Expr::Comma(left, right) => {
                self.collect_locations(left, value, at, out, depth + 1)?;
                self.collect_locations(right, value, at, out, depth + 1)
            }
            Expr::Pipe(left, right) => {
                let mut lefts: Vec<(Vec<Json>, Json)> = Vec::new();
                self.collect_locations(left, value, at, &mut lefts, depth + 1)?;
                for (path, sub) in lefts {
                    self.collect_locations(right, &sub, &path, out, depth + 1)?;
                }
                Ok(())
            }
            Expr::Optional(inner) => match self.collect_locations(inner, value, at, out, depth + 1)
            {
                Ok(()) => Ok(()),
                Err(error) if !error.fatal => Ok(()),
                Err(error) => Err(error),
            },
            Expr::Call { name, args } if name == "empty" && args.is_empty() => Ok(()),
            Expr::Call { name, args } if name == "select" && args.len() == 1 => {
                for candidate in self.eval_values(&args[0], value, depth + 1)? {
                    if candidate.is_truthy() {
                        out.push((at.to_vec(), value.clone()));
                        break;
                    }
                }
                Ok(())
            }
            other => match self.eval_single(other, value, depth + 1)? {
                Some(result) => Err(EvalErr::runtime(
                    format!("Invalid path expression with result {}", result.compact()),
                    0,
                    0,
                )),
                None => Ok(()),
            },
        }
    }

    /// Apply one access step as a location: the segments it denotes, each with
    /// the value that segment currently holds.
    fn location_steps(
        &self,
        value: &Json,
        step: &Step,
        depth: usize,
    ) -> Result<Vec<(Json, Json)>, EvalErr> {
        let position = (step.line, step.column);
        match &step.kind {
            StepKind::Field(name) => location_field(value, name, position),
            StepKind::Index(number) => location_index(value, *number, position),
            StepKind::IndexValue(expr) => match self.eval_single(expr, value, depth + 1)? {
                None => Ok(Vec::new()),
                Some(Json::String(name)) => location_field(value, &name, position),
                Some(Json::Number(number)) => location_index(value, number, position),
                Some(other) => Err(EvalErr::runtime(
                    format!(
                        "Cannot index {} with {}",
                        value.type_name(),
                        other.type_name()
                    ),
                    position.0,
                    position.1,
                )),
            },
            StepKind::Iterate => iterate_locations(value, position),
            StepKind::Slice { start, end } => {
                let start = self.slice_bound(start.as_deref(), value, depth + 1, position)?;
                let end = self.slice_bound(end.as_deref(), value, depth + 1, position)?;
                match value {
                    // A slice of `null` denotes nothing to delete.
                    Json::Null => Ok(Vec::new()),
                    Json::Array(items) => {
                        let (from, to) = slice_bounds(items.len(), start, end);
                        Ok((from..to)
                            .map(|at| (Json::Number(at as f64), items[at].clone()))
                            .collect())
                    }
                    other => Err(EvalErr::runtime(
                        format!("Cannot index {} with object", other.type_name()),
                        position.0,
                        position.1,
                    )),
                }
            }
        }
    }

    /// Build the regex for a call, honouring an optional `flags` argument.
    ///
    /// The pattern may be a string, an object `{re, flags}` or an array
    /// `[re, flags]`; a second `;`-separated argument is the flags string and
    /// overrides the one carried by the pattern.
    fn regex_argument(
        &self,
        args: &[Expr],
        input: &Json,
        depth: usize,
    ) -> Result<RegexSpec, EvalErr> {
        let mut regex = compile_regex(&self.argument(args, 0, input, depth)?, false)?;
        if args.len() > 1 {
            let flags = as_string(&self.argument(args, 1, input, depth)?, 0, 0)?;
            apply_flags(&mut regex, &flags)?;
        }
        Ok(regex)
    }

    /// Evaluate one argument, failing when it produces no value.
    fn argument(
        &self,
        args: &[Expr],
        index: usize,
        input: &Json,
        depth: usize,
    ) -> Result<Json, EvalErr> {
        match args.get(index) {
            Some(expr) => self.eval_single(expr, input, depth + 1)?.ok_or_else(|| {
                EvalErr::runtime(format!("argument {} produced no value", index + 1), 0, 0)
            }),
            None => Err(EvalErr::runtime("missing argument", 0, 0)),
        }
    }

    /// Check that a call passes at most `max` arguments.
    fn expect_args(&self, args: &[Expr], max: usize, name: &str) -> Result<(), EvalErr> {
        if args.len() > max {
            return Err(EvalErr::runtime(
                format!(
                    "`{name}` expects at most {max} arguments, found {}",
                    args.len()
                ),
                0,
                0,
            ));
        }
        Ok(())
    }

    /// Evaluate a `sort_by`/`group_by`/`unique_by`/`min_by`/`max_by` key.
    fn key_of(&self, args: &[Expr], item: &Json, depth: usize) -> Result<Json, EvalErr> {
        match args.first() {
            None => Ok(item.clone()),
            Some(expr) => Ok(self
                .eval_single(expr, item, depth + 1)?
                .unwrap_or(Json::Null)),
        }
    }
}

/// Describe a value for `error`, `error(.)` and `tostring`-like messages.
fn describe_value(value: &Json) -> String {
    match value {
        Json::String(text) => text.clone(),
        other => other.compact(),
    }
}

/// Sort `(key, value)` pairs by key with jq's ordering; the sort is stable.
fn sort_pairs(mut keyed: Vec<(Vec<Json>, Json)>) -> Result<Vec<(Vec<Json>, Json)>, EvalErr> {
    let mut failure: Option<String> = None;
    keyed.sort_by(|a, b| {
        if failure.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match key_compare(&a.0, &b.0) {
            Ok(ordering) => ordering,
            Err(message) => {
                failure = Some(message);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(message) = failure {
        return Err(EvalErr::runtime(message, 0, 0));
    }
    Ok(keyed)
}

/// Sort `(key, value)` pairs and keep only the values.
fn sort_keyed(keyed: Vec<(Vec<Json>, Json)>) -> Result<Vec<Json>, EvalErr> {
    Ok(sort_pairs(keyed)?
        .into_iter()
        .map(|(_, value)| value)
        .collect())
}

/// Compare two key sequences, extending the shorter one with `null`.
fn key_compare(left: &[Json], right: &[Json]) -> Result<std::cmp::Ordering, String> {
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).unwrap_or(&Json::Null);
        let b = right.get(index).unwrap_or(&Json::Null);
        let ordering = compare_values(a, b, 0)?;
        if ordering != std::cmp::Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(std::cmp::Ordering::Equal)
}

/// Drop adjacent duplicates from a key-sorted list.
fn unique_keyed(sorted: Vec<(Vec<Json>, Json)>) -> Result<Vec<Json>, EvalErr> {
    let mut out: Vec<Json> = Vec::new();
    let mut previous: Option<Vec<Json>> = None;
    for (key, value) in sorted {
        let duplicate = match &previous {
            Some(previous) => {
                key_compare(previous, &key).map_err(|message| EvalErr::runtime(message, 0, 0))?
                    == std::cmp::Ordering::Equal
            }
            None => false,
        };
        if !duplicate {
            out.push(value);
            previous = Some(key);
        }
    }
    Ok(out)
}

/// Reverse jq ordering of two paths, for deleting right-most paths first.
fn compare_paths(left: &[Json], right: &[Json]) -> std::cmp::Ordering {
    let a = Json::Array(left.to_vec());
    let b = Json::Array(right.to_vec());
    compare_values(&a, &b, 0).unwrap_or(std::cmp::Ordering::Equal)
}

/// The first element of an array or the first character of a string.
fn first_value(value: &Json, line: usize, column: usize) -> Result<Option<Json>, EvalErr> {
    match value {
        Json::Array(items) => Ok(items.first().cloned()),
        Json::String(text) => Ok(text.chars().next().map(|c| Json::String(c.to_string()))),
        Json::Null => Ok(None),
        other => Err(EvalErr::runtime(
            format!("{} has no first element", other.type_name()),
            line,
            column,
        )),
    }
}

/// The last element of an array or the last character of a string.
fn last_value(value: &Json, line: usize, column: usize) -> Result<Option<Json>, EvalErr> {
    match value {
        Json::Array(items) => Ok(items.last().cloned()),
        Json::String(text) => Ok(text
            .chars()
            .next_back()
            .map(|c| Json::String(c.to_string()))),
        Json::Null => Ok(None),
        other => Err(EvalErr::runtime(
            format!("{} has no last element", other.type_name()),
            line,
            column,
        )),
    }
}

/// Require a value to be a path: an array of strings and numbers.
fn as_path(value: &Json, line: usize, column: usize) -> Result<Vec<Json>, EvalErr> {
    match value {
        Json::Array(items) => {
            for item in items {
                if !matches!(item, Json::String(_) | Json::Number(_)) {
                    return Err(EvalErr::runtime(
                        format!(
                            "path elements must be strings or numbers, found {}",
                            item.type_name()
                        ),
                        line,
                        column,
                    ));
                }
            }
            Ok(items.clone())
        }
        other => Err(EvalErr::runtime(
            format!("{} is not a valid path", other.type_name()),
            line,
            column,
        )),
    }
}

/// Follow a path, yielding `null` for parts that do not exist.
fn get_path(value: &Json, path: &[Json], line: usize, column: usize) -> Result<Json, EvalErr> {
    let mut current = value.clone();
    for key in path {
        current = match (key, &current) {
            (Json::String(name), Json::Object(entries)) => entries
                .iter()
                .find(|(existing, _)| existing == name)
                .map(|(_, item)| item.clone())
                .unwrap_or(Json::Null),
            (Json::String(_) | Json::Number(_), Json::Null) => Json::Null,
            (Json::Number(number), Json::Array(items)) => {
                let index = index_from_number(*number, (line, column))?;
                resolve_index(items.len(), index)
                    .and_then(|at| items.get(at).cloned())
                    .unwrap_or(Json::Null)
            }
            (Json::String(name), other) => {
                return Err(EvalErr::runtime(
                    format!("Cannot index {} with \"{name}\"", other.type_name()),
                    line,
                    column,
                ));
            }
            (Json::Number(_), other) => {
                return Err(EvalErr::runtime(
                    format!("Cannot index {} with a number", other.type_name()),
                    line,
                    column,
                ));
            }
            (other, _) => {
                return Err(EvalErr::runtime(
                    format!("invalid path element {}", other.type_name()),
                    line,
                    column,
                ));
            }
        };
    }
    Ok(current)
}

/// Rebuild a container with one path removed.
fn delete_path(value: &Json, path: &[Json]) -> Result<Json, EvalErr> {
    let (key, rest) = match path.split_first() {
        Some((key, rest)) => (key, rest),
        None => return Ok(value.clone()),
    };
    match (key, value) {
        (Json::String(name), Json::Object(entries)) => {
            let mut out: Vec<(String, Json)> = Vec::new();
            for (existing, item) in entries {
                if existing == name {
                    if rest.is_empty() {
                        continue;
                    }
                    out.push((existing.clone(), delete_path(item, rest)?));
                } else {
                    out.push((existing.clone(), item.clone()));
                }
            }
            Ok(Json::Object(out))
        }
        (Json::Number(number), Json::Array(items)) => {
            let index = index_from_number(*number, (0, 0))?;
            let mut out: Vec<Json> = Vec::new();
            for (at, item) in items.iter().enumerate() {
                if resolve_index(items.len(), index) == Some(at) {
                    if rest.is_empty() {
                        continue;
                    }
                    out.push(delete_path(item, rest)?);
                } else {
                    out.push(item.clone());
                }
            }
            Ok(Json::Array(out))
        }
        (Json::String(_), Json::Null) | (Json::Number(_), Json::Null) => Ok(Json::Null),
        (Json::String(name), other) => Err(EvalErr::runtime(
            format!("Cannot index {} with \"{name}\"", other.type_name()),
            0,
            0,
        )),
        (Json::Number(_), other) => Err(EvalErr::runtime(
            format!("Cannot index {} with a number", other.type_name()),
            0,
            0,
        )),
        (other, _) => Err(EvalErr::runtime(
            format!("invalid path element {}", other.type_name()),
            0,
            0,
        )),
    }
}

/// Rebuild a container with `value` stored at `path`, creating containers.
fn set_path(value: &Json, path: &[Json], new_value: &Json, depth: usize) -> Result<Json, EvalErr> {
    if depth > MAX_DEPTH {
        return Err(EvalErr::runtime(
            format!("path exceeds maximum nesting depth ({MAX_DEPTH})"),
            0,
            0,
        ));
    }
    let (key, rest) = match path.split_first() {
        Some((key, rest)) => (key, rest),
        None => return Ok(new_value.clone()),
    };
    if rest.is_empty() {
        return match (key, value) {
            (Json::String(name), Json::Object(entries)) => {
                let mut out = entries.clone();
                match out.iter_mut().find(|(existing, _)| existing == name) {
                    Some(slot) => slot.1 = new_value.clone(),
                    None => out.push((name.clone(), new_value.clone())),
                }
                Ok(Json::Object(out))
            }
            (Json::Number(number), Json::Array(items)) => {
                let index = index_from_number(*number, (0, 0))?;
                let mut out = items.clone();
                let at = if index < 0 {
                    let resolved = out.len() as i64 + index;
                    if resolved < 0 {
                        return Err(EvalErr::runtime("array index out of range", 0, 0));
                    }
                    resolved as usize
                } else {
                    index as usize
                };
                if at < out.len() {
                    out[at] = new_value.clone();
                } else if at == out.len() {
                    out.push(new_value.clone());
                } else {
                    return Err(EvalErr::runtime("array index out of range", 0, 0));
                }
                Ok(Json::Array(out))
            }
            (Json::String(name), Json::Null) => {
                Ok(Json::Object(vec![(name.clone(), new_value.clone())]))
            }
            (Json::Number(_), Json::Null) => Ok(Json::Array(vec![new_value.clone()])),
            (Json::String(name), other) => Err(EvalErr::runtime(
                format!("Cannot index {} with \"{name}\"", other.type_name()),
                0,
                0,
            )),
            (Json::Number(_), other) => Err(EvalErr::runtime(
                format!("Cannot index {} with a number", other.type_name()),
                0,
                0,
            )),
            (other, _) => Err(EvalErr::runtime(
                format!("invalid path element {}", other.type_name()),
                0,
                0,
            )),
        };
    }
    let child = match (key, value) {
        (Json::String(name), Json::Object(entries)) => entries
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, item)| item.clone())
            .unwrap_or(Json::Null),
        (Json::Number(number), Json::Array(items)) => {
            let index = index_from_number(*number, (0, 0))?;
            resolve_index(items.len(), index)
                .and_then(|at| items.get(at).cloned())
                .unwrap_or(Json::Null)
        }
        (_, Json::Null) => Json::Null,
        (Json::String(name), other) => {
            return Err(EvalErr::runtime(
                format!("Cannot index {} with \"{name}\"", other.type_name()),
                0,
                0,
            ));
        }
        (Json::Number(_), other) => {
            return Err(EvalErr::runtime(
                format!("Cannot index {} with a number", other.type_name()),
                0,
                0,
            ));
        }
        (other, _) => {
            return Err(EvalErr::runtime(
                format!("invalid path element {}", other.type_name()),
                0,
                0,
            ));
        }
    };
    let updated = set_path(&child, rest, new_value, depth + 1)?;
    set_path(value, &path[..1], &updated, depth + 1)
}

/// Collect every non-root path of a value.
fn collect_paths(
    value: &Json,
    prefix: &mut Vec<Json>,
    out: &mut Vec<Vec<Json>>,
    depth: usize,
) -> Result<(), EvalErr> {
    if depth > MAX_DEPTH {
        return Err(EvalErr::runtime(
            format!("value exceeds maximum nesting depth ({MAX_DEPTH})"),
            0,
            0,
        ));
    }
    if out.len() > MAX_VALUES {
        return Err(EvalErr::fatal(
            format!("too many values (limit {MAX_VALUES})"),
            0,
            0,
        ));
    }
    match value {
        Json::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                prefix.push(Json::Number(index as f64));
                out.push(prefix.clone());
                collect_paths(item, prefix, out, depth + 1)?;
                prefix.pop();
            }
        }
        Json::Object(entries) => {
            for (key, item) in entries {
                prefix.push(Json::String(key.clone()));
                out.push(prefix.clone());
                collect_paths(item, prefix, out, depth + 1)?;
                prefix.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

/// A regular expression plus the metadata needed to report named captures.
struct RegexSpec {
    /// The pattern with named groups rewritten to plain groups.
    pattern: String,
    /// The compiled pattern.
    regex: Regex,
    /// Capture-group names by group index; `None` for unnamed groups.
    names: Vec<Option<String>>,
    /// Whether `i` was requested.
    ignore_case: bool,
    /// Whether the `g` flag was requested.
    global: bool,
}

/// Compile a rewritten pattern into a [`RegexSpec`].
fn build_regex_spec(
    pattern: String,
    names: Vec<Option<String>>,
    ignore_case: bool,
    global: bool,
) -> Result<RegexSpec, EvalErr> {
    match Regex::new_with_flags(&pattern, ignore_case) {
        Ok(regex) => Ok(RegexSpec {
            pattern,
            regex,
            names,
            ignore_case,
            global,
        }),
        Err(message) => Err(EvalErr::runtime(
            format!("invalid regular expression: {message}"),
            0,
            0,
        )),
    }
}

/// Apply a jq flags string to a compiled spec.
///
/// Flags are additive, so this only recompiles when `i` changes the pattern.
fn apply_flags(spec: &mut RegexSpec, flags: &str) -> Result<(), EvalErr> {
    if flags.is_empty() {
        return Ok(());
    }
    let mut ignore_case = spec.ignore_case;
    let mut global = spec.global;
    for flag in flags.chars() {
        match flag {
            'i' => ignore_case = true,
            'g' => global = true,
            'x' | 'm' | 's' | 'n' | 'l' | 'p' => {
                return Err(EvalErr::runtime(
                    format!("unsupported regular expression flag `{flag}`"),
                    0,
                    0,
                ));
            }
            other => {
                return Err(EvalErr::runtime(
                    format!("unknown regular expression flag `{other}`"),
                    0,
                    0,
                ));
            }
        }
    }
    if ignore_case == spec.ignore_case {
        spec.global = global;
        Ok(())
    } else {
        *spec = build_regex_spec(
            spec.pattern.clone(),
            spec.names.clone(),
            ignore_case,
            global,
        )?;
        Ok(())
    }
}

/// Build a [`RegexSpec`] from a `{re: …, flags: …}` object, if that is what
/// `value` is.
fn try_regex_spec(value: &Json, _unused: bool) -> Result<Option<RegexSpec>, EvalErr> {
    match value {
        Json::Object(_) => compile_regex(value, true).map(Some),
        _ => Ok(None),
    }
}

/// Compile a regex argument: a pattern string, `{re, flags}` or `[re, flags]`.
fn compile_regex(value: &Json, global_default: bool) -> Result<RegexSpec, EvalErr> {
    let (pattern, flags) = match value {
        Json::String(pattern) => (pattern.clone(), String::new()),
        Json::Array(items) => {
            let pattern = match items.first() {
                Some(Json::String(text)) => text.clone(),
                Some(other) => {
                    return Err(EvalErr::runtime(
                        format!(
                            "the first element of a regular expression array must be a string, found {}",
                            other.type_name()
                        ),
                        0,
                        0,
                    ));
                }
                None => {
                    return Err(EvalErr::runtime(
                        "a regular expression array must start with a pattern string",
                        0,
                        0,
                    ));
                }
            };
            let flags = match items.get(1) {
                Some(Json::String(text)) => text.clone(),
                None => String::new(),
                Some(other) => {
                    return Err(EvalErr::runtime(
                        format!(
                            "the second element of a regular expression array must be a string, found {}",
                            other.type_name()
                        ),
                        0,
                        0,
                    ));
                }
            };
            (pattern, flags)
        }
        Json::Object(entries) => {
            let pattern = entries
                .iter()
                .find(|(key, _)| key == "re")
                .map(|(_, value)| value.clone())
                .unwrap_or(Json::Null);
            let pattern = match pattern {
                Json::String(text) => text,
                other => {
                    return Err(EvalErr::runtime(
                        format!(
                            "the `re` field must be a string, found {}",
                            other.type_name()
                        ),
                        0,
                        0,
                    ));
                }
            };
            let flags = entries
                .iter()
                .find(|(key, _)| key == "flags")
                .map(|(_, value)| value.clone())
                .unwrap_or(Json::Null);
            let flags = match flags {
                Json::String(text) => text,
                Json::Null => String::new(),
                other => {
                    return Err(EvalErr::runtime(
                        format!(
                            "the `flags` field must be a string, found {}",
                            other.type_name()
                        ),
                        0,
                        0,
                    ));
                }
            };
            (pattern, flags)
        }
        other => {
            return Err(EvalErr::runtime(
                format!(
                    "a regular expression must be a string, an object or an array, found {}",
                    other.type_name()
                ),
                0,
                0,
            ));
        }
    };

    let (rewritten, names) = rewrite_named_groups(&pattern);
    let mut spec = build_regex_spec(rewritten, names, false, global_default)?;
    apply_flags(&mut spec, &flags)?;
    Ok(spec)
}

/// Rewrite `(?<name>…)` and `(?P<name>…)` into plain groups, recording the
/// name of every capture group by index.
fn rewrite_named_groups(pattern: &str) -> (String, Vec<Option<String>>) {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len());
    let mut names: Vec<Option<String>> = Vec::new();
    let mut index = 0usize;
    let mut in_class = false;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' {
            out.push(character);
            if let Some(next) = chars.get(index + 1) {
                out.push(*next);
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if in_class {
            out.push(character);
            if character == ']' {
                in_class = false;
            }
            index += 1;
            continue;
        }
        if character == '[' {
            in_class = true;
            out.push(character);
            index += 1;
            continue;
        }
        if character != '(' {
            out.push(character);
            index += 1;
            continue;
        }
        let named = chars.get(index + 1) == Some(&'?')
            && (chars.get(index + 2) == Some(&'<')
                && chars
                    .get(index + 3)
                    .is_some_and(|next| *next != '=' && *next != '!')
                || (chars.get(index + 2) == Some(&'P') && chars.get(index + 3) == Some(&'<')));
        if !named {
            // A capture group starts unless this is `(?:`, `(?=`, `(?!`, `(?<`
            // lookbehind or a flag group.
            let capture = !matches!(chars.get(index + 1), Some('?'))
                || matches!(chars.get(index + 1), Some('?'))
                    && matches!(chars.get(index + 2), Some('P'))
                    && chars.get(index + 3) == Some(&'=');
            if capture {
                names.push(None);
            }
            out.push(character);
            index += 1;
            continue;
        }
        let name_start = if chars.get(index + 2) == Some(&'P') {
            index + 4
        } else {
            index + 3
        };
        names.push(None);
        out.push('(');
        let mut cursor = name_start;
        let mut name = String::new();
        while let Some(next) = chars.get(cursor) {
            if *next == '>' {
                break;
            }
            name.push(*next);
            cursor += 1;
        }
        if let Some(slot) = names.last_mut() {
            *slot = Some(name);
        }
        index = cursor + 1;
    }
    (out, names)
}

/// Find every match of `regex` in `text`, returning capture spans per match.
fn find_matches(regex: &RegexSpec, text: &str, global: bool) -> Vec<Vec<Option<(usize, usize)>>> {
    let mut out = Vec::new();
    let mut search = 0usize;
    while search <= text.len() {
        let haystack = match text.get(search..) {
            Some(haystack) => haystack,
            None => break,
        };
        let captures = match regex.regex.captures(haystack) {
            Some(captures) => captures,
            None => break,
        };
        let (start, end) = match captures.first().copied().flatten() {
            Some((start, end)) => (start + search, end + search),
            None => break,
        };
        let shifted: Vec<Option<(usize, usize)>> = captures
            .iter()
            .map(|span| span.map(|(a, b)| (a + search, b + search)))
            .collect();
        out.push(shifted);
        if out.len() > MAX_VALUES {
            break;
        }
        if !global {
            break;
        }
        if end == start {
            match text.get(end..).and_then(|rest| rest.chars().next()) {
                Some(character) => search = end + character.len_utf8(),
                None => break,
            }
        } else {
            search = end;
        }
    }
    out
}

/// Build the `match` object for one match.
/// Build the object `match` reports for one match.
///
/// `captures` holds the whole match at index 0 followed by one entry per
/// capture group; jq's `captures` array drops index 0, and
/// [`replacement_context`] is what re-adds the named groups for `sub`/`gsub`.
fn match_object(regex: &RegexSpec, text: &str, captures: &[Option<(usize, usize)>]) -> Json {
    let (offset, length, string) = match captures.first().copied().flatten() {
        Some((start, end)) => (
            start as f64,
            (end - start) as f64,
            Json::String(text.get(start..end).unwrap_or("").to_string()),
        ),
        None => (-1.0, 0.0, Json::Null),
    };
    let mut entries: Vec<Json> = Vec::new();
    for (index, span) in captures.iter().enumerate().skip(1) {
        let name = regex
            .names
            .get(index - 1)
            .cloned()
            .flatten()
            .map(Json::String)
            .unwrap_or(Json::Null);
        let (item_offset, item_length, item_string) = match span {
            Some((start, end)) => (
                *start as f64,
                (*end - *start) as f64,
                Json::String(text.get(*start..*end).unwrap_or("").to_string()),
            ),
            None => (-1.0, 0.0, Json::Null),
        };
        entries.push(Json::Object(vec![
            (String::from("offset"), Json::Number(item_offset)),
            (String::from("length"), Json::Number(item_length)),
            (String::from("string"), item_string),
            (String::from("name"), name),
        ]));
    }
    Json::Object(vec![
        (String::from("offset"), Json::Number(offset)),
        (String::from("length"), Json::Number(length)),
        (String::from("string"), string),
        (String::from("captures"), Json::Array(entries)),
    ])
}

/// The object a `sub`/`gsub` replacement filter sees.
///
/// It is the [`match_object`] plus one field per named group (`(?<x>…)` adds
/// `.x`), which is how a replacement refers to a named capture.
fn replacement_context(regex: &RegexSpec, text: &str, captures: &[Option<(usize, usize)>]) -> Json {
    let base = match_object(regex, text, captures);
    let Json::Object(mut entries) = base else {
        return base;
    };
    for (index, name) in regex.names.iter().enumerate() {
        let Some(name) = name else {
            continue;
        };
        let value = match captures.get(index + 1).copied().flatten() {
            Some((start, end)) => Json::String(text.get(start..end).unwrap_or("").to_string()),
            None => Json::Null,
        };
        if !entries.iter().any(|(key, _)| key == name) {
            entries.push((name.clone(), value));
        }
    }
    Json::Object(entries)
}

/// Compile and run a filter against one input document.
///
/// This is the testing entry point: it uses no variables beyond the ones the
/// filter binds itself, and it returns every value the filter produced.
///
/// # Errors
///
/// A compile error is `jq: error: MESSAGE at line L, column C`; a runtime
/// error is `jq: error: MESSAGE`.
pub fn run_filter(program: &str, input: &Json) -> Result<Vec<Json>, String> {
    let expr = FilterParser::parse(program).map_err(|error| error.render())?;
    let env: Vec<(String, Json)> = Vec::new();
    let evaluator = Evaluator::new(&env);
    evaluator
        .eval_values(&expr, input, 0)
        .map_err(|error| format!("jq: error: {}", error.message))
}

/// Options collected from the command line.
#[derive(Debug, Clone, Default)]
struct Options {
    /// `-r`: write strings without quotes.
    raw_output: bool,
    /// `-j`: like `-r`, without a trailing newline.
    join_output: bool,
    /// `-c`: one compact value per line.
    compact: bool,
    /// `-n`: run the filter once with `null` instead of reading input.
    null_input: bool,
    /// `-s`: read every document into one array.
    slurp: bool,
    /// `-e`: the exit status reflects the last output value.
    exit_status: bool,
    /// `-a`: escape non-ASCII characters.
    ascii: bool,
    /// Indentation width in spaces; `None` means pretty-print with two.
    indent: Option<usize>,
    /// `--tab`: indent with one tab per level.
    tab: bool,
    /// Variables from `--arg` and `--argjson`.
    vars: Vec<(String, Json)>,
    /// The filter operand.
    filter: Option<String>,
    /// The file operands.
    files: Vec<String>,
}

/// The message printed when the command line cannot be understood.
const USAGE: &str = "Usage: jq [options] filter [files...]";

/// Build a usage-error result.
fn usage_error(message: &str) -> JqRun {
    JqRun {
        stdout: String::new(),
        stderr: format!("jq: error: {message}\n{USAGE}\n"),
        exit_code: 2,
    }
}

/// Parse the operands that follow the command word.
fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut operands: Vec<String> = Vec::new();
    let mut index = 0usize;
    let mut no_more_options = false;

    while index < args.len() {
        let arg = args[index].as_str();
        if no_more_options {
            operands.push(arg.to_string());
            index += 1;
            continue;
        }
        match arg {
            "--" => no_more_options = true,
            "-r" | "--raw-output" => options.raw_output = true,
            "-j" | "--join-output" => {
                options.join_output = true;
                options.raw_output = true;
            }
            "-c" | "--compact-output" => options.compact = true,
            "-n" | "--null-input" => options.null_input = true,
            "-s" | "--slurp" => options.slurp = true,
            "-e" | "--exit-status" => options.exit_status = true,
            "-a" | "--ascii-output" => options.ascii = true,
            "--tab" => options.tab = true,
            "--indent" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--indent takes a number between 0 and 7".to_string())?;
                let width = value
                    .parse::<usize>()
                    .map_err(|_| "--indent takes a number between 0 and 7".to_string())?;
                if width > 7 {
                    return Err("--indent takes a number between 0 and 7".to_string());
                }
                options.indent = Some(width);
            }
            "--arg" => {
                let name = args
                    .get(index + 1)
                    .ok_or_else(|| "--arg takes two parameters (name and value)".to_string())?;
                let value = args
                    .get(index + 2)
                    .ok_or_else(|| "--arg takes two parameters (name and value)".to_string())?;
                options
                    .vars
                    .push((name.clone(), Json::String(value.clone())));
                index += 2;
            }
            "--argjson" => {
                let name = args.get(index + 1).ok_or_else(|| {
                    "--argjson takes two parameters (name and JSON text)".to_string()
                })?;
                let text = args.get(index + 2).ok_or_else(|| {
                    "--argjson takes two parameters (name and JSON text)".to_string()
                })?;
                let value = Json::parse(text).map_err(|message| {
                    format!("Invalid JSON text passed to --argjson: {message}")
                })?;
                options.vars.push((name.clone(), value));
                index += 2;
            }
            other if other.starts_with("--") => {
                return Err(format!("Unknown option `{other}`"));
            }
            // `jq -3`, `jq -.a`, `jq -"a"` and `jq -(1)` are filters, not
            // options: only a dash followed by a letter starts an option
            // cluster, so the dash is a minus sign everywhere else.
            other
                if other.starts_with('-')
                    && other.len() > 1
                    && !other.as_bytes()[1].is_ascii_alphabetic() =>
            {
                operands.push(other.to_string());
            }
            other if other.starts_with('-') && other.len() > 1 => {
                for flag in other.chars().skip(1) {
                    match flag {
                        'r' => options.raw_output = true,
                        'j' => {
                            options.join_output = true;
                            options.raw_output = true;
                        }
                        'c' => options.compact = true,
                        'n' => options.null_input = true,
                        's' => options.slurp = true,
                        'e' => options.exit_status = true,
                        'a' => options.ascii = true,
                        _ => return Err(format!("Unknown option `-{flag}`")),
                    }
                }
            }
            other => operands.push(other.to_string()),
        }
        index += 1;
    }

    let mut operands = operands.into_iter();
    options.filter = operands.next();
    options.files = operands.collect();
    Ok(options)
}

/// Append to standard output, failing once [`MAX_OUTPUT_BYTES`] is reached.
fn push_output(stdout: &mut String, text: &str) -> Result<(), String> {
    if stdout.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
        return Err(format!(
            "output exceeds the maximum of {MAX_OUTPUT_BYTES} bytes"
        ));
    }
    stdout.push_str(text);
    Ok(())
}

/// Read every document from one source, appending to `out`.
fn read_documents(text: &str, source: &str, out: &mut Vec<Json>) -> Result<(), String> {
    let mut parser = DocParser::new(text);
    loop {
        match parser.next_document() {
            Ok(None) => return Ok(()),
            Ok(Some(document)) => {
                if out.len() >= MAX_VALUES {
                    return Err(format!(
                        "jq: error (at {source}): too many input values (limit {MAX_VALUES})"
                    ));
                }
                out.push(document);
            }
            Err(message) => {
                return Err(format!("jq: error (at {source}): Parse error: {message}"));
            }
        }
    }
}

/// Render one output value as text, without the trailing newline.
fn render_value(value: &Json, options: &Options) -> Result<String, String> {
    if options.raw_output
        && let Json::String(text) = value
    {
        return Ok(text.clone());
    }
    if options.compact {
        return value.render(0, options.ascii);
    }
    if options.tab {
        return value
            .render(0, options.ascii)
            .map(|text| reindent_with_tabs(&text));
    }
    value.render(options.indent.unwrap_or(2), options.ascii)
}

/// Re-render pretty JSON using one tab per level.
///
/// The compact form is produced first and then re-indented, which keeps a
/// single indentation implementation for both spaces and tabs.
fn reindent_with_tabs(compact: &str) -> String {
    let mut out = String::with_capacity(compact.len() + 16);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for character in compact.chars() {
        if in_string {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                out.push(character);
            }
            '{' | '[' => {
                depth += 1;
                out.push(character);
                out.push('\n');
                for _ in 0..depth {
                    out.push('\t');
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                out.push('\n');
                for _ in 0..depth {
                    out.push('\t');
                }
                out.push(character);
            }
            ',' => {
                out.push(character);
                out.push('\n');
                for _ in 0..depth {
                    out.push('\t');
                }
            }
            ':' => {
                out.push_str(": ");
            }
            other => out.push(other),
        }
    }
    out
}

/// Run `jq`. See the [module documentation](self) for the supported flags,
/// filter subset, error messages and exit codes.
pub fn run(args: &[String], stdin: &str, read_file: &dyn Fn(&str) -> Option<String>) -> JqRun {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => return usage_error(&message),
    };
    let filter = match &options.filter {
        Some(filter) => filter.clone(),
        None => return usage_error("no filter given"),
    };

    // Compile before touching the input, so a bad filter is reported first.
    let expr = match FilterParser::parse(&filter) {
        Ok(expr) => expr,
        Err(error) => {
            return JqRun {
                stdout: String::new(),
                stderr: format!("{}\n", error.render()),
                exit_code: 3,
            };
        }
    };

    // Open every file operand, reporting the first missing one.
    let mut sources: Vec<(String, String)> = Vec::new();
    if options.files.is_empty() {
        sources.push(("<stdin>".to_string(), stdin.to_string()));
    } else {
        for name in &options.files {
            if name == "-" {
                sources.push(("<stdin>".to_string(), stdin.to_string()));
                continue;
            }
            match read_file(name) {
                Some(contents) => sources.push((name.clone(), contents)),
                None => {
                    return JqRun {
                        stdout: String::new(),
                        stderr: format!("jq: error: Could not open {name}\n"),
                        exit_code: 2,
                    };
                }
            }
        }
    }

    let mut inputs: Vec<Json> = Vec::new();
    if options.null_input {
        inputs.push(Json::Null);
    } else {
        for (name, contents) in &sources {
            if let Err(message) = read_documents(contents, name, &mut inputs) {
                return JqRun {
                    stdout: String::new(),
                    stderr: format!("{message}\n"),
                    exit_code: 5,
                };
            }
        }
        if options.slurp {
            let slurped = Json::Array(std::mem::take(&mut inputs));
            inputs.push(slurped);
        }
    }

    let env = options.vars.clone();
    let evaluator = Evaluator::new(&env);
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut last: Option<Json> = None;
    let mut produced_any = false;
    let mut exit_code = 0i32;

    'inputs: for (position, input) in inputs.iter().enumerate() {
        let values = match evaluator.eval_values(&expr, input, 0) {
            Ok(values) => values,
            Err(error) => {
                let source = sources
                    .get(position.min(sources.len().saturating_sub(1)))
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("<stdin>");
                let location = if error.line > 0 {
                    format!("{source}:{0}", error.line)
                } else {
                    source.to_string()
                };
                let _ = location;
                stderr.push_str(&format!("jq: error (at {source}): {}\n", error.message));
                exit_code = 5;
                break 'inputs;
            }
        };
        for value in values {
            if let Err(message) = value.check_depth() {
                stderr.push_str(&format!("jq: error (at <stdin>): {message}\n"));
                exit_code = 5;
                break 'inputs;
            }
            let rendered = match render_value(&value, &options) {
                Ok(rendered) => rendered,
                Err(message) => {
                    stderr.push_str(&format!("jq: error (at <stdin>): {message}\n"));
                    exit_code = 5;
                    break 'inputs;
                }
            };
            if let Err(message) = push_output(&mut stdout, &rendered) {
                stderr.push_str(&format!("jq: error (at <stdin>): {message}\n"));
                exit_code = 5;
                break 'inputs;
            }
            if !options.join_output && push_output(&mut stdout, "\n").is_err() {
                stderr.push_str(&format!(
                    "jq: error (at <stdin>): output exceeds the maximum of {MAX_OUTPUT_BYTES} bytes\n"
                ));
                exit_code = 5;
                break 'inputs;
            }
            produced_any = true;
            last = Some(value);
        }
    }

    if exit_code == 0 && options.exit_status {
        exit_code = match &last {
            None => 1,
            Some(value) => {
                if value.is_truthy() {
                    0
                } else {
                    4
                }
            }
        };
        let _ = produced_any;
    }

    JqRun {
        stdout,
        stderr,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a filter over one JSON document with no file operands.
    fn run_filter_cli(program: &str, input: &str) -> JqRun {
        let argv = vec!["-c".to_string(), program.to_string()];
        super::run(&argv, input, &|_| None)
    }

    /// Run a filter and require clean output; returns every output line.
    fn lines(program: &str, input: &str) -> String {
        let result = run_filter_cli(program, input);
        assert_eq!(
            result.exit_code, 0,
            "filter {program:?} on {input:?}: {}",
            result.stderr
        );
        assert!(
            result.stderr.is_empty(),
            "filter {program:?} on {input:?} wrote stderr: {}",
            result.stderr
        );
        result.stdout
    }

    /// The single compact value a filter produces.
    fn one(program: &str, input: &str) -> String {
        let out = lines(program, input);
        let text = out.strip_suffix('\n').unwrap_or(&out);
        assert!(
            !text.contains('\n'),
            "filter {program:?} on {input:?} produced {out:?}, expected one value"
        );
        text.to_string()
    }

    /// Expect a runtime failure (exit 5) whose message contains `needle`.
    fn fails(program: &str, input: &str, needle: &str) {
        let result = run_filter_cli(program, input);
        assert_eq!(
            result.exit_code, 5,
            "filter {program:?} on {input:?}: rc {} {}",
            result.exit_code, result.stderr
        );
        assert!(
            result.stderr.contains(needle),
            "filter {program:?} on {input:?} said {:?}, expected {needle:?}",
            result.stderr
        );
    }

    /// Expect a compile failure (exit 3).
    fn compile_fails(program: &str) {
        let result = run_filter_cli(program, "null");
        assert_eq!(
            result.exit_code, 3,
            "filter {program:?}: rc {} {}",
            result.exit_code, result.stderr
        );
        assert!(
            result.stderr.contains("jq: error:"),
            "filter {program:?}: {:?}",
            result.stderr
        );
    }

    // ---- paths, getpath/setpath, del ------------------------------------

    #[test]
    fn paths_reports_raw_components() {
        assert_eq!(one("[paths]", r#"{"a":{"b":1}}"#), r#"[["a"],["a","b"]]"#);
        assert_eq!(one("[paths]", r#"{"a b":1}"#), r#"[["a b"]]"#);
        assert_eq!(one("[paths]", r#"[1,[2]]"#), "[[0],[1],[1,0]]");
    }

    #[test]
    fn leaf_paths_skips_containers() {
        assert_eq!(one("[leaf_paths]", r#"{"a":{"b":1}}"#), r#"[["a","b"]]"#);
        assert_eq!(one("[leaf_paths]", r#"[1,[2]]"#), "[[0],[1,0]]");
    }

    #[test]
    fn paths_takes_a_predicate() {
        assert_eq!(
            one("[paths(type == \"number\")]", r#"{"a":1,"b":"x"}"#),
            r#"[["a"]]"#
        );
        assert_eq!(one("[paths(. > 1)]", "[1,2,3]"), "[[1],[2]]");
    }

    #[test]
    fn getpath_reads_a_path() {
        assert_eq!(one(r#"getpath(["a","b"])"#, r#"{"a":{"b":7}}"#), "7");
        assert_eq!(one(r#"getpath(["nope"])"#, "{}"), "null");
        assert_eq!(one(r#"getpath([0])"#, "[9]"), "9");
    }

    #[test]
    fn setpath_writes_a_path() {
        assert_eq!(one(r#"setpath(["a"]; 9)"#, "{}"), r#"{"a":9}"#);
        assert_eq!(one(r#"setpath(["a","b"]; 1)"#, "{}"), r#"{"a":{"b":1}}"#);
        assert_eq!(one(r#"setpath([1]; 9)"#, "[1,2,3]"), "[1,9,3]");
        assert_eq!(one(r#"setpath([3]; 9)"#, "[1,2,3]"), "[1,2,3,9]");
    }

    #[test]
    fn setpath_rejects_bad_paths() {
        fails(r#"setpath("a"; 1)"#, "{}", "is not a valid path");
        fails(
            r#"getpath([true])"#,
            "{}",
            "path elements must be strings or numbers",
        );
    }

    #[test]
    fn del_removes_one_field() {
        assert_eq!(one("del(.a)", r#"{"a":1,"b":2}"#), r#"{"b":2}"#);
        assert_eq!(one("del(.missing)", r#"{"a":1}"#), r#"{"a":1}"#);
        fails("del(.a)", "[1,2,3]", r#"Cannot index array with "a""#);
        assert_eq!(one("del(.a)", "null"), "null");
    }

    #[test]
    fn del_accepts_a_comma_stream() {
        assert_eq!(one("del(.a, .b)", r#"{"a":1,"b":2,"c":3}"#), r#"{"c":3}"#);
        assert_eq!(one("del(.[0], .[2])", "[1,2,3]"), "[2]");
        assert_eq!(one("del(.a, .a)", r#"{"a":1}"#), "{}");
    }

    #[test]
    fn del_removes_indexes_and_slices() {
        assert_eq!(one("del(.[1])", "[1,2,3]"), "[1,3]");
        assert_eq!(one("del(.[-1])", "[1,2,3]"), "[1,2]");
        assert_eq!(one("del(.[5])", "[1,2,3]"), "[1,2,3]");
        assert_eq!(one("del(.[1:3])", "[1,2,3,4]"), "[1,4]");
        assert_eq!(one("del(.[])", r#"{"a":1}"#), "{}");
    }

    #[test]
    fn del_removes_nested_paths() {
        assert_eq!(
            one("del(.a.b)", r#"{"a":{"b":1,"c":2}}"#),
            r#"{"a":{"c":2}}"#
        );
        assert_eq!(one("del(.a[0])", r#"{"a":[1,2]}"#), r#"{"a":[2]}"#);
    }

    #[test]
    fn del_handles_optional_and_empty() {
        assert_eq!(one("del(.a?)", "[1,2,3]"), "[1,2,3]");
        assert_eq!(one("del(empty)", "[1,2,3]"), "[1,2,3]");
        assert_eq!(one("del(select(false))", "[1,2,3]"), "[1,2,3]");
        assert_eq!(one("del(.[] | select(. > 1))", "[1,2,3]"), "[1]");
    }

    #[test]
    fn del_rejects_non_locations() {
        fails("del(1)", "{}", "Invalid path expression");
        fails(
            "del(.a | .b)",
            r#"{"a":1}"#,
            r#"Cannot index number with "b""#,
        );
        fails("del(.[])", "null", "Cannot iterate over null");
    }

    // ---- has / in ------------------------------------------------------

    #[test]
    fn has_tests_container_membership() {
        assert_eq!(one(r#"has("a")"#, r#"{"a":1}"#), "true");
        assert_eq!(one(r#"has("z")"#, r#"{"a":1}"#), "false");
        assert_eq!(one("has(0)", "[1,2]"), "true");
        assert_eq!(one("has(9)", "[1,2]"), "false");
        assert_eq!(one(r#"has("a")"#, "null"), "false");
    }

    #[test]
    fn in_tests_membership_from_the_key_side() {
        assert_eq!(one(r#"in({"a":1})"#, r#""a""#), "true");
        assert_eq!(one(r#"in({"a":1})"#, r#""z""#), "false");
        assert_eq!(one("in([1,2,3])", "0"), "true");
        assert_eq!(one("in([1,2,3])", "5"), "false");
        fails(
            r#".a | in({"a":1})"#,
            r#"{"a":1}"#,
            "Cannot check whether object has a number key",
        );
    }

    #[test]
    fn in_and_has_reject_mismatched_key_types() {
        fails(
            r#"in({"a":1})"#,
            "null",
            "Cannot check whether object has a null key",
        );
        fails(
            r#"has(1)"#,
            r#"{"a":1}"#,
            "Cannot check whether object has a number key",
        );
        fails(
            r#"has("a")"#,
            "[1,2]",
            "Cannot check whether array has a string key",
        );
    }

    // ---- add / any / all ------------------------------------------------

    #[test]
    fn add_folds_arrays_and_objects() {
        assert_eq!(one("add", "[1,2,3]"), "6");
        assert_eq!(one("add", "[]"), "null");
        assert_eq!(one("add", r#"{"a":1,"b":2}"#), "3");
        assert_eq!(one("add", "{}"), "null");
        assert_eq!(one("add", r#"[{"a":1},{"b":2}]"#), r#"{"a":1,"b":2}"#);
        assert_eq!(one("add", "[[1,2],[3]]"), "[1,2,3]");
        assert_eq!(one("add", r#"["a","b"]"#), r#""ab""#);
    }

    #[test]
    fn add_requires_an_iterable() {
        fails("add", "null", "Cannot iterate over null");
        fails("add", "5", "Cannot iterate over number");
    }

    #[test]
    fn any_and_all_iterate_objects_too() {
        assert_eq!(one("any", "[false,true]"), "true");
        assert_eq!(one("any", "[false,false]"), "false");
        assert_eq!(one("all", "[true,true]"), "true");
        assert_eq!(one("all", r#"{"a":true,"b":false}"#), "false");
        assert_eq!(one("any", r#"{"a":true}"#), "true");
        assert_eq!(one("any(. > 1)", "[1,2]"), "true");
        assert_eq!(one("all(. > 0)", "[1,2]"), "true");
    }

    // ---- foreach --------------------------------------------------------

    #[test]
    fn foreach_emits_the_updated_state() {
        assert_eq!(one("[foreach .[] as $x (0; . + $x)]", "[1,2]"), "[1,3]");
        assert_eq!(
            lines("foreach .[] as $x (0; . + $x)", "[1,2,3]"),
            "1\n3\n6\n"
        );
    }

    #[test]
    fn foreach_accepts_an_extract_filter() {
        assert_eq!(
            one("[foreach .[] as $x (0; . + $x; . * 10)]", "[1,2]"),
            "[10,30]"
        );
        assert_eq!(one("[foreach .[] as $x (0; . + $x; $x)]", "[1,2]"), "[1,2]");
    }

    #[test]
    fn reduce_accumulates() {
        assert_eq!(one("reduce .[] as $x (0; . + $x)", "[1,2,3]"), "6");
        assert_eq!(one("reduce .[] as $x ([]; . + [$x])", "[1,2]"), "[1,2]");
    }

    // ---- optional iteration --------------------------------------------

    #[test]
    fn optional_iterate_swallows_type_errors() {
        assert_eq!(one("[.[]?]", "5"), "[]");
        assert_eq!(one("[.[]?]", "null"), "[]");
        assert_eq!(one("[.[]?]", r#""ab""#), "[]");
        assert_eq!(one("[.[]?]", "[1,2]"), "[1,2]");
        assert_eq!(one("[.[]?]", r#"{"a":1}"#), "[1]");
    }

    #[test]
    fn plain_iterate_reports_type_errors() {
        fails(".[]", "5", "Cannot iterate over number");
        fails(".[]", "null", "Cannot iterate over null");
    }

    #[test]
    fn optional_field_swallows_type_errors() {
        assert_eq!(lines(".a?", "[1,2]"), "");
        assert_eq!(lines(".a?", r#""x""#), "");
        assert_eq!(one(".a?", r#"{"a":1}"#), "1");
    }

    // ---- regex builtins -------------------------------------------------

    #[test]
    fn test_matches_a_pattern() {
        assert_eq!(one(r#"test("^a")"#, r#""abc""#), "true");
        assert_eq!(one(r#"test("^A")"#, r#""abc""#), "false");
        assert_eq!(one(r#"test("^A"; "i")"#, r#""abc""#), "true");
        assert_eq!(one(r#"test({re: "A", flags: "i"})"#, r#""abc""#), "true");
        assert_eq!(one(r#"test(["A", "i"])"#, r#""abc""#), "true");
    }

    #[test]
    fn match_reports_one_match_by_default() {
        assert_eq!(
            one(r#"match("a")"#, r#""aa""#),
            r#"{"offset":0,"length":1,"string":"a","captures":[]}"#
        );
        assert_eq!(
            one(r#"[match("a"; "g")]"#, r#""aa""#),
            r#"[{"offset":0,"length":1,"string":"a","captures":[]},{"offset":1,"length":1,"string":"a","captures":[]}]"#
        );
        assert_eq!(
            lines(r#"match("a")"#, r#""aa""#),
            "{\"offset\":0,\"length\":1,\"string\":\"a\",\"captures\":[]}\n"
        );
    }

    #[test]
    fn match_captures_exclude_the_whole_match() {
        assert_eq!(
            one(r#"match("(a)(b)")"#, r#""xaby""#),
            r#"{"offset":1,"length":2,"string":"ab","captures":[{"offset":1,"length":1,"string":"a","name":null},{"offset":2,"length":1,"string":"b","name":null}]}"#
        );
        assert_eq!(
            one(r#"match("(?<x>a)")"#, r#""ab""#),
            r#"{"offset":0,"length":1,"string":"a","captures":[{"offset":0,"length":1,"string":"a","name":"x"}]}"#
        );
    }

    #[test]
    fn match_handles_non_participating_groups() {
        assert_eq!(
            one(r#"match("(a)(z)?")"#, r#""ab""#),
            r#"{"offset":0,"length":1,"string":"a","captures":[{"offset":0,"length":1,"string":"a","name":null},{"offset":-1,"length":0,"string":null,"name":null}]}"#
        );
    }

    #[test]
    fn capture_builds_an_object_of_named_groups() {
        assert_eq!(
            one(r#"capture("(?<x>a)(?<y>b)")"#, r#""ab""#),
            r#"{"x":"a","y":"b"}"#
        );
        assert_eq!(one(r#"capture("(?<x>z)")"#, r#""ab""#), "");
        assert_eq!(
            one(r#"capture("(?<x>a)(?<y>z)?")"#, r#""ab""#),
            r#"{"x":"a","y":null}"#
        );
    }

    #[test]
    fn sub_and_gsub_replace_text() {
        assert_eq!(one(r#"sub("a"; "z")"#, r#""aaa""#), r#""zaa""#);
        assert_eq!(one(r#"gsub("a"; "z")"#, r#""aaa""#), r#""zzz""#);
        assert_eq!(one(r#"gsub("A"; "z"; "i")"#, r#""aaa""#), r#""zzz""#);
        assert_eq!(one(r#"gsub("a"; "z"; "g")"#, r#""aaa""#), r#""zzz""#);
    }

    #[test]
    fn sub_replacement_sees_named_captures() {
        assert_eq!(one(r#"sub("(?<x>a)"; .x)"#, r#""ab""#), r#""ab""#);
        assert_eq!(
            one(r#"sub("(?<x>[a-z]+)-"; "\(.x)!")"#, r#""a-b""#),
            r#""a!b""#
        );
        assert_eq!(
            one(r#"sub("(?<x>a)"; (.x | ascii_upcase))"#, r#""ab""#),
            r#""Ab""#
        );
        assert_eq!(
            one(r#"sub("(a)"; .captures[0].string)"#, r#""ab""#),
            r#""ab""#
        );
    }

    #[test]
    fn splits_uses_a_regular_expression() {
        assert_eq!(one(r#"[splits(",")]"#, r#""a,b""#), r#"["a","b"]"#);
        assert_eq!(one(r#"[splits("[,;]")]"#, r#""a,b;c""#), r#"["a","b","c"]"#);
        assert_eq!(one(r#"[splits("A"; "i")]"#, r#""xay""#), r#"["x","y"]"#);
    }

    #[test]
    fn regex_builtins_report_bad_flags() {
        fails(
            r#"test("a"; "q")"#,
            r#""a""#,
            "unknown regular expression flag",
        );
        fails(
            r#"test("a"; "m")"#,
            r#""a""#,
            "unsupported regular expression flag",
        );
        fails(r#"test("(")"#, r#""a""#, "invalid regular expression");
        fails(r#"test(5)"#, r#""a""#, "a regular expression must be");
    }

    // ---- parser: postfix after a term, CLI options ----------------------

    #[test]
    fn an_array_literal_can_be_indexed() {
        assert_eq!(one("[1,2][0]", "null"), "1");
        assert_eq!(one("[1,2][1]", "null"), "2");
        assert_eq!(one("[[1]][0][0]", "null"), "1");
        assert_eq!(one("[1,2][1:]", "null"), "[2]");
    }

    #[test]
    fn an_object_literal_can_be_indexed() {
        assert_eq!(one("{a:1}.a", "null"), "1");
        assert_eq!(one(r#"{a:1}["a"]"#, "null"), "1");
        assert_eq!(one("{a:1}.b", "null"), "null");
    }

    #[test]
    fn binary_minus_is_not_a_negative_index() {
        assert_eq!(one("1-1", "null"), "0");
        assert_eq!(one("5-1", "null"), "4");
        assert_eq!(one("10-2-3", "null"), "5");
        assert_eq!(one(".[-1]", "[1,2,3]"), "3");
    }

    #[test]
    fn brackets_build_arrays() {
        assert_eq!(one("[1,2]", "null"), "[1,2]");
        assert_eq!(one("[]", "null"), "[]");
        assert_eq!(one("[]|add", "null"), "null");
        assert_eq!(one("[[]]", "null"), "[[]]");
        assert_eq!(one(".", "[0]"), "[0]");
    }

    #[test]
    fn slices_parse() {
        assert_eq!(one(".[1:3]", "[1,2,3,4]"), "[2,3]");
        assert_eq!(one("[.[2:]]", "[1,2,3,4]"), "[[3,4]]");
        assert_eq!(one("[.[:2]]", "[1,2,3,4]"), "[[1,2]]");
        assert_eq!(one("[.[-2:]]", "[1,2,3,4]"), "[[3,4]]");
        assert_eq!(one("[.[3:1]]", "[1,2,3,4]"), "[[]]");
    }

    #[test]
    fn arguments_allow_the_comma_operator() {
        assert_eq!(one("del(.a, .b)", r#"{"a":1,"b":2}"#), "{}");
        assert_eq!(one("map(.a, .b)", r#"[{"a":1,"b":2}]"#), "[1,2]");
        assert_eq!(one("map(.a, .b)", r#"[{"a":1,"b":2}]"#), "[1,2]");
    }

    #[test]
    fn a_leading_number_filter_is_not_an_option() {
        assert_eq!(one("-3", "null"), "-3");
        assert_eq!(one("-.a", r#"{"a":1}"#), "-1");
        assert_eq!(render_cli(&["-c", "-3"], "null").exit_code, 0);
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        let result = render_cli(&["-Z", "."], "null");
        assert_eq!(result.exit_code, 2);
        assert!(
            result.stderr.contains("Unknown option `-Z`"),
            "{}",
            result.stderr
        );
        let result = render_cli(&["--bogus", "."], "null");
        assert_eq!(result.exit_code, 2);
        assert!(
            result.stderr.contains("Unknown option `--bogus`"),
            "{}",
            result.stderr
        );
    }

    /// Invoke the CLI with an explicit argument vector.
    fn render_cli(args: &[&str], input: &str) -> JqRun {
        let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        super::run(&owned, input, &|_| None)
    }

    #[test]
    fn division_and_remainder_by_zero_report_jq_messages() {
        fails(
            "1/0",
            "null",
            "cannot be divided because the divisor is zero",
        );
        fails(
            "1%0",
            "null",
            "cannot be divided (remainder) because the divisor is zero",
        );
        assert_eq!(
            one("try (1/0) catch .", "null"),
            r#""number (1) and number (0) cannot be divided because the divisor is zero""#
        );
    }

    // ---- core evaluation ------------------------------------------------

    #[test]
    fn identity_and_field_access() {
        assert_eq!(one(".", r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(one(".a", r#"{"a":{"b":2}}"#), r#"{"b":2}"#);
        assert_eq!(one(".a.b", r#"{"a":{"b":2}}"#), "2");
        assert_eq!(one(".missing", "{}"), "null");
        assert_eq!(one(r#"."a b""#, r#"{"a b":1}"#), "1");
        assert_eq!(one(r#".["a b"]"#, r#"{"a b":1}"#), "1");
    }

    #[test]
    fn indexing_and_iteration() {
        assert_eq!(one(".[0]", "[1,2]"), "1");
        assert_eq!(one(".[-1]", "[1,2]"), "2");
        assert_eq!(one(".[9]", "[1,2]"), "null");
        assert_eq!(one("[.[]]", "[1,2]"), "[1,2]");
        assert_eq!(one("[.[]]", r#"{"a":1,"b":2}"#), "[1,2]");
        fails(".[0]", r#""ab""#, "Cannot index string with a number");
    }

    #[test]
    fn arithmetic_and_comparison() {
        assert_eq!(one("1+2", "null"), "3");
        assert_eq!(one("2*3", "null"), "6");
        assert_eq!(one("7/2", "null"), "3.5");
        assert_eq!(one("7%3", "null"), "1");
        assert_eq!(one(r#""a"+"b""#, "null"), r#""ab""#);
        assert_eq!(one("[1]+[2]", "null"), "[1,2]");
        assert_eq!(one(r#"{"a":1}+{"b":2}"#, "null"), r#"{"a":1,"b":2}"#);
        assert_eq!(one(r#""ab"*2"#, "null"), r#""abab""#);
        assert_eq!(one("[1,2]-[1]", "null"), "[2]");
        assert_eq!(one("1 < 2", "null"), "true");
        assert_eq!(one("null < false", "null"), "true");
    }

    #[test]
    fn logic_and_alternative() {
        assert_eq!(one("true and false", "null"), "false");
        assert_eq!(one("true or false", "null"), "true");
        assert_eq!(one("true|not", "null"), "false");
        assert_eq!(one(r#"null // "d""#, "null"), r#""d""#);
        assert_eq!(one("false // 1", "null"), "1");
        assert_eq!(one("1 // 2", "null"), "1");
    }

    #[test]
    fn conditionals_and_errors() {
        assert_eq!(one("if true then 1 else 2 end", "null"), "1");
        assert_eq!(one("if null then 1 else 2 end", "null"), "2");
        assert_eq!(one("if 1 then 2 end", "null"), "2");
        assert_eq!(one(r#"try error("x") catch "c""#, "null"), r#""c""#);
        assert_eq!(one("try (1/0) catch \"caught\"", "null"), r#""caught""#);
        assert_eq!(one("[.[]|tonumber?]", r#"["1","x","2"]"#), "[1,2]");
    }

    #[test]
    fn bindings_and_variables() {
        assert_eq!(one(". as $x | $x", "1"), "1");
        assert_eq!(one("1 as $a | 2 as $b | $a + $b", "null"), "3");
        assert_eq!(one("[1,2] | .[0] as $a | $a", "null"), "1");
    }

    #[test]
    fn constants_and_type_names() {
        assert_eq!(one("type", "null"), r#""null""#);
        assert_eq!(one("type", "1"), r#""number""#);
        assert_eq!(one("type", r#""a""#), r#""string""#);
        assert_eq!(one("type", "[]"), r#""array""#);
        assert_eq!(one("type", "{}"), r#""object""#);
        assert_eq!(one("type", "true"), r#""boolean""#);
        assert_eq!(one("1|tostring", "null"), r#""1""#);
        assert_eq!(one(r#""3"|tonumber"#, "null"), "3");
        assert_eq!(one(r#"{"a":1}|tojson"#, "null"), r#""{\"a\":1}""#);
    }

    // ---- builtins: collections -----------------------------------------

    #[test]
    fn keys_and_entries() {
        assert_eq!(one("keys", r#"{"b":1,"a":2}"#), r#"["a","b"]"#);
        assert_eq!(one("keys_unsorted", r#"{"b":1,"a":2}"#), r#"["b","a"]"#);
        assert_eq!(one("keys", "[5,6]"), "[0,1]");
        assert_eq!(
            one("to_entries", r#"{"a":1}"#),
            r#"[{"key":"a","value":1}]"#
        );
        assert_eq!(
            one("from_entries", r#"[{"key":"a","value":1}]"#),
            r#"{"a":1}"#
        );
        assert_eq!(one("values", r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(lines("values", "null"), "");
        assert_eq!(one("values", "[1,2]"), "[1,2]");
    }

    #[test]
    fn lengths_and_string_conversions() {
        assert_eq!(one("length", "null"), "0");
        assert_eq!(one("length", "-3"), "3");
        assert_eq!(one("length", r#""abc""#), "3");
        assert_eq!(one("length", "[1,2]"), "2");
        assert_eq!(one("length", r#"{"a":1}"#), "1");
        assert_eq!(one("utf8bytelength", r#""é""#), "2");
        assert_eq!(one("explode", r#""hi""#), "[104,105]");
        assert_eq!(one("implode", "[104,105]"), r#""hi""#);
        assert_eq!(one("ascii_upcase", r#""hi""#), r#""HI""#);
        assert_eq!(one("ascii_downcase", r#""HI""#), r#""hi""#);
    }

    #[test]
    fn sorting_and_selection() {
        assert_eq!(one("sort", "[3,1,2]"), "[1,2,3]");
        assert_eq!(one("unique", "[1,1,2]"), "[1,2]");
        assert_eq!(one("reverse", "[1,2,3]"), "[3,2,1]");
        assert_eq!(one("first", "[1,2]"), "1");
        assert_eq!(one("last", "[1,2]"), "2");
        assert_eq!(one("min", "[3,1]"), "1");
        assert_eq!(one("max", "[3,1]"), "3");
        assert_eq!(one("min_by(.n)", r#"[{"n":3},{"n":1}]"#), r#"{"n":1}"#);
        assert_eq!(one("max_by(.n)", r#"[{"n":3},{"n":1}]"#), r#"{"n":3}"#);
        assert_eq!(
            one("sort_by(.n)", r#"[{"n":2},{"n":1}]"#),
            r#"[{"n":1},{"n":2}]"#
        );
        assert_eq!(
            one("group_by(.n)", r#"[{"n":1},{"n":2},{"n":1}]"#),
            r#"[[{"n":1},{"n":1}],[{"n":2}]]"#
        );
        assert_eq!(one("unique_by(.n)", r#"[{"n":1},{"n":1}]"#), r#"[{"n":1}]"#);
    }

    #[test]
    fn map_limit_and_nth() {
        assert_eq!(one("[.[]|.*2]", "[1,2,3]"), "[2,4,6]");
        assert_eq!(one("map(.+1)", "[1,2]"), "[2,3]");
        assert_eq!(one("map_values(.+1)", r#"{"a":1}"#), r#"{"a":2}"#);
        assert_eq!(one("[limit(2; .[])]", "[1,2,3,4]"), "[1,2]");
        assert_eq!(one("nth(1)", "[1,2,3]"), "2");
        assert_eq!(one("nth(0; .[])", "[4,5]"), "4");
        assert_eq!(one("[.[]|select(.>1)]", "[1,2,3]"), "[2,3]");
    }

    #[test]
    fn string_builtins() {
        assert_eq!(one(r#"join("-")"#, r#"["a","b"]"#), r#""a-b""#);
        assert_eq!(one(r#"split(",")"#, r#""a,b""#), r#"["a","b"]"#);
        assert_eq!(one(r#"ltrimstr("a")"#, r#""abc""#), r#""bc""#);
        assert_eq!(one(r#"rtrimstr("c")"#, r#""abc""#), r#""ab""#);
        assert_eq!(one(r#"startswith("a")"#, r#""abc""#), "true");
        assert_eq!(one(r#"endswith("c")"#, r#""abc""#), "true");
        assert_eq!(one(r#""abc"/"b""#, "null"), r#"["a","c"]"#);
    }

    #[test]
    fn math_builtins() {
        assert_eq!(one("floor", "1.7"), "1");
        assert_eq!(one("ceil", "1.2"), "2");
        assert_eq!(one("round", "1.5"), "2");
        assert_eq!(one("sqrt", "9"), "3");
        assert_eq!(one("fabs", "-3"), "3");
    }

    #[test]
    fn string_interpolation_is_supported() {
        assert_eq!(one(r#""x\(1+1)y""#, "null"), r#""x2y""#);
        assert_eq!(one(r#""\(.a)-\(.a)""#, r#"{"a":1}"#), r#""1-1""#);
        assert_eq!(one(r#"{k:"\(.a)"}"#, r#"{"a":1}"#), r#"{"k":"1"}"#);
        assert_eq!(one(r#"{(.a): 1}"#, r#"{"a":"k"}"#), r#"{"k":1}"#);
    }

    #[test]
    fn unsupported_constructs_are_compile_errors() {
        for program in [
            "def f: 1; f",
            "[..]",
            "@base64",
            "walk(.)",
            "path(.a)",
            "recurse",
            "ascii(65)",
            "input",
            "debug",
            "label $x | 1",
            ".a |= 1",
            ".a = 1",
            "nosuchfunction",
        ] {
            compile_fails(program);
        }
    }

    #[test]
    fn limits_are_enforced() {
        // A filter nested far past the limit is rejected while parsing.
        let deep = format!("{}1{}", "(".repeat(400), ")".repeat(400));
        compile_fails(&deep);
        let wide = vec!["1"; 4000].join(" + ");
        compile_fails(&wide);
        assert!(one(&vec!["1"; 40].join(" + "), "null") == "40");
    }

    #[test]
    fn comments_run_to_end_of_line() {
        assert_eq!(one("# a comment\n1", "null"), "1");
        assert_eq!(one("1 # trailing", "null"), "1");
    }
}
