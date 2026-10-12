//! A small, dependency-free POSIX ERE (extended regular expression) engine.
//!
//! This module is written for the `just-bash` runtime, which treats scripts as
//! untrusted input. Two properties therefore matter as much as matching
//! correctness:
//!
//! * **Bounded work.** Classic backtracking engines (this one included) can take
//!   exponential time on patterns such as `(a+)+$`. Every match attempt is
//!   limited to [`DEFAULT_STEP_BUDGET`] virtual-machine steps; when the budget is
//!   exhausted the attempt is abandoned. See [`Regex::is_match`].
//! * **No panics.** Every code path returns `Result`/`Option`. No indexing is
//!   performed without a bounds check, there is no `unsafe`, and no arithmetic
//!   can overflow on adversarial input.
//!
//! # Supported syntax
//!
//! | Construct | Meaning |
//! |-----------|---------|
//! | `abc` | literal characters |
//! | `.` | any character except `\n` |
//! | `*` `+` `?` | repetition: 0+, 1+, 0 or 1 (greedy) |
//! | `{m}` `{m,}` `{m,n}` | bounded repetition (greedy) |
//! | `\|` | alternation |
//! | `( )` | capturing group, numbered by opening parenthesis from 1 |
//! | `(?: )` | non-capturing group |
//! | `^` `$` | whole-text anchors (see below) |
//! | `[...]` `[^...]` | bracket expression / negated bracket expression |
//! | `a-z` | character range inside a bracket expression |
//! | `[:alpha:]` … | POSIX character classes inside a bracket expression |
//! | `\.` `\*` … | escaping any metacharacter makes it a literal |
//! | `\d \D \w \W \s \S` | GNU shorthands for digit / word / whitespace |
//! | `\b \B` | word boundary / non-boundary |
//! | `\n \t \r` | line feed, tab, carriage return |
//! | `\1` … `\9` | backreference to an earlier (or later) capture group |
//!
//! A `{` that is not followed by a well-formed bound is a literal `{` (GNU
//! behaviour), so `a{` matches the two characters `a{`. Likewise a `*`, `+` or
//! `?` with no preceding atom is treated as a literal, and `\[`/`\]` behave as
//! the plain characters.
//!
//! # Semantics
//!
//! * Matching is **leftmost first-success** (Perl-style), not POSIX
//!   leftmost-longest: `a|ab` finds `a` in `"ab"`. All repetition operators are
//!   greedy and there are no lazy quantifiers.
//! * `^` and `$` are **whole-text** anchors. They match only at offset 0 and at
//!   the very end of the haystack; they never match around an interior `\n`
//!   (there is no multiline mode).
//! * Matching operates on `char`s (so a `.` consumes a whole UTF-8 scalar), but
//!   every offset exposed by the public API is a **byte** offset into the
//!   original `&str`.
//! * `ignore_case` is **ASCII-only** case folding. POSIX character classes are
//!   ASCII-only as well: `[:alpha:]` does not match `é`.
//! * `replace_all` and `split` advance by one character after a zero-width
//!   match, so they always terminate.
//!
//! # Errors and limits
//!
//! Compilation returns `Err(String)` with a human-readable message for
//! unclosed `(`/`[`, an unmatched `)`, a backwards repetition range
//! (`a{2,1}`), an unknown POSIX class name, an invalid range (`[z-a]`), a
//! trailing backslash, a reference to a non-existent capture group, a
//! repetition bound above [`MAX_REPEAT_BOUND`][], or a program that would exceed
//! `MAX_PROGRAM_LEN` instructions.

/// Maximum number of backtracking virtual-machine steps allowed for one match
/// attempt before that attempt is abandoned.
///
/// The budget is per *attempt*, i.e. per starting offset: a search that walks a
/// long haystack re-uses a fresh budget at each position, so scanning a large
/// input with a cheap pattern works. If any single attempt exhausts the budget
/// the whole search is abandoned: [`Regex::is_match`] returns `false` and
/// [`Regex::find`] / [`Regex::captures`] return `None`. That is the documented
/// behaviour for pathological patterns such as `(a+)+$`; no panic and no hang.
pub const DEFAULT_STEP_BUDGET: usize = 100_000;

/// Largest repetition bound accepted in `{m}`, `{m,}` and `{m,n}`.
///
/// Bounded repetition is compiled by expansion, so an unconstrained bound such
/// as `a{999999999}` would allocate an unbounded program. Bounds above this
/// value are a compile error.
const MAX_REPEAT_BOUND: usize = 10_000;

/// Upper bound on the number of compiled instructions.
///
/// Guards against nested bounded repetitions (`(a{1000}){1000}`) exploding the
/// program size during compilation; compilation fails cleanly instead.
const MAX_PROGRAM_LEN: usize = 100_000;

/// A compiled regular expression.
///
/// See the [module documentation](self) for the supported syntax and semantics.
#[derive(Debug, Clone)]
pub struct Regex {
    /// Compiled program executed by [`Regex::run`].
    prog: Vec<Inst>,
    /// Number of capturing groups in the pattern.
    n_groups: usize,
    /// Size of the save-slot vector: two slots per group plus one per loop
    /// progress slot.
    n_slots: usize,
    /// ASCII case-insensitive matching flag.
    ignore_case: bool,
}

impl Regex {
    /// Compile an ERE (POSIX extended regular expression). Errors are
    /// human-readable.
    pub fn new(pattern: &str) -> Result<Self, String> {
        Self::new_with_flags(pattern, false)
    }

    /// Compile with flags. `ignore_case` makes matching ASCII case-insensitive.
    pub fn new_with_flags(pattern: &str, ignore_case: bool) -> Result<Self, String> {
        let (ast, n_groups) = parse_pattern(pattern)?;
        let (prog, n_slots) = compile_pattern(&ast, n_groups)?;
        Ok(Self {
            prog,
            n_groups,
            n_slots,
            ignore_case,
        })
    }

    /// True if the pattern matches anywhere in `text`.
    ///
    /// Returns `false` if the backtracking budget is exhausted (see
    /// [`DEFAULT_STEP_BUDGET`]).
    pub fn is_match(&self, text: &str) -> bool {
        let hay = Search::new(text);
        self.search_from(&hay, 0).is_some()
    }

    /// Leftmost match as byte offsets `(start, end)` into `text`.
    ///
    /// Returns `None` when there is no match or when the backtracking budget is
    /// exhausted.
    pub fn find(&self, text: &str) -> Option<(usize, usize)> {
        let hay = Search::new(text);
        self.search_from(&hay, 0)?
            .into_iter()
            .next()
            .unwrap_or(None)
    }

    /// Leftmost match with capture groups: index 0 = whole match, 1..n = groups.
    /// A group that did not participate is `None`.
    ///
    /// Returns `None` when there is no match or when the backtracking budget is
    /// exhausted.
    pub fn captures(&self, text: &str) -> Option<Vec<Option<(usize, usize)>>> {
        let hay = Search::new(text);
        self.search_from(&hay, 0)
    }

    /// Number of capture groups in the pattern.
    pub fn group_count(&self) -> usize {
        self.n_groups
    }

    /// Replace every non-overlapping match. `replacement` supports `$0`..`$9`,
    /// `${N}`, `&` (whole match), and `\` escapes for `$` and `\`.
    ///
    /// A reference to a group that does not exist, or that did not participate
    /// in the match, expands to the empty string. A `\` followed by anything
    /// other than `$` or `\` is kept literally, as is a lone trailing `\`.
    /// Zero-width matches advance by one character so the call always
    /// terminates.
    pub fn replace_all(&self, text: &str, replacement: &str) -> String {
        let hay = Search::new(text);
        let mut out = String::with_capacity(text.len());
        let mut last = 0usize;
        let mut search = 0usize;
        let mut prev_end: Option<usize> = None;

        loop {
            if search > text.len() {
                break;
            }
            let caps = match self.search_from(&hay, search) {
                Some(caps) => caps,
                None => break,
            };
            let (mstart, mend) = match caps.first().copied().flatten() {
                Some(span) => span,
                None => break,
            };
            if mstart == mend && prev_end == Some(mstart) {
                // A zero-width match sitting immediately after the previous
                // match is not a new match; skip it so `a*` on "ba" does not
                // produce an extra replacement.
                match next_char_end(text, mend) {
                    Some(next) => {
                        search = next;
                        continue;
                    }
                    None => break,
                }
            }
            out.push_str(text.get(last..mstart).unwrap_or(""));
            out.push_str(&self.expand(replacement, text, &caps));
            prev_end = Some(mend);
            if mend == mstart {
                // Zero-width match: keep the character it matched in front of
                // and continue after it, guaranteeing progress.
                match next_char_end(text, mend) {
                    Some(next) => {
                        out.push_str(text.get(mend..next).unwrap_or(""));
                        last = next;
                        search = next;
                    }
                    None => {
                        // Nothing left to copy; `last` is the end of the text.
                        last = mend;
                        break;
                    }
                }
            } else {
                last = mend;
                search = mend;
            }
        }
        out.push_str(text.get(last..).unwrap_or(""));
        out
    }

