//! A small, dependency-free `awk` implementation (GNU-compatible subset).
//!
//! The runtime treats scripts as untrusted input, so this module follows the
//! same rules as the rest of `just-bash`:
//!
//! * **Bounded work.** Total input records, executed statements/loop
//!   iterations and accumulated standard output are all capped (see
//!   [`MAX_RECORDS`], [`MAX_STEPS`] and [`MAX_OUTPUT_BYTES`]). Exceeding a bound
//!   is a clean error, never a hang or an allocation spike.
//! * **No panics, no `unsafe`.** Every failure path is a `awk: …` diagnostic on
//!   stderr with exit status 2. Malformed programs, missing files, division by
//!   zero, bad regular expressions and unsupported constructs are all errors;
//!   nothing is a silent no-op.
//! * **Deterministic output.** Floating point is `f64`, `rand()` is a fixed-seed
//!   LCG owned by the interpreter, and `for (k in a)` walks array keys in sorted
//!   order instead of hash order.
//!
//! # Supported language
//!
//! Program structure: `BEGIN { … }`, `END { … }`, `pattern { action }`,
//! `{ action }`, `/re/ { … }`, `pattern1, pattern2 { … }` ranges and bare
//! expressions as patterns (a rule without an action prints `$0`). `;` and
//! newlines separate rules and statements, `#` starts a comment.
//!
//! Expressions: `$0`…`$N`, `NF`, `NR`, `FNR`, `FS`, `OFS`, `ORS`, `RS`,
//! `FILENAME`, `SUBSEP`, `RSTART`, `RLENGTH`, numbers (decimal and `0x` hex),
//! strings with escape processing, regular expression literals, assignment
//! (including `+= -= *= /= %= ^=`), `?:`, `||`, `&&`, `in`, `~`, `!~`,
//! comparisons, concatenation by juxtaposition, arithmetic, unary `+ - !`,
//! prefix/postfix `++`/`--` and `( )` grouping. Arrays support `a[i]`,
//! `a[i,j]`, `delete a[i]`, `delete a`, `(i in a)` and `for (k in a)`.
//!
//! Statements: `print`, `printf`, `if`/`else`, `while`, `do … while`,
//! `for (init; cond; post)`, `for (k in a)`, `break`, `continue`, `next`,
//! `nextfile` (treated as `next`), `exit [code]`, `{ … }` and `;`.
//!
//! Built-ins: `length`, `substr`, `index`, `split`, `sub`, `gsub`, `match`,
//! `sprintf`, `tolower`, `toupper`, `int`, `sqrt`, `exp`, `log`, `sin`, `cos`,
//! `atan2`, `rand` and `srand`.
//!
//! # Unsupported constructs (all are errors, exit status 2)
//!
//! `getline`, `system()`, user-defined `function`s, output redirection
//! (`print > file`, `| cmd`), `RS` values other than `"\n"`, and any option
//! other than `-F`, `-v`, `-f` and `--` (in particular `--version`/`--help`).
//!
//! # GNU versus BSD
//!
//! The subset follows GNU awk where the two implementations disagree, so
//! differential runs against macOS's BSD `awk` (version 20200816) show a few
//! expected differences: `-F t` and `-Ft` mean a literal `t` here (BSD treats a
//! bare `t`/`n` as tab/newline), `0x1f` is the hexadecimal literal `31` (BSD
//! reads `0` then the identifier `x1f`), `length(x)` of an uninitialised `x` is
//! `0` rather than an error, and `for (k in a)` visits keys in sorted order
//! instead of hash order.
//!
//! # Limits and error reporting
//!
//! [`MAX_RECORDS`] caps the number of records read across all inputs,
//! [`MAX_STEPS`] caps executed statements and loop iterations, and
//! [`MAX_OUTPUT_BYTES`] caps standard output. Hitting a bound reports a
//! `awk: … limit …` line on stderr and exits with status 2; output produced
//! before the limit was reached is kept (and never exceeds the byte cap).
//! Additional internal caps bound the source size, the number of tokens, the
//! nesting depth of expressions/statements, the number of fields, and the total
//! bytes read from inputs.

use std::collections::HashMap;
use std::rc::Rc;

use just_bash_regex::Regex;

/// Result of one `awk` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwkRun {
    /// Everything written to standard output.
    ///
    /// When a bound is hit the text produced before the failure is kept; the
    /// string never exceeds [`MAX_OUTPUT_BYTES`] bytes.
    pub stdout: String,
    /// Diagnostics, each on its own `awk: …` line. Empty on success.
    pub stderr: String,
    /// `0` on success, `2` for every error (including `exit` codes requested by
    /// the script).
    pub exit_code: i32,
}

/// Maximum number of input records read across all inputs.
///
/// The counter spans every file and standard input. Reading one record more
/// than this is an error: `awk: record limit exceeded (MAX_RECORDS = …)`.
pub const MAX_RECORDS: usize = 100_000;

/// Maximum number of executed statements plus loop iterations.
///
/// Every statement, every loop iteration (including the condition test of
/// `while`/`for`/`do`) and every record's rule evaluation consumes one step.
/// Exceeding the budget is an error:
/// `awk: step limit exceeded (MAX_STEPS = …)`.
pub const MAX_STEPS: usize = 1_000_000;

/// Maximum number of bytes accumulated on standard output.
///
/// A write that would push the buffer past this bound fails with
/// `awk: output limit exceeded (MAX_OUTPUT_BYTES = …)`; earlier output is kept.
pub const MAX_OUTPUT_BYTES: usize = 1_048_576;

/// Maximum total size of the program text given inline or through `-f`.
const MAX_SOURCE_BYTES: usize = 1 << 20;

/// Maximum number of bytes read from all inputs together.
const MAX_INPUT_BYTES: usize = 4 << 20;

/// Maximum number of lexical tokens in one program.
const MAX_TOKENS: usize = 50_000;

/// Maximum nesting depth of expressions and statements.
const MAX_DEPTH: usize = 64;

/// Largest field index (`$N`, `N`) accepted.
const MAX_FIELDS: usize = 4096;

/// Default precision used when a number is converted to a string (`%.6g`).
const NUMBER_PRECISION: usize = 6;

/// Seed of the deterministic `rand()` sequence.
const DEFAULT_RANDOM_SEED: u64 = 1;

/// Multiplier of the interpreter's LCG.
const LCG_MULTIPLIER: u64 = 6_364_136_223_846_793_005;

/// Increment of the interpreter's LCG.
const LCG_INCREMENT: u64 = 1_442_695_040_888_963_407;

/// Run awk. `args` are the operands AFTER the command word.
///
/// `stdin` is the standard input. File operands are read through `read_file`
/// (return `None` for a missing file, which must produce exit status 2 and a
/// `awk: can't open file NAME` line on stderr).
pub fn run(args: &[String], stdin: &str, read_file: &dyn Fn(&str) -> Option<String>) -> AwkRun {
    let config = match parse_args(args, read_file) {
        Ok(config) => config,
        Err(message) => return failure(&message),
    };
    let program = match Parser::parse_source(&config.program) {
        Ok(program) => program,
        Err(message) => return failure(&message),
    };
    let mut interp = Interp::new(stdin, config);
    match interp.execute(&program) {
        Ok(()) => AwkRun {
            stdout: interp.out,
            stderr: String::new(),
            exit_code: interp.exit_code,
        },
        Err(message) => AwkRun {
            stdout: interp.out,
            stderr: format!("awk: {message}\n"),
            exit_code: 2,
        },
    }
}

/// Build a failed run with no output.
fn failure(message: &str) -> AwkRun {
    AwkRun {
        stdout: String::new(),
        stderr: format!("awk: {message}\n"),
        exit_code: 2,
    }
}

/// One input source: a file name (`None` means standard input) plus the
/// command-line assignments that must be applied before it is read.
#[derive(Debug, Clone)]
struct SourceSpec {
    /// File operand, `None` for the implicit standard input.
    name: Option<String>,
    /// `var=value` operands that precede this file on the command line.
    assigns: Vec<(String, String)>,
}

/// Parsed command line.
struct Config<'a> {
    /// Program text: the inline operand, or every `-f` file joined in order.
    program: String,
    /// `-v` assignments, in command-line order.
    assignments: Vec<(String, String)>,
    /// `-F` value, if any.
    fs: Option<String>,
    /// Input sources, in order.
    sources: Vec<SourceSpec>,
    /// `var=value` operands with no following file (applied before `END`).
    trailing: Vec<(String, String)>,
    /// Reader for `-f` program files and input file operands.
    read_file: &'a dyn Fn(&str) -> Option<String>,
}

/// Parse the command line, reading `-f` programs through `read_file`.
fn parse_args<'a>(
    args: &[String],
    read_file: &'a dyn Fn(&str) -> Option<String>,
) -> Result<Config<'a>, String> {
    let mut fs: Option<String> = None;
    let mut assignments: Vec<(String, String)> = Vec::new();
    let mut programs: Vec<String> = Vec::new();
    let mut source_bytes = 0usize;
    let mut operands: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut index = 0usize;

    while index < args.len() {
        let arg = args[index].clone();
        if options_done {
            operands.push(arg);
            index += 1;
            continue;
        }
        if arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some(value) = rest.strip_prefix("field-separator=") {
                fs = Some(decode_escapes(value));
                index += 1;
                continue;
            }
            return Err(match rest {
                "version" => "--version is not supported".to_string(),
                "help" => "--help is not supported".to_string(),
                _ => format!("unknown option -- '{rest}'"),
            });
        }
        if arg == "-" {
            operands.push(arg);
            index += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix('-') {
            if rest.is_empty() {
                operands.push(arg);
                index += 1;
                continue;
            }
            let mut chars = rest.chars();
            let flag = chars.next().unwrap_or('?');
            let tail: String = chars.collect();
            match flag {
                'F' | 'v' | 'f' => {
                    let value = if tail.is_empty() {
                        index += 1;
                        match args.get(index) {
                            Some(value) => value.clone(),
                            None => return Err(format!("option requires an argument -- '{flag}'")),
                        }
                    } else {
                        tail
                    };
                    match flag {
                        'F' => fs = Some(decode_escapes(&value)),
                        'v' => match split_assignment(&value) {
                            Some((name, raw)) => assignments.push((name, decode_escapes(&raw))),
                            None => return Err(format!("invalid -v option argument: {value}")),
                        },
                        _ => match read_file(&value) {
                            Some(text) => {
                                source_bytes = source_bytes.saturating_add(text.len());
                                if source_bytes > MAX_SOURCE_BYTES {
                                    return Err(format!(
                                        "program too large (MAX_SOURCE_BYTES = {MAX_SOURCE_BYTES})"
                                    ));
                                }
                                programs.push(text);
                            }
                            None => return Err(format!("can't open file {value}")),
                        },
                    }
                }
                _ => return Err(format!("unknown option -- '{flag}'")),
            }
            index += 1;
            continue;
        }
        operands.push(arg);
        index += 1;
    }

    let program = if programs.is_empty() {
        if operands.is_empty() {
            return Err("no program given".to_string());
        }
        operands.remove(0)
    } else {
        programs.join("\n")
    };
    if program.len() > MAX_SOURCE_BYTES {
        return Err(format!(
            "program too large (MAX_SOURCE_BYTES = {MAX_SOURCE_BYTES})"
        ));
    }

    // The first operand is the program only when no `-f` was given; every later
    // operand is a `var=value` assignment or a file name.
    let mut sources: Vec<SourceSpec> = Vec::new();
    let mut pending: Vec<(String, String)> = Vec::new();
    for operand in operands {
        match split_assignment(&operand) {
            Some((name, raw)) => pending.push((name, decode_escapes(&raw))),
            None => sources.push(SourceSpec {
                name: Some(operand),
                assigns: std::mem::take(&mut pending),
            }),
        }
    }
    if sources.is_empty() {
        sources.push(SourceSpec {
            name: None,
            assigns: std::mem::take(&mut pending),
        });
    }

    Ok(Config {
        program,
        assignments,
        fs,
        sources,
        trailing: pending,
        read_file,
    })
}

/// Split `name=value`. Returns `None` unless `name` is a valid identifier.
fn split_assignment(text: &str) -> Option<(String, String)> {
    let (name, value) = text.split_once('=')?;
    if is_identifier(name) {
        Some((name.to_string(), value.to_string()))
    } else {
        None
    }
}

/// True for `[A-Za-z_][A-Za-z0-9_]*`.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Process backslash escapes in a string literal, `-v` value or `-F` value.
///
/// `\n \t \r \\ \" \/ \a \b \f \v` and one to three octal digits are
/// recognised; any other escape keeps the escaped character and drops the
/// backslash, which is what GNU awk does (with a warning we do not emit).
fn decode_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c != '\\' {
            out.push(c);
            index += 1;
            continue;
        }
        index += 1;
        let Some(&next) = chars.get(index) else {
            out.push('\\');
            break;
        };
        index += 1;
        match next {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '/' => out.push('/'),
            'a' => out.push('\u{7}'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0'..='7' => {
                let mut value = next.to_digit(8).unwrap_or(0);
                let mut digits = 1;
                while digits < 3 {
                    match chars.get(index) {
                        Some(&digit) if ('0'..='7').contains(&digit) => {
                            value = value * 8 + digit.to_digit(8).unwrap_or(0);
                            index += 1;
                            digits += 1;
                        }
                        _ => break,
                    }
                }
                if let Some(decoded) = char::from_u32(value % 256) {
                    out.push(decoded);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Scan a number at the start of `text`.
///
/// Accepts decimal forms (`1`, `1.5`, `.5`, `1e-3`) and `0x` hexadecimal.
/// Returns the value and the number of bytes consumed.
fn scan_number(text: &str) -> Option<(f64, usize)> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        index += 1;
    }
    let negative = index > 0 && bytes[0] == b'-';
    if index + 1 < bytes.len() && bytes[index] == b'0' && (bytes[index + 1] | 32) == b'x' {
        let mut cursor = index + 2;
        let mut value = 0.0f64;
        let mut digits = 0usize;
        while cursor < bytes.len() && bytes[cursor].is_ascii_hexdigit() {
            let digit = (bytes[cursor] as char).to_digit(16).unwrap_or(0) as f64;
            value = value * 16.0 + digit;
            cursor += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        if negative {
            value = -value;
        }
        return Some((value, cursor));
    }
    let mut cursor = index;
    let mut int_digits = 0usize;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0usize;
    if cursor < bytes.len() && bytes[cursor] == b'.' {
        let mut probe = cursor + 1;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
            frac_digits += 1;
        }
        if int_digits > 0 || frac_digits > 0 {
            cursor = probe;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if cursor < bytes.len() && (bytes[cursor] | 32) == b'e' {
        let mut probe = cursor + 1;
        if probe < bytes.len() && (bytes[probe] == b'+' || bytes[probe] == b'-') {
            probe += 1;
        }
        let digits_start = probe;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
        }
        if probe > digits_start {
            cursor = probe;
        }
    }
    let value: f64 = text.get(..cursor)?.parse().ok()?;
    Some((value, cursor))
}

/// Parse a whole string as a number, ignoring surrounding blanks.
///
/// This is awk's "strnum" test: the value is numeric only when the entire
/// string (up to ASCII blanks) is a number.
fn parse_full_number(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return None;
    }
    match scan_number(trimmed) {
        Some((value, used)) if used == trimmed.len() => Some(value),
        _ => None,
    }
}

/// Longest numeric prefix of `text`, as awk converts strings to numbers.
fn parse_number_prefix(text: &str) -> f64 {
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    match scan_number(trimmed) {
        Some((value, _)) => value,
        None => 0.0,
    }
}

/// One lexical token.
#[derive(Debug, Clone)]
enum TokenKind {
    /// End of a line: a statement/rule separator.
    Newline,
    /// Numeric literal (decimal or `0x` hexadecimal).
    Number(f64),
    /// String literal with escapes already processed.
    Str(String),
    /// Regular expression literal, compiled at lex time.
    RegexLit(Rc<Regex>),
    /// `/` in a position where a value may not follow (division).
    Slash,
    /// Variable or array name, or a built-in function name.
    Ident(String),
    /// `$`
    Dollar,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `;`
    Semi,
    /// `,`
    Comma,
    /// `=`
    Assign,
    /// `+=`
    AddAssign,
    /// `-=`
    SubAssign,
    /// `*=`
    MulAssign,
    /// `/=`
    DivAssign,
    /// `%=`
    ModAssign,
    /// `^=`
    PowAssign,
    /// `||`
    Or,
    /// `&&`
    And,
    /// `!`
    Not,
    /// `~`
    Match,
    /// `!~`
    NotMatch,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `%`
    Percent,
    /// `^`
    Caret,
    /// `++`
    Increment,
    /// `--`
    Decrement,
    /// `?`
    Question,
    /// `:`
    Colon,
    /// `>>` (output append redirection)
    Append,
    /// `>` (output redirection)
    Redirect,
    /// `|` (pipe redirection)
    Pipe,
    /// `in`
    In,
    /// `if`
    If,
    /// `else`
    Else,
    /// `while`
    While,
    /// `do`
    Do,
    /// `for`
    For,
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `next`
    Next,
    /// `nextfile`
    NextFile,
    /// `exit`
    Exit,
    /// `delete`
    Delete,
    /// `print`
    Print,
    /// `printf`
    Printf,
    /// `getline`
    Getline,
    /// `function` / `func`
    Function,
}

impl PartialEq for TokenKind {
    /// Compare tokens, treating regular expression literals as equal only when
    /// they are the very same compiled object.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TokenKind::Number(a), TokenKind::Number(b)) => a == b,
            (TokenKind::Str(a), TokenKind::Str(b)) => a == b,
            (TokenKind::Ident(a), TokenKind::Ident(b)) => a == b,
            (TokenKind::RegexLit(a), TokenKind::RegexLit(b)) => Rc::ptr_eq(a, b),
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }
}

/// A token together with the line it came from.
#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    line: usize,
}

/// Turns program text into tokens.
struct Lexer {
    /// Program characters.
    chars: Vec<char>,
    /// Current offset into [`Lexer::chars`].
    pos: usize,
    /// Current 1-based line number.
    line: usize,
    /// True when a `/` at this point starts a regex literal rather than a
    /// division, i.e. the previous token cannot end an operand.
    regex_allowed: bool,
}