    /// Split `text` on matches of the pattern (like `str::split`), returning
    /// pieces. Zero-width matches advance by one character, so a pattern that
    /// can match the empty string splits between every character.
    pub fn split(&self, text: &str) -> Vec<String> {
        let hay = Search::new(text);
        let mut out = Vec::new();
        let mut last = 0usize;
        let mut search = 0usize;
        let mut prev_end: Option<usize> = None;

        loop {
            if search > text.len() {
                break;
            }
            let caps = match self.search_from(&hay, search) {
                Some(caps) => caps,
                None => break,
            };
            let (mstart, mend) = match caps.first().copied().flatten() {
                Some(span) => span,
                None => break,
            };
            if mstart == mend && prev_end == Some(mstart) {
                // Same "adjacent zero-width match" rule as `replace_all`.
                match next_char_end(text, mend) {
                    Some(next) => {
                        search = next;
                        continue;
                    }
                    None => break,
                }
            }
            out.push(text.get(last..mstart).unwrap_or("").to_string());
            prev_end = Some(mend);
            if mend == mstart {
                match next_char_end(text, mend) {
                    Some(next) => {
                        last = mend;
                        search = next;
                    }
                    None => {
                        // Nothing left to copy; `last` is the end of the text.
                        last = mend;
                        break;
                    }
                }
            } else {
                last = mend;
                search = mend;
            }
        }
        out.push(text.get(last..).unwrap_or("").to_string());
        out
    }

    /// Search for the leftmost match at or after `from_byte`.
    ///
    /// Returns one byte-offset span per group (index 0 is the whole match).
    /// `None` means "no match" or "budget exhausted" — the two are deliberately
    /// indistinguishable to the caller, and neither panics.
    fn search_from(
        &self,
        hay: &Search<'_>,
        from_byte: usize,
    ) -> Option<Vec<Option<(usize, usize)>>> {
        let n = hay.chars.len();
        let first = hay.char_index_at_or_after(from_byte);
        for start in first..=n {
            match self.run(hay, start, DEFAULT_STEP_BUDGET) {
                RunResult::Matched(slots) => return Some(self.to_spans(hay, &slots)),
                // One runaway attempt abandons the whole search; scanning
                // continues only while every attempt stays inside its budget.
                RunResult::Exhausted => return None,
                RunResult::NoMatch => {}
            }
        }
        None
    }

    /// Convert char-index save slots into byte-offset spans.
    fn to_spans(&self, hay: &Search<'_>, slots: &[Option<usize>]) -> Vec<Option<(usize, usize)>> {
        let mut out = Vec::with_capacity(self.n_groups + 1);
        for group in 0..=self.n_groups {
            let start = slots.get(2 * group).copied().flatten();
            let end = slots.get(2 * group + 1).copied().flatten();
            out.push(match (start, end) {
                (Some(s), Some(e)) => Some((hay.byte_at(s), hay.byte_at(e))),
                _ => None,
            });
        }
        out
    }

    /// Execute the compiled program starting at char index `start`.
    ///
    /// This is the backtracking virtual machine: `pc` is the program counter,
    /// `pos` is a char index, and `stack` holds saved (pc, pos, slots) states
    /// pushed by `Split`. `slots` is snapshot-and-restored on backtracking so
    /// captures (and loop progress marks) are always consistent with the
    /// current path.
    fn run(&self, hay: &Search<'_>, start: usize, budget: usize) -> RunResult {
        let chars = hay.chars.as_slice();
        let n = chars.len();
        let ignore_case = self.ignore_case;

        let mut slots: Vec<Option<usize>> = vec![None; self.n_slots];
        let mut stack: Vec<(usize, usize, Vec<Option<usize>>)> = Vec::new();
        let mut pc = 0usize;
        let mut pos = start;
        let mut steps = 0usize;

        loop {
            steps += 1;
            if steps > budget {
                return RunResult::Exhausted;
            }
            // `get` rather than indexing: a malformed program can never panic.
            let inst = match self.prog.get(pc) {
                Some(inst) => inst,
                None => return RunResult::NoMatch,
            };

            let mut advance: Option<(usize, usize)> = None;
            let mut fail = false;

            match inst {
                Inst::Char(expected) => match chars.get(pos) {
                    Some((_, c)) if chars_equal(*c, *expected, ignore_case) => {
                        advance = Some((pc + 1, pos + 1));
                    }
                    _ => fail = true,
                },
                Inst::Any => match chars.get(pos) {
                    Some((_, c)) if *c != '\n' => advance = Some((pc + 1, pos + 1)),
                    _ => fail = true,
                },
                Inst::Class(set) => match chars.get(pos) {
                    Some((_, c)) if class_matches(set, *c, ignore_case) => {
                        advance = Some((pc + 1, pos + 1));
                    }
                    _ => fail = true,
                },
                // Try `a` first; remember to retry `b` later.
                Inst::Split(a, b) => {
                    stack.push((*b, pos, slots.clone()));
                    advance = Some((*a, pos));
                }
                Inst::Jmp(target) => advance = Some((*target, pos)),
                Inst::Save(slot) => {
                    if let Some(cell) = slots.get_mut(*slot) {
                        *cell = Some(pos);
                    }
                    advance = Some((pc + 1, pos));
                }
                // Mark where this loop iteration started so `Progress` can tell
                // whether the body consumed anything.
                Inst::PosSave(slot) => {
                    if let Some(cell) = slots.get_mut(*slot) {
                        *cell = Some(pos);
                    }
                    advance = Some((pc + 1, pos));
                }
                // Loop-progress guard: when the loop body consumed nothing the
                // iteration is the last one. Jump straight to the loop exit
                // rather than backtracking, so captures recorded by that final
                // (empty) iteration stay visible, as in Perl and Python.
                // This is what makes `(a?)*` terminate.
                Inst::Progress { slot, exit } => {
                    let mark = slots.get(*slot).copied().flatten();
                    if mark == Some(pos) {
                        advance = Some((*exit, pos));
                    } else {
                        advance = Some((pc + 1, pos));
                    }
                }
                Inst::Start => {
                    if pos == 0 {
                        advance = Some((pc + 1, pos));
                    } else {
                        fail = true;
                    }
                }
                Inst::End => {
                    if pos == n {
                        advance = Some((pc + 1, pos));
                    } else {
                        fail = true;
                    }
                }
                Inst::WordBoundary(negated) => {
                    let before = match pos.checked_sub(1).and_then(|i| chars.get(i)) {
                        Some((_, c)) => is_word(*c),
                        None => false,
                    };
                    let after = match chars.get(pos) {
                        Some((_, c)) => is_word(*c),
                        None => false,
                    };
                    let boundary = before != after;
                    // `\b` needs a boundary, `\B` needs its absence.
                    if boundary != *negated {
                        advance = Some((pc + 1, pos));
                    } else {
                        fail = true;
                    }
                }
                Inst::Backref(slot) => {
                    let span = slots
                        .get(*slot)
                        .copied()
                        .flatten()
                        .zip(slots.get(*slot + 1).copied().flatten());
                    match span {
                        Some((group_start, group_end)) => {
                            let len = group_end.saturating_sub(group_start);
                            let matches = pos + len <= n
                                && (0..len).all(|k| {
                                    match (chars.get(group_start + k), chars.get(pos + k)) {
                                        (Some((_, a)), Some((_, b))) => {
                                            chars_equal(*a, *b, ignore_case)
                                        }
                                        _ => false,
                                    }
                                });
                            if matches {
                                advance = Some((pc + 1, pos + len));
                            } else {
                                fail = true;
                            }
                        }
                        // The group never participated (or is still open).
                        None => fail = true,
                    }
                }
                Inst::Match => return RunResult::Matched(slots),
            }

            if fail {
                match stack.pop() {
                    Some((saved_pc, saved_pos, saved_slots)) => {
                        pc = saved_pc;
                        pos = saved_pos;
                        slots = saved_slots;
                    }
                    None => return RunResult::NoMatch,
                }
            } else if let Some((next_pc, next_pos)) = advance {
                pc = next_pc;
                pos = next_pos;
            } else {
                // Unreachable by construction; fail closed instead of panicking.
                return RunResult::NoMatch;
            }
        }
    }

    /// Expand a replacement template against one match.
    fn expand(&self, replacement: &str, text: &str, caps: &[Option<(usize, usize)>]) -> String {
        let r: Vec<char> = replacement.chars().collect();
        let mut out = String::with_capacity(replacement.len());
        let mut i = 0usize;

        while i < r.len() {
            match r[i] {
                '\\' => match r.get(i + 1) {
                    // `\$` and `\\` are the two documented escapes.
                    Some('$') => {
                        out.push('$');
                        i += 2;
                    }
                    Some('\\') => {
                        out.push('\\');
                        i += 2;
                    }
                    // Anything else keeps the backslash verbatim.
                    Some(other) => {
                        out.push('\\');
                        out.push(*other);
                        i += 2;
                    }
                    None => {
                        out.push('\\');
                        i += 1;
                    }
                },
                '$' => {
                    if r.get(i + 1) == Some(&'{') {
                        // `${N}`: digits up to `}`.
                        let mut j = i + 2;
                        let mut digits = String::new();
                        let mut closed = false;
                        while let Some(&c) = r.get(j) {
                            if c == '}' {
                                closed = true;
                                break;
                            }
                            if c.is_ascii_digit() {
                                digits.push(c);
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        if closed && !digits.is_empty() {
                            if let Ok(group) = digits.parse::<usize>() {
                                push_group(&mut out, text, caps, group);
                            }
                            i = j + 1;
                        } else {
                            // Malformed: `$` is a literal and `{` is re-scanned.
                            out.push('$');
                            i += 1;
                        }
                    } else {
                        match r.get(i + 1).and_then(|c| c.to_digit(10)) {
                            Some(group) => {
                                push_group(&mut out, text, caps, group as usize);
                                i += 2;
                            }
                            None => {
                                out.push('$');
                                i += 1;
                            }
                        }
                    }
                }
                '&' => {
                    push_group(&mut out, text, caps, 0);
                    i += 1;
                }
                other => {
                    out.push(other);
                    i += 1;
                }
            }
        }
        out
    }
}

/// Outcome of one call to [`Regex::run`].
enum RunResult {
    /// The program reached `Match`; the slots describe the captures.
    Matched(Vec<Option<usize>>),
    /// The program ran out of alternatives.
    NoMatch,
    /// The step budget was exhausted before a verdict was reached.
    Exhausted,
}

// ---------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------

/// A bracket expression: a (possibly negated) union of ranges and POSIX
/// classes. Kept as a list of items rather than a bitmap so that non-ASCII
/// ranges work without special cases.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassSet {
    negated: bool,
    items: Vec<ClassItem>,
}

/// One member of a [`ClassSet`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    /// An inclusive character range, e.g. `a-z` (or a single char when the
    /// bounds are equal).
    Range(char, char),
    /// A POSIX class such as `[:digit:]`.
    Posix(PosixClass),
}

/// The POSIX character classes, plus the internal `Word` class used by `\w`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PosixClass {
    Alpha,
    Digit,
    Alnum,
    Space,
    Upper,
    Lower,
    Punct,
    XDigit,
    Blank,
    Cntrl,
    Print,
    Graph,
    /// `\w` = alphanumeric or `_`. Not reachable through `[:name:]`.
    Word,
}

impl PosixClass {
    /// Look up a `[:name:]` class. Returns `None` for unknown names.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "alpha" => Self::Alpha,
            "digit" => Self::Digit,
            "alnum" => Self::Alnum,
            "space" => Self::Space,
            "upper" => Self::Upper,
            "lower" => Self::Lower,
            "punct" => Self::Punct,
            "xdigit" => Self::XDigit,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "print" => Self::Print,
            "graph" => Self::Graph,
            _ => return None,
        })
    }

    /// ASCII-only membership test, matching POSIX `is*` predicates.
    fn matches(self, c: char) -> bool {
        match self {
            Self::Alpha => c.is_ascii_alphabetic(),
            Self::Digit => c.is_ascii_digit(),
            Self::Alnum => c.is_ascii_alphanumeric(),
            // POSIX `[:space:]` is exactly isspace(3).
            Self::Space => matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r'),
            Self::Upper => c.is_ascii_uppercase(),
            Self::Lower => c.is_ascii_lowercase(),
            Self::Punct => c.is_ascii_punctuation(),
            Self::XDigit => c.is_ascii_hexdigit(),
            Self::Blank => matches!(c, ' ' | '\t'),
            Self::Cntrl => c.is_ascii_control(),
            Self::Print => c.is_ascii_graphic() || c == ' ',
            Self::Graph => c.is_ascii_graphic(),
            Self::Word => is_word(c),
        }
    }
}

/// True for the characters `\w` and `\b` consider "word" characters:
/// ASCII letters, ASCII digits and `_`.
fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Membership test used by [`Inst::Class`].
///
/// With `ignore_case` the case-swapped character is tried as well, which makes
/// `[a-z]` match `A` while keeping `[^a]` from matching `A`.
fn class_matches(set: &ClassSet, c: char, ignore_case: bool) -> bool {
    let hit = class_hit(set, c)
        || (ignore_case
            && match swapped_case(c) {
                Some(other) => class_hit(set, other),
                None => false,
            });
    hit != set.negated
}

/// Raw membership test, ignoring negation and case folding.
fn class_hit(set: &ClassSet, c: char) -> bool {
    set.items.iter().any(|item| match item {
        ClassItem::Range(low, high) => *low <= c && c <= *high,
        ClassItem::Posix(posix) => posix.matches(c),
    })
}