impl Lexer {
    /// Build a lexer over `program`.
    fn new(program: &str) -> Self {
        Self {
            chars: program.chars().collect(),
            pos: 0,
            line: 1,
            regex_allowed: true,
        }
    }

    /// Current character, if any.
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// Character `offset` positions ahead, if any.
    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    /// Consume and return the current character.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// Consume `c` if it is next.
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Remaining program text from the current position.
    fn rest(&self) -> String {
        self.chars.iter().skip(self.pos).collect()
    }

    /// Tokenize the whole program.
    fn tokenize(mut self) -> Result<Vec<Token>, String> {
        let mut tokens: Vec<Token> = Vec::new();
        loop {
            let line = self.line;
            let Some(kind) = self.next_token()? else {
                break;
            };
            tokens.push(Token { kind, line });
            if tokens.len() > MAX_TOKENS {
                return Err(format!("program too complex (MAX_TOKENS = {MAX_TOKENS})"));
            }
        }
        Ok(tokens)
    }

    /// Skip blanks, comments and escaped newlines. Returns false at end of
    /// input.
    fn skip_trivia(&mut self) -> bool {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\r') => {
                    self.pos += 1;
                }
                Some('\\') if self.peek_at(1) == Some('\n') => {
                    self.pos += 2;
                    self.line += 1;
                }
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(_) => return true,
                None => return false,
            }
        }
    }

    /// Produce the next token, or `None` at end of input.
    fn next_token(&mut self) -> Result<Option<TokenKind>, String> {
        if !self.skip_trivia() {
            return Ok(None);
        }
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(None),
        };
        if c == '\n' {
            self.pos += 1;
            self.line += 1;
            self.regex_allowed = true;
            return Ok(Some(TokenKind::Newline));
        }
        if c.is_ascii_digit()
            || (c == '.' && matches!(self.peek_at(1), Some(d) if d.is_ascii_digit()))
        {
            let rest = self.rest();
            let Some((value, used)) = scan_number(&rest) else {
                return Err(format!("invalid number at line {}", self.line));
            };
            self.pos += used;
            self.regex_allowed = false;
            return Ok(Some(TokenKind::Number(value)));
        }
        if c == '"' {
            let text = self.lex_string()?;
            self.regex_allowed = false;
            return Ok(Some(TokenKind::Str(text)));
        }
        if c == '/' && self.regex_allowed {
            let pattern = self.lex_regex()?;
            let regex = Regex::new(&pattern)
                .map_err(|message| format!("invalid regular expression /{pattern}/: {message}"))?;
            self.regex_allowed = false;
            return Ok(Some(TokenKind::RegexLit(Rc::new(regex))));
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut name = String::new();
            while let Some(ch) = self.peek() {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    name.push(ch);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let kind = match name.as_str() {
                "in" => TokenKind::In,
                "if" => TokenKind::If,
                "else" => TokenKind::Else,
                "while" => TokenKind::While,
                "do" => TokenKind::Do,
                "for" => TokenKind::For,
                "break" => TokenKind::Break,
                "continue" => TokenKind::Continue,
                "next" => TokenKind::Next,
                "nextfile" => TokenKind::NextFile,
                "exit" => TokenKind::Exit,
                "delete" => TokenKind::Delete,
                "print" => TokenKind::Print,
                "printf" => TokenKind::Printf,
                "getline" => TokenKind::Getline,
                "function" | "func" => TokenKind::Function,
                _ => TokenKind::Ident(name),
            };
            self.regex_allowed = matches!(
                kind,
                TokenKind::In
                    | TokenKind::If
                    | TokenKind::Else
                    | TokenKind::While
                    | TokenKind::Do
                    | TokenKind::For
                    | TokenKind::Break
                    | TokenKind::Continue
                    | TokenKind::Next
                    | TokenKind::NextFile
                    | TokenKind::Exit
                    | TokenKind::Delete
                    | TokenKind::Print
                    | TokenKind::Printf
                    | TokenKind::Getline
                    | TokenKind::Function
            );
            return Ok(Some(kind));
        }
        Ok(Some(self.lex_operator()?))
    }

    /// Read a `"…"` literal and process its escapes.
    fn lex_string(&mut self) -> Result<String, String> {
        let start_line = self.line;
        self.pos += 1;
        let mut raw = String::new();
        loop {
            match self.bump() {
                Some('"') => break,
                Some('\\') => {
                    raw.push('\\');
                    match self.bump() {
                        Some(next) => raw.push(next),
                        None => {
                            return Err(format!("unterminated string at line {start_line}"));
                        }
                    }
                }
                Some('\n') => {
                    return Err(format!("unterminated string at line {start_line}"));
                }
                Some(c) => raw.push(c),
                None => return Err(format!("unterminated string at line {start_line}")),
            }
        }
        Ok(decode_escapes(&raw))
    }

    /// Read a `/…/` literal, returning the raw pattern text.
    fn lex_regex(&mut self) -> Result<String, String> {
        let start_line = self.line;
        self.pos += 1;
        let mut pattern = String::new();
        let mut in_bracket = false;
        loop {
            match self.bump() {
                Some('\\') => {
                    pattern.push('\\');
                    match self.bump() {
                        Some(next) => pattern.push(next),
                        None => {
                            return Err(format!(
                                "unterminated regular expression at line {start_line}"
                            ));
                        }
                    }
                }
                Some('[') => {
                    in_bracket = true;
                    pattern.push('[');
                }
                Some(']') => {
                    in_bracket = false;
                    pattern.push(']');
                }
                Some('/') if !in_bracket => break,
                Some('\n') => {
                    return Err(format!(
                        "unterminated regular expression at line {start_line}"
                    ));
                }
                Some(c) => pattern.push(c),
                None => {
                    return Err(format!(
                        "unterminated regular expression at line {start_line}"
                    ));
                }
            }
        }
        Ok(pattern)
    }

    /// Read a punctuation or operator token.
    fn lex_operator(&mut self) -> Result<TokenKind, String> {
        let c = self.peek().unwrap_or('?');
        self.pos += 1;
        let kind = match c {
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ';' => TokenKind::Semi,
            ',' => TokenKind::Comma,
            '?' => TokenKind::Question,
            ':' => TokenKind::Colon,
            '$' => TokenKind::Dollar,
            '+' => {
                if self.eat('+') {
                    TokenKind::Increment
                } else if self.eat('=') {
                    TokenKind::AddAssign
                } else {
                    TokenKind::Plus
                }
            }
            '-' => {
                if self.eat('-') {
                    TokenKind::Decrement
                } else if self.eat('=') {
                    TokenKind::SubAssign
                } else {
                    TokenKind::Minus
                }
            }
            '*' => {
                if self.eat('=') {
                    TokenKind::MulAssign
                } else {
                    TokenKind::Star
                }
            }
            '/' => {
                if self.eat('=') {
                    TokenKind::DivAssign
                } else {
                    TokenKind::Slash
                }
            }
            '%' => {
                if self.eat('=') {
                    TokenKind::ModAssign
                } else {
                    TokenKind::Percent
                }
            }
            '^' => {
                if self.eat('=') {
                    TokenKind::PowAssign
                } else {
                    TokenKind::Caret
                }
            }
            '=' => {
                if self.eat('=') {
                    TokenKind::Eq
                } else {
                    TokenKind::Assign
                }
            }
            '!' => {
                if self.eat('=') {
                    TokenKind::Ne
                } else if self.eat('~') {
                    TokenKind::NotMatch
                } else {
                    TokenKind::Not
                }
            }
            '~' => TokenKind::Match,
            '<' => {
                if self.eat('=') {
                    TokenKind::Le
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.eat('>') {
                    TokenKind::Append
                } else if self.eat('=') {
                    TokenKind::Ge
                } else {
                    TokenKind::Redirect
                }
            }
            '|' => {
                if self.eat('|') {
                    TokenKind::Or
                } else {
                    TokenKind::Pipe
                }
            }
            '&' => {
                if self.eat('&') {
                    TokenKind::And
                } else {
                    return Err("unexpected '&' (use \"&&\" for logical and)".to_string());
                }
            }
            '\\' => {
                return Err("unexpected '\\' (only a line continuation is allowed)".to_string());
            }
            other => return Err(format!("unexpected character '{other}'")),
        };
        self.regex_allowed = !matches!(
            kind,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::Increment | TokenKind::Decrement
        );
        Ok(kind)
    }
}

/// One rule of the program.
#[derive(Debug, Clone)]
struct Rule {
    /// Pattern that selects records, if any.
    pattern: Option<Pattern>,
    /// Action executed for matching records, if any.
    action: Option<Vec<Stmt>>,
    /// True for the first half of an `a, b { … }` range.
    range_start: bool,
    /// True for the second half of an `a, b { … }` range: the rule only turns
    /// the range off and never runs an action of its own.
    range_end: bool,
}

/// Rule pattern.
#[derive(Debug, Clone)]
enum Pattern {
    /// Bare expression: matched when its value is true.
    Expr(Expr),
    /// `pattern1, pattern2 { … }` — matched in the inclusive range between the
    /// two patterns becoming true.
    Range(Box<Pattern>, Box<Pattern>),
}

/// An expression node.
#[derive(Debug, Clone)]
enum Expr {
    /// Numeric literal.
    Number(f64),
    /// String literal (escapes already processed).
    Str(String),
    /// Regular expression literal.
    Regex(Rc<Regex>),
    /// `$expr` — a field, or the whole record for `$0`.
    Field(Box<Expr>),
    /// Variable or array-element reference.
    Var(Box<LValue>),
    /// `lvalue = value`
    Assign(Box<LValue>, Box<Expr>),
    /// `lvalue op= value`
    AssignOp(Box<LValue>, AssignOp, Box<Expr>),
    /// `++lvalue` / `--lvalue`
    PreInc(Box<LValue>, f64),
    /// `lvalue++` / `lvalue--`
    PostInc(Box<LValue>, f64),
    /// `left ? yes : no`
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `left || right`
    Or(Box<Expr>, Box<Expr>),
    /// `left && right`
    And(Box<Expr>, Box<Expr>),
    /// `(expr in array)`
    InArray(Box<Expr>, String),
    /// `left ~ right`
    MatchOp(Box<Expr>, Box<Expr>),
    /// `left !~ right`
    NotMatchOp(Box<Expr>, Box<Expr>),
    /// `left < right` and friends.
    Compare(Box<Expr>, CompareOp, Box<Expr>),
    /// Juxtaposition concatenation, already flattened.
    Concat(Vec<Expr>),
    /// `left op right` for arithmetic.
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    /// `-expr`
    Neg(Box<Expr>),
    /// `+expr`
    Pos(Box<Expr>),
    /// `!expr`
    Not(Box<Expr>),
    /// Built-in call.
    Call(Builtin, Vec<Expr>),
    /// Multi-dimensional array index, joined with `SUBSEP` at run time.
    Subsep(Vec<Expr>),
}