/// ASCII case swap; `None` for characters with no ASCII counterpart.
fn swapped_case(c: char) -> Option<char> {
    if c.is_ascii_lowercase() {
        Some(c.to_ascii_uppercase())
    } else if c.is_ascii_uppercase() {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// Character comparison honouring the (ASCII-only) ignore-case flag.
fn chars_equal(a: char, b: char, ignore_case: bool) -> bool {
    a == b || (ignore_case && a.eq_ignore_ascii_case(&b))
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

/// Parsed pattern. Quantifiers are always greedy, so no greediness flag is
/// needed.
#[derive(Debug, Clone)]
enum Ast {
    /// A single literal character.
    Char(char),
    /// `.` — any character except `\n`.
    Any,
    /// A bracket expression.
    Class(ClassSet),
    /// `( ... )`; `index` is `Some(n)` for a capturing group.
    Group {
        index: Option<usize>,
        body: Box<Ast>,
    },
    /// A sequence of nodes.
    Concat(Vec<Ast>),
    /// `a|b|c`. At least two branches.
    Alt(Vec<Ast>),
    /// Greedy repetition. `max == None` means unbounded.
    Repeat {
        body: Box<Ast>,
        min: usize,
        max: Option<usize>,
    },
    /// `^` — start of the whole text.
    Start,
    /// `$` — end of the whole text.
    End,
    /// `\b` (`negated == false`) or `\B` (`negated == true`).
    WordBoundary(bool),
    /// `\1`..`\9`: slot index of the group's opening save.
    Backref(usize),
}

fn repeat(body: Ast, min: usize, max: Option<usize>) -> Ast {
    Ast::Repeat {
        body: Box::new(body),
        min,
        max,
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// A class member as it appears in the source: either a single character or a
/// whole POSIX class (from `[:name:]`, `\d`, `\w` or `\s`).
enum ClassPiece {
    Ch(char),
    Cls(PosixClass),
}

/// Recursive-descent parser for the supported ERE subset.
struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// Capture-group counter, increased at each opening `(`.
    n_groups: usize,
    /// Highest backreference number seen; validated against `n_groups` once the
    /// whole pattern has been parsed (backreferences may point forwards).
    max_backref: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// `alternation := concat ('|' concat)*`
    fn parse_alt(&mut self) -> Result<Ast, String> {
        let mut branches = vec![self.parse_concat()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap_or(Ast::Concat(Vec::new())))
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    /// `concat := repeat*`
    fn parse_concat(&mut self) -> Result<Ast, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            items.push(self.parse_quantifier(atom)?);
        }
        Ok(Ast::Concat(items))
    }

    /// `repeat := atom quantifier?`
    fn parse_quantifier(&mut self, atom: Ast) -> Result<Ast, String> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(repeat(atom, 0, None))
            }
            Some('+') => {
                self.pos += 1;
                Ok(repeat(atom, 1, None))
            }
            Some('?') => {
                self.pos += 1;
                Ok(repeat(atom, 0, Some(1)))
            }
            Some('{') => match self.parse_bound()? {
                Some((min, max, end)) => {
                    self.pos = end;
                    Ok(repeat(atom, min, max))
                }
                // Not a well-formed bound: leave the `{` to be re-read as a
                // literal by the next `parse_atom` call (GNU behaviour).
                None => Ok(atom),
            },
            _ => Ok(atom),
        }
    }

    /// Try to read `{m}`, `{m,}` or `{m,n}` starting at the current `{`.
    ///
    /// Returns `Ok(None)` when the braces are not a valid bound (the caller
    /// then treats `{` as a literal) and `Err` for a backwards range or a bound
    /// above [`MAX_REPEAT_BOUND`]. On success the tuple is
    /// `(min, max, position_after_brace)`.
    fn parse_bound(&self) -> Result<Option<(usize, Option<usize>, usize)>, String> {
        let ch = self.chars.as_slice();
        let mut i = self.pos + 1; // skip '{'
        let low_start = i;
        while matches!(ch.get(i), Some(c) if c.is_ascii_digit()) {
            i += 1;
        }
        let low = if i > low_start {
            parse_usize(&ch[low_start..i])
        } else {
            None
        };

        let (min, max) = match ch.get(i) {
            // `{m}`
            Some('}') => match low {
                Some(m) => (m, Some(m)),
                None => return Ok(None),
            },
            // `{m,}` or `{m,n}`
            Some(',') => {
                i += 1;
                let high_start = i;
                while matches!(ch.get(i), Some(c) if c.is_ascii_digit()) {
                    i += 1;
                }
                if ch.get(i) != Some(&'}') {
                    return Ok(None);
                }
                match low {
                    // `{,n}` is not a bound; POSIX/GNU treat it literally.
                    None => return Ok(None),
                    Some(m) if i == high_start => (m, None),
                    Some(m) => match parse_usize(&ch[high_start..i]) {
                        Some(n) => {
                            if m > n {
                                return Err(format!(
                                    "repetition range out of order: {{{m},{n}}} has a lower \
                                     bound that exceeds its upper bound"
                                ));
                            }
                            (m, Some(n))
                        }
                        None => return Ok(None),
                    },
                }
            }
            _ => return Ok(None),
        };

        check_bound(min)?;
        if let Some(n) = max {
            check_bound(n)?;
        }
        // `i` points at the closing '}' on every success path.
        Ok(Some((min, max, i + 1)))
    }

    /// Parse one atom. A quantifier with nothing in front of it (`*`, `+`, `?`)
    /// is a literal, and `{`/`}` reached here are literals too.
    fn parse_atom(&mut self) -> Result<Ast, String> {
        let c = match self.next() {
            Some(c) => c,
            None => return Ok(Ast::Concat(Vec::new())),
        };
        match c {
            '(' => {
                let open = self.pos - 1;
                // `(?:` is a non-capturing group; `(?` followed by anything
                // else is a capturing group whose body starts with `?`.
                let capturing = !(self.peek() == Some('?') && self.peek_at(1) == Some(':'));
                if !capturing {
                    self.pos += 2;
                }
                let index = if capturing {
                    self.n_groups += 1;
                    Some(self.n_groups)
                } else {
                    None
                };
                let body = self.parse_alt()?;
                if self.next() != Some(')') {
                    return Err(format!(
                        "unclosed group: missing ')' for group opened at offset {open}"
                    ));
                }
                Ok(Ast::Group {
                    index,
                    body: Box::new(body),
                })
            }
            '[' => self.parse_class(),
            '.' => Ok(Ast::Any),
            '^' => Ok(Ast::Start),
            '$' => Ok(Ast::End),
            // Lenient GNU-style handling: a leading quantifier is a literal.
            '*' | '+' | '?' | '{' | '}' => Ok(Ast::Char(c)),
            '\\' => self.parse_escape(),
            other => Ok(Ast::Char(other)),
        }
    }

    /// Parse the body of a bracket expression; the opening `[` is consumed.
    fn parse_class(&mut self) -> Result<Ast, String> {
        let open = self.pos - 1;
        let mut negated = false;
        if self.peek() == Some('^') {
            negated = true;
            self.pos += 1;
        }

        let mut items: Vec<ClassItem> = Vec::new();
        // A `]` in first position is a literal `]`.
        let mut first = true;

        loop {
            let c = match self.peek() {
                Some(c) => c,
                None => {
                    return Err(format!(
                        "unclosed bracket expression starting at offset {open}: missing ']'"
                    ));
                }
            };
            if c == ']' && !first {
                self.pos += 1;
                break;
            }

            // POSIX class: `[:name:]`.
            if c == '[' && self.peek_at(1) == Some(':') {
                self.pos += 2;
                let mut name = String::new();
                loop {
                    match self.next() {
                        Some(':') if self.peek() == Some(']') => {
                            self.pos += 1;
                            break;
                        }
                        Some(ch) => name.push(ch),
                        None => {
                            return Err(format!(
                                "unclosed POSIX character class in the bracket expression \
                                 starting at offset {open}"
                            ));
                        }
                    }
                }
                let class = PosixClass::from_name(&name)
                    .ok_or_else(|| format!("unknown POSIX character class '[:{name}:]'"))?;
                items.push(ClassItem::Posix(class));
                first = false;
                continue;
            }

            match self.next_class_piece(open)? {
                ClassPiece::Cls(class) => items.push(ClassItem::Posix(class)),
                ClassPiece::Ch(low) => {
                    // A `-` is a range operator only when something other than
                    // `]` follows it; otherwise it is a literal `-`.
                    let is_range = self.peek() == Some('-')
                        && matches!(self.peek_at(1), Some(high) if high != ']');
                    if is_range {
                        self.pos += 1; // consume '-'
                        match self.next_class_piece(open)? {
                            ClassPiece::Ch(high) => {
                                if low > high {
                                    return Err(format!(
                                        "invalid character range '{low}-{high}' in the bracket \
                                         expression starting at offset {open}: lower bound \
                                         exceeds upper bound"
                                    ));
                                }
                                items.push(ClassItem::Range(low, high));
                            }
                            ClassPiece::Cls(_) => {
                                return Err(format!(
                                    "invalid character range in the bracket expression starting \
                                     at offset {open}: the right-hand side is a character class"
                                ));
                            }
                        }
                    } else {
                        items.push(ClassItem::Range(low, low));
                    }
                }
            }
            first = false;
        }

        Ok(Ast::Class(ClassSet { negated, items }))
    }

    /// Read one bracket-expression member, resolving backslash escapes.
    ///
    /// Inside brackets `\d`, `\w` and `\s` expand to POSIX classes (a GNU
    /// convenience); any other escaped character stands for itself, with
    /// `\n`, `\t`, `\r` as the usual control characters.
    fn next_class_piece(&mut self, open: usize) -> Result<ClassPiece, String> {
        let c = match self.next() {
            Some(c) => c,
            None => {
                return Err(format!(
                    "unclosed bracket expression starting at offset {open}: missing ']'"
                ));
            }
        };
        if c != '\\' {
            return Ok(ClassPiece::Ch(c));
        }
        let escaped = match self.next() {
            Some(c) => c,
            None => {
                return Err(format!(
                    "unclosed bracket expression starting at offset {open}: trailing backslash"
                ));
            }
        };
        Ok(match escaped {
            'n' => ClassPiece::Ch('\n'),
            't' => ClassPiece::Ch('\t'),
            'r' => ClassPiece::Ch('\r'),
            'd' => ClassPiece::Cls(PosixClass::Digit),
            'w' => ClassPiece::Cls(PosixClass::Word),
            's' => ClassPiece::Cls(PosixClass::Space),
            other => ClassPiece::Ch(other),
        })
    }

    /// Parse an escape sequence outside brackets.
    fn parse_escape(&mut self) -> Result<Ast, String> {
        let escaped = match self.next() {
            Some(c) => c,
            None => return Err("trailing backslash at end of pattern".to_string()),
        };
        Ok(match escaped {
            'd' => Ast::Class(ClassSet {
                negated: false,
                items: vec![ClassItem::Posix(PosixClass::Digit)],
            }),
            'D' => Ast::Class(ClassSet {
                negated: true,
                items: vec![ClassItem::Posix(PosixClass::Digit)],
            }),
            'w' => Ast::Class(ClassSet {
                negated: false,
                items: vec![ClassItem::Posix(PosixClass::Word)],
            }),
            'W' => Ast::Class(ClassSet {
                negated: true,
                items: vec![ClassItem::Posix(PosixClass::Word)],
            }),
            's' => Ast::Class(ClassSet {
                negated: false,
                items: vec![ClassItem::Posix(PosixClass::Space)],
            }),
            'S' => Ast::Class(ClassSet {
                negated: true,
                items: vec![ClassItem::Posix(PosixClass::Space)],
            }),
            'b' => Ast::WordBoundary(false),
            'B' => Ast::WordBoundary(true),
            'n' => Ast::Char('\n'),
            't' => Ast::Char('\t'),
            'r' => Ast::Char('\r'),
            '1'..='9' => {
                let n = escaped.to_digit(10).unwrap_or(0) as usize;
                if n > self.max_backref {
                    self.max_backref = n;
                }
                Ast::Backref(2 * n)
            }
            // `\.`, `\*`, `\(`, `\\`, `\/`, ... and `\0`: the literal character.
            other => Ast::Char(other),
        })
    }
}

/// Parse a full pattern, validating backreferences once the group count is
/// known.
fn parse_pattern(pattern: &str) -> Result<(Ast, usize), String> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        n_groups: 0,
        max_backref: 0,
    };
    let ast = parser.parse_alt()?;
    if parser.pos < parser.chars.len() {
        // `parse_concat` stops at `)` and `parse_alt` consumes `|`, so anything
        // left over is an unmatched closing parenthesis.
        return Err(format!("unmatched ')' at offset {}", parser.pos));
    }
    if parser.max_backref > parser.n_groups {
        return Err(format!(
            "backreference \\{} refers to a group that does not exist: the pattern has {} \
             capture group(s)",
            parser.max_backref, parser.n_groups
        ));
    }
    Ok((ast, parser.n_groups))
}

/// Parse a run of ASCII digits without overflowing.
fn parse_usize(digits: &[char]) -> Option<usize> {
    let mut value: usize = 0;
    for digit in digits {
        value = value
            .checked_mul(10)?
            .checked_add(digit.to_digit(10)? as usize)?;
    }
    Some(value)
}

/// Reject repetition bounds that would explode the compiled program.
fn check_bound(bound: usize) -> Result<(), String> {
    if bound > MAX_REPEAT_BOUND {
        Err(format!(
            "repetition count {bound} exceeds the maximum supported bound of {MAX_REPEAT_BOUND}"
        ))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

/// One virtual-machine instruction.
#[derive(Debug, Clone)]
enum Inst {
    /// Consume one literal character.
    Char(char),
    /// Consume one character that is not `\n`.
    Any,
    /// Consume one character accepted by the class.
    Class(ClassSet),
    /// Try the first target, remembering the second for backtracking.
    Split(usize, usize),
    /// Unconditional jump.
    Jmp(usize),
    /// Record the current position in a save slot.
    Save(usize),
    /// Record the current position in a loop-progress slot.
    PosSave(usize),
    /// Stop iterating when the position did not move since `PosSave` recorded
    /// it, continuing at `exit`.
    Progress { slot: usize, exit: usize },
    /// Assert the start of the whole text.
    Start,
    /// Assert the end of the whole text.
    End,
    /// Assert a word boundary (`true` = assert its absence).
    WordBoundary(bool),
    /// Match the text previously captured in the given slot pair.
    Backref(usize),
    /// Accept.
    Match,
}

/// Emits a program from the AST, patching jump targets as it goes.
///
/// Bounded repetition (`{m,n}`) is compiled by expansion; unbounded repetition
/// becomes a `Split`/`Jmp` loop with a `Progress` guard so bodies that can match
/// the empty string (`(a?)*`) still terminate.
struct Compiler {
    prog: Vec<Inst>,
    /// Number of save slots reserved for capture groups.
    base_slots: usize,
    /// Number of loop-progress slots allocated so far.
    pos_slots: usize,
}

impl Compiler {
    fn new(n_groups: usize) -> Self {
        Self {
            prog: Vec::new(),
            // Two slots per group, with group 0 tracking the whole match.
            base_slots: 2 * (n_groups + 1),
            pos_slots: 0,
        }
    }

    fn len(&self) -> usize {
        self.prog.len()
    }

    /// Append an instruction, refusing to build an unbounded program.
    fn emit(&mut self, inst: Inst) -> Result<(), String> {
        if self.prog.len() >= MAX_PROGRAM_LEN {
            return Err(format!(
                "pattern too large: the compiled program exceeds {MAX_PROGRAM_LEN} instructions"
            ));
        }
        self.prog.push(inst);
        Ok(())
    }

    fn alloc_pos_slot(&mut self) -> usize {
        let slot = self.base_slots + self.pos_slots;
        self.pos_slots += 1;
        slot
    }

    fn patch(&mut self, at: usize, inst: Inst) {
        if let Some(cell) = self.prog.get_mut(at) {
            *cell = inst;
        }
    }

    fn compile(&mut self, ast: &Ast) -> Result<(), String> {
        match ast {
            Ast::Concat(items) => {
                for item in items {
                    self.compile(item)?;
                }
            }
            Ast::Char(c) => self.emit(Inst::Char(*c))?,
            Ast::Any => self.emit(Inst::Any)?,
            Ast::Class(set) => self.emit(Inst::Class(set.clone()))?,
            Ast::Group { index, body } => {
                match index {
                    Some(group) => {
                        // Group 0 is the whole match, so capture groups start
                        // at slot 2.
                        self.emit(Inst::Save(2 * group))?;
                        self.compile(body)?;
                        self.emit(Inst::Save(2 * group + 1))?;
                    }
                    None => self.compile(body)?,
                }
            }
            Ast::Alt(branches) => self.compile_alt(branches)?,
            Ast::Repeat { body, min, max } => self.compile_repeat(body, *min, *max)?,
            Ast::Start => self.emit(Inst::Start)?,
            Ast::End => self.emit(Inst::End)?,
            Ast::WordBoundary(negated) => self.emit(Inst::WordBoundary(*negated))?,
            Ast::Backref(slot) => self.emit(Inst::Backref(*slot))?,
        }
        Ok(())
    }

    /// `a|b|c` becomes nested `Split`s; each branch jumps over the rest.
    fn compile_alt(&mut self, branches: &[Ast]) -> Result<(), String> {
        match branches {
            [] => Ok(()),
            [only] => self.compile(only),
            [first, rest @ ..] => {
                let split_at = self.len();
                self.emit(Inst::Split(0, 0))?;
                let first_start = self.len();
                self.compile(first)?;
                let jmp_at = self.len();
                self.emit(Inst::Jmp(0))?;
                let rest_start = self.len();
                self.compile_alt(rest)?;
                let end = self.len();
                // Prefer the first branch, fall back to the rest.
                self.patch(split_at, Inst::Split(first_start, rest_start));
                self.patch(jmp_at, Inst::Jmp(end));
                Ok(())
            }
        }
    }

    fn compile_repeat(&mut self, body: &Ast, min: usize, max: Option<usize>) -> Result<(), String> {
        match max {
            Some(high) => {
                // Mandatory copies, then optional ones that either enter the
                // body or skip to the end of the repetition.
                for _ in 0..min {
                    self.compile(body)?;
                }
                let mut splits = Vec::with_capacity(high.saturating_sub(min));
                for _ in min..high {
                    splits.push(self.len());
                    self.emit(Inst::Split(0, 0))?;
                    self.compile(body)?;
                }
                let end = self.len();
                for at in splits {
                    self.patch(at, Inst::Split(at + 1, end));
                }
                Ok(())
            }
            None => {
                for _ in 0..min {
                    self.compile(body)?;
                }
                self.compile_star(body)
            }
        }
    }

    /// Compile an unbounded greedy loop:
    ///
    /// ```text
    /// loop:  Split(body, end)      ; prefer another iteration
    /// body:  PosSave(slot)
    ///        <body>
    ///        Progress(slot, end)    ; stop when the body consumed nothing
    ///        Jmp(loop)
    /// end:
    /// ```
    fn compile_star(&mut self, body: &Ast) -> Result<(), String> {
        let slot = self.alloc_pos_slot();
        let loop_start = self.len();
        self.emit(Inst::Split(0, 0))?;
        let body_start = self.len();
        self.emit(Inst::PosSave(slot))?;
        self.compile(body)?;
        let progress_at = self.len();
        self.emit(Inst::Progress { slot, exit: 0 })?;
        self.emit(Inst::Jmp(loop_start))?;
        let end = self.len();
        self.patch(loop_start, Inst::Split(body_start, end));
        self.patch(progress_at, Inst::Progress { slot, exit: end });
        Ok(())
    }
}

/// Compile a whole pattern, wrapping it in `Save(0)`/`Save(1)` so slot pair 0
/// always holds the overall match.
fn compile_pattern(ast: &Ast, n_groups: usize) -> Result<(Vec<Inst>, usize), String> {
    let mut compiler = Compiler::new(n_groups);
    compiler.emit(Inst::Save(0))?;
    compiler.compile(ast)?;
    compiler.emit(Inst::Save(1))?;
    compiler.emit(Inst::Match)?;
    let n_slots = compiler.base_slots + compiler.pos_slots;
    Ok((compiler.prog, n_slots))
}

// ---------------------------------------------------------------------------
// Haystack index
// ---------------------------------------------------------------------------

/// The haystack plus its char/byte mapping.
///
/// Matching works on char indices; `byte_at` converts back to the byte offsets
/// the public API promises. Keeping one index per public call also keeps
/// `replace_all` and `split` linear in the number of matches.
struct Search<'t> {
    text: &'t str,
    chars: Vec<(usize, char)>,
}

impl<'t> Search<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            chars: text.char_indices().collect(),
        }
    }

    /// Byte offset of a char index; the length for the one-past-the-end index.
    fn byte_at(&self, char_index: usize) -> usize {
        match self.chars.get(char_index) {
            Some((byte, _)) => *byte,
            None => self.text.len(),
        }
    }

    /// First char index whose byte offset is at or after `byte`.
    fn char_index_at_or_after(&self, byte: usize) -> usize {
        self.chars.partition_point(|(offset, _)| *offset < byte)
    }
}

/// Byte offset just past the character starting at `byte`, if any.
fn next_char_end(text: &str, byte: usize) -> Option<usize> {
    text.get(byte..)?
        .chars()
        .next()
        .map(|c| byte + c.len_utf8())
}