/// Assignment operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssignOp {
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
    /// `%=`
    Mod,
    /// `^=`
    Pow,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareOp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==`
    Eq,
    /// `!=`
    Ne,
}

/// Arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `^`
    Pow,
}

/// An assignable location whose subscripts have already been evaluated.
///
/// awk evaluates the subscript of `a[i++] = i` before the right-hand side, so
/// the target is resolved first and the value is computed afterwards.
#[derive(Debug, Clone)]
enum Target {
    /// Scalar variable.
    Scalar(String),
    /// Array element with a concrete key.
    Element(String, String),
    /// Field `$N`.
    Field(usize),
}

/// Assignable locations.
#[derive(Debug, Clone)]
enum LValue {
    /// Scalar variable.
    Scalar(String),
    /// Array element; the index is already joined with `SUBSEP` where needed.
    Element(String, Box<Expr>),
    /// Field `$N`.
    Field(Box<Expr>),
}

/// Built-in functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    /// `length([x])`
    Length,
    /// `substr(s, m [, n])`
    Substr,
    /// `index(s, t)`
    Index,
    /// `split(s, a [, fs])`
    Split,
    /// `sub(re, repl [, target])`
    Sub,
    /// `gsub(re, repl [, target])`
    Gsub,
    /// `match(s, re)`
    Match,
    /// `sprintf(fmt, …)`
    Sprintf,
    /// `tolower(s)`
    Tolower,
    /// `toupper(s)`
    Toupper,
    /// `int(x)`
    Int,
    /// `sqrt(x)`
    Sqrt,
    /// `exp(x)`
    Exp,
    /// `log(x)`
    Log,
    /// `sin(x)`
    Sin,
    /// `cos(x)`
    Cos,
    /// `atan2(y, x)`
    Atan2,
    /// `rand()`
    Rand,
    /// `srand([x])`
    Srand,
}

/// Statement kinds.
#[derive(Debug, Clone)]
enum StmtKind {
    /// Empty statement (`;` or a stray separator).
    Empty,
    /// `{ … }`
    Block(Vec<Stmt>),
    /// `print [args]`
    Print(Vec<Expr>),
    /// `printf fmt, args…`
    Printf(Vec<Expr>),
    /// `if (cond) then [else other]`
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    /// `while (cond) body`
    While(Expr, Box<Stmt>),
    /// `do body while (cond)`
    DoWhile(Box<Stmt>, Expr),
    /// `for (init; cond; post) body`
    For(
        Option<Box<Stmt>>,
        Option<Expr>,
        Option<Box<Stmt>>,
        Box<Stmt>,
    ),
    /// `for (key in array) body`
    ForIn(String, String, Box<Stmt>),
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `next`
    Next,
    /// `exit [code]`
    Exit(Option<Expr>),
    /// `delete array` / `delete array[index]`
    Delete(String, Option<Expr>),
    /// Expression statement.
    Expr(Expr),
}

/// A statement with its source line.
#[derive(Debug, Clone)]
struct Stmt {
    kind: StmtKind,
    line: usize,
}

/// A parsed program.
#[derive(Debug, Clone)]
struct Program {
    /// `BEGIN` rules, in source order.
    begin: Vec<Stmt>,
    /// `END` rules, in source order.
    end: Vec<Stmt>,
    /// Main rules, in source order.
    rules: Vec<Rule>,
}

/// Recursive-descent parser for the supported awk subset.
struct Parser {
    /// Token stream from the lexer.
    tokens: Vec<Token>,
    /// Index of the next token.
    pos: usize,
    /// Current expression/statement nesting depth.
    depth: usize,
    /// True while parsing a `print`/`printf` argument, where a top-level `>` is
    /// a redirection rather than a comparison.
    no_redirect: bool,
}

impl Parser {
    /// Lex and parse `source` into a [`Program`].
    fn parse_source(source: &str) -> Result<Program, String> {
        if source.len() > MAX_SOURCE_BYTES {
            return Err(format!(
                "program too large (MAX_SOURCE_BYTES = {MAX_SOURCE_BYTES})"
            ));
        }
        let tokens = Lexer::new(source).tokenize()?;
        let mut parser = Parser {
            tokens,
            pos: 0,
            depth: 0,
            no_redirect: false,
        };
        parser.parse_program()
    }

    /// Line number of the next token.
    fn line(&self) -> usize {
        self.tokens.get(self.pos).map(|t| t.line).unwrap_or(0)
    }

    /// Kind of the next token.
    fn peek(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|t| &t.kind)
    }

    /// Kind of the token `offset` positions ahead.
    fn peek_ahead(&self, offset: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + offset).map(|t| &t.kind)
    }

    /// Name of the next token when it is an identifier.
    fn peek_ident(&self) -> Option<&str> {
        match self.peek() {
            Some(TokenKind::Ident(name)) => Some(name.as_str()),
            _ => None,
        }
    }

    /// True at end of the token stream.
    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// True when the next token has the given kind.
    fn check(&self, kind: &TokenKind) -> bool {
        self.peek() == Some(kind)
    }

    /// Consume the next token when it has the given kind.
    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consume the next token, which must have the given kind.
    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<(), String> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(self.syntax(&format!("expected {what}")))
        }
    }

    /// Build a syntax error pointing at the current token.
    fn syntax(&self, message: &str) -> String {
        format!("syntax error at line {}: {message}", self.line())
    }

    /// Skip newline tokens.
    fn skip_newlines(&mut self) {
        while self.check(&TokenKind::Newline) {
            self.pos += 1;
        }
    }

    /// Skip newlines and semicolons.
    fn skip_separators(&mut self) {
        while matches!(
            self.peek(),
            Some(TokenKind::Newline) | Some(TokenKind::Semi)
        ) {
            self.pos += 1;
        }
    }

    /// Increase the nesting depth, failing past [`MAX_DEPTH`].
    fn enter(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!(
                "program too deeply nested (MAX_DEPTH = {MAX_DEPTH})"
            ));
        }
        Ok(())
    }

    /// Decrease the nesting depth.
    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Parse the whole program.
    fn parse_program(&mut self) -> Result<Program, String> {
        let mut begin: Vec<Stmt> = Vec::new();
        let mut end: Vec<Stmt> = Vec::new();
        let mut items: Vec<Rule> = Vec::new();

        loop {
            self.skip_separators();
            if self.at_end() {
                break;
            }
            let ends_with_block = self.parse_item(&mut begin, &mut end, &mut items)?;
            if ends_with_block {
                continue;
            }
            match self.peek() {
                None
                | Some(TokenKind::Newline)
                | Some(TokenKind::Semi)
                | Some(TokenKind::RBrace) => {}
                Some(_) => {
                    return Err(self.syntax("expected a newline or ';' between rules"));
                }
            }
        }

        let mut rules: Vec<Rule> = Vec::new();
        for item in items {
            match item.pattern {
                Some(Pattern::Range(start, stop)) => {
                    rules.push(Rule {
                        pattern: Some(*start),
                        action: item.action.clone(),
                        range_start: true,
                        range_end: false,
                    });
                    rules.push(Rule {
                        pattern: Some(*stop),
                        action: None,
                        range_start: false,
                        range_end: true,
                    });
                }
                other => rules.push(Rule {
                    pattern: other,
                    action: item.action,
                    range_start: false,
                    range_end: false,
                }),
            }
        }
        Ok(Program { begin, end, rules })
    }

    /// Parse one top-level item: a `BEGIN`/`END` rule, a pattern/action rule,
    /// or a bare `{ action }` rule.
    fn parse_item(
        &mut self,
        begin: &mut Vec<Stmt>,
        end: &mut Vec<Stmt>,
        items: &mut Vec<Rule>,
    ) -> Result<bool, String> {
        if let Some(name) = self.peek_ident()
            && (name == "BEGIN" || name == "END")
        {
            let is_begin = name == "BEGIN";
            self.pos += 1;
            self.skip_newlines();
            let action = self.parse_block()?;
            if is_begin {
                begin.extend(action);
            } else {
                end.extend(action);
            }
            return Ok(true);
        }
        if self.check(&TokenKind::LBrace) {
            let action = self.parse_block()?;
            items.push(Rule {
                pattern: None,
                action: Some(action),
                range_start: false,
                range_end: false,
            });
            return Ok(true);
        }
        let pattern = self.parse_pattern()?;
        if self.eat(&TokenKind::Comma) {
            self.skip_newlines();
            let stop = self.parse_pattern()?;
            let action = self.maybe_action()?;
            items.push(Rule {
                pattern: Some(Pattern::Range(Box::new(pattern), Box::new(stop))),
                action,
                range_start: false,
                range_end: false,
            });
            return Ok(self.peek_is_block_end());
        }
        let action = self.maybe_action()?;
        items.push(Rule {
            pattern: Some(pattern),
            action,
            range_start: false,
            range_end: false,
        });
        Ok(self.peek_is_block_end())
    }

    /// True when the last parsed rule ended with a `}` that had no separating
    /// newline or `;` after it.
    fn peek_is_block_end(&self) -> bool {
        self.previous_token_is_brace()
    }

    /// True when the most recently consumed token was `}`.
    fn previous_token_is_brace(&self) -> bool {
        matches!(
            self.tokens.get(self.pos.wrapping_sub(1)).map(|t| &t.kind),
            Some(TokenKind::RBrace)
        )
    }

    /// True when the most recently consumed token already terminated the
    /// statement (`;` or `}`).
    fn previous_token_is_separator(&self) -> bool {
        matches!(
            self.tokens.get(self.pos.wrapping_sub(1)).map(|t| &t.kind),
            Some(TokenKind::Semi) | Some(TokenKind::RBrace)
        )
    }

    /// Parse a rule pattern.
    fn parse_pattern(&mut self) -> Result<Pattern, String> {
        if let Some(name) = self.peek_ident()
            && (name == "BEGIN" || name == "END")
        {
            return Err(format!(
                "syntax error at line {}: {name} is only valid at the start of a rule",
                self.line()
            ));
        }
        let expr = self.parse_expr()?;
        Ok(Pattern::Expr(expr))
    }

    /// Parse `{ … }` when it directly follows (possibly after newlines).
    fn maybe_action(&mut self) -> Result<Option<Vec<Stmt>>, String> {
        let saved = self.pos;
        self.skip_newlines();
        if self.check(&TokenKind::LBrace) {
            Ok(Some(self.parse_block()?))
        } else {
            self.pos = saved;
            Ok(None)
        }
    }

    /// Parse a `{ … }` block.
    fn parse_block(&mut self) -> Result<Vec<Stmt>, String> {
        self.enter()?;
        let result = self.parse_block_inner();
        self.leave();
        result
    }

    /// Body of [`Parser::parse_block`].
    fn parse_block_inner(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(&TokenKind::LBrace, "'{'")?;
        let mut statements: Vec<Stmt> = Vec::new();
        loop {
            self.skip_separators();
            if self.eat(&TokenKind::RBrace) {
                break;
            }
            if self.at_end() {
                return Err(self.syntax("expected '}'"));
            }
            let statement = self.parse_statement()?;
            // A statement that consumed its own `;` or ended with a block
            // needs no further separator.
            let self_terminated = self.previous_token_is_separator();
            statements.push(statement);
            if self_terminated {
                continue;
            }
            match self.peek() {
                None
                | Some(TokenKind::Semi)
                | Some(TokenKind::Newline)
                | Some(TokenKind::RBrace) => {}
                Some(_) => {
                    return Err(self.syntax("expected ';', newline or '}' after statement"));
                }
            }
        }
        Ok(statements)
    }

    /// Parse one statement.
    fn parse_statement(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        self.enter()?;
        let kind = self.parse_statement_kind();
        self.leave();
        Ok(Stmt { kind: kind?, line })
    }

    /// Parse a statement after a `if (...)`/`while (...)`/`for (...)` header:
    /// an optional run of newlines is allowed before it.
    fn parse_body(&mut self) -> Result<Stmt, String> {
        self.skip_newlines();
        self.parse_statement()
    }

    /// Parse the statement kind at the current position.
    fn parse_statement_kind(&mut self) -> Result<StmtKind, String> {
        match self.peek().cloned() {
            Some(TokenKind::Semi) => {
                self.pos += 1;
                Ok(StmtKind::Empty)
            }
            Some(TokenKind::LBrace) => Ok(StmtKind::Block(self.parse_block()?)),
            Some(TokenKind::Print) => {
                self.pos += 1;
                Ok(StmtKind::Print(self.parse_print_args()?))
            }
            Some(TokenKind::Printf) => {
                self.pos += 1;
                Ok(StmtKind::Printf(self.parse_print_args()?))
            }
            Some(TokenKind::If) => self.parse_if(),
            Some(TokenKind::While) => {
                self.pos += 1;
                self.expect(&TokenKind::LParen, "'(' after while")?;
                self.skip_newlines();
                let condition = self.parse_expr()?;
                self.skip_newlines();
                self.expect(&TokenKind::RParen, "')' after while condition")?;
                let body = self.parse_body()?;
                Ok(StmtKind::While(condition, Box::new(body)))
            }
            Some(TokenKind::Do) => {
                self.pos += 1;
                let body = self.parse_body()?;
                self.skip_newlines();
                self.expect(&TokenKind::While, "'while' after do body")?;
                self.expect(&TokenKind::LParen, "'(' after while")?;
                self.skip_newlines();
                let condition = self.parse_expr()?;
                self.skip_newlines();
                self.expect(&TokenKind::RParen, "')' after while condition")?;
                Ok(StmtKind::DoWhile(Box::new(body), condition))
            }
            Some(TokenKind::For) => self.parse_for(),
            Some(TokenKind::Break) => {
                self.pos += 1;
                Ok(StmtKind::Break)
            }
            Some(TokenKind::Continue) => {
                self.pos += 1;
                Ok(StmtKind::Continue)
            }
            Some(TokenKind::Next) => {
                self.pos += 1;
                Ok(StmtKind::Next)
            }
            Some(TokenKind::NextFile) => {
                self.pos += 1;
                Ok(StmtKind::Next)
            }
            Some(TokenKind::Exit) => {
                self.pos += 1;
                let code = if self.starts_expression() {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(StmtKind::Exit(code))
            }
            Some(TokenKind::Delete) => self.parse_delete(),
            Some(TokenKind::Getline) => Err(format!(
                "syntax error at line {}: getline is not supported",
                self.line()
            )),
            Some(TokenKind::Function) => Err(format!(
                "syntax error at line {}: user-defined functions are not supported",
                self.line()
            )),
            Some(TokenKind::Pipe) => Err(format!(
                "syntax error at line {}: pipes are not supported",
                self.line()
            )),
            Some(TokenKind::Redirect) | Some(TokenKind::Append) => Err(format!(
                "syntax error at line {}: output redirection is not supported",
                self.line()
            )),
            Some(TokenKind::RBrace) => Err(self.syntax("unexpected '}'")),
            Some(TokenKind::Else) => Err(self.syntax("'else' without 'if'")),
            _ => {
                let expr = self.parse_expr()?;
                Ok(StmtKind::Expr(expr))
            }
        }
    }

    /// Parse `if (cond) then [else other]`.
    fn parse_if(&mut self) -> Result<StmtKind, String> {
        self.pos += 1;
        self.expect(&TokenKind::LParen, "'(' after if")?;
        self.skip_newlines();
        let condition = self.parse_expr()?;
        self.skip_newlines();
        self.expect(&TokenKind::RParen, "')' after if condition")?;
        let then_branch = self.parse_body()?;
        let saved = self.pos;
        self.skip_separators();
        if self.eat(&TokenKind::Else) {
            let else_branch = self.parse_body()?;
            Ok(StmtKind::If(
                condition,
                Box::new(then_branch),
                Some(Box::new(else_branch)),
            ))
        } else {
            self.pos = saved;
            Ok(StmtKind::If(condition, Box::new(then_branch), None))
        }
    }

    /// Parse `for (k in a) body` and `for (init; cond; post) body`.
    fn parse_for(&mut self) -> Result<StmtKind, String> {
        self.pos += 1;
        self.expect(&TokenKind::LParen, "'(' after for")?;
        self.skip_newlines();
        if let (Some(TokenKind::Ident(key)), Some(TokenKind::In), Some(TokenKind::Ident(array))) = (
            self.peek().cloned(),
            self.peek_ahead(1).cloned(),
            self.peek_ahead(2).cloned(),
        ) && matches!(self.peek_ahead(3), Some(TokenKind::RParen))
        {
            self.pos += 4;
            let body = self.parse_body()?;
            return Ok(StmtKind::ForIn(key, array, Box::new(body)));
        }
        let init = if self.check(&TokenKind::Semi) {
            None
        } else {
            Some(Box::new(self.parse_simple_statement()?))
        };
        self.expect(&TokenKind::Semi, "';' in for header")?;
        self.skip_newlines();
        let condition = if self.check(&TokenKind::Semi) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect(&TokenKind::Semi, "';' in for header")?;
        self.skip_newlines();
        let post = if self.check(&TokenKind::RParen) {
            None
        } else {
            Some(Box::new(self.parse_simple_statement()?))
        };
        self.skip_newlines();
        self.expect(&TokenKind::RParen, "')' after for header")?;
        let body = self.parse_body()?;
        Ok(StmtKind::For(init, condition, post, Box::new(body)))
    }

    /// Parse the restricted statement forms allowed in a `for` header.
    fn parse_simple_statement(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        if self.check(&TokenKind::Delete) {
            let kind = self.parse_delete()?;
            return Ok(Stmt { kind, line });
        }
        let expr = self.parse_expr()?;
        Ok(Stmt {
            kind: StmtKind::Expr(expr),
            line,
        })
    }

    /// Parse `delete a` and `delete a[i]`.
    fn parse_delete(&mut self) -> Result<StmtKind, String> {
        self.pos += 1;
        let name = match self.peek().cloned() {
            Some(TokenKind::Ident(name)) => {
                self.pos += 1;
                name
            }
            _ => return Err(self.syntax("'delete' requires an array name")),
        };
        if self.eat(&TokenKind::LBracket) {
            let parts = self.parse_index_parts()?;
            Ok(StmtKind::Delete(name, Some(build_index(parts))))
        } else {
            Ok(StmtKind::Delete(name, None))
        }
    }

    /// True when the next token can start an expression.
    fn starts_expression(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                TokenKind::Number(_)
                    | TokenKind::Str(_)
                    | TokenKind::RegexLit(_)
                    | TokenKind::Ident(_)
                    | TokenKind::Dollar
                    | TokenKind::LParen
                    | TokenKind::Not
                    | TokenKind::Minus
                    | TokenKind::Plus
                    | TokenKind::Increment
                    | TokenKind::Decrement
            )
        )
    }

    /// True when `print`/`printf` still has an argument to read.
    fn starts_print_argument(&self) -> bool {
        match self.peek() {
            None
            | Some(TokenKind::Newline)
            | Some(TokenKind::Semi)
            | Some(TokenKind::RBrace)
            | Some(TokenKind::RParen)
            | Some(TokenKind::Redirect)
            | Some(TokenKind::Append)
            | Some(TokenKind::Pipe) => false,
            Some(_) => true,
        }
    }

    /// Parse the argument list of `print`/`printf`, rejecting redirection.
    fn parse_print_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args: Vec<Expr> = Vec::new();
        if self.check(&TokenKind::LParen) && self.paren_list_is_complete() {
            self.pos += 1;
            let saved = self.no_redirect;
            self.no_redirect = false;
            self.skip_newlines();
            if !self.check(&TokenKind::RParen) {
                args.push(self.parse_expr_no_redirect()?);
                while self.eat(&TokenKind::Comma) {
                    self.skip_newlines();
                    args.push(self.parse_expr_no_redirect()?);
                }
            }
            self.skip_newlines();
            self.no_redirect = saved;
            self.expect(&TokenKind::RParen, "')' after argument list")?;
        } else if self.starts_print_argument() {
            args.push(self.parse_expr_no_redirect()?);
            while self.eat(&TokenKind::Comma) {
                self.skip_newlines();
                args.push(self.parse_expr_no_redirect()?);
            }
        }
        match self.peek() {
            Some(TokenKind::Redirect) | Some(TokenKind::Append) => {
                return Err(format!(
                    "syntax error at line {}: output redirection is not supported",
                    self.line()
                ));
            }
            Some(TokenKind::Pipe) => {
                return Err(format!(
                    "syntax error at line {}: output pipes are not supported",
                    self.line()
                ));
            }
            _ => {}
        }
        Ok(args)
    }

    /// Decide whether a `(` after `print`/`printf` starts an argument list.
    ///
    /// `print (a) (b)` means concatenation, so the parenthesised reading is
    /// accepted only when the closing `)` is followed by something that ends
    /// the statement.
    fn paren_list_is_complete(&self) -> bool {
        let mut depth = 0usize;
        let mut offset = 0usize;
        loop {
            match self.peek_ahead(offset) {
                None => return false,
                Some(TokenKind::LParen) => depth += 1,
                Some(TokenKind::RParen) => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(
                            self.peek_ahead(offset + 1),
                            None | Some(TokenKind::Newline)
                                | Some(TokenKind::Semi)
                                | Some(TokenKind::RBrace)
                                | Some(TokenKind::Redirect)
                                | Some(TokenKind::Append)
                                | Some(TokenKind::Pipe)
                        );
                    }
                }
                Some(TokenKind::Newline) => return false,
                Some(_) => {}
            }
            offset += 1;
        }
    }
}

/// Build the index expression for `a[i]`/`a[i,j]`.
fn build_index(parts: Vec<Expr>) -> Expr {
    if parts.len() == 1 {
        match parts.into_iter().next() {
            Some(expr) => expr,
            None => Expr::Str(String::new()),
        }
    } else {
        Expr::Subsep(parts)
    }
}

impl Parser {
    /// Parse one expression.
    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_assign()
    }

    /// Parse an expression in which a top-level `>` is a redirection rather
    /// than a comparison (the argument list of `print`/`printf`).
    fn parse_expr_no_redirect(&mut self) -> Result<Expr, String> {
        let saved = self.no_redirect;
        self.no_redirect = true;
        let result = self.parse_assign();
        self.no_redirect = saved;
        result
    }

    /// Assignment level, right associative: `lvalue = expr`, `lvalue op= expr`.
    fn parse_assign(&mut self) -> Result<Expr, String> {
        let target = self.parse_ternary()?;
        let op = match self.peek() {
            Some(TokenKind::Assign) => None,
            Some(TokenKind::AddAssign) => Some(AssignOp::Add),
            Some(TokenKind::SubAssign) => Some(AssignOp::Sub),
            Some(TokenKind::MulAssign) => Some(AssignOp::Mul),
            Some(TokenKind::DivAssign) => Some(AssignOp::Div),
            Some(TokenKind::ModAssign) => Some(AssignOp::Mod),
            Some(TokenKind::PowAssign) => Some(AssignOp::Pow),
            _ => return Ok(target),
        };
        self.pos += 1;
        self.skip_newlines();
        let value = self.parse_assign()?;
        let lvalue = self.as_lvalue(target)?;
        Ok(match op {
            None => Expr::Assign(Box::new(lvalue), Box::new(value)),
            Some(op) => Expr::AssignOp(Box::new(lvalue), op, Box::new(value)),
        })
    }

    /// Turn a parsed expression into an assignable location.
    fn as_lvalue(&self, expr: Expr) -> Result<LValue, String> {
        match expr {
            Expr::Var(lvalue) => Ok(*lvalue),
            Expr::Field(index) => Ok(LValue::Field(index)),
            _ => Err(format!(
                "syntax error at line {}: expression is not assignable",
                self.line()
            )),
        }
    }

    /// `cond ? yes : no`
    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let condition = self.parse_or()?;
        if self.eat(&TokenKind::Question) {
            self.skip_newlines();
            let yes = self.parse_assign()?;
            self.skip_newlines();
            self.expect(&TokenKind::Colon, "':' in conditional expression")?;
            self.skip_newlines();
            let no = self.parse_ternary()?;
            return Ok(Expr::Ternary(
                Box::new(condition),
                Box::new(yes),
                Box::new(no),
            ));
        }
        Ok(condition)
    }

    /// `||`
    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.eat(&TokenKind::Or) {
            self.skip_newlines();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `&&`
    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_in()?;
        while self.eat(&TokenKind::And) {
            self.skip_newlines();
            let right = self.parse_in()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// `expr in array`, including the `(i, j) in a` index-list form.
    fn parse_in(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_match()?;
        while self.check(&TokenKind::In) {
            let Some(TokenKind::Ident(array)) = self.peek_ahead(1).cloned() else {
                break;
            };
            self.pos += 2;
            left = Expr::InArray(Box::new(left), array);
        }
        Ok(left)
    }

    /// `(i, j) in a` alongside `i SUBSEP j in a`: a parenthesised comma list on
    /// the left of `in` is an index list, so join it with SUBSEP before the
    /// membership test.
    fn parse_paren_index_in(&mut self) -> Result<Option<Expr>, String> {
        if !self.check(&TokenKind::LParen) {
            return Ok(None);
        }
        let saved = self.pos;
        self.pos += 1;
        self.skip_newlines();
        let mut parts = Vec::new();
        loop {
            match self.parse_assign() {
                Ok(expr) => parts.push(expr),
                Err(_) => {
                    self.pos = saved;
                    return Ok(None);
                }
            }
            self.skip_newlines();
            if self.check(&TokenKind::Comma) {
                self.pos += 1;
                self.skip_newlines();
                continue;
            }
            break;
        }
        if parts.len() < 2 || !self.check(&TokenKind::RParen) {
            self.pos = saved;
            return Ok(None);
        }
        self.pos += 1;
        if !self.check(&TokenKind::In) {
            self.pos = saved;
            return Ok(None);
        }
        let Some(TokenKind::Ident(array)) = self.peek_ahead(1).cloned() else {
            self.pos = saved;
            return Ok(None);
        };
        self.pos += 2;
        Ok(Some(Expr::InArray(Box::new(build_index(parts)), array)))
    }

    /// `~` and `!~`
    fn parse_match(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_relational()?;
        loop {
            let negated = match self.peek() {
                Some(TokenKind::Match) => false,
                Some(TokenKind::NotMatch) => true,
                _ => break,
            };
            self.pos += 1;
            self.skip_newlines();
            let right = self.parse_relational()?;
            left = if negated {
                Expr::NotMatchOp(Box::new(left), Box::new(right))
            } else {
                Expr::MatchOp(Box::new(left), Box::new(right))
            };
        }
        Ok(left)
    }

    /// `<` `<=` `>` `>=` `==` `!=` (non-associative, like awk).
    fn parse_relational(&mut self) -> Result<Expr, String> {
        let left = self.parse_concat()?;
        let op = match self.peek() {
            Some(TokenKind::Lt) => CompareOp::Lt,
            Some(TokenKind::Le) => CompareOp::Le,
            Some(TokenKind::Redirect) if !self.no_redirect => CompareOp::Gt,
            Some(TokenKind::Ge) => CompareOp::Ge,
            Some(TokenKind::Eq) => CompareOp::Eq,
            Some(TokenKind::Ne) => CompareOp::Ne,
            _ => return Ok(left),
        };
        self.pos += 1;
        self.skip_newlines();
        let right = self.parse_concat()?;
        Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
    }

    /// Concatenation by juxtaposition.
    fn parse_concat(&mut self) -> Result<Expr, String> {
        let first = self.parse_additive()?;
        let mut parts = vec![first];
        while self.starts_concat_operand() {
            parts.push(self.parse_additive()?);
        }
        if parts.len() == 1 {
            match parts.pop() {
                Some(expr) => Ok(expr),
                None => Err(self.syntax("expected an expression")),
            }
        } else {
            Ok(Expr::Concat(parts))
        }
    }

    /// True when the next token begins another concatenation operand.
    fn starts_concat_operand(&self) -> bool {
        match self.peek() {
            Some(TokenKind::Number(_))
            | Some(TokenKind::Str(_))
            | Some(TokenKind::RegexLit(_))
            | Some(TokenKind::Ident(_))
            | Some(TokenKind::Dollar)
            | Some(TokenKind::LParen)
            | Some(TokenKind::Increment)
            | Some(TokenKind::Decrement)
            | Some(TokenKind::Not) => true,
            // `>` never continues a concatenation: it is either a comparison
            // (handled one level up) or a redirection (rejected).
            _ => false,
        }
    }

    /// `+` and `-`
    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(TokenKind::Plus) => BinaryOp::Add,
                Some(TokenKind::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.pos += 1;
            self.skip_newlines();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    /// `*`, `/` and `%`
    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(TokenKind::Star) => BinaryOp::Mul,
                Some(TokenKind::Slash) => BinaryOp::Div,
                Some(TokenKind::Percent) => BinaryOp::Mod,
                _ => break,
            };
            self.pos += 1;
            self.skip_newlines();
            let right = self.parse_unary()?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    /// Unary `+ - !`, prefix `++`/`--`, and right-associative `^`.
    fn parse_unary(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let result = self.parse_unary_inner();
        self.leave();
        result
    }

    /// Body of [`Parser::parse_unary`].
    fn parse_unary_inner(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(TokenKind::Minus) => {
                self.pos += 1;
                let operand = self.parse_unary()?;
                Ok(Expr::Neg(Box::new(operand)))
            }
            Some(TokenKind::Plus) => {
                self.pos += 1;
                let operand = self.parse_unary()?;
                Ok(Expr::Pos(Box::new(operand)))
            }
            Some(TokenKind::Not) => {
                self.pos += 1;
                let operand = self.parse_unary()?;
                Ok(Expr::Not(Box::new(operand)))
            }
            Some(TokenKind::Increment) | Some(TokenKind::Decrement) => {
                let delta = if self.check(&TokenKind::Increment) {
                    1.0
                } else {
                    -1.0
                };
                self.pos += 1;
                let target = self.parse_postfix()?;
                let lvalue = self.as_lvalue(target)?;
                Ok(Expr::PreInc(Box::new(lvalue), delta))
            }
            _ => {
                let base = self.parse_postfix()?;
                if self.check(&TokenKind::Caret) {
                    self.pos += 1;
                    self.skip_newlines();
                    let exponent = self.parse_unary()?;
                    return Ok(Expr::Binary(
                        Box::new(base),
                        BinaryOp::Pow,
                        Box::new(exponent),
                    ));
                }
                Ok(base)
            }
        }
    }

    /// A primary expression with postfix `++`/`--`.
    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        while let Some(kind) = self.peek().cloned() {
            match kind {
                TokenKind::Increment | TokenKind::Decrement => {
                    let delta = if kind == TokenKind::Increment {
                        1.0
                    } else {
                        -1.0
                    };
                    self.pos += 1;
                    let lvalue = self.as_lvalue(expr)?;
                    expr = Expr::PostInc(Box::new(lvalue), delta);
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// A primary expression: literal, name, array element, field, call, group.
    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(TokenKind::Number(value)) => {
                self.pos += 1;
                Ok(Expr::Number(value))
            }
            Some(TokenKind::Str(text)) => {
                self.pos += 1;
                Ok(Expr::Str(text))
            }
            Some(TokenKind::RegexLit(regex)) => {
                self.pos += 1;
                Ok(Expr::Regex(regex))
            }
            Some(TokenKind::Dollar) => {
                self.pos += 1;
                let index = self.parse_field_index()?;
                Ok(Expr::Field(Box::new(index)))
            }
            Some(TokenKind::LParen) => {
                // `(i, j) in a` is an index list, not a grouping.
                if let Some(expr) = self.parse_paren_index_in()? {
                    return Ok(expr);
                }
                self.pos += 1;
                self.skip_newlines();
                let saved = self.no_redirect;
                self.no_redirect = false;
                let inner = self.parse_assign()?;
                self.no_redirect = saved;
                self.skip_newlines();
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(inner)
            }
            Some(TokenKind::Ident(name)) => {
                self.pos += 1;
                if self.check(&TokenKind::LParen) {
                    return self.parse_call(&name);
                }
                if self.eat(&TokenKind::LBracket) {
                    let parts = self.parse_index_parts()?;
                    let index = build_index(parts);
                    return Ok(Expr::Var(Box::new(LValue::Element(name, Box::new(index)))));
                }
                if name == "getline" {
                    return Err(format!(
                        "syntax error at line {}: getline is not supported",
                        self.line()
                    ));
                }
                Ok(Expr::Var(Box::new(LValue::Scalar(name))))
            }
            Some(TokenKind::Getline) => Err(format!(
                "syntax error at line {}: getline is not supported",
                self.line()
            )),
            Some(TokenKind::Function) => Err(format!(
                "syntax error at line {}: user-defined functions are not supported",
                self.line()
            )),
            Some(other) => Err(self.syntax(&format!("unexpected token {}", token_label(&other)))),
            None => Err(self.syntax("unexpected end of program")),
        }
    }

    /// Parse the operand of `$`: a primary expression without `$` recursion.
    fn parse_field_index(&mut self) -> Result<Expr, String> {
        self.enter()?;
        let result = self.parse_field_index_inner();
        self.leave();
        result
    }

    /// Body of [`Parser::parse_field_index`].
    fn parse_field_index_inner(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(TokenKind::Number(value)) => {
                self.pos += 1;
                Ok(Expr::Number(value))
            }
            Some(TokenKind::Dollar) => {
                self.pos += 1;
                let inner = self.parse_field_index()?;
                Ok(Expr::Field(Box::new(inner)))
            }
            Some(TokenKind::Increment) | Some(TokenKind::Decrement) => {
                let delta = if self.check(&TokenKind::Increment) {
                    1.0
                } else {
                    -1.0
                };
                self.pos += 1;
                let target = self.parse_postfix()?;
                let lvalue = self.as_lvalue(target)?;
                Ok(Expr::PreInc(Box::new(lvalue), delta))
            }
            Some(TokenKind::Minus) => {
                self.pos += 1;
                let operand = self.parse_field_index()?;
                Ok(Expr::Neg(Box::new(operand)))
            }
            Some(TokenKind::Plus) => {
                self.pos += 1;
                let operand = self.parse_field_index()?;
                Ok(Expr::Pos(Box::new(operand)))
            }
            Some(TokenKind::LParen) => {
                self.pos += 1;
                self.skip_newlines();
                let saved = self.no_redirect;
                self.no_redirect = false;
                let inner = self.parse_assign()?;
                self.no_redirect = saved;
                self.skip_newlines();
                self.expect(&TokenKind::RParen, "')'")?;
                Ok(inner)
            }
            Some(TokenKind::Str(text)) => {
                self.pos += 1;
                Ok(Expr::Str(text))
            }
            Some(TokenKind::Ident(name)) => {
                self.pos += 1;
                if self.check(&TokenKind::LParen) {
                    return self.parse_call(&name);
                }
                if self.eat(&TokenKind::LBracket) {
                    let parts = self.parse_index_parts()?;
                    let index = build_index(parts);
                    return Ok(Expr::Var(Box::new(LValue::Element(name, Box::new(index)))));
                }
                Ok(Expr::Var(Box::new(LValue::Scalar(name))))
            }
            Some(other) => Err(self.syntax(&format!("unexpected token {}", token_label(&other)))),
            None => Err(self.syntax("unexpected end of program after '$'")),
        }
    }

    /// Parse `[expr, expr…]`, including the closing bracket.
    fn parse_index_parts(&mut self) -> Result<Vec<Expr>, String> {
        let mut parts: Vec<Expr> = Vec::new();
        self.skip_newlines();
        if self.check(&TokenKind::RBracket) {
            return Err(self.syntax("empty array subscript"));
        }
        parts.push(self.parse_expr()?);
        while self.eat(&TokenKind::Comma) {
            self.skip_newlines();
            parts.push(self.parse_expr()?);
        }
        self.skip_newlines();
        self.expect(&TokenKind::RBracket, "']'")?;
        Ok(parts)
    }

    /// Parse a built-in call whose name has already been consumed.
    fn parse_call(&mut self, name: &str) -> Result<Expr, String> {
        let builtin = match builtin_by_name(name) {
            Some(builtin) => builtin,
            None => match name {
                "system" | "close" | "fflush" => {
                    return Err(format!(
                        "syntax error at line {}: {name}() is not supported",
                        self.line()
                    ));
                }
                other => {
                    return Err(format!(
                        "syntax error at line {}: call to undefined function {other}() \
                         (user-defined functions are not supported)",
                        self.line()
                    ));
                }
            },
        };
        self.expect(&TokenKind::LParen, "'(' after function name")?;
        self.skip_newlines();
        let mut args: Vec<Expr> = Vec::new();
        if !self.check(&TokenKind::RParen) {
            args.push(self.parse_expr()?);
            while self.eat(&TokenKind::Comma) {
                self.skip_newlines();
                args.push(self.parse_expr()?);
            }
        }
        self.skip_newlines();
        self.expect(&TokenKind::RParen, "')' after function arguments")?;
        let (min, max) = builtin_arity(builtin);
        if args.len() < min || args.len() > max {
            let expected = if min == max {
                format!("{min}")
            } else {
                format!("{min} to {max}")
            };
            return Err(format!(
                "syntax error at line {}: {name}() takes {expected} argument(s), got {}",
                self.line(),
                args.len()
            ));
        }
        Ok(Expr::Call(builtin, args))
    }
}

/// Human-readable label for a token, used in error messages.
fn token_label(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Newline => "end of line".to_string(),
        TokenKind::Number(_) => "number".to_string(),
        TokenKind::Str(_) => "string".to_string(),
        TokenKind::RegexLit(_) => "regular expression".to_string(),
        TokenKind::Slash => "'/'".to_string(),
        TokenKind::Ident(name) => format!("'{name}'"),
        TokenKind::Dollar => "'$'".to_string(),
        TokenKind::LBrace => "'{'".to_string(),
        TokenKind::RBrace => "'}'".to_string(),
        TokenKind::LParen => "'('".to_string(),
        TokenKind::RParen => "')'".to_string(),
        TokenKind::LBracket => "'['".to_string(),
        TokenKind::RBracket => "']'".to_string(),
        TokenKind::Semi => "';'".to_string(),
        TokenKind::Comma => "','".to_string(),
        TokenKind::Assign => "'='".to_string(),
        TokenKind::AddAssign => "'+='".to_string(),
        TokenKind::SubAssign => "'-='".to_string(),
        TokenKind::MulAssign => "'*='".to_string(),
        TokenKind::DivAssign => "'/='".to_string(),
        TokenKind::ModAssign => "'%='".to_string(),
        TokenKind::PowAssign => "'^='".to_string(),
        TokenKind::Or => "'||'".to_string(),
        TokenKind::And => "'&&'".to_string(),
        TokenKind::Not => "'!'".to_string(),
        TokenKind::Match => "'~'".to_string(),
        TokenKind::NotMatch => "'!~'".to_string(),
        TokenKind::Lt => "'<'".to_string(),
        TokenKind::Le => "'<='".to_string(),
        TokenKind::Ge => "'>='".to_string(),
        TokenKind::Eq => "'=='".to_string(),
        TokenKind::Ne => "'!='".to_string(),
        TokenKind::Plus => "'+'".to_string(),
        TokenKind::Minus => "'-'".to_string(),
        TokenKind::Star => "'*'".to_string(),
        TokenKind::Percent => "'%'".to_string(),
        TokenKind::Caret => "'^'".to_string(),
        TokenKind::Increment => "'++'".to_string(),
        TokenKind::Decrement => "'--'".to_string(),
        TokenKind::Question => "'?'".to_string(),
        TokenKind::Colon => "':'".to_string(),
        TokenKind::Append => "'>>'".to_string(),
        TokenKind::Redirect => "'>'".to_string(),
        TokenKind::Pipe => "'|'".to_string(),
        TokenKind::In => "'in'".to_string(),
        TokenKind::If => "'if'".to_string(),
        TokenKind::Else => "'else'".to_string(),
        TokenKind::While => "'while'".to_string(),
        TokenKind::Do => "'do'".to_string(),
        TokenKind::For => "'for'".to_string(),
        TokenKind::Break => "'break'".to_string(),
        TokenKind::Continue => "'continue'".to_string(),
        TokenKind::Next => "'next'".to_string(),
        TokenKind::NextFile => "'nextfile'".to_string(),
        TokenKind::Exit => "'exit'".to_string(),
        TokenKind::Delete => "'delete'".to_string(),
        TokenKind::Print => "'print'".to_string(),
        TokenKind::Printf => "'printf'".to_string(),
        TokenKind::Getline => "'getline'".to_string(),
        TokenKind::Function => "'function'".to_string(),
    }
}

/// Look up a built-in function by name.
fn builtin_by_name(name: &str) -> Option<Builtin> {
    let builtin = match name {
        "length" => Builtin::Length,
        "substr" => Builtin::Substr,
        "index" => Builtin::Index,
        "split" => Builtin::Split,
        "sub" => Builtin::Sub,
        "gsub" => Builtin::Gsub,
        "match" => Builtin::Match,
        "sprintf" => Builtin::Sprintf,
        "tolower" => Builtin::Tolower,
        "toupper" => Builtin::Toupper,
        "int" => Builtin::Int,
        "sqrt" => Builtin::Sqrt,
        "exp" => Builtin::Exp,
        "log" => Builtin::Log,
        "sin" => Builtin::Sin,
        "cos" => Builtin::Cos,
        "atan2" => Builtin::Atan2,
        "rand" => Builtin::Rand,
        "srand" => Builtin::Srand,
        _ => return None,
    };
    Some(builtin)
}

/// Accepted argument-count range of a built-in.
fn builtin_arity(builtin: Builtin) -> (usize, usize) {
    match builtin {
        Builtin::Length => (0, 1),
        Builtin::Substr => (2, 3),
        Builtin::Index => (2, 2),
        Builtin::Split => (2, 3),
        Builtin::Sub | Builtin::Gsub => (2, 3),
        Builtin::Match => (2, 2),
        Builtin::Sprintf => (1, 64),
        Builtin::Tolower | Builtin::Toupper => (1, 1),
        Builtin::Int
        | Builtin::Sqrt
        | Builtin::Exp
        | Builtin::Log
        | Builtin::Sin
        | Builtin::Cos => (1, 1),
        Builtin::Atan2 => (2, 2),
        Builtin::Rand => (0, 0),
        Builtin::Srand => (0, 1),
    }
}

/// A runtime value.
///
/// `StrNum` carries a string together with the numeric value awk derived from
/// it at assignment time, which is what makes `"10" < "9"` false for input
/// fields while `"abc" < "abd"` stays a string comparison.
#[derive(Debug, Clone)]
enum Value {
    /// Number, also used for booleans (`0`/`1`).
    Num(f64),
    /// Pure string (string literals, concatenations, command-line `-v` values).
    Str(Rc<str>),
    /// String that came from input and looks like a number.
    StrNum(Rc<str>, f64),
}

impl Value {
    /// Build a value from a string literal or computed string.
    fn str(text: impl Into<Rc<str>>) -> Self {
        Value::Str(text.into())
    }

    /// Numeric value, using the longest numeric prefix for strings.
    fn num(&self) -> f64 {
        match self {
            Value::Num(value) => *value,
            Value::Str(text) => parse_number_prefix(text),
            Value::StrNum(_, value) => *value,
        }
    }

    /// String value, using the default output format for numbers.
    fn text(&self) -> String {
        match self {
            Value::Num(value) => number_to_string(*value),
            Value::Str(text) | Value::StrNum(text, _) => text.to_string(),
        }
    }

    /// True when the value is an input-derived numeric string or a number.
    fn is_numeric(&self) -> bool {
        !matches!(self, Value::Str(_))
    }

    /// awk truth: numbers are true when non-zero, strings when non-empty.
    fn truthy(&self) -> bool {
        match self {
            Value::Num(value) => *value != 0.0,
            Value::Str(text) | Value::StrNum(text, _) => !text.is_empty(),
        }
    }
}

/// Wrap a boolean as an awk value.
fn bool_num(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

/// One variable slot: either a scalar or an array.
#[derive(Debug, Clone, Default)]
struct Cell {
    /// Scalar value, if the slot holds one.
    scalar: Option<Value>,
    /// Array contents, if the slot holds an array.
    array: Option<HashMap<String, Value>>,
}

/// How `FS` splits a record.
#[derive(Debug, Clone)]
enum FsSpec {
    /// The default `" "`: runs of blanks and tabs, trimmed.
    Default,
    /// Empty `FS`: one field per character.
    Empty,
    /// Single character, taken literally.
    Char(char),
    /// Multi-character separator, used as an ERE.
    Regex(Rc<Regex>),
}

/// Split `text` on runs of blanks and tabs, dropping leading/trailing blanks.
fn split_blanks(text: &str) -> Vec<String> {
    text.split([' ', '\t'])
        .filter(|piece| !piece.is_empty())
        .map(|piece| piece.to_string())
        .collect()
}

/// The byte offset just past the character starting at `index`.
fn next_char_end(text: &str, index: usize) -> Option<usize> {
    let rest = text.get(index..)?;
    let mut chars = rest.char_indices();
    match (chars.next(), chars.next()) {
        (Some(_), Some((next, _))) => Some(index + next),
        (Some((_, c)), None) => Some(index + c.len_utf8()),
        (None, _) => None,
    }
}

/// Format a number the way awk prints it: integers without a decimal point,
/// everything else through `%g` with [`NUMBER_PRECISION`] significant digits.
fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 {
            "-inf".to_string()
        } else {
            "inf".to_string()
        };
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    if magnitude <= 1e18 && value.fract() == 0.0 {
        return format!("{}", value as i64);
    }
    format!("{:.*e}", NUMBER_PRECISION - 1, value)
        .parse::<f64>()
        .map(|rounded| format_g(rounded, NUMBER_PRECISION, false))
        .unwrap_or_else(|_| format_g(value, NUMBER_PRECISION, false))
}

/// `%g`-style formatting with a caller-chosen precision and `#` flag.
fn format_g(value: f64, precision: usize, alternate: bool) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 {
            "-inf".to_string()
        } else {
            "inf".to_string()
        };
    }
    let precision = precision.clamp(1, 17);
    if value == 0.0 {
        return if alternate {
            format!("{:.*}", precision - 1, 0.0)
        } else {
            "0".to_string()
        };
    }
    let exponent = value.abs().log10().floor() as i32;
    if exponent < -4 || exponent >= precision as i32 {
        let mantissa = format!("{:.*e}", precision - 1, value);
        let mantissa = trim_g_mantissa(&mantissa, alternate);
        return normalize_exponent(&mantissa);
    }
    let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
    let text = format!("{:.*}", decimals, value);
    if alternate {
        text
    } else {
        trim_g_fraction(text)
    }
}