/// Append the text of capture group `group`, if it participated.
fn push_group(out: &mut String, text: &str, caps: &[Option<(usize, usize)>], group: usize) {
    if let Some(Some((start, end))) = caps.get(group)
        && let Some(slice) = text.get(*start..*end)
    {
        out.push_str(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a pattern, reporting the compile error if it is invalid.
    fn re(pattern: &str) -> Regex {
        match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(err) => panic!("pattern {pattern:?} failed to compile: {err}"),
        }
    }

    /// Compile with ASCII ignore-case.
    fn re_i(pattern: &str) -> Regex {
        match Regex::new_with_flags(pattern, true) {
            Ok(regex) => regex,
            Err(err) => panic!("pattern {pattern:?} failed to compile: {err}"),
        }
    }

    /// Captured substrings (index 0 = whole match) for a successful match.
    fn groups(pattern: &str, text: &str) -> Vec<Option<String>> {
        match re(pattern).captures(text) {
            Some(caps) => caps
                .into_iter()
                .map(|span| span.map(|(s, e)| text.get(s..e).unwrap_or("").to_string()))
                .collect(),
            None => panic!("pattern {pattern:?} did not match {text:?}"),
        }
    }

    // ---- literals ---------------------------------------------------------

    #[test]
    fn literal_matches_and_offsets() {
        let regex = re("world");
        assert!(regex.is_match("hello world"));
        assert_eq!(regex.find("hello world"), Some((6, 11)));
    }

    #[test]
    fn literal_does_not_match() {
        assert!(!re("world").is_match("hello"));
        assert_eq!(re("world").find("hello"), None);
    }

    #[test]
    fn empty_pattern_matches_empty_at_offset_zero() {
        let regex = re("");
        assert!(regex.is_match("abc"));
        assert_eq!(regex.find("abc"), Some((0, 0)));
        assert_eq!(regex.group_count(), 0);
        assert_eq!(regex.captures(""), Some(vec![Some((0, 0))]));
    }

    #[test]
    fn find_is_leftmost() {
        let regex = re("ab");
        assert_eq!(regex.find("xxabxxab"), Some((2, 4)));
    }

    // ---- dot --------------------------------------------------------------

    #[test]
    fn dot_matches_any_char_except_newline() {
        let regex = re("a.c");
        assert!(regex.is_match("abc"));
        assert!(regex.is_match("a c"));
        assert!(!regex.is_match("a\nc"));
        assert!(!regex.is_match("ac"));
    }

    #[test]
    fn dot_matches_utf8_scalar_as_one_char() {
        assert_eq!(re(".").find("é"), Some((0, 2)));
    }

    // ---- repetition -------------------------------------------------------

    #[test]
    fn star_repetition() {
        let regex = re("ab*c");
        assert!(regex.is_match("ac"));
        assert!(regex.is_match("abbbc"));
        assert!(!regex.is_match("adc"));
        assert_eq!(re("a*").find("aaa"), Some((0, 3)));
    }

    #[test]
    fn plus_repetition() {
        let regex = re("ab+c");
        assert!(regex.is_match("abc"));
        assert!(regex.is_match("abbbc"));
        assert!(!regex.is_match("ac"));
    }

    #[test]
    fn question_repetition() {
        let regex = re("ab?c");
        assert!(regex.is_match("ac"));
        assert!(regex.is_match("abc"));
        assert!(!regex.is_match("abbc"));
    }

    #[test]
    fn repeat_exact_count() {
        let regex = re("^a{3}$");
        assert!(regex.is_match("aaa"));
        assert!(!regex.is_match("aa"));
        assert!(!regex.is_match("aaaa"));
    }

    #[test]
    fn repeat_at_least_count() {
        let regex = re("^a{2,}$");
        assert!(!regex.is_match("a"));
        assert!(regex.is_match("aa"));
        assert!(regex.is_match("aaaaa"));
    }

    #[test]
    fn repeat_bounded_range() {
        let regex = re("^a{1,3}$");
        assert!(!regex.is_match(""));
        assert!(regex.is_match("a"));
        assert!(regex.is_match("aaa"));
        assert!(!regex.is_match("aaaa"));
    }

    #[test]
    fn repeat_zero_matches_empty() {
        let regex = re("ba{0}");
        assert_eq!(regex.find("b"), Some((0, 1)));
        assert_eq!(re("a{0}").find("aaa"), Some((0, 0)));
    }

    #[test]
    fn repetition_is_greedy() {
        assert_eq!(re("a+").find("aaaa"), Some((0, 4)));
        assert_eq!(re("<.*>").find("<a><b>"), Some((0, 6)));
    }

    #[test]
    fn nullable_body_repetition_terminates() {
        assert!(re("(a?)*").is_match(""));
        assert!(re("(a*)*").is_match("aaa"));
        assert_eq!(re("(a?)*b").find("aab"), Some((0, 3)));
        assert!(!re("(a?)*b").is_match("aa"));
        // The final iteration matched empty; its capture stays visible.
        assert_eq!(
            re("(a?)*").captures(""),
            Some(vec![Some((0, 0)), Some((0, 0))])
        );
        assert_eq!(
            re("(a)*").captures("aa"),
            Some(vec![Some((0, 2)), Some((1, 2))])
        );
    }

    #[test]
    fn invalid_bound_is_a_literal_open_brace() {
        assert!(re("a{").is_match("a{"));
        assert!(!re("a{").is_match("a"));
        assert!(re("a{b}").is_match("a{b}"));
        assert!(re("{2,3}").is_match("{2,3}"));
        assert!(re("a{,2}").is_match("a{,2}"));
        assert!(re("a{2").is_match("a{2"));
    }

    #[test]
    fn leading_quantifier_is_a_literal() {
        assert!(re("*a").is_match("*a"));
        assert!(re("+").is_match("+"));
        assert!(re("a**").is_match("a**"));
    }

    // ---- alternation and groups -------------------------------------------

    #[test]
    fn alternation_picks_first_successful_branch() {
        let regex = re("cat|dog");
        assert!(regex.is_match("hotdog"));
        assert!(regex.is_match("cat"));
        assert!(!regex.is_match("cow"));
        // Perl-style: the first branch wins, not the longest match.
        assert_eq!(re("a|ab").find("ab"), Some((0, 1)));
    }

    #[test]
    fn alternation_with_empty_branch() {
        assert_eq!(re("x|").find("abc"), Some((0, 0)));
        assert_eq!(re("x|").find("x"), Some((0, 1)));
        assert_eq!(re("|x").find("x"), Some((0, 0)));
    }

    #[test]
    fn group_count_numbers_by_opening_paren() {
        assert_eq!(re("abc").group_count(), 0);
        assert_eq!(re("(a)(b)").group_count(), 2);
        assert_eq!(re("(a)(?:b)(c)").group_count(), 2);
        assert_eq!(re(r"\(a\)").group_count(), 0);
        assert_eq!(re("((a)(b))").group_count(), 3);
    }

    #[test]
    fn capturing_groups_report_spans() {
        assert_eq!(
            groups("(a)(b)(?:c)", "abc"),
            vec![
                Some("abc".to_string()),
                Some("a".to_string()),
                Some("b".to_string())
            ]
        );
    }

    #[test]
    fn nested_groups_report_outer_and_inner_spans() {
        assert_eq!(
            groups("((a)(b))", "ab"),
            vec![
                Some("ab".to_string()),
                Some("ab".to_string()),
                Some("a".to_string()),
                Some("b".to_string()),
            ]
        );
    }

    #[test]
    fn non_capturing_group_matches_without_a_slot() {
        let regex = re("(?:ab)+c");
        assert!(regex.is_match("ababc"));
        assert_eq!(regex.group_count(), 0);
        assert_eq!(regex.find("ababc"), Some((0, 5)));
    }

    #[test]
    fn non_participating_group_is_none() {
        assert_eq!(
            groups("(a)|(b)", "b"),
            vec![Some("b".to_string()), None, Some("b".to_string())]
        );
        assert_eq!(groups("(a)?b", "b"), vec![Some("b".to_string()), None]);
    }

    #[test]
    fn captures_none_when_there_is_no_match() {
        assert_eq!(re("(a)").captures("zzz"), None);
    }

    // ---- anchors ----------------------------------------------------------

    #[test]
    fn caret_anchors_the_start() {
        assert!(re("^ab").is_match("abc"));
        assert!(!re("^b").is_match("ab"));
    }

    #[test]
    fn dollar_anchors_the_end() {
        assert!(re("bc$").is_match("abc"));
        assert!(!re("b$").is_match("abc"));
    }

    #[test]
    fn anchors_span_the_whole_text_only() {
        // No multiline mode: an interior newline is not an anchor point.
        assert!(!re("^b").is_match("a\nb"));
        assert!(!re("a$").is_match("a\nb"));
        assert!(re("^a").is_match("a\nb"));
        assert!(re("^a$").is_match("a"));
        assert!(re("^$").is_match(""));
        assert!(!re("^$").is_match("\n"));
        assert_eq!(re("^a$").find("a"), Some((0, 1)));
    }

    // ---- bracket expressions ---------------------------------------------

    #[test]
    fn class_matches_membership() {
        let regex = re("^[abc]+$");
        assert!(regex.is_match("cab"));
        assert!(!regex.is_match("cad"));
    }

    #[test]
    fn class_supports_ranges() {
        assert!(re("^[a-c]+$").is_match("abc"));
        assert!(!re("^[a-c]+$").is_match("abd"));
        assert!(re("^[a-zA-Z0-9_]+$").is_match("aZ9_"));
    }

    #[test]
    fn negated_class() {
        assert!(re("^[^abc]+$").is_match("xyz"));
        assert!(!re("^[^abc]+$").is_match("xay"));
    }

    #[test]
    fn negated_class_matches_newline_unlike_dot() {
        assert!(re("[^x]").is_match("\n"));
        assert!(!re(".").is_match("\n"));
    }

    #[test]
    fn class_close_bracket_first_is_literal() {
        assert!(re("[]]").is_match("]"));
        assert!(re("[]a]+").is_match("a]]a"));
        assert!(!re("[]]").is_match("a"));
    }

    #[test]
    fn class_escapes_inside() {
        assert!(re(r"[\]]").is_match("]"));
        assert!(!re(r"[\]]").is_match("a"));
        assert!(re(r"[\]\\]").is_match("\\"));
        assert!(re(r"[\d]+").is_match("123"));
        assert!(!re(r"[\d]+").is_match("abc"));
        assert!(re(r"[\w]+").is_match("a_1"));
        assert!(re(r"[\s]").is_match("\t"));
    }

    #[test]
    fn class_dash_is_literal_at_the_edges() {
        assert!(re("^[a-]+$").is_match("a-a"));
        assert!(re("^[-a]+$").is_match("-a"));
        assert!(re(r"^[a\-z]+$").is_match("a-z"));
    }

    #[test]
    fn class_dot_is_literal() {
        assert!(re("^[.]$").is_match("."));
        assert!(!re("^[.]$").is_match("a"));
    }

    // ---- POSIX classes ----------------------------------------------------

    #[test]
    fn posix_alpha() {
        assert!(re("^[[:alpha:]]+$").is_match("abcXYZ"));
        assert!(!re("^[[:alpha:]]+$").is_match("ab1"));
    }

    #[test]
    fn posix_digit() {
        assert!(re("^[[:digit:]]+$").is_match("0123"));
        assert!(!re("^[[:digit:]]+$").is_match("12a"));
    }

    #[test]
    fn posix_alnum() {
        assert!(re("^[[:alnum:]]+$").is_match("a1Z9"));
        assert!(!re("^[[:alnum:]]+$").is_match("a_1"));
    }

    #[test]
    fn posix_space() {
        assert!(re("^[[:space:]]+$").is_match(" \t\n\r"));
        assert!(!re("^[[:space:]]+$").is_match(" a"));
    }

    #[test]
    fn posix_upper_and_lower() {
        assert!(re("^[[:upper:]]+$").is_match("ABC"));
        assert!(!re("^[[:upper:]]+$").is_match("aBC"));
        assert!(re("^[[:lower:]]+$").is_match("abc"));
        assert!(!re("^[[:lower:]]+$").is_match("Abc"));
    }

    #[test]
    fn posix_punct() {
        assert!(re("^[[:punct:]]+$").is_match("!?,;"));
        assert!(!re("^[[:punct:]]+$").is_match("a!"));
    }

    #[test]
    fn posix_xdigit() {
        assert!(re("^[[:xdigit:]]+$").is_match("09afAF"));
        assert!(!re("^[[:xdigit:]]+$").is_match("g"));
    }

    #[test]
    fn posix_blank_excludes_newline() {
        assert!(re("^[[:blank:]]+$").is_match(" \t"));
        assert!(!re("^[[:blank:]]+$").is_match("\n"));
    }

    #[test]
    fn posix_cntrl() {
        assert!(re("^[[:cntrl:]]+$").is_match("\u{1}\u{7f}"));
        assert!(!re("^[[:cntrl:]]+$").is_match("a"));
    }

    #[test]
    fn posix_print_and_graph() {
        assert!(re("^[[:print:]]+$").is_match("a b!"));
        assert!(!re("^[[:print:]]+$").is_match("a\nb"));
        assert!(re("^[[:graph:]]+$").is_match("a!b"));
        assert!(!re("^[[:graph:]]+$").is_match(" "));
    }

    #[test]
    fn posix_classes_are_ascii_only() {
        assert!(!re("[[:alpha:]]").is_match("é"));
        assert!(!re(r"\w").is_match("é"));
    }

    #[test]
    fn combinable_class_with_posix_member() {
        assert!(re("^[[:digit:]_]+$").is_match("1_2"));
    }

    // ---- GNU shorthands and escapes ---------------------------------------

    #[test]
    fn shorthand_digit_word_space() {
        assert!(re(r"^\d+$").is_match("123"));
        assert!(!re(r"^\d+$").is_match("12a"));
        assert!(re(r"^\w+$").is_match("a_1Z"));
        assert!(!re(r"^\w+$").is_match("a-1"));
        assert!(re(r"^\s+$").is_match(" \t\n"));
        assert!(!re(r"^\s+$").is_match(" a"));
    }

    #[test]
    fn negated_shorthands() {
        assert!(re(r"^\D+$").is_match("abc"));
        assert!(!re(r"^\D+$").is_match("a1"));
        assert!(re(r"^\W+$").is_match("-+"));
        assert!(!re(r"^\W+$").is_match("a"));
        assert!(re(r"^\S+$").is_match("abc"));
        assert!(!re(r"^\S+$").is_match("a b"));
    }

    #[test]
    fn word_boundaries() {
        assert!(re(r"\bcat\b").is_match("the cat sat"));
        assert!(!re(r"\bcat\b").is_match("scatter"));
        assert!(re(r"\bcat").is_match("cats"));
        assert!(re(r"\Bcat").is_match("scatter"));
        assert!(!re(r"\Bcat").is_match("cat"));
        assert_eq!(re(r"\bcat\b").find("a cat!"), Some((2, 5)));
    }

    #[test]
    fn escaped_metacharacters_are_literals() {
        assert!(re(r"a\.b").is_match("a.b"));
        assert!(!re(r"a\.b").is_match("axb"));
        assert!(re(r"a\*b").is_match("a*b"));
        assert!(!re(r"a\*b").is_match("aab"));
        assert!(re(r"\(x\)").is_match("(x)"));
        assert!(re(r"a\|b").is_match("a|b"));
        assert!(!re(r"a\|b").is_match("a"));
        assert!(re(r"a\+b").is_match("a+b"));
        assert!(re(r"a\?b").is_match("a?b"));
        assert!(re(r"a\{b\}").is_match("a{b}"));
        assert!(re(r"a\[b\]").is_match("a[b]"));
        assert!(re(r"\^x").is_match("^x"));
        assert!(re(r"x\$").is_match("x$"));
        assert!(re(r"a\/b").is_match("a/b"));
        assert!(re(r"a\\b").is_match("a\\b"));
    }

    #[test]
    fn escaped_control_characters() {
        assert!(re(r"a\nb").is_match("a\nb"));
        assert!(re(r"a\tb").is_match("a\tb"));
        assert!(re(r"a\rb").is_match("a\rb"));
    }

    // ---- backreferences ---------------------------------------------------

    #[test]
    fn backreference_matches_repeated_text() {
        assert!(re(r"(a|b)\1").is_match("aa"));
        assert!(!re(r"(a|b)\1").is_match("ab"));
        assert_eq!(re(r"(ab)\1").find("xabab"), Some((1, 5)));
        assert_eq!(
            groups(r"(ab)\1", "abab"),
            vec![Some("abab".to_string()), Some("ab".to_string())]
        );
    }

    #[test]
    fn backreference_can_point_forwards_but_never_matches() {
        // Compiles (the group exists) but the group has not participated yet.
        assert!(!re(r"\1(a)").is_match("aa"));
    }

    #[test]
    fn backreference_to_missing_group_is_an_error() {
        assert!(Regex::new(r"(a)\2").is_err());
        assert!(Regex::new(r"\1").is_err());
        assert!(Regex::new(r"(a)(b)\3").is_err());
    }

    // ---- UTF-8 ------------------------------------------------------------

    #[test]
    fn utf8_offsets_are_bytes() {
        // "café": c=0, a=1, f=2, é=3..5
        assert_eq!(re("é").find("café"), Some((3, 5)));
        assert_eq!(re("^.$").find("é"), Some((0, 2)));
        assert_eq!(re("é+").find("éé"), Some((0, 4)));
        assert_eq!(re("[é]").find("é"), Some((0, 2)));
    }

    #[test]
    fn utf8_capture_offsets_are_bytes() {
        // "éx": é = 0..2, x = 2..3
        assert_eq!(
            re("(é)x").captures("éx"),
            Some(vec![Some((0, 3)), Some((0, 2))])
        );
        assert_eq!(
            re("(a)(é)").captures("aé"),
            Some(vec![Some((0, 3)), Some((0, 1)), Some((1, 3))])
        );
    }

    #[test]
    fn utf8_replace_and_split_keep_boundaries() {
        assert_eq!(re("é").replace_all("café", "e"), "cafe");
        assert_eq!(re("é").replace_all("éé", "<&>"), "<é><é>");
        assert_eq!(re("é").split("aéb"), vec!["a".to_string(), "b".to_string()]);
    }

    // ---- ignore case ------------------------------------------------------

    #[test]
    fn ignore_case_literal() {
        assert!(re_i("abc").is_match("ABC"));
        assert!(re_i("abc").is_match("aBc"));
        assert!(!re("abc").is_match("ABC"));
    }

    #[test]
    fn ignore_case_applies_to_classes() {
        assert!(re_i("^[a-z]+$").is_match("ABC"));
        assert!(re_i("^[A-Z]+$").is_match("abc"));
        assert!(!re_i("^[^a]+$").is_match("A"));
        assert!(!re_i("^[^a]+$").is_match("a"));
    }

    #[test]
    fn ignore_case_applies_to_backreferences() {
        assert!(re_i(r"(a)\1").is_match("aA"));
    }

    #[test]
    fn ignore_case_is_ascii_only() {
        assert!(re_i("é").is_match("é"));
        assert!(!re_i("é").is_match("É"));
    }

    // ---- step budget ------------------------------------------------------

    #[test]
    fn default_step_budget_is_documented_and_nonzero() {
        assert_eq!(DEFAULT_STEP_BUDGET, 100_000);
    }

    #[test]
    fn step_budget_abandons_pathological_pattern() {
        let regex = re("(a+)+$");
        let text = format!("{}!", "a".repeat(64));
        // Must return quickly with "no match" rather than hanging.
        assert!(!regex.is_match(&text));
        assert_eq!(regex.find(&text), None);
        assert_eq!(regex.captures(&text), None);
    }

    #[test]
    fn step_budget_does_not_disturb_ordinary_matches() {
        let regex = re("(a+)+$");
        assert!(regex.is_match("aaaa"));
        assert_eq!(regex.find("aaaa"), Some((0, 4)));
        let nested = re("^(a|b)*$");
        assert!(nested.is_match("ababababab"));
        assert!(!nested.is_match("abababababc"));
    }

    #[test]
    fn long_haystack_gets_a_fresh_budget_per_position() {
        let text = format!("{}needle", "x".repeat(50_000));
        assert_eq!(re("needle").find(&text), Some((50_000, 50_006)));
        assert!(!re("haystack").is_match(&text));
    }

    // ---- replace_all ------------------------------------------------------

    #[test]
    fn replace_all_basic() {
        assert_eq!(re("b+").replace_all("abbbc", "-"), "a-c");
        assert_eq!(re("z").replace_all("abc", "-"), "abc");
        assert_eq!(re("a").replace_all("", "-"), "");
    }

    #[test]
    fn replace_all_expands_numbered_groups() {
        assert_eq!(
            re(r"(\w+)@(\w+)").replace_all("user@host", "$2:$1"),
            "host:user"
        );
        assert_eq!(
            re(r"(\w+)@(\w+)").replace_all("user@host", "${2}${1}"),
            "hostuser"
        );
        assert_eq!(re("abc").replace_all("abc", "<$0>"), "<abc>");
        assert_eq!(re("abc").replace_all("abc", "<${0}>"), "<abc>");
    }

    #[test]
    fn replace_all_supports_multi_digit_braced_groups() {
        let regex = re("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)");
        assert_eq!(regex.group_count(), 11);
        assert_eq!(regex.replace_all("abcdefghijk", "${11}"), "k");
        assert_eq!(regex.replace_all("abcdefghijk", "${1}${11}"), "ak");
    }

    #[test]
    fn replace_all_ampersand_is_the_whole_match() {
        assert_eq!(re("abc").replace_all("xabc", "[&]"), "x[abc]");
        assert_eq!(re("b+").replace_all("abbb", "<&>"), "a<bbb>");
    }

    #[test]
    fn replace_all_escapes() {
        assert_eq!(re("a").replace_all("a", r"\$"), "$");
        assert_eq!(re("a").replace_all("a", r"\\"), "\\");
        assert_eq!(re("a").replace_all("a", r"\$1"), "$1");
        // A backslash before anything else is kept verbatim.
        assert_eq!(re("a").replace_all("a", r"\x"), "\\x");
        // A lone trailing backslash is kept verbatim.
        assert_eq!(re("a").replace_all("a", "x\\"), "x\\");
    }

    #[test]
    fn replace_all_missing_or_absent_group_expands_to_empty() {
        assert_eq!(re("(a)").replace_all("a", "<$3>"), "<>");
        assert_eq!(re("(a)").replace_all("a", "<${9}>"), "<>");
        assert_eq!(re("(a)?").replace_all("b", "<$1>"), "<>b<>");
    }

    #[test]
    fn replace_all_zero_width_matches_make_progress() {
        assert_eq!(re("").replace_all("ab", "-"), "-a-b-");
        assert_eq!(re("a*").replace_all("ba", "X"), "XbX");
        assert_eq!(re("a*").replace_all("", "X"), "X");
        assert_eq!(re(r"\b").replace_all("ab cd", "|"), "|ab| |cd|");
    }

    // ---- split ------------------------------------------------------------

    #[test]
    fn split_basic() {
        assert_eq!(
            re(",").split("a,b,c"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert_eq!(
            re(r"\s+").split("a  b\tc"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn split_keeps_leading_and_trailing_empties() {
        assert_eq!(
            re(",").split(",a,"),
            vec!["".to_string(), "a".to_string(), "".to_string()]
        );
    }

    #[test]
    fn split_without_match_returns_the_input() {
        assert_eq!(re("z").split("abc"), vec!["abc".to_string()]);
        assert_eq!(re("z").split(""), vec!["".to_string()]);
    }

    #[test]
    fn split_on_zero_width_match_makes_progress() {
        assert_eq!(
            re("").split("ab"),
            vec![
                "".to_string(),
                "a".to_string(),
                "b".to_string(),
                "".to_string()
            ]
        );
        assert_eq!(
            re("a*").split("ba"),
            vec!["".to_string(), "b".to_string(), "".to_string()]
        );
    }

    // ---- compile errors ---------------------------------------------------

    #[test]
    fn error_unclosed_group() {
        assert!(Regex::new("(").is_err());
        assert!(Regex::new("(ab").is_err());
        assert!(Regex::new("a(b").is_err());
        assert!(Regex::new("(a|b").is_err());
        let err = Regex::new("(ab").unwrap_err();
        assert!(err.contains("unclosed group"), "unexpected message: {err}");
    }

    #[test]
    fn error_unclosed_bracket_expression() {
        assert!(Regex::new("[").is_err());
        assert!(Regex::new("[abc").is_err());
        assert!(Regex::new("[]").is_err());
        assert!(Regex::new("[^abc").is_err());
        let err = Regex::new("[abc").unwrap_err();
        assert!(
            err.contains("unclosed bracket expression"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn error_unmatched_close_paren() {
        assert!(Regex::new("a)").is_err());
        assert!(Regex::new("(a))").is_err());
    }

    #[test]
    fn error_repetition_range_out_of_order() {
        assert!(Regex::new("a{2,1}").is_err());
        assert!(Regex::new("a{5,2}").is_err());
        let err = Regex::new("a{2,1}").unwrap_err();
        assert!(err.contains("out of order"), "unexpected message: {err}");
    }

    #[test]
    fn error_repetition_bound_too_large() {
        assert!(Regex::new("a{20000}").is_err());
        assert!(Regex::new("a{1,20000}").is_err());
        // Exactly at the limit: 10_000 copies is accepted.
        assert!(re("a{10000}").is_match(&"a".repeat(10_000)));
    }

    #[test]
    fn error_unknown_posix_class() {
        assert!(Regex::new("[[:foo:]]").is_err());
        assert!(Regex::new("[[:alpha]]").is_err());
        let err = Regex::new("[[:foo:]]").unwrap_err();
        assert!(
            err.contains("unknown POSIX character class"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn error_invalid_character_range() {
        assert!(Regex::new("[z-a]").is_err());
        assert!(Regex::new(r"[a-\d]").is_err());
    }

    #[test]
    fn error_trailing_backslash() {
        assert!(Regex::new("a\\").is_err());
        assert!(Regex::new(r"[\").is_err());
    }

    #[test]
    fn error_pattern_too_large() {
        // Nested bounded repetition would expand past the program limit.
        assert!(Regex::new("(a{10000}){10000}").is_err());
    }

    // ---- API surface ------------------------------------------------------

    #[test]
    fn regex_is_clone_and_debug() {
        let regex = re("a+");
        let copy = regex.clone();
        assert!(copy.is_match("aa"));
        assert!(!format!("{regex:?}").is_empty());
    }

    #[test]
    fn new_with_flags_false_is_case_sensitive() {
        assert!(Regex::new_with_flags("a", false).unwrap().is_match("a"));
        assert!(!Regex::new_with_flags("a", false).unwrap().is_match("A"));
    }

    #[test]
    fn unicode_literals_in_classes_and_ranges() {
        assert!(re("^[à-ÿ]+$").is_match("éà"));
        assert!(!re("^[à-ÿ]+$").is_match("A"));
    }
}