/// Drop trailing zeros (and a trailing point) from a `%e` mantissa.
fn trim_g_mantissa(text: &str, alternate: bool) -> String {
    if alternate {
        return text.to_string();
    }
    match text.split_once('e') {
        Some((mantissa, exponent)) => {
            format!("{}e{exponent}", trim_g_fraction(mantissa.to_string()))
        }
        None => trim_g_fraction(text.to_string()),
    }
}

/// Drop trailing zeros (and a trailing point) from a fixed-point number.
fn trim_g_fraction(text: String) -> String {
    if !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Print an exponent with a sign and at least two digits, as C does.
fn normalize_exponent(text: &str) -> String {
    match text.split_once('e') {
        Some((mantissa, exponent)) => {
            let (sign, digits) = match exponent.strip_prefix('-') {
                Some(rest) => ('-', rest),
                None => ('+', exponent.strip_prefix('+').unwrap_or(exponent)),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => text.to_string(),
    }
}

/// Clamp a float into `i64` range for integer conversions.
fn to_i64(value: f64) -> i64 {
    if value.is_nan() { 0 } else { value as i64 }
}

/// Interpreter state: variables, the current record and the global budgets.
struct Interp<'a> {
    /// Accumulated standard output.
    out: String,
    /// Executed statements and loop iterations so far.
    steps: usize,
    /// Input records read so far.
    records: usize,
    /// Exit status requested by the script.
    exit_code: i32,
    /// Variables and arrays.
    vars: HashMap<String, Cell>,
    /// Command-line assignments, re-applied whenever a variable is read.
    cmdline: Vec<(String, Value)>,
    /// Current record number and per-file record number.
    nr: usize,
    fnr: usize,
    /// Current input file name.
    filename: String,
    /// `$0` and the fields derived from it.
    record: Record,
    /// Compiled regular expressions, keyed by pattern text.
    regex_cache: HashMap<String, Rc<Regex>>,
    /// Range-pattern state, one flag per rule index.
    range_active: Vec<bool>,
    /// State of the deterministic `rand()` generator.
    random: u64,
    /// Value returned by the next `srand()` with no argument.
    random_seed: u64,
    /// True once `exit` was executed (skips the remaining input).
    exiting: bool,
    /// True once the `END` rules ran (they run at most once).
    end_ran: bool,
    /// Standard input text.
    stdin: &'a str,
    /// Program-file reader supplied by the caller.
    read_file: &'a dyn Fn(&str) -> Option<String>,
    /// Input sources in command-line order.
    sources: Vec<SourceSpec>,
    /// Command-line assignments that follow the last file.
    trailing: Vec<(String, String)>,
}

/// The current record: `$0` plus the fields split from it.
#[derive(Debug, Clone, Default)]
struct Record {
    /// `$0`.
    line: String,
    /// `$1`…`$N`, possibly shorter than `NF` while fields are being extended.
    fields: Vec<String>,
    /// Value of `NF`.
    nf: usize,
}

impl Record {
    /// Replace `$0` and the fields, preserving a rebuild marker.
    fn set_line(&mut self, line: String) {
        self.line = line;
    }

    /// Grow the field vector so that `$index` can be assigned.
    fn ensure(&mut self, index: usize) {
        while self.fields.len() < index {
            self.fields.push(String::new());
        }
        if self.nf < index {
            self.nf = index;
        }
    }

    /// `$index`, or the empty string when it does not exist.
    fn field(&self, index: usize) -> String {
        if index == 0 {
            return self.line.clone();
        }
        self.fields.get(index - 1).cloned().unwrap_or_default()
    }
}

/// What a statement asked the interpreter to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Continue with the next statement.
    Normal,
    /// Leave the innermost loop.
    Break,
    /// Start the next iteration of the innermost loop.
    Continue,
    /// Abandon the current record.
    Next,
    /// Stop the program and run the `END` rules.
    Exit,
}

impl<'a> Interp<'a> {
    /// Create an interpreter for `stdin` and a parsed command line.
    fn new(stdin: &'a str, config: Config<'a>) -> Self {
        let mut interp = Interp {
            out: String::new(),
            steps: 0,
            records: 0,
            exit_code: 0,
            vars: HashMap::new(),
            cmdline: Vec::new(),
            nr: 0,
            fnr: 0,
            filename: String::new(),
            record: Record::default(),
            regex_cache: HashMap::new(),
            range_active: Vec::new(),
            random: DEFAULT_RANDOM_SEED,
            random_seed: DEFAULT_RANDOM_SEED,
            exiting: false,
            end_ran: false,
            stdin,
            read_file: config.read_file,
            sources: config.sources,
            trailing: config.trailing,
        };
        interp.set_builtin("FS", " ");
        interp.set_builtin("OFS", " ");
        interp.set_builtin("ORS", "\n");
        interp.set_builtin("RS", "\n");
        interp.set_builtin("SUBSEP", "\u{1c}");
        interp.set_builtin("FILENAME", "");
        interp.set_builtin("RSTART", "0");
        interp.set_builtin("RLENGTH", "0");
        if let Some(fs) = config.fs {
            interp.set_builtin("FS", &fs);
        }
        interp.cmdline = config
            .assignments
            .into_iter()
            .map(|(name, value)| (name, interp.value_from_input(&value)))
            .collect();
        interp
    }

    /// Install a built-in variable with a plain string value.
    fn set_builtin(&mut self, name: &str, value: &str) {
        self.vars.insert(
            name.to_string(),
            Cell {
                scalar: Some(Value::str(value.to_string())),
                array: None,
            },
        );
    }

    /// Create an input-derived value (strnum when it looks like a number).
    fn value_from_input(&self, text: &str) -> Value {
        match parse_full_number(text) {
            Some(value) => Value::StrNum(Rc::from(text), value),
            None => Value::str(text.to_string()),
        }
    }

    /// Read a variable, honouring command-line assignments.
    fn get_scalar(&self, name: &str) -> Value {
        if let Some(cell) = self.vars.get(name) {
            if cell.array.is_some() {
                return Value::str(String::new());
            }
            if let Some(value) = &cell.scalar {
                return value.clone();
            }
        }
        for (key, value) in &self.cmdline {
            if key == name {
                return value.clone();
            }
        }
        // A variable that was never set and is not a built-in is an empty
        // string; awk treats it as both 0 and "".
        Value::StrNum(Rc::from(""), 0.0)
    }

    /// Write a scalar variable, keeping the command-line list in sync.
    fn set_scalar(&mut self, name: &str, value: Value) {
        if let Some((_, entry)) = self.cmdline.iter_mut().find(|(key, _)| key == name) {
            *entry = value.clone();
        }
        let cell = self.vars.entry(name.to_string()).or_default();
        cell.array = None;
        cell.scalar = Some(value);
    }

    /// Mutable access to an array, creating it when needed.
    fn array_mut(&mut self, name: &str) -> Result<&mut HashMap<String, Value>, String> {
        let cell = self.vars.entry(name.to_string()).or_default();
        if cell.array.is_none() {
            if cell.scalar.is_some() && !cell.scalar.as_ref().is_some_and(|v| v.text().is_empty()) {
                return Err(format!(
                    "can't use variable {name} as an array and a scalar at the same time"
                ));
            }
            cell.array = Some(HashMap::new());
            cell.scalar = None;
        }
        cell.array
            .as_mut()
            .ok_or_else(|| format!("can't use variable {name} as an array"))
    }

    /// Read-only access to an array; a missing array reads as empty.
    fn array_ref(&self, name: &str) -> Option<&HashMap<String, Value>> {
        self.vars.get(name).and_then(|cell| cell.array.as_ref())
    }

    /// Array element value (empty string when absent).
    fn get_element(&self, name: &str, index: &str) -> Value {
        self.array_ref(name)
            .and_then(|array| array.get(index))
            .cloned()
            .unwrap_or_else(|| Value::StrNum(Rc::from(""), 0.0))
    }

    /// Store an array element.
    fn set_element(&mut self, name: &str, index: &str, value: Value) -> Result<(), String> {
        self.array_mut(name)?.insert(index.to_string(), value);
        Ok(())
    }

    /// Compile (and cache) a regular expression from pattern text.
    fn compile_regex(&mut self, pattern: &str) -> Result<Rc<Regex>, String> {
        if let Some(regex) = self.regex_cache.get(pattern) {
            return Ok(Rc::clone(regex));
        }
        let regex = Rc::new(
            Regex::new(pattern)
                .map_err(|message| format!("invalid regular expression: {message}"))?,
        );
        if self.regex_cache.len() < 1024 {
            self.regex_cache
                .insert(pattern.to_string(), Rc::clone(&regex));
        }
        Ok(regex)
    }

    /// Consume one step from the budget.
    fn step(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(format!("step limit exceeded (MAX_STEPS = {MAX_STEPS})"));
        }
        Ok(())
    }

    /// Append text to standard output, enforcing [`MAX_OUTPUT_BYTES`].
    fn write(&mut self, text: &str) -> Result<(), String> {
        if self.out.len().saturating_add(text.len()) > MAX_OUTPUT_BYTES {
            return Err(format!(
                "output limit exceeded (MAX_OUTPUT_BYTES = {MAX_OUTPUT_BYTES})"
            ));
        }
        self.out.push_str(text);
        Ok(())
    }

    /// Run the whole program: `BEGIN`, the main loop, then `END`.
    fn execute(&mut self, program: &Program) -> Result<(), String> {
        self.range_active = vec![false; program.rules.len()];
        for statement in &program.begin {
            self.step()?;
            if self.exec(statement)? == Flow::Exit {
                self.exiting = true;
                break;
            }
        }
        if !self.exiting {
            self.main_loop(program)?;
        }
        self.run_end(program)
    }

    /// Read every input source and apply the main rules to each record.
    fn main_loop(&mut self, program: &Program) -> Result<(), String> {
        let sources: Vec<SourceSpec> = std::mem::take(&mut self.sources);
        for source in &sources {
            for (name, value) in &source.assigns {
                self.set_scalar(name, self.value_from_input(value));
            }
            let (name, text) = match &source.name {
                None => (String::new(), self.stdin.to_string()),
                Some(path) => match (self.read_file)(path) {
                    Some(text) => (path.clone(), text),
                    None => return Err(format!("can't open file {path}")),
                },
            };
            if text.len() > MAX_INPUT_BYTES {
                return Err(format!(
                    "input too large (MAX_INPUT_BYTES = {MAX_INPUT_BYTES})"
                ));
            }
            self.filename = name.clone();
            self.set_scalar("FILENAME", Value::str(name));
            self.fnr = 0;
            let records = split_records(&text)?;
            for record in records {
                self.records += 1;
                if self.records > MAX_RECORDS {
                    return Err(format!(
                        "record limit exceeded (MAX_RECORDS = {MAX_RECORDS})"
                    ));
                }
                self.nr += 1;
                self.fnr += 1;
                self.set_scalar("NR", Value::Num(self.nr as f64));
                self.set_scalar("FNR", Value::Num(self.fnr as f64));
                self.set_record(record)?;
                if self.run_rules(program)? == Flow::Exit {
                    self.exiting = true;
                    return Ok(());
                }
            }
        }
        let trailing: Vec<(String, String)> = std::mem::take(&mut self.trailing);
        for (name, value) in &trailing {
            self.set_scalar(name, self.value_from_input(value));
        }
        Ok(())
    }

    /// Evaluate the main rules for the current record.
    fn run_rules(&mut self, program: &Program) -> Result<Flow, String> {
        self.step()?;
        for (index, rule) in program.rules.iter().enumerate() {
            if rule.range_end {
                let stop = match &rule.pattern {
                    Some(Pattern::Expr(expr)) => self.eval(expr)?.truthy(),
                    _ => true,
                };
                if stop
                    && let Some(previous) = index.checked_sub(1)
                    && let Some(slot) = self.range_active.get_mut(previous)
                {
                    *slot = false;
                }
                continue;
            }
            let matched = match &rule.pattern {
                None => true,
                Some(Pattern::Expr(expr)) => {
                    if rule.range_start {
                        if self.range_active.get(index).copied().unwrap_or(false) {
                            true
                        } else {
                            let value = self.eval(expr)?;
                            if value.truthy() {
                                if let Some(slot) = self.range_active.get_mut(index) {
                                    *slot = true;
                                }
                                true
                            } else {
                                false
                            }
                        }
                    } else {
                        self.eval(expr)?.truthy()
                    }
                }
                Some(Pattern::Range(_, _)) => false,
            };
            if !matched {
                continue;
            }
            match &rule.action {
                Some(statements) => {
                    for statement in statements {
                        self.step()?;
                        match self.exec(statement)? {
                            Flow::Normal => {}
                            Flow::Exit => return Ok(Flow::Exit),
                            Flow::Next => return Ok(Flow::Next),
                            Flow::Break | Flow::Continue => {
                                return Err("break or continue used outside a loop".to_string());
                            }
                        }
                    }
                }
                None => {
                    let record = self.record.line.clone();
                    self.write(&record)?;
                    let ors = self.ors();
                    self.write(&ors)?;
                }
            }
        }
        Ok(Flow::Normal)
    }

    /// Run the `END` rules once.
    fn run_end(&mut self, program: &Program) -> Result<(), String> {
        if self.end_ran {
            return Ok(());
        }
        self.end_ran = true;
        for statement in &program.end {
            self.step()?;
            if self.exec(statement)? == Flow::Exit {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Current `ORS`.
    fn ors(&self) -> String {
        let value = self.get_scalar("ORS");
        value.text()
    }

    /// Current `OFS`.
    fn ofs(&self) -> String {
        let value = self.get_scalar("OFS");
        value.text()
    }

    /// Current `SUBSEP`.
    fn subsep(&self) -> String {
        let value = self.get_scalar("SUBSEP");
        value.text()
    }

    /// Install a new record: set `$0` and split it into fields.
    fn set_record(&mut self, line: String) -> Result<(), String> {
        self.record.set_line(line);
        self.split_current_record()
    }

    /// Split `$0` into fields according to `FS`.
    fn split_current_record(&mut self) -> Result<(), String> {
        let line = self.record.line.clone();
        let spec = self.field_separator()?;
        let fields = match spec {
            FsSpec::Default => split_blanks(&line),
            FsSpec::Empty => line.chars().map(|c| c.to_string()).collect(),
            FsSpec::Char(c) => {
                if line.is_empty() {
                    Vec::new()
                } else {
                    line.split(c).map(|piece| piece.to_string()).collect()
                }
            }
            FsSpec::Regex(regex) => {
                if line.is_empty() {
                    Vec::new()
                } else {
                    regex.split(&line)
                }
            }
        };
        self.record.nf = fields.len();
        self.record.fields = fields;
        self.sync_nf();
        Ok(())
    }

    /// Rebuild `$0` from the fields, joined with `OFS`.
    fn rebuild_record(&mut self) {
        let ofs = self.ofs();
        self.record.line = self.record.fields.join(&ofs);
    }

    /// Publish `NF` after it changed.
    fn sync_nf(&mut self) {
        let nf = self.record.nf;
        self.set_scalar("NF", Value::Num(nf as f64));
    }

    /// Resolve the current `FS` value to a splitting strategy.
    fn field_separator(&mut self) -> Result<FsSpec, String> {
        let text = self.get_scalar("FS").text();
        if text == " " {
            return Ok(FsSpec::Default);
        }
        if text.is_empty() {
            return Ok(FsSpec::Empty);
        }
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(single), None) => {
                if single == '\\' {
                    Ok(FsSpec::Char('\\'))
                } else {
                    Ok(FsSpec::Char(single))
                }
            }
            _ => {
                let regex = self.compile_regex(&text)?;
                Ok(FsSpec::Regex(regex))
            }
        }
    }
}

/// Split one input text into records. Only `RS = "\n"` is supported.
///
/// A trailing newline does not create an extra empty record, and a completely
/// empty input produces no records at all.
fn split_records(text: &str) -> Result<Vec<String>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    Ok(trimmed.split('\n').map(|line| line.to_string()).collect())
}

impl<'a> Interp<'a> {
    /// Execute one statement, reporting run-time failures with a line number.
    fn exec(&mut self, statement: &Stmt) -> Result<Flow, String> {
        match self.exec_inner(statement) {
            Ok(flow) => Ok(flow),
            Err(message) => {
                if message.starts_with("line ") {
                    Err(message)
                } else {
                    Err(format!("line {}: {message}", statement.line))
                }
            }
        }
    }

    /// Execute one statement.
    fn exec_inner(&mut self, statement: &Stmt) -> Result<Flow, String> {
        match &statement.kind {
            StmtKind::Empty => Ok(Flow::Normal),
            StmtKind::Block(statements) => {
                for inner in statements {
                    self.step()?;
                    match self.exec(inner)? {
                        Flow::Normal => {}
                        other => return Ok(other),
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::Expr(expr) => {
                self.eval(expr)?;
                Ok(Flow::Normal)
            }
            StmtKind::Print(args) => {
                let text = self.render_print(args)?;
                self.write(&text)?;
                Ok(Flow::Normal)
            }
            StmtKind::Printf(args) => {
                let text = self.render_printf(args)?;
                self.write(&text)?;
                Ok(Flow::Normal)
            }
            StmtKind::If(condition, then_branch, else_branch) => {
                if self.eval(condition)?.truthy() {
                    self.exec(then_branch)
                } else if let Some(other) = else_branch {
                    self.exec(other)
                } else {
                    Ok(Flow::Normal)
                }
            }
            StmtKind::While(condition, body) => {
                loop {
                    self.step()?;
                    if !self.eval(condition)?.truthy() {
                        break;
                    }
                    match self.exec(body)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        other => return Ok(other),
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::DoWhile(body, condition) => {
                loop {
                    self.step()?;
                    match self.exec(body)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        other => return Ok(other),
                    }
                    self.step()?;
                    if !self.eval(condition)?.truthy() {
                        break;
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::For(init, condition, post, body) => {
                if let Some(init) = init {
                    self.step()?;
                    match self.exec(init)? {
                        Flow::Normal => {}
                        other => return Ok(other),
                    }
                }
                loop {
                    self.step()?;
                    if let Some(condition) = condition
                        && !self.eval(condition)?.truthy()
                    {
                        break;
                    }
                    match self.exec(body)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        other => return Ok(other),
                    }
                    if let Some(post) = post {
                        self.step()?;
                        match self.exec(post)? {
                            Flow::Normal => {}
                            other => return Ok(other),
                        }
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::ForIn(key, array, body) => {
                let keys = self.sorted_keys(array)?;
                for index in keys {
                    self.set_scalar(key, Value::str(index));
                    self.step()?;
                    match self.exec(body)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        other => return Ok(other),
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::Break => Ok(Flow::Break),
            StmtKind::Continue => Ok(Flow::Continue),
            StmtKind::Next => Ok(Flow::Next),
            StmtKind::Exit(code) => {
                if let Some(code) = code {
                    self.exit_code = to_i64(self.eval(code)?.num()) as i32;
                }
                Ok(Flow::Exit)
            }
            StmtKind::Delete(name, index) => {
                match index {
                    Some(index) => {
                        let key = self.eval(index)?.text();
                        if let Some(array) = self.array_ref(name)
                            && array.contains_key(&key)
                        {
                            self.array_mut(name)?.remove(&key);
                        }
                    }
                    None => {
                        if self.vars.get(name).is_some_and(|cell| cell.array.is_some()) {
                            self.array_mut(name)?.clear();
                        }
                    }
                }
                Ok(Flow::Normal)
            }
        }
    }

    /// Array keys in sorted order, for deterministic `for (k in a)`.
    fn sorted_keys(&mut self, name: &str) -> Result<Vec<String>, String> {
        let mut keys: Vec<String> = match self.array_ref(name) {
            Some(array) => array.keys().cloned().collect(),
            None => Vec::new(),
        };
        keys.sort();
        Ok(keys)
    }

    /// Build the text written by one `print` statement.
    fn render_print(&mut self, args: &[Expr]) -> Result<String, String> {
        let mut text = String::new();
        if args.is_empty() {
            text.push_str(&self.record.line);
        } else {
            let ofs = self.ofs();
            let mut first = true;
            for arg in args {
                if !first {
                    text.push_str(&ofs);
                }
                first = false;
                let value = self.eval(arg)?;
                text.push_str(&value.text());
            }
        }
        let ors = self.ors();
        text.push_str(&ors);
        Ok(text)
    }

    /// Build the text written by one `printf` statement.
    fn render_printf(&mut self, args: &[Expr]) -> Result<String, String> {
        let mut values: Vec<Value> = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg)?);
        }
        let format = match values.first() {
            Some(value) => value.text(),
            None => String::new(),
        };
        format_values(&format, &values[1..])
    }
}

impl<'a> Interp<'a> {
    /// Evaluate an expression.
    fn eval(&mut self, expr: &Expr) -> Result<Value, String> {
        match expr {
            Expr::Number(value) => Ok(Value::Num(*value)),
            Expr::Str(text) => Ok(Value::str(text.clone())),
            Expr::Regex(regex) => {
                let matched = regex.is_match(&self.record.line);
                Ok(Value::Num(bool_num(matched)))
            }
            Expr::Subsep(parts) => {
                let separator = self.subsep();
                let mut text = String::new();
                let mut first = true;
                for part in parts {
                    if !first {
                        text.push_str(&separator);
                    }
                    first = false;
                    let value = self.eval(part)?;
                    text.push_str(&value.text());
                }
                Ok(Value::str(text))
            }
            Expr::Field(index) => {
                let value = self.eval(index)?;
                let position = to_i64(value.num());
                if position < 0 {
                    return Err("negative field index".to_string());
                }
                if position as usize > MAX_FIELDS {
                    return Err(format!(
                        "field index out of range (MAX_FIELDS = {MAX_FIELDS})"
                    ));
                }
                Ok(self.value_from_input(&self.record.field(position as usize)))
            }
            Expr::Var(lvalue) => self.read_lvalue(lvalue),
            Expr::Assign(lvalue, value) => {
                let target = self.resolve_target(lvalue)?;
                let value = self.eval(value)?;
                self.write_target(&target, value.clone())?;
                Ok(value)
            }
            Expr::AssignOp(lvalue, op, value) => {
                let target = self.resolve_target(lvalue)?;
                let left = self.read_target(&target)?.num();
                let right = self.eval(value)?.num();
                let result = match op {
                    AssignOp::Add => left + right,
                    AssignOp::Sub => left - right,
                    AssignOp::Mul => left * right,
                    AssignOp::Div => {
                        if right == 0.0 {
                            return Err("division by zero".to_string());
                        }
                        left / right
                    }
                    AssignOp::Mod => {
                        if right == 0.0 {
                            return Err("division by zero in %".to_string());
                        }
                        left % right
                    }
                    AssignOp::Pow => left.powf(right),
                };
                let value = Value::Num(result);
                self.write_target(&target, value.clone())?;
                Ok(value)
            }
            Expr::PreInc(lvalue, delta) => {
                let target = self.resolve_target(lvalue)?;
                let value = Value::Num(self.read_target(&target)?.num() + delta);
                self.write_target(&target, value.clone())?;
                Ok(value)
            }
            Expr::PostInc(lvalue, delta) => {
                let target = self.resolve_target(lvalue)?;
                let previous = self.read_target(&target)?;
                let value = Value::Num(previous.num() + delta);
                self.write_target(&target, value)?;
                Ok(previous)
            }
            Expr::Ternary(condition, yes, no) => {
                if self.eval(condition)?.truthy() {
                    self.eval(yes)
                } else {
                    self.eval(no)
                }
            }
            Expr::Or(left, right) => {
                if self.eval(left)?.truthy() {
                    return Ok(Value::Num(1.0));
                }
                Ok(Value::Num(bool_num(self.eval(right)?.truthy())))
            }
            Expr::And(left, right) => {
                if !self.eval(left)?.truthy() {
                    return Ok(Value::Num(0.0));
                }
                Ok(Value::Num(bool_num(self.eval(right)?.truthy())))
            }
            Expr::InArray(index, array) => {
                let key = self.eval(index)?.text();
                let present = self
                    .array_ref(array)
                    .is_some_and(|map| map.contains_key(&key));
                Ok(Value::Num(bool_num(present)))
            }
            Expr::MatchOp(left, right) => {
                let text = self.eval(left)?.text();
                let regex = self.eval_regex(right)?;
                Ok(Value::Num(bool_num(regex.is_match(&text))))
            }
            Expr::NotMatchOp(left, right) => {
                let text = self.eval(left)?.text();
                let regex = self.eval_regex(right)?;
                Ok(Value::Num(bool_num(!regex.is_match(&text))))
            }
            Expr::Compare(left, op, right) => {
                let left = self.eval(left)?;
                let right = self.eval(right)?;
                Ok(Value::Num(bool_num(compare(&left, *op, &right))))
            }
            Expr::Concat(parts) => {
                let mut text = String::new();
                for part in parts {
                    let value = self.eval(part)?;
                    text.push_str(&value.text());
                }
                Ok(Value::str(text))
            }
            Expr::Binary(left, op, right) => {
                let left = self.eval(left)?.num();
                let right = self.eval(right)?.num();
                let result = match op {
                    BinaryOp::Add => left + right,
                    BinaryOp::Sub => left - right,
                    BinaryOp::Mul => left * right,
                    BinaryOp::Div => {
                        if right == 0.0 {
                            return Err("division by zero".to_string());
                        }
                        left / right
                    }
                    BinaryOp::Mod => {
                        if right == 0.0 {
                            return Err("division by zero in %".to_string());
                        }
                        left % right
                    }
                    BinaryOp::Pow => left.powf(right),
                };
                Ok(Value::Num(result))
            }
            Expr::Neg(operand) => Ok(Value::Num(-self.eval(operand)?.num())),
            Expr::Pos(operand) => Ok(Value::Num(self.eval(operand)?.num())),
            Expr::Not(operand) => Ok(Value::Num(bool_num(!self.eval(operand)?.truthy()))),
            Expr::Call(builtin, args) => self.call(*builtin, args),
        }
    }

    /// Evaluate an expression used as a regular expression.
    fn eval_regex(&mut self, expr: &Expr) -> Result<Rc<Regex>, String> {
        match expr {
            Expr::Regex(regex) => Ok(Rc::clone(regex)),
            other => {
                let pattern = self.eval(other)?.text();
                self.compile_regex(&pattern)
            }
        }
    }

    /// Evaluate the subscript of an assignable location.
    fn resolve_target(&mut self, lvalue: &LValue) -> Result<Target, String> {
        match lvalue {
            LValue::Scalar(name) => Ok(Target::Scalar(name.clone())),
            LValue::Element(name, index) => {
                let key = self.eval(index)?.text();
                Ok(Target::Element(name.clone(), key))
            }
            LValue::Field(index) => Ok(Target::Field(self.field_position(index)?)),
        }
    }

    /// Evaluate and range-check a field index.
    fn field_position(&mut self, index: &Expr) -> Result<usize, String> {
        let value = self.eval(index)?;
        let position = to_i64(value.num());
        if position < 0 {
            return Err("negative field index".to_string());
        }
        if position as usize > MAX_FIELDS {
            return Err(format!(
                "field index out of range (MAX_FIELDS = {MAX_FIELDS})"
            ));
        }
        Ok(position as usize)
    }

    /// Read the current value of a resolved location.
    fn read_target(&mut self, target: &Target) -> Result<Value, String> {
        match target {
            Target::Scalar(name) => {
                if name == "NF" {
                    self.sync_nf();
                }
                Ok(self.get_scalar(name))
            }
            Target::Element(name, key) => Ok(self.get_element(name, key)),
            Target::Field(position) => Ok(self.value_from_input(&self.record.field(*position))),
        }
    }

    /// Read the current value of an assignable location.
    fn read_lvalue(&mut self, lvalue: &LValue) -> Result<Value, String> {
        let target = self.resolve_target(lvalue)?;
        // Referencing an element brings it into existence, which is what
        // makes `if (a[i] == "")` work the way awk's does.
        if let Target::Element(name, key) = &target
            && !self
                .array_ref(name)
                .is_some_and(|map| map.contains_key(key))
        {
            self.set_element(name, key, Value::StrNum(Rc::from(""), 0.0))?;
        }
        self.read_target(&target)
    }

    /// Store a value into a resolved location.
    fn write_target(&mut self, target: &Target, value: Value) -> Result<(), String> {
        match target {
            Target::Scalar(name) => {
                if name == "RS" && value.text() != "\n" {
                    return Err("only RS = \"\\n\" is supported".to_string());
                }
                if name == "NF" {
                    let new_nf = to_i64(value.num());
                    if new_nf < 0 {
                        return Err("negative value assigned to NF".to_string());
                    }
                    if new_nf as usize > MAX_FIELDS {
                        return Err(format!("NF out of range (MAX_FIELDS = {MAX_FIELDS})"));
                    }
                    self.record.nf = new_nf as usize;
                    self.record.ensure(self.record.nf);
                    self.record.fields.truncate(self.record.nf);
                    self.rebuild_record();
                    self.set_scalar("NF", Value::Num(self.record.nf as f64));
                    return Ok(());
                }
                self.set_scalar(name, value);
                Ok(())
            }
            Target::Element(name, key) => self.set_element(name, key, value),
            Target::Field(position) => {
                let position = *position;
                if position == 0 {
                    self.record.line = value.text();
                    self.split_current_record()?;
                    return Ok(());
                }
                self.record.ensure(position);
                let text = value.text();
                match self.record.fields.get_mut(position - 1) {
                    Some(slot) => *slot = text,
                    None => return Err("field index out of range".to_string()),
                }
                self.rebuild_record();
                self.sync_nf();
                Ok(())
            }
        }
    }

    /// Store a value into an assignable location.
    fn write_lvalue(&mut self, lvalue: &LValue, value: Value) -> Result<(), String> {
        let target = self.resolve_target(lvalue)?;
        self.write_target(&target, value)
    }

    /// Call a built-in function.
    fn call(&mut self, builtin: Builtin, args: &[Expr]) -> Result<Value, String> {
        match builtin {
            Builtin::Length => {
                if args.is_empty() {
                    return Ok(Value::Num(self.record.line.chars().count() as f64));
                }
                if let Expr::Var(lvalue) = &args[0]
                    && let LValue::Scalar(name) = lvalue.as_ref()
                    && let Some(array) = self.array_ref(name)
                {
                    return Ok(Value::Num(array.len() as f64));
                }
                let value = self.eval(&args[0])?;
                Ok(Value::Num(value.text().chars().count() as f64))
            }
            Builtin::Substr => {
                let text = self.eval(&args[0])?.text();
                let start = self.eval(&args[1])?.num();
                let chars: Vec<char> = text.chars().collect();
                let total = chars.len() as f64;
                let begin = start.trunc();
                let begin = if begin < 1.0 { 1.0 } else { begin };
                let begin = begin.min(total + 1.0);
                let from = (begin as usize).saturating_sub(1);
                let length = if args.len() > 2 {
                    let requested = self.eval(&args[2])?.num().trunc();
                    if requested <= 0.0 {
                        0
                    } else {
                        requested as usize
                    }
                } else {
                    chars.len().saturating_sub(from)
                };
                let end = from.saturating_add(length).min(chars.len());
                let slice: String = chars.get(from..end).unwrap_or(&[]).iter().collect();
                Ok(Value::str(slice))
            }
            Builtin::Index => {
                let haystack = self.eval(&args[0])?.text();
                let needle = self.eval(&args[1])?.text();
                let position = match (needle.is_empty(), haystack.find(&needle)) {
                    (true, _) => 1.0,
                    (false, Some(byte_offset)) => {
                        (haystack.get(..byte_offset).unwrap_or("").chars().count() + 1) as f64
                    }
                    (false, None) => 0.0,
                };
                Ok(Value::Num(position))
            }
            Builtin::Split => {
                let text = self.eval(&args[0])?.text();
                let name = match &args[1] {
                    Expr::Var(lvalue) => match lvalue.as_ref() {
                        LValue::Scalar(name) => name.clone(),
                        LValue::Element(..) => {
                            return Err(
                                "split() requires an array name as its second argument".to_string()
                            );
                        }
                        LValue::Field(_) => {
                            return Err(
                                "split() requires an array name as its second argument".to_string()
                            );
                        }
                    },
                    _ => {
                        return Err(
                            "split() requires an array name as its second argument".to_string()
                        );
                    }
                };
                let spec = if args.len() > 2 {
                    match &args[2] {
                        Expr::Regex(regex) => FsSpec::Regex(Rc::clone(regex)),
                        other => {
                            let separator = self.eval(other)?.text();
                            self.separator_spec(&separator)?
                        }
                    }
                } else {
                    self.field_separator()?
                };
                let pieces = apply_separator(&spec, &text);
                self.array_mut(&name)?.clear();
                for (index, piece) in pieces.iter().enumerate() {
                    let value = self.value_from_input(piece);
                    self.array_mut(&name)?
                        .insert((index + 1).to_string(), value);
                }
                Ok(Value::Num(pieces.len() as f64))
            }
            Builtin::Sub | Builtin::Gsub => {
                let global = builtin == Builtin::Gsub;
                let regex = self.eval_regex(&args[0])?;
                let replacement = self.eval(&args[1])?.text();
                let outcome = if args.len() > 2 {
                    let lvalue = as_target(&args[2])?;
                    let text = self.read_lvalue(&lvalue)?.text();
                    let (replaced, count) = substitute(&regex, &text, &replacement, global);
                    if count > 0 {
                        self.write_lvalue(&lvalue, Value::str(replaced))?;
                    }
                    count
                } else {
                    let (replaced, count) =
                        substitute(&regex, &self.record.line, &replacement, global);
                    if count > 0 {
                        self.record.line = replaced;
                        self.split_current_record()?;
                    }
                    count
                };
                Ok(Value::Num(outcome as f64))
            }
            Builtin::Match => {
                let text = self.eval(&args[0])?.text();
                let regex = self.eval_regex(&args[1])?;
                let (start, length) = match regex.find(&text) {
                    Some((first, last)) => {
                        let before = text.get(..first).unwrap_or("");
                        let matched = text.get(first..last).unwrap_or("");
                        (
                            (before.chars().count() + 1) as f64,
                            matched.chars().count() as f64,
                        )
                    }
                    None => (0.0, -1.0),
                };
                self.set_scalar("RSTART", Value::Num(start));
                self.set_scalar("RLENGTH", Value::Num(length));
                Ok(Value::Num(start))
            }
            Builtin::Sprintf => {
                let mut values: Vec<Value> = Vec::with_capacity(args.len());
                for arg in args {
                    values.push(self.eval(arg)?);
                }
                let format = match values.first() {
                    Some(value) => value.text(),
                    None => String::new(),
                };
                Ok(Value::str(format_values(&format, &values[1..])?))
            }
            Builtin::Tolower => {
                let text = self.eval(&args[0])?.text();
                Ok(Value::str(text.to_lowercase()))
            }
            Builtin::Toupper => {
                let text = self.eval(&args[0])?.text();
                Ok(Value::str(text.to_uppercase()))
            }
            Builtin::Int => {
                let value = self.eval(&args[0])?.num();
                Ok(Value::Num(value.trunc()))
            }
            Builtin::Sqrt => {
                let value = self.eval(&args[0])?.num();
                if value < 0.0 {
                    return Err("sqrt of a negative value".to_string());
                }
                Ok(Value::Num(value.sqrt()))
            }
            Builtin::Exp => {
                let value = self.eval(&args[0])?.num();
                Ok(Value::Num(value.exp()))
            }
            Builtin::Log => {
                let value = self.eval(&args[0])?.num();
                if value <= 0.0 {
                    return Err("log of a non-positive value".to_string());
                }
                Ok(Value::Num(value.ln()))
            }
            Builtin::Sin => {
                let value = self.eval(&args[0])?.num();
                Ok(Value::Num(value.sin()))
            }
            Builtin::Cos => {
                let value = self.eval(&args[0])?.num();
                Ok(Value::Num(value.cos()))
            }
            Builtin::Atan2 => {
                let y = self.eval(&args[0])?.num();
                let x = self.eval(&args[1])?.num();
                Ok(Value::Num(y.atan2(x)))
            }
            Builtin::Rand => Ok(Value::Num(self.next_random())),
            Builtin::Srand => {
                let previous = self.random_seed;
                let seed = if args.is_empty() {
                    self.random_seed
                } else {
                    let value = self.eval(&args[0])?.num();
                    to_i64(value) as u64
                };
                self.random_seed = seed;
                self.random = seed;
                Ok(Value::Num(previous as f64))
            }
        }
    }

    /// Next value of the deterministic `rand()` sequence, in `[0, 1)`.
    fn next_random(&mut self) -> f64 {
        self.random = self
            .random
            .wrapping_mul(LCG_MULTIPLIER)
            .wrapping_add(LCG_INCREMENT);
        let bits = self.random >> 11;
        (bits as f64) / ((1u64 << 53) as f64)
    }

    /// Resolve an explicit separator string into a splitting strategy.
    fn separator_spec(&mut self, text: &str) -> Result<FsSpec, String> {
        if text == " " {
            return Ok(FsSpec::Default);
        }
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            _ if text.is_empty() => Ok(FsSpec::Empty),
            (Some(single), None) => {
                let restored = if single == '\\' { '\\' } else { single };
                Ok(FsSpec::Char(restored))
            }
            _ => Ok(FsSpec::Regex(self.compile_regex(text)?)),
        }
    }
}

/// Split `text` with a resolved separator strategy.
fn apply_separator(spec: &FsSpec, text: &str) -> Vec<String> {
    match spec {
        FsSpec::Default => split_blanks(text),
        FsSpec::Empty => text.chars().map(|c| c.to_string()).collect(),
        FsSpec::Char(c) => text.split(*c).map(|piece| piece.to_string()).collect(),
        FsSpec::Regex(regex) => regex.split(text),
    }
}

/// Extract an assignable target from a `sub`/`gsub` argument.
fn as_target(expr: &Expr) -> Result<LValue, String> {
    match expr {
        Expr::Var(lvalue) => Ok(lvalue.as_ref().clone()),
        Expr::Field(index) => Ok(LValue::Field(index.clone())),
        _ => Err("sub()/gsub() require a variable or field as their target".to_string()),
    }
}

/// Compare two values the way awk does: numerically when both sides are
/// numeric (numbers or numeric strings) and lexically otherwise.
fn compare(left: &Value, op: CompareOp, right: &Value) -> bool {
    if left.is_numeric() && right.is_numeric() {
        let left = left.num();
        let right = right.num();
        return match op {
            CompareOp::Lt => left < right,
            CompareOp::Le => left <= right,
            CompareOp::Gt => left > right,
            CompareOp::Ge => left >= right,
            CompareOp::Eq => left == right,
            CompareOp::Ne => left != right,
        };
    }
    let left = left.text();
    let right = right.text();
    match op {
        CompareOp::Lt => left < right,
        CompareOp::Le => left <= right,
        CompareOp::Gt => left > right,
        CompareOp::Ge => left >= right,
        CompareOp::Eq => left == right,
        CompareOp::Ne => left != right,
    }
}

/// Replace matches of `regex` in `text`, returning the new text and the number
/// of replacements. `&` and `\&` insert the matched text literally; the
/// backslash is not otherwise special, as in awk.
fn substitute(regex: &Regex, text: &str, replacement: &str, global: bool) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut count = 0usize;
    let mut previous_end: Option<usize> = None;
    loop {
        if cursor > text.len() {
            break;
        }
        let Some((start, end)) = regex.find(text.get(cursor..).unwrap_or("")) else {
            break;
        };
        let start = cursor + start;
        let end = cursor + end;
        if start == end && previous_end == Some(start) {
            match next_char_end(text, start) {
                Some(next) => {
                    cursor = next;
                    continue;
                }
                None => break,
            }
        }
        out.push_str(text.get(cursor..start).unwrap_or(""));
        push_replacement(&mut out, text.get(start..end).unwrap_or(""), replacement);
        count += 1;
        previous_end = Some(end);
        if end == start {
            match next_char_end(text, end) {
                Some(next) => {
                    out.push_str(text.get(end..next).unwrap_or(""));
                    cursor = next;
                }
                None => {
                    cursor = end;
                    break;
                }
            }
        } else {
            cursor = end;
        }
        if !global {
            break;
        }
    }
    out.push_str(text.get(cursor..).unwrap_or(""));
    (out, count)
}

/// Expand one awk replacement (`&`, `\&`, `\\`).
fn push_replacement(out: &mut String, matched: &str, replacement: &str) {
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '&' => out.push_str(matched),
            '\\' => match chars.peek().copied() {
                Some('&') => {
                    chars.next();
                    out.push('&');
                }
                Some('\\') => {
                    chars.next();
                    out.push('\\');
                }
                _ => out.push('\\'),
            },
            other => out.push(other),
        }
    }
}

/// Format a value list with a `printf`-style format string.
///
/// Conversions `%d %i %o %x %X %u %c %s %e %E %f %F %g %G %a %A %%` are
/// supported with the usual flags (`- + space # 0`), width and precision.
/// Extra arguments are ignored, but a conversion with no argument left is
/// `awk: not enough args in printf(FORMAT)`, exactly as awk reports it.
fn format_values(format: &str, args: &[Value]) -> Result<String, String> {
    let chars: Vec<char> = format.chars().collect();
    let mut out = String::with_capacity(format.len());
    let mut index = 0usize;
    let mut next_arg = 0usize;
    while index < chars.len() {
        let c = chars[index];
        if c != '%' {
            out.push(c);
            index += 1;
            continue;
        }
        index += 1;
        if chars.get(index) == Some(&'%') {
            out.push('%');
            index += 1;
            continue;
        }
        let spec = parse_conversion(&chars, &mut index)?;
        let value = match args.get(next_arg) {
            Some(value) => {
                next_arg += 1;
                value.clone()
            }
            None => {
                return Err(format!("not enough args in printf({format})"));
            }
        };
        out.push_str(&render_conversion(&spec, &value)?);
    }
    Ok(out)
}

/// One parsed `printf` conversion.
#[derive(Debug, Clone)]
struct Conversion {
    /// Conversion character.
    kind: char,
    /// `-` flag: left justify.
    left: bool,
    /// `+` flag: always show a sign.
    plus: bool,
    /// ` ` flag: space before positive numbers.
    space: bool,
    /// `#` flag: alternate form.
    alternate: bool,
    /// `0` flag: zero padding.
    zero: bool,
    /// Field width (already clamped).
    width: usize,
    /// Precision, if any (already clamped).
    precision: Option<usize>,
}

/// Parse one conversion specification; `index` points just past the `%`.
fn parse_conversion(chars: &[char], index: &mut usize) -> Result<Conversion, String> {
    let mut spec = Conversion {
        kind: 's',
        left: false,
        plus: false,
        space: false,
        alternate: false,
        zero: false,
        width: 0,
        precision: None,
    };
    loop {
        match chars.get(*index) {
            Some('-') => spec.left = true,
            Some('+') => spec.plus = true,
            Some(' ') => spec.space = true,
            Some('#') => spec.alternate = true,
            Some('0') => spec.zero = true,
            _ => break,
        }
        *index += 1;
    }
    let mut width = 0usize;
    while let Some(digit) = chars.get(*index).and_then(|c| c.to_digit(10)) {
        width = width
            .saturating_mul(10)
            .saturating_add(digit as usize)
            .min(10_000);
        *index += 1;
    }
    spec.width = width;
    if chars.get(*index) == Some(&'.') {
        *index += 1;
        let mut precision = 0usize;
        while let Some(digit) = chars.get(*index).and_then(|c| c.to_digit(10)) {
            precision = precision
                .saturating_mul(10)
                .saturating_add(digit as usize)
                .min(10_000);
            *index += 1;
        }
        spec.precision = Some(precision);
    }
    match chars.get(*index) {
        Some(kind) => {
            spec.kind = *kind;
            *index += 1;
        }
        None => return Err("incomplete printf conversion at end of format".to_string()),
    }
    Ok(spec)
}

/// Render one conversion for one value.
fn render_conversion(spec: &Conversion, value: &Value) -> Result<String, String> {
    let body = match spec.kind {
        'd' | 'i' => {
            let number = to_i64(value.num());
            format_signed(number, spec)
        }
        'o' => {
            let digits = format!("{:o}", to_i64(value.num()) as u64);
            if spec.alternate && !digits.starts_with('0') {
                format!("0{digits}")
            } else {
                digits
            }
        }
        'x' | 'X' => {
            let number = to_i64(value.num()) as u64;
            let digits = format!("{number:x}");
            let digits = if spec.kind == 'X' {
                digits.to_uppercase()
            } else {
                digits
            };
            if spec.alternate && number != 0 {
                if spec.kind == 'X' {
                    format!("0X{digits}")
                } else {
                    format!("0x{digits}")
                }
            } else {
                digits
            }
        }
        'u' => {
            let digits = format!("{}", to_i64(value.num()) as u64);
            digits
        }
        'c' => {
            if value.is_numeric() {
                match char::from_u32(value.num().abs() as u32) {
                    Some(c) => c.to_string(),
                    None => "\0".to_string(),
                }
            } else {
                match value.text().chars().next() {
                    Some(c) => c.to_string(),
                    None => "\0".to_string(),
                }
            }
        }
        's' => {
            let text = value.text();
            match spec.precision {
                Some(limit) => text.chars().take(limit).collect(),
                None => text,
            }
        }
        'e' | 'E' => {
            let precision = spec.precision.unwrap_or(NUMBER_PRECISION);
            let text = format!("{:.*e}", precision, value.num());
            let text = normalize_exponent(&text);
            if spec.kind == 'E' {
                text.to_uppercase()
            } else {
                text
            }
        }
        'f' | 'F' => {
            let precision = spec.precision.unwrap_or(NUMBER_PRECISION);
            format!("{:.*}", precision, value.num())
        }
        'g' | 'G' => {
            let precision = spec.precision.unwrap_or(NUMBER_PRECISION).max(1);
            let text = format_g(value.num(), precision, spec.alternate);
            if spec.kind == 'G' {
                text.to_uppercase()
            } else {
                text
            }
        }
        'a' | 'A' => {
            let text = format!("{:x?}", value.num());
            if spec.kind == 'A' {
                text.to_uppercase()
            } else {
                text
            }
        }
        other => {
            return Err(format!("unsupported printf conversion %{other}"));
        }
    };
    Ok(pad(body, spec))
}

/// Render a signed integer with the `+`/` `/`0` flags applied.
fn format_signed(number: i64, spec: &Conversion) -> String {
    let magnitude = number.unsigned_abs();
    let prefix = if number < 0 {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    };
    let digits = format!("{magnitude}");
    let digits = match spec.precision {
        Some(precision) if digits.len() < precision => {
            format!("{}{}", "0".repeat(precision - digits.len()), digits)
        }
        _ => digits,
    };
    format!("{prefix}{digits}")
}

/// Apply width and justification to a rendered conversion.
fn pad(body: String, spec: &Conversion) -> String {
    let width = body.chars().count();
    if width >= spec.width {
        return body;
    }
    let fill = spec.width - width;
    if spec.left {
        let mut out = body;
        out.push_str(&" ".repeat(fill));
        return out;
    }
    let zero_pad = spec.zero
        && matches!(
            spec.kind,
            'd' | 'i' | 'o' | 'x' | 'X' | 'u' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G'
        );
    if zero_pad {
        let (sign, rest) = match body.strip_prefix('-') {
            Some(rest) => ("-", rest.to_string()),
            None => match body.strip_prefix('+') {
                Some(rest) => ("+", rest.to_string()),
                None => match body.strip_prefix(' ') {
                    Some(rest) => (" ", rest.to_string()),
                    None => ("", body),
                },
            },
        };
        return format!("{sign}{}{rest}", "0".repeat(fill));
    }
    format!("{}{}", " ".repeat(fill), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a program against stdin with no file operands.
    fn run(program: &str, input: &str) -> AwkRun {
        super::run(&[program.to_string()], input, &|_| None)
    }

    /// Run a program and require clean output.
    fn ok(program: &str, input: &str) -> String {
        let result = run(program, input);
        assert_eq!(
            result.exit_code, 0,
            "program {program:?}: {}",
            result.stderr
        );
        assert!(
            result.stderr.is_empty(),
            "program {program:?} wrote stderr: {}",
            result.stderr
        );
        result.stdout
    }

    /// Run with explicit arguments and a file table.
    fn run_files(args: &[&str], input: &str, files: &[(&str, &str)]) -> AwkRun {
        let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let table: Vec<(String, String)> = files
            .iter()
            .map(|(name, body)| (name.to_string(), body.to_string()))
            .collect();
        let reader = move |path: &str| {
            table
                .iter()
                .find(|(name, _)| name == path)
                .map(|(_, body)| body.clone())
        };
        super::run(&owned, input, &reader)
    }

    fn err(program: &str, input: &str) -> AwkRun {
        let result = run(program, input);
        assert_eq!(result.exit_code, 2, "expected failure for {program:?}");
        assert!(!result.stderr.is_empty(), "{program:?} needs a message");
        assert!(result.stderr.contains("awk:"), "{:?}", result.stderr);
        result
    }

    // ── Fields ──────────────────────────────────────────────────────────────

    #[test]
    fn prints_fields_split_on_whitespace() {
        assert_eq!(ok("{print $1}", "alpha beta\n"), "alpha\n");
        assert_eq!(ok("{print $2}", "alpha beta gamma\n"), "beta\n");
        assert_eq!(ok("{print $3}", "a b\n"), "\n");
    }

    #[test]
    fn whitespace_fs_skips_leading_and_trailing_blanks() {
        assert_eq!(ok("{print NF}", "   a   b   \n"), "2\n");
        assert_eq!(ok("{print $1}", "\t\tx\n"), "x\n");
    }

    #[test]
    fn bare_print_prints_the_whole_record() {
        assert_eq!(ok("{print}", "one two\n"), "one two\n");
        assert_eq!(ok("1", "keep\n"), "keep\n");
    }

    #[test]
    fn assigning_a_field_rebuilds_the_record_with_ofs() {
        assert_eq!(
            ok("BEGIN{OFS=\"-\"} {$2=\"X\"; print}", "a b c\n"),
            "a-X-c\n"
        );
    }

    #[test]
    fn assigning_a_far_field_extends_nf() {
        assert_eq!(ok("{$5=\"e\"; print NF, $0}", "a b\n"), "5 a b   e\n");
    }

    #[test]
    fn assigning_record_resplits_fields() {
        assert_eq!(ok("{$0=\"x y z\"; print NF, $2}", "a\n"), "3 y\n");
    }

    #[test]
    fn command_line_field_separator() {
        let result = run_files(&["-F,", "{print $2}"], "", &[]);
        assert_eq!(result.stdout, "");
        let args = ["-F,", "{print $2}"];
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let result = super::run(&owned, "a,b,c\n", &|_| None);
        assert_eq!(result.stdout, "b\n");
    }

    #[test]
    fn tab_field_separator() {
        let args = ["-F", "\\t", "{print $2}"];
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let result = super::run(&owned, "a\tb\tc\n", &|_| None);
        assert_eq!(result.stdout, "b\n");
    }

    #[test]
    fn regex_field_separator() {
        let args = ["-F", "[,;]", "{print $2}"];
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let result = super::run(&owned, "a,b;c\n", &|_| None);
        assert_eq!(result.stdout, "b\n");
    }

    // ── Records and files ───────────────────────────────────────────────────

    #[test]
    fn nr_counts_across_inputs() {
        assert_eq!(ok("{print NR}", "a\nb\nc\n"), "1\n2\n3\n");
    }

    #[test]
    fn fnr_and_filename_reset_per_file() {
        let result = run_files(
            &["{print FILENAME, FNR, NR}", "one.txt", "two.txt"],
            "",
            &[("one.txt", "a\nb\n"), ("two.txt", "c\n")],
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "one.txt 1 1\none.txt 2 2\ntwo.txt 1 3\n");
    }

    #[test]
    fn begin_and_end_run_once_in_order() {
        assert_eq!(
            ok(
                "BEGIN{print \"b\"} {print $1} END{print \"e\", NR}",
                "x\ny\n"
            ),
            "b\nx\ny\ne 2\n"
        );
    }

    #[test]
    fn end_sees_the_last_record() {
        // `$0` is the last record, so `$1` is its first field (matches awk).
        assert_eq!(ok("END{print $1, NF}", "a b\nc d\n"), "c 2\n");
        assert_eq!(ok("END{print $0}", "a b\nc d\n"), "c d\n");
    }

    #[test]
    fn begin_runs_without_input() {
        assert_eq!(ok("BEGIN{print \"only\"}", ""), "only\n");
    }

    // ── Rule forms ──────────────────────────────────────────────────────────

    #[test]
    fn pattern_only_rule_prints_matching_records() {
        assert_eq!(ok("$1 == \"b\"", "a\nb\nc\n"), "b\n");
    }

    #[test]
    fn regex_pattern_selects_records() {
        assert_eq!(ok("/at/ {print $1}", "cat\ndog\nhat\n"), "cat\nhat\n");
    }

    #[test]
    fn negated_regex_pattern() {
        assert_eq!(ok("!/at/ {print $1}", "cat\ndog\n"), "dog\n");
    }

    #[test]
    fn range_pattern_selects_between_matches() {
        assert_eq!(
            ok("/start/,/stop/ {print $1}", "a\nstart\nmid\nstop\nz\n"),
            "start\nmid\nstop\n"
        );
    }

    #[test]
    fn action_only_rule_runs_for_every_record() {
        assert_eq!(ok("{n++} END{print n}", "a\nb\nc\n"), "3\n");
    }

    // ── Output formatting ───────────────────────────────────────────────────

    #[test]
    fn ofs_and_ors_control_print() {
        assert_eq!(
            ok("BEGIN{OFS=\":\"; ORS=\"|\"} {print $1, $2}", "a b\n"),
            "a:b|"
        );
    }

    #[test]
    fn printf_supports_the_common_conversions() {
        assert_eq!(
            ok(
                "BEGIN{printf \"%s %d %i %x %o %c %%\\n\", \"s\", 42, -7, 255, 8, 65}",
                ""
            ),
            "s 42 -7 ff 10 A %\n"
        );
    }

    #[test]
    fn printf_supports_float_formats_and_width() {
        assert_eq!(
            ok("BEGIN{printf \"[%5.2f][%e]\\n\", 3.14159, 2}", ""),
            "[ 3.14][2.000000e+00]\n"
        );
    }

    #[test]
    fn sprintf_returns_a_string() {
        assert_eq!(ok("BEGIN{x = sprintf(\"%03d\", 7); print x}", ""), "007\n");
    }

    #[test]
    fn printf_with_too_few_arguments_fails() {
        // Extra arguments are ignored, but a conversion with no argument left
        // is an error, exactly as awk reports it.
        assert_eq!(ok("BEGIN{printf \"%s\\n\", \"a\", \"b\"}", ""), "a\n");
        assert_eq!(ok("BEGIN{printf \"plain\\n\"}", ""), "plain\n");
        for program in [
            "BEGIN{printf \"%s\\n\"}",
            "BEGIN{printf \"%s %s\\n\", \"a\"}",
            "BEGIN{printf \"%d\\n\"}",
            "BEGIN{print sprintf(\"%s\")}",
        ] {
            let result = err(program, "");
            assert!(
                result.stderr.contains("not enough args in printf"),
                "{program:?}: {}",
                result.stderr
            );
        }
    }

    #[test]
    fn print_numbers_do_not_gain_a_decimal_point() {
        assert_eq!(ok("BEGIN{print 1 + 2, 4 / 2, 1 / 3}", ""), "3 2 0.333333\n");
    }

    // ── Statements ──────────────────────────────────────────────────────────

    #[test]
    fn if_else_if_else() {
        let program =
            "{if ($1 > 2) print \"hi\"; else if ($1 == 2) print \"mid\"; else print \"lo\"}";
        assert_eq!(ok(program, "1\n2\n3\n"), "lo\nmid\nhi\n");
    }

    #[test]
    fn while_loop_accumulates() {
        assert_eq!(
            ok("BEGIN{i=0; while (i < 3) {s = s i; i++}; print s}", ""),
            "012\n"
        );
    }

    #[test]
    fn do_while_runs_the_body_at_least_once() {
        assert_eq!(ok("BEGIN{i=9; do {n++} while (i < 3); print n}", ""), "1\n");
    }

    #[test]
    fn c_style_for_loop() {
        assert_eq!(
            ok("BEGIN{for (i = 0; i < 3; i++) s = s i; print s}", ""),
            "012\n"
        );
    }

    #[test]
    fn for_in_walks_keys_in_sorted_order() {
        assert_eq!(
            ok("{a[$1] = 1} END{for (k in a) print k}", "c\na\nb\n"),
            "a\nb\nc\n"
        );
    }

    #[test]
    fn break_and_continue() {
        assert_eq!(
            ok(
                "BEGIN{for (i = 0; i < 5; i++) {if (i == 3) break; s = s i}; print s}",
                ""
            ),
            "012\n"
        );
        assert_eq!(
            ok(
                "BEGIN{for (i = 0; i < 4; i++) {if (i % 2) continue; s = s i}; print s}",
                ""
            ),
            "02\n"
        );
    }

    #[test]
    fn next_skips_the_remaining_rules() {
        assert_eq!(ok("NR == 2 {next} {print $1}", "a\nb\nc\n"), "a\nc\n");
    }

    #[test]
    fn exit_stops_processing_and_sets_the_status() {
        let result = run("{print $1; if (NR == 2) exit 3}", "a\nb\nc\n");
        assert_eq!(result.stdout, "a\nb\n");
        assert_eq!(result.exit_code, 3);
    }

    #[test]
    fn exit_without_a_code_succeeds() {
        let result = run("NR == 1 {exit}", "a\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn end_still_runs_after_exit() {
        let result = run("NR == 1 {exit} END{print \"end\"}", "a\nb\n");
        assert_eq!(result.stdout, "end\n");
    }

    // ── Arrays ──────────────────────────────────────────────────────────────

    #[test]
    fn array_assignment_and_lookup() {
        assert_eq!(
            ok("BEGIN{a[\"k\"] = 7; print a[\"k\"], a[\"missing\"]}", ""),
            "7 \n"
        );
    }

    #[test]
    fn multidimensional_indices_use_subsep() {
        assert_eq!(ok("BEGIN{a[1,2] = \"v\"; print a[1,2]}", ""), "v\n");
        assert_eq!(
            ok(
                "BEGIN{a[1,2] = \"v\"; if ((1,2) in a) print \"in\"; else print \"out\"}",
                ""
            ),
            "in\n"
        );
        // The joined key is reachable through SUBSEP as well.
        assert_eq!(
            ok("BEGIN{a[1,2] = \"v\"; k = 1 SUBSEP 2; print (k in a)}", ""),
            "1\n"
        );
    }

    #[test]
    fn in_operator_and_delete() {
        assert_eq!(
            ok(
                "BEGIN{a[\"x\"] = 1; print (\"x\" in a), (\"y\" in a); delete a[\"x\"]; print (\"x\" in a)}",
                ""
            ),
            "1 0\n0\n"
        );
    }

    #[test]
    fn delete_clears_the_whole_array() {
        assert_eq!(
            ok(
                "BEGIN{a[1] = 1; a[2] = 2; delete a; n = 0; for (k in a) n++; print n}",
                ""
            ),
            "0\n"
        );
    }

    #[test]
    fn word_count_is_deterministic() {
        assert_eq!(
            ok(
                "{for (i = 1; i <= NF; i++) c[$i]++} END{for (w in c) print w, c[w]}",
                "b a b\n"
            ),
            "a 1\nb 2\n"
        );
    }

    // ── Built-ins ───────────────────────────────────────────────────────────

    #[test]
    fn length_of_string_and_record() {
        assert_eq!(ok("{print length($1), length()}", "abcd\n"), "4 4\n");
        assert_eq!(ok("BEGIN{print length(\"\")}", ""), "0\n");
    }

    #[test]
    fn substr_with_and_without_length() {
        assert_eq!(
            ok(
                "BEGIN{print substr(\"abcdef\", 2), substr(\"abcdef\", 2, 3)}",
                ""
            ),
            "bcdef bcd\n"
        );
        assert_eq!(
            ok(
                "BEGIN{print substr(\"abcdef\", 2); print substr(\"abcdef\", 2, 3)}",
                ""
            ),
            "bcdef\nbcd\n"
        );
    }

    #[test]
    fn substr_handles_out_of_range_arguments() {
        assert_eq!(
            ok("BEGIN{print \"[\" substr(\"abc\", 9) \"]\"}", ""),
            "[]\n"
        );
        assert_eq!(
            ok("BEGIN{print \"[\" substr(\"abc\", 2, 99) \"]\"}", ""),
            "[bc]\n"
        );
    }

    #[test]
    fn index_finds_a_substring() {
        assert_eq!(
            ok(
                "BEGIN{print index(\"hello\", \"ll\"), index(\"hello\", \"z\")}",
                ""
            ),
            "3 0\n"
        );
    }

    #[test]
    fn split_fills_an_array_and_returns_the_count() {
        assert_eq!(
            ok(
                "BEGIN{n = split(\"a:b:c\", parts, \":\"); print n, parts[1], parts[3]}",
                ""
            ),
            "3 a c\n"
        );
    }

    #[test]
    fn split_uses_fs_by_default() {
        assert_eq!(ok("{n = split($0, p); print n}", "a b c\n"), "3\n");
    }

    #[test]
    fn sub_replaces_only_the_first_match() {
        assert_eq!(
            ok("BEGIN{s = \"aaa\"; n = sub(/a/, \"b\", s); print n, s}", ""),
            "1 baa\n"
        );
    }

    #[test]
    fn gsub_replaces_every_match_and_counts_them() {
        assert_eq!(
            ok(
                "BEGIN{s = \"aaa\"; n = gsub(/a/, \"b\", s); print n, s}",
                ""
            ),
            "3 bbb\n"
        );
    }

    #[test]
    fn gsub_defaults_to_the_current_record() {
        assert_eq!(ok("{gsub(/a/, \"A\"); print}", "banana\n"), "bAnAnA\n");
    }

    #[test]
    fn match_sets_rstart_and_rlength() {
        assert_eq!(
            ok(
                "BEGIN{if (match(\"abcdef\", /cd/)) print RSTART, RLENGTH; else print \"no\"}",
                ""
            ),
            "3 2\n"
        );
        assert_eq!(
            ok("BEGIN{match(\"abc\", /zz/); print RSTART, RLENGTH}", ""),
            "0 -1\n"
        );
    }

    #[test]
    fn case_conversion() {
        assert_eq!(
            ok("BEGIN{print tolower(\"AbC\"), toupper(\"AbC\")}", ""),
            "abc ABC\n"
        );
    }

    #[test]
    fn numeric_builtins() {
        assert_eq!(ok("BEGIN{print int(3.9), int(-3.9)}", ""), "3 -3\n");
        assert_eq!(ok("BEGIN{print sqrt(9), exp(0), log(1)}", ""), "3 1 0\n");
    }

    #[test]
    fn atan2_is_available() {
        assert_eq!(ok("BEGIN{printf \"%.4f\\n\", atan2(0, 1)}", ""), "0.0000\n");
    }

    #[test]
    fn rand_is_deterministic_per_seed() {
        assert_eq!(
            ok(
                "BEGIN{srand(1); a = rand(); srand(1); b = rand(); print (a == b)}",
                ""
            ),
            "1\n"
        );
    }

    // ── Operators ───────────────────────────────────────────────────────────

    #[test]
    fn arithmetic_operators() {
        assert_eq!(
            ok("BEGIN{print 7 + 3, 7 - 3, 7 * 3, 7 / 2, 7 % 3, 2 ^ 10}", ""),
            "10 4 21 3.5 1 1024\n"
        );
    }

    #[test]
    fn assignment_operators() {
        assert_eq!(
            ok(
                "BEGIN{x = 10; x += 5; x -= 3; x *= 2; x /= 4; x %= 5; print x}",
                ""
            ),
            "1\n"
        );
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(ok("BEGIN{print \"a\" \"b\" \"c\"}", ""), "abc\n");
        assert_eq!(ok("{print $1 $2}", "x y\n"), "xy\n");
    }

    #[test]
    fn comparison_uses_numeric_or_string_rules() {
        // Both sides numeric: numeric comparison.
        assert_eq!(ok("BEGIN{print (2 < 10)}", ""), "1\n");
        // String operands: lexicographic.
        assert_eq!(ok("BEGIN{print (\"2\" < \"10\")}", ""), "0\n");
    }

    #[test]
    fn logical_operators_and_negation() {
        assert_eq!(
            ok("BEGIN{print (1 && 0), (1 || 0), !0, !1}", ""),
            "0 1 1 0\n"
        );
    }

    #[test]
    fn ternary_selects_a_branch() {
        assert_eq!(
            ok("{x = ($1 > 2 ? \"hi\" : \"lo\"); print x}", "1\n3\n"),
            "lo\nhi\n"
        );
    }

    #[test]
    fn increment_and_decrement_operators() {
        assert_eq!(
            ok("BEGIN{i = 1; print i++, i, ++i, i--, i}", ""),
            "1 2 3 3 2\n"
        );
    }

    #[test]
    fn match_and_not_match_operators() {
        assert_eq!(
            ok("BEGIN{print (\"abc\" ~ /b/), (\"abc\" !~ /b/)}", ""),
            "1 0\n"
        );
    }

    #[test]
    fn unary_operators() {
        assert_eq!(ok("BEGIN{print -3 + 1, +4}", ""), "-2 4\n");
    }

    #[test]
    fn exponentiation_binds_tighter_than_multiplication() {
        assert_eq!(ok("BEGIN{print 2 * 3 ^ 2}", ""), "18\n");
    }

    // ── Command line ────────────────────────────────────────────────────────

    #[test]
    fn dash_v_assigns_before_begin() {
        let result = run_files(&["-v", "x=5", "BEGIN{print x}"], "", &[]);
        assert_eq!(result.stdout, "5\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn dash_v_accepts_escape_sequences() {
        let result = run_files(&["-v", "x=a\\tb", "BEGIN{print x}"], "", &[]);
        assert_eq!(result.stdout, "a\tb\n");
    }

    #[test]
    fn program_file_is_read_from_the_filesystem() {
        let result = run_files(
            &["-f", "prog.awk"],
            "one\ntwo\n",
            &[("prog.awk", "{print $1}\n")],
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "one\ntwo\n");
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let result = run_files(&["--", "{print $1}"], "z\n", &[]);
        assert_eq!(result.stdout, "z\n");
    }

    #[test]
    fn assignment_operand_applies_to_later_files() {
        let result = run_files(
            &["{print FILENAME, x}", "x=1", "data.txt"],
            "",
            &[("data.txt", "row\n")],
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "data.txt 1\n");
    }

    // ── Error contract ──────────────────────────────────────────────────────

    #[test]
    fn missing_file_is_an_error() {
        let result = err_on_files(&["{print}", "nope.txt"]);
        assert!(result.stderr.contains("nope.txt"), "{}", result.stderr);
    }

    #[test]
    fn unknown_option_is_an_error() {
        let result = err_on_files(&["-Z", "{print}"]);
        assert!(
            result.stderr.contains("-Z") || result.stderr.contains('Z'),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let result = err("BEGIN{print 1 / 0}", "");
        assert!(
            result.stderr.contains("zero") || result.stderr.contains("division"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn malformed_programs_are_errors_not_panics() {
        for program in [
            "{print $1",
            "BEGIN{print \"unterminated}",
            "BEGIN{print /unterminated}",
            "BEGIN{if (1 {print}}",
            "BEGIN{print 1 +}",
        ] {
            let result = run(program, "a\n");
            assert_eq!(result.exit_code, 2, "program {program:?} should fail");
            assert!(
                !result.stderr.is_empty(),
                "program {program:?} needs a message"
            );
        }
    }

    #[test]
    fn unsupported_constructs_fail_closed() {
        for program in [
            "{getline x}",
            "BEGIN{system(\"echo hi\")}",
            "function f() {return 1} BEGIN{print f()}",
            "{print $1 > \"out.txt\"}",
            "BEGIN{RS = \"x\"} {print}",
        ] {
            let result = run(program, "a\n");
            assert_eq!(
                result.exit_code, 2,
                "program {program:?} should fail closed"
            );
            assert!(
                !result.stderr.is_empty(),
                "program {program:?} needs a diagnostic"
            );
        }
    }

    #[test]
    fn rstart_and_rlength_start_at_zero() {
        assert_eq!(ok("BEGIN{print RSTART, RLENGTH}", ""), "0 0\n");
    }

    fn err_on_files(args: &[&str]) -> AwkRun {
        let owned: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let result = super::run(&owned, "a\n", &|_| None);
        assert_eq!(result.exit_code, 2, "args {args:?}: {}", result.stderr);
        assert!(result.stderr.contains("awk:"), "{:?}", result.stderr);
        result
    }

    // ── Bounds ──────────────────────────────────────────────────────────────

    #[test]
    fn record_limit_stops_a_huge_input() {
        let input = "x\n".repeat(MAX_RECORDS + 10);
        let result = run("{n++} END{print n}", &input);
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("awk:"), "{}", result.stderr);
    }

    #[test]
    fn step_limit_stops_an_infinite_loop() {
        let result = run("BEGIN{while (1) {}}", "");
        assert_eq!(result.exit_code, 2);
        assert!(!result.stderr.is_empty());
    }

    #[test]
    fn output_limit_stops_runaway_printing() {
        let result = run("BEGIN{while (1) print \"0123456789\"}", "");
        assert_eq!(result.exit_code, 2);
        assert!(!result.stderr.is_empty());
        assert!(
            result.stdout.len() <= MAX_OUTPUT_BYTES,
            "stdout {} exceeded the cap",
            result.stdout.len()
        );
    }

    #[test]
    fn output_before_a_limit_is_kept() {
        let result = run("BEGIN{print \"first\"} {while (1) {}}", "a\n");
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stdout, "first\n");
    }
}
