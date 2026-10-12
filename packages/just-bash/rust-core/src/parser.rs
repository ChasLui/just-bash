use crate::fs::Error;

// ─── Public limits ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_command_substitution_depth: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_command_substitution_depth: 50,
        }
    }
}

// ─── Word representation ─────────────────────────────────────────────────────

/// Determines how a [`WordPart`] is expanded during execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordPartKind {
    /// Single-quoted literal — copied verbatim, no `$` expansion.
    Literal,
    /// A single backslash-escaped character — copied verbatim, no `$`
    /// expansion, and never treated as shell syntax (upstream #347).
    Escaped,
    /// Normal text — `$VAR`, `${VAR}`, `$?`, `$1`, etc. are expanded.
    Variable,
    /// Command substitution `$(…)` — the inner script is run and its stdout
    /// (trailing newlines stripped) replaces the word part.
    CommandSub,
    /// Arithmetic expansion `$((…))` — the inner expression is evaluated and
    /// the decimal result replaces the word part.
    Arithmetic,
    /// Process substitution `<(…)` — the inner script runs and the word expands
    /// to a path holding its output.
    ProcSubIn,
    /// Process substitution `>(…)` — the word expands to a path that is fed to
    /// the inner script after the command completes.
    ProcSubOut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordPart {
    pub text: String,
    pub kind: WordPartKind,
    /// True when this part came from a quoted context.
    ///
    /// Pathname expansion only treats unquoted `*`/`?`/`[` as metacharacters,
    /// so `"$x"` never globs while `$x` may.
    pub quoted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<WordPart>,
    /// True when any part of this word came from a quoted or escaped context.
    ///
    /// Reserved words (`if`, `for`, `then`, …) and redirection/pipeline
    /// operators are only recognized for entirely unquoted words, so `\if`,
    /// `'if'` and `"if"` are ordinary command names (upstream #347).
    pub quoted: bool,
}

impl Word {
    /// Create a single-part word that will be variable-expanded.
    pub fn literal(value: impl Into<String>) -> Self {
        Self {
            parts: vec![WordPart {
                text: value.into(),
                kind: WordPartKind::Variable,
                quoted: false,
            }],
            quoted: false,
        }
    }

    /// Return the raw (unexpanded) text of all parts joined.
    pub fn text(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }

    /// Whether this word may still act as a reserved word or shell operator.
    ///
    /// Anything that was quoted or escaped loses keyword status, matching
    /// upstream's "preserve escaped reserved words" behavior (upstream #347).
    pub fn is_bare(&self) -> bool {
        !self.quoted
    }

    /// True when this word is exactly `keyword` and is entirely unquoted.
    pub fn is_reserved(&self, keyword: &str) -> bool {
        self.is_bare() && self.text() == keyword
    }

    /// A word with no parts at all (used for redirects that carry no target).
    pub fn empty() -> Self {
        Self {
            parts: Vec::new(),
            quoted: false,
        }
    }

    /// True when the unquoted expansion of this word may contain glob
    /// metacharacters: only [`WordPartKind::Variable`] and
    /// [`WordPartKind::CommandSub`] parts that were not quoted qualify.
    pub fn may_glob(&self) -> bool {
        self.parts.iter().any(|part| {
            !part.quoted
                && matches!(part.kind, WordPartKind::Variable | WordPartKind::CommandSub)
                && part.text.chars().any(|ch| matches!(ch, '*' | '?' | '['))
        })
    }
}

// ─── Redirections ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub mode: RedirectMode,
    /// Target word. Empty for [`RedirectMode::Dup`], [`RedirectMode::Close`] and
    /// [`RedirectMode::Heredoc`] (the body lives in the mode).
    pub target: Word,
    /// File descriptor being redirected: 0 for input, 1 for output.
    pub fd: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectMode {
    Read,
    Write,
    Append,
    /// `n>&m` — duplicate descriptor `m`.
    Dup(u32),
    /// `n>&-` — close descriptor `n`.
    Close,
    /// `<<<word` — the expanded word (plus a newline) becomes standard input.
    HereString,
    /// `<<DELIM` / `<<-DELIM` — the collected body becomes standard input.
    Heredoc {
        body: String,
        /// `false` when the delimiter was quoted (`<<'EOF'`), which suppresses
        /// parameter and command expansion in the body.
        expand: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

// ─── Pipeline connector ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineConnector {
    Always,
    And,
    Or,
}

// ─── Control-flow AST nodes ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfStatement {
    pub condition: Vec<Statement>,
    pub body: Vec<Statement>,
    pub elif_clauses: Vec<(Vec<Statement>, Vec<Statement>)>,
    pub else_body: Option<Vec<Statement>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileStatement {
    /// `until` instead of `while`: the body runs while the condition FAILS.
    pub until: bool,
    pub condition: Vec<Statement>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForStatement {
    pub var: String,
    pub items: Vec<Word>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseStatement {
    pub word: Word,
    pub clauses: Vec<CaseClause>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseClause {
    /// `pat1|pat2` alternatives; matching any of them selects the clause.
    pub patterns: Vec<Word>,
    pub body: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    pub body: Vec<Statement>,
}

/// `name=(items)` / `name+=(items)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayAssign {
    pub name: String,
    /// `+=` appends to the existing array instead of replacing it.
    pub append: bool,
    /// Element words, in order; `[k]=v` words carry their own index.
    pub items: Vec<Word>,
}

/// `[[ … ]]` conditional expression tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CondExpr {
    And(Box<CondExpr>, Box<CondExpr>),
    Or(Box<CondExpr>, Box<CondExpr>),
    Not(Box<CondExpr>),
    Term(CondTerm),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CondTerm {
    /// `-f file`, `-z str`, …
    Unary { op: String, operand: Word },
    /// `a == b`, `a -eq b`, `a =~ re`, …
    Binary { left: Word, op: String, right: Word },
    /// A bare word: true when non-empty.
    Atom(Word),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementKind {
    Pipeline(Vec<CommandInvocation>),
    If(IfStatement),
    While(WhileStatement),
    For(ForStatement),
    Case(CaseStatement),
    Function(FunctionDef),
    /// `{ list; }` — runs in the current shell state.
    Group(Vec<Statement>),
    /// `( list )` — runs in a child shell; only filesystem changes survive.
    Subshell(Vec<Statement>),
    Cond(CondExpr),
    ArrayAssign(ArrayAssign),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// How this statement connects to the previous one.
    pub connector: PipelineConnector,
    pub kind: StatementKind,
    /// Trailing `&`: the statement runs in a child shell and is recorded as a job.
    pub background: bool,
    /// Leading `!`: invert the statement's exit status.
    pub negated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub statements: Vec<Statement>,
}

/// Unary `[[ ]]` operators this runtime understands.
pub const COND_UNARY_OPS: &[&str] = &[
    "-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-k", "-L", "-n", "-N", "-O", "-p", "-r", "-s",
    "-S", "-t", "-u", "-v", "-w", "-x", "-z", "-G", "-R",
];

/// Binary `[[ ]]` operators this runtime understands.
pub const COND_BINARY_OPS: &[&str] = &[
    "==", "=", "!=", "=~", "<", ">", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef",
];

// ─── Entry points ─────────────────────────────────────────────────────────────

pub fn parse_script(source: &str) -> Result<Script, Error> {
    parse_script_with_limits(source, ParseLimits::default())
}

pub fn parse_script_with_limits(source: &str, limits: ParseLimits) -> Result<Script, Error> {
    let tokens = lex(source, limits)?;
    let mut pos = 0;
    let statements = parse_statements(&tokens, &mut pos, &[])?;
    if pos < tokens.len() {
        return Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            describe_token(&tokens[pos])
        )));
    }
    Ok(Script { statements })
}

// ─── Internal token type ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(Word),
    Pipe,
    AndIf,
    OrIf,
    /// A bare `&` — background the preceding statement.
    Background,
    /// `;;` — terminates a `case` clause.
    DoubleSemi,
    LParen,
    RParen,
    /// A standalone `{`.
    LBrace,
    /// A standalone `}`.
    RBrace,
    /// Input redirection on descriptor `u32`.
    RedirectRead(u32),
    /// Output redirection on descriptor `u32`.
    RedirectWrite(u32),
    /// Appending output redirection on descriptor `u32`.
    RedirectAppend(u32),
    /// `n>&m`: duplicate descriptor `m`; `None` means close (`>&-`).
    RedirectDup(u32, Option<u32>),
    /// `<<<word`.
    RedirectHereString(u32),
    /// `<<DELIM`: descriptor, collected body, and whether the body expands.
    RedirectHeredoc(u32, String, bool),
    /// `name=(` / `name+=(`.
    ArrayAssign(String, bool),
    /// `[[ … ]]`, already split into condition words.
    Condition(Vec<Word>),
    Separator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

fn describe_token(token: &Token) -> String {
    match token {
        Token::Word(word) => word.text(),
        Token::Pipe => "|".to_string(),
        Token::AndIf => "&&".to_string(),
        Token::OrIf => "||".to_string(),
        Token::Background => "&".to_string(),
        Token::DoubleSemi => ";;".to_string(),
        Token::LParen => "(".to_string(),
        Token::RParen => ")".to_string(),
        Token::LBrace => "{".to_string(),
        Token::RBrace => "}".to_string(),
        Token::RedirectRead(_) => "<".to_string(),
        Token::RedirectWrite(_) => ">".to_string(),
        Token::RedirectAppend(_) => ">>".to_string(),
        Token::RedirectDup(_, _) => ">&".to_string(),
        Token::RedirectHereString(_) => "<<<".to_string(),
        Token::RedirectHeredoc(_, _, _) => "<<".to_string(),
        Token::ArrayAssign(name, _) => format!("{name}=("),
        Token::Condition(_) => "[[".to_string(),
        Token::Separator => "newline".to_string(),
    }
}

// ─── Lexer ────────────────────────────────────────────────────────────────────

/// A `<<DELIM` whose body has not been collected yet.
struct PendingHeredoc {
    /// Index in `tokens` of the [`Token::RedirectHeredoc`] to patch.
    token_index: usize,
    delimiter: String,
    strip_tabs: bool,
    expand: bool,
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    tokens: Vec<Token>,
    current: Word,
    started: bool,
    word_quoted: bool,
    quote: Option<Quote>,
    limits: ParseLimits,
    pending_heredocs: Vec<PendingHeredoc>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str, limits: ParseLimits) -> Self {
        Self {
            chars: source.chars().peekable(),
            tokens: Vec::new(),
            current: Word::empty(),
            started: false,
            word_quoted: false,
            quote: None,
            limits,
            pending_heredocs: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Vec<Token>, Error> {
        while let Some(ch) = self.chars.next() {
            match (self.quote, ch) {
                (None, '#') if !self.started => self.skip_comment()?,
                (None, '\'') => {
                    self.started = true;
                    self.word_quoted = true;
                    self.quote = Some(Quote::Single);
                }
                (None, '"') => {
                    self.started = true;
                    self.word_quoted = true;
                    self.quote = Some(Quote::Double);
                }
                (Some(Quote::Single), '\'') | (Some(Quote::Double), '"') => self.quote = None,
                (None, c) if c.is_whitespace() && c != '\n' => self.flush_word(),
                (None, '\n') => {
                    self.flush_word();
                    self.tokens.push(Token::Separator);
                    self.consume_heredoc_bodies()?;
                }
                (None, ';') => {
                    self.flush_word();
                    if self.chars.next_if_eq(&';').is_some() {
                        self.tokens.push(Token::DoubleSemi);
                    } else {
                        self.tokens.push(Token::Separator);
                    }
                }
                (None, '|') => {
                    self.flush_word();
                    if self.chars.next_if_eq(&'|').is_some() {
                        self.tokens.push(Token::OrIf);
                    } else {
                        self.tokens.push(Token::Pipe);
                    }
                }
                (None, '&') => {
                    self.flush_word();
                    if self.chars.next_if_eq(&'&').is_some() {
                        self.tokens.push(Token::AndIf);
                    } else {
                        self.tokens.push(Token::Background);
                    }
                }
                (None, '<') => self.lex_less()?,
                (None, '>') => self.lex_greater()?,
                (None, '(') => self.lex_lparen(),
                (None, ')') => {
                    self.flush_word();
                    self.tokens.push(Token::RParen);
                }
                (None, '{') => self.lex_brace(true),
                (None, '}') => self.lex_brace(false),
                (None, '[') => self.lex_bracket()?,
                (Some(Quote::Single), c) => self.push_char(c, WordPartKind::Literal),
                // Inside double quotes, only `$`, `` ` ``, `"`, `\` and newline
                // are special; any other backslash stays a literal backslash.
                (Some(Quote::Double), '\\') => match self.chars.peek().copied() {
                    Some('\n') => {
                        self.chars.next();
                    }
                    Some(next) if next == '$' || next == '`' || next == '"' || next == '\\' => {
                        self.chars.next();
                        self.push_char(next, WordPartKind::Escaped);
                    }
                    _ => self.push_char('\\', WordPartKind::Literal),
                },
                // Command substitution `$(...)` / arithmetic `$((...))`.
                (quote_state, '$') if quote_state != Some(Quote::Single) => {
                    if self.chars.next_if_eq(&'(').is_some() {
                        let (inner, kind) = if self.chars.next_if_eq(&'(').is_some() {
                            (
                                consume_arithmetic_substitution(&mut self.chars)?,
                                WordPartKind::Arithmetic,
                            )
                        } else {
                            (
                                consume_command_substitution(
                                    &mut self.chars,
                                    self.limits.max_command_substitution_depth,
                                )?,
                                WordPartKind::CommandSub,
                            )
                        };
                        self.push_part(inner, kind, false);
                    } else {
                        self.push_char('$', WordPartKind::Variable);
                    }
                }
                // Backslash escape outside single/double quotes: the escaped
                // character is kept verbatim and the word loses reserved status.
                (_, '\\') => match self.chars.next() {
                    Some('\n') => {}
                    Some(next) => {
                        self.word_quoted = true;
                        self.push_char(next, WordPartKind::Escaped);
                    }
                    None => self.push_char('\\', WordPartKind::Variable),
                },
                (_, c) => {
                    let kind = if self.quote == Some(Quote::Single) {
                        WordPartKind::Literal
                    } else {
                        WordPartKind::Variable
                    };
                    self.push_char(c, kind);
                }
            }
        }

        if self.quote.is_some() {
            return Err(Error::Parse(
                "unexpected EOF while looking for matching quote".to_string(),
            ));
        }
        if !self.pending_heredocs.is_empty() {
            // A trailing `<<EOF` with no newline still consumes the rest of the
            // input as its body.
            self.consume_heredoc_bodies()?;
        }
        self.flush_word();
        Ok(self.tokens)
    }

    fn skip_comment(&mut self) -> Result<(), Error> {
        for next in self.chars.by_ref() {
            if next == '\n' {
                self.tokens.push(Token::Separator);
                self.consume_heredoc_bodies()?;
                break;
            }
        }
        Ok(())
    }

    /// `<` — process substitution, heredoc, here-string, `<&n`, or a plain read.
    fn lex_less(&mut self) -> Result<(), Error> {
        if self.chars.peek() == Some(&'(') {
            self.chars.next();
            let inner = consume_command_substitution(
                &mut self.chars,
                self.limits.max_command_substitution_depth,
            )?;
            self.push_part(inner, WordPartKind::ProcSubIn, false);
            return Ok(());
        }
        let fd = take_fd_prefix(&mut self.current, &mut self.started, 0);
        self.flush_word();
        if self.chars.peek() == Some(&'<') {
            self.chars.next();
            if self.chars.peek() == Some(&'<') {
                self.chars.next();
                self.tokens.push(Token::RedirectHereString(fd));
                return Ok(());
            }
            let strip_tabs = self.chars.next_if_eq(&'-').is_some();
            while matches!(self.chars.peek(), Some(c) if *c == ' ' || *c == '\t') {
                self.chars.next();
            }
            let raw = self.read_heredoc_delimiter();
            if raw.is_empty() {
                return Err(Error::Parse(
                    "syntax error near unexpected token `newline'".to_string(),
                ));
            }
            let (delimiter, expand) = unquote_delimiter(&raw)?;
            let token_index = self.tokens.len();
            self.tokens
                .push(Token::RedirectHeredoc(fd, String::new(), expand));
            self.pending_heredocs.push(PendingHeredoc {
                token_index,
                delimiter,
                strip_tabs,
                expand,
            });
            return Ok(());
        }
        if dup_follows(&self.chars) {
            self.chars.next();
            let token = lex_dup_target(&mut self.chars, fd)?;
            self.tokens.push(token);
        } else {
            self.tokens.push(Token::RedirectRead(fd));
        }
        Ok(())
    }

    /// `>` — process substitution, append, `>&n`, or a plain write.
    fn lex_greater(&mut self) -> Result<(), Error> {
        if self.chars.peek() == Some(&'(') {
            self.chars.next();
            let inner = consume_command_substitution(
                &mut self.chars,
                self.limits.max_command_substitution_depth,
            )?;
            self.push_part(inner, WordPartKind::ProcSubOut, false);
            return Ok(());
        }
        let fd = take_fd_prefix(&mut self.current, &mut self.started, 1);
        self.flush_word();
        if self.chars.next_if_eq(&'>').is_some() {
            self.tokens.push(Token::RedirectAppend(fd));
        } else if dup_follows(&self.chars) {
            self.chars.next();
            let token = lex_dup_target(&mut self.chars, fd)?;
            self.tokens.push(token);
        } else {
            self.tokens.push(Token::RedirectWrite(fd));
        }
        Ok(())
    }

    /// `(` — an array assignment (`name=(`) when it directly follows `name=`.
    fn lex_lparen(&mut self) {
        if let Some((name, append)) = self.take_array_prefix() {
            self.tokens.push(Token::ArrayAssign(name, append));
            return;
        }
        self.flush_word();
        self.tokens.push(Token::LParen);
    }

    fn take_array_prefix(&mut self) -> Option<(String, bool)> {
        if !self.started || self.word_quoted {
            return None;
        }
        let text = self.current.text();
        let (name, append) = if let Some(name) = text.strip_suffix("+=") {
            (name, true)
        } else if let Some(name) = text.strip_suffix('=') {
            (name, false)
        } else {
            return None;
        };
        if !is_valid_name(name) {
            return None;
        }
        self.current = Word::empty();
        self.started = false;
        self.word_quoted = false;
        Some((name.to_string(), append))
    }

    /// `{` / `}` are reserved words only when they stand alone.
    fn lex_brace(&mut self, open: bool) {
        let standalone = !self.started && self.at_operator_boundary();
        if standalone {
            self.flush_word();
            self.tokens
                .push(if open { Token::LBrace } else { Token::RBrace });
        } else {
            let ch = if open { '{' } else { '}' };
            self.push_char(ch, WordPartKind::Variable);
        }
    }

    /// `[[` opens a conditional; a lone `[` is an ordinary word character.
    fn lex_bracket(&mut self) -> Result<(), Error> {
        if !self.started && self.chars.peek() == Some(&'[') {
            self.chars.next();
            if self.at_operator_boundary() || matches!(self.chars.peek(), Some('!') | Some('(')) {
                let raw = self.consume_condition()?;
                let words = lex_condition_words(&raw, self.limits.max_command_substitution_depth)?;
                self.tokens.push(Token::Condition(words));
                return Ok(());
            }
            self.push_char('[', WordPartKind::Variable);
            self.push_char('[', WordPartKind::Variable);
            return Ok(());
        }
        self.push_char('[', WordPartKind::Variable);
        Ok(())
    }

    /// The next character cannot continue the current word (whitespace,
    /// operator, or end of input).
    fn at_operator_boundary(&mut self) -> bool {
        match self.chars.peek() {
            None => true,
            Some(c) => c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')'),
        }
    }

    /// Capture the raw text of `[[ … ]]` up to the matching `]]`.
    fn consume_condition(&mut self) -> Result<String, Error> {
        let mut raw = String::new();
        let mut quote: Option<Quote> = None;
        while let Some(ch) = self.chars.next() {
            match quote {
                Some(Quote::Single) => {
                    raw.push(ch);
                    if ch == '\'' {
                        quote = None;
                    }
                }
                Some(Quote::Double) => {
                    raw.push(ch);
                    if ch == '\\' {
                        if let Some(next) = self.chars.next() {
                            raw.push(next);
                        }
                    } else if ch == '"' {
                        quote = None;
                    }
                }
                None => match ch {
                    '\'' => {
                        raw.push(ch);
                        quote = Some(Quote::Single);
                    }
                    '"' => {
                        raw.push(ch);
                        quote = Some(Quote::Double);
                    }
                    '\\' => {
                        raw.push(ch);
                        if let Some(next) = self.chars.next() {
                            raw.push(next);
                        }
                    }
                    ']' if self.chars.peek() == Some(&']') => {
                        self.chars.next();
                        if self.at_operator_boundary() {
                            return Ok(raw);
                        }
                        raw.push_str("]]");
                    }
                    _ => raw.push(ch),
                },
            }
        }
        Err(Error::Parse(
            "syntax error: unexpected end of file, expected `]]'".to_string(),
        ))
    }

    fn read_heredoc_delimiter(&mut self) -> String {
        let mut raw = String::new();
        while let Some(&ch) = self.chars.peek() {
            if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
                break;
            }
            raw.push(ch);
            self.chars.next();
        }
        raw
    }

    /// Read the body of every outstanding heredoc, in the order they appeared.
    fn consume_heredoc_bodies(&mut self) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending_heredocs);
        for pending in pending {
            let mut body = String::new();
            let mut line = String::new();
            let mut found = false;
            for ch in self.chars.by_ref() {
                if ch == '\n' {
                    if heredoc_line_matches(&line, &pending) {
                        found = true;
                        break;
                    }
                    // `<<-` strips leading tabs from every body line.
                    if pending.strip_tabs {
                        body.push_str(line.trim_start_matches('\t'));
                    } else {
                        body.push_str(&line);
                    }
                    body.push('\n');
                    line.clear();
                } else {
                    line.push(ch);
                }
            }
            if !found && !line.is_empty() {
                // End of input without a trailing newline: bash compares the
                // partial final line and then warns; this runtime simply keeps
                // whatever it read.
                if !heredoc_line_matches(&line, &pending) {
                    body.push_str(&line);
                }
            }
            match self.tokens.get_mut(pending.token_index) {
                Some(Token::RedirectHeredoc(_, slot, _)) => *slot = body,
                _ => unreachable!("heredoc placeholder token"),
            }
            // `expand` is already recorded in the token.
            debug_assert!(self.tokens.get(pending.token_index).is_some_and(
                |t| matches!(t, Token::RedirectHeredoc(_, _, e) if *e == pending.expand)
            ));
        }
        Ok(())
    }

    fn push_char(&mut self, ch: char, kind: WordPartKind) {
        self.push_part(ch.to_string(), kind, self.quote == Some(Quote::Double));
    }

    fn push_part(&mut self, text: String, kind: WordPartKind, quoted: bool) {
        self.started = true;
        if !matches!(
            kind,
            WordPartKind::CommandSub
                | WordPartKind::Escaped
                | WordPartKind::ProcSubIn
                | WordPartKind::ProcSubOut
                | WordPartKind::Arithmetic
        ) && let Some(part) = self
            .current
            .parts
            .last_mut()
            .filter(|p| p.kind == kind && p.quoted == quoted)
        {
            part.text.push_str(&text);
            return;
        }
        self.current.parts.push(WordPart { text, kind, quoted });
    }

    fn flush_word(&mut self) {
        if self.started || !self.current.parts.is_empty() {
            self.current.quoted = self.word_quoted;
            self.tokens.push(Token::Word(std::mem::replace(
                &mut self.current,
                Word::empty(),
            )));
            self.started = false;
            self.word_quoted = false;
        }
    }
}

fn heredoc_line_matches(line: &str, pending: &PendingHeredoc) -> bool {
    if pending.strip_tabs {
        line.trim_start_matches('\t') == pending.delimiter
    } else {
        line == pending.delimiter
    }
}

fn lex(source: &str, limits: ParseLimits) -> Result<Vec<Token>, Error> {
    Lexer::new(source, limits).run()
}

/// Remove quotes and escapes from a heredoc delimiter and report whether any
/// were present (a quoted delimiter suppresses body expansion).
fn unquote_delimiter(raw: &str) -> Result<(String, bool), Error> {
    let mut delimiter = String::new();
    let mut expand = true;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                expand = false;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(next) => delimiter.push(next),
                        None => {
                            return Err(Error::Parse(
                                "unexpected EOF while looking for matching quote".to_string(),
                            ));
                        }
                    }
                }
            }
            '"' => {
                expand = false;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(next) => delimiter.push(next),
                            None => {
                                return Err(Error::Parse(
                                    "unexpected EOF while looking for matching quote".to_string(),
                                ));
                            }
                        },
                        Some(next) => delimiter.push(next),
                        None => {
                            return Err(Error::Parse(
                                "unexpected EOF while looking for matching quote".to_string(),
                            ));
                        }
                    }
                }
            }
            '\\' => {
                expand = false;
                match chars.next() {
                    Some(next) => delimiter.push(next),
                    None => delimiter.push('\\'),
                }
            }
            other => delimiter.push(other),
        }
    }
    if delimiter.is_empty() {
        return Err(Error::Parse(
            "syntax error: empty here-document delimiter".to_string(),
        ));
    }
    Ok((delimiter, expand))
}

// ─── Condition word lexer ─────────────────────────────────────────────────────

/// Split the inside of `[[ … ]]` into words, honoring quotes, escapes and
/// command/arithmetic substitution.
pub fn lex_condition_words(raw: &str, max_depth: usize) -> Result<Vec<Word>, Error> {
    let mut words: Vec<Word> = Vec::new();
    let mut current = Word::empty();
    let mut started = false;
    let mut word_quoted = false;
    let mut quote: Option<Quote> = None;
    let mut chars = raw.chars().peekable();

    macro_rules! flush {
        () => {
            if started || !current.parts.is_empty() {
                current.quoted = word_quoted;
                words.push(std::mem::replace(&mut current, Word::empty()));
                started = false;
                word_quoted = false;
            }
        };
    }

    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '\'') => {
                started = true;
                word_quoted = true;
                quote = Some(Quote::Single);
            }
            (None, '"') => {
                started = true;
                word_quoted = true;
                quote = Some(Quote::Double);
            }
            (Some(Quote::Single), '\'') | (Some(Quote::Double), '"') => quote = None,
            (None, c) if c.is_whitespace() => flush!(),
            (Some(Quote::Single), c) => {
                started = true;
                push_fragment_char(&mut current, c, WordPartKind::Literal, true);
            }
            (Some(Quote::Double), '\\') => match chars.peek().copied() {
                Some(next) if matches!(next, '$' | '`' | '"' | '\\') => {
                    chars.next();
                    push_fragment_char(&mut current, next, WordPartKind::Escaped, false);
                }
                _ => push_fragment_char(&mut current, '\\', WordPartKind::Literal, true),
            },
            (quote_state, '$') if quote_state != Some(Quote::Single) => {
                if chars.next_if_eq(&'(').is_some() {
                    let (inner, kind) = if chars.next_if_eq(&'(').is_some() {
                        (
                            consume_arithmetic_substitution(&mut chars)?,
                            WordPartKind::Arithmetic,
                        )
                    } else {
                        (
                            consume_command_substitution(&mut chars, max_depth)?,
                            WordPartKind::CommandSub,
                        )
                    };
                    started = true;
                    current.parts.push(WordPart {
                        text: inner,
                        kind,
                        quoted: quote.is_some(),
                    });
                } else {
                    started = true;
                    push_fragment_char(&mut current, '$', WordPartKind::Variable, false);
                }
            }
            (_, '\\') => match chars.next() {
                Some('\n') => {}
                Some(next) => {
                    started = true;
                    word_quoted = true;
                    push_fragment_char(&mut current, next, WordPartKind::Escaped, false);
                }
                None => {
                    started = true;
                    push_fragment_char(&mut current, '\\', WordPartKind::Variable, false);
                }
            },
            (_, c) => {
                let kind = if quote == Some(Quote::Single) {
                    WordPartKind::Literal
                } else {
                    WordPartKind::Variable
                };
                started = true;
                push_fragment_char(&mut current, c, kind, quote == Some(Quote::Double));
            }
        }
    }
    if quote.is_some() {
        return Err(Error::Parse(
            "unexpected EOF while looking for matching quote".to_string(),
        ));
    }
    flush!();
    // The trailing flush is the last consumer of these flags; naming them here
    // keeps the macro's resets from looking like dead assignments.
    let _ = (started, word_quoted);
    Ok(words)
}

fn push_fragment_char(word: &mut Word, ch: char, kind: WordPartKind, quoted: bool) {
    if !matches!(
        kind,
        WordPartKind::CommandSub
            | WordPartKind::Escaped
            | WordPartKind::Arithmetic
            | WordPartKind::ProcSubIn
            | WordPartKind::ProcSubOut
    ) && let Some(part) = word
        .parts
        .last_mut()
        .filter(|p| p.kind == kind && p.quoted == quoted)
    {
        part.text.push(ch);
        return;
    }
    word.parts.push(WordPart {
        text: ch.to_string(),
        kind,
        quoted,
    });
}

/// Build a single [`Word`] from a heredoc body.
///
/// A heredoc body is expanded like a double-quoted string except that quotes
/// are literal and a backslash only escapes `$`, `` ` ``, `\` and newline.
pub fn parse_heredoc_word(body: &str, max_depth: usize) -> Result<Word, Error> {
    let mut word = Word::empty();
    let mut started = false;
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.peek().copied() {
                Some('\n') => {
                    chars.next();
                }
                Some(next) if matches!(next, '$' | '`' | '\\') => {
                    chars.next();
                    started = true;
                    push_fragment_char(&mut word, next, WordPartKind::Escaped, false);
                }
                _ => {
                    started = true;
                    push_fragment_char(&mut word, '\\', WordPartKind::Variable, false);
                }
            },
            '$' => {
                if chars.next_if_eq(&'(').is_some() {
                    let (inner, kind) = if chars.next_if_eq(&'(').is_some() {
                        (
                            consume_arithmetic_substitution(&mut chars)?,
                            WordPartKind::Arithmetic,
                        )
                    } else {
                        (
                            consume_command_substitution(&mut chars, max_depth)?,
                            WordPartKind::CommandSub,
                        )
                    };
                    started = true;
                    word.parts.push(WordPart {
                        text: inner,
                        kind,
                        quoted: false,
                    });
                } else {
                    started = true;
                    push_fragment_char(&mut word, '$', WordPartKind::Variable, false);
                }
            }
            other => {
                started = true;
                push_fragment_char(&mut word, other, WordPartKind::Variable, false);
            }
        }
    }
    let _ = started;
    Ok(word)
}

// ─── Condition expression parser ──────────────────────────────────────────────

/// Parse the inside of `[[ … ]]` into an expression tree.
pub fn parse_condition(words: &[Word]) -> Result<CondExpr, Error> {
    let mut tokens: Vec<CondToken> = Vec::new();
    for word in words {
        if word.is_bare() && word.parts.len() == 1 && word.parts[0].kind == WordPartKind::Variable {
            let text = word.text();
            if !text.is_empty() && text.chars().all(|ch| ch == '(' || ch == ')') {
                for ch in text.chars() {
                    tokens.push(CondToken::Operator(ch.to_string()));
                }
                continue;
            }
            if COND_UNARY_OPS.contains(&text.as_str())
                || COND_BINARY_OPS.contains(&text.as_str())
                || matches!(text.as_str(), "&&" | "||" | "!" | "(" | ")")
            {
                tokens.push(CondToken::Operator(text));
                continue;
            }
        }
        tokens.push(CondToken::Operand(word.clone()));
    }
    let mut parser = ConditionParser { tokens, pos: 0 };
    let expr = parser.parse_or()?;
    if parser.pos < parser.tokens.len() {
        return Err(Error::Parse(
            "syntax error in conditional expression".to_string(),
        ));
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CondToken {
    Operator(String),
    Operand(Word),
}

struct ConditionParser {
    tokens: Vec<CondToken>,
    pos: usize,
}

impl ConditionParser {
    fn peek_operator(&self) -> Option<&str> {
        match self.tokens.get(self.pos) {
            Some(CondToken::Operator(op)) => Some(op.as_str()),
            _ => None,
        }
    }

    fn parse_or(&mut self) -> Result<CondExpr, Error> {
        let mut left = self.parse_and()?;
        while self.peek_operator() == Some("||") {
            self.pos += 1;
            let right = self.parse_and()?;
            left = CondExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<CondExpr, Error> {
        let mut left = self.parse_not()?;
        while self.peek_operator() == Some("&&") {
            self.pos += 1;
            let right = self.parse_not()?;
            left = CondExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<CondExpr, Error> {
        if self.peek_operator() == Some("!") {
            self.pos += 1;
            return Ok(CondExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<CondExpr, Error> {
        if self.peek_operator() == Some("(") {
            self.pos += 1;
            let inner = self.parse_or()?;
            if self.peek_operator() != Some(")") {
                return Err(Error::Parse(
                    "syntax error in conditional expression".to_string(),
                ));
            }
            self.pos += 1;
            return Ok(inner);
        }
        self.parse_term()
    }

    fn parse_term(&mut self) -> Result<CondExpr, Error> {
        match self.tokens.get(self.pos).cloned() {
            Some(CondToken::Operator(op)) if COND_UNARY_OPS.contains(&op.as_str()) => {
                self.pos += 1;
                match self.tokens.get(self.pos).cloned() {
                    Some(CondToken::Operand(operand)) => {
                        self.pos += 1;
                        Ok(CondExpr::Term(CondTerm::Unary { op, operand }))
                    }
                    _ => Err(Error::Parse(
                        "syntax error in conditional expression".to_string(),
                    )),
                }
            }
            Some(CondToken::Operand(left)) => {
                if let Some(CondToken::Operator(op)) = self.tokens.get(self.pos + 1).cloned()
                    && COND_BINARY_OPS.contains(&op.as_str())
                {
                    return match self.tokens.get(self.pos + 2).cloned() {
                        Some(CondToken::Operand(right)) => {
                            self.pos += 3;
                            Ok(CondExpr::Term(CondTerm::Binary { left, op, right }))
                        }
                        _ => Err(Error::Parse(
                            "syntax error in conditional expression".to_string(),
                        )),
                    };
                }
                self.pos += 1;
                Ok(CondExpr::Term(CondTerm::Atom(left)))
            }
            _ => Err(Error::Parse(
                "syntax error in conditional expression".to_string(),
            )),
        }
    }
}

// ─── Recursive-descent parser ─────────────────────────────────────────────────

/// Parse a flat list of statements, stopping when a terminator keyword, `)`,
/// `}` or `;;` is seen (without consuming it) or the token stream ends.
fn parse_statements(
    tokens: &[Token],
    pos: &mut usize,
    terminators: &[&str],
) -> Result<Vec<Statement>, Error> {
    let mut statements: Vec<Statement> = Vec::new();
    let mut connector = PipelineConnector::Always;

    loop {
        skip_separators(tokens, pos);
        if *pos >= tokens.len() {
            break;
        }
        if is_stop_token(tokens, *pos, terminators) {
            break;
        }

        let Some((kind, negated)) = parse_single_statement(tokens, pos, terminators)? else {
            // If we couldn't parse a statement but have a pending connector, error
            if !matches!(connector, PipelineConnector::Always) {
                return Err(Error::Parse(format!(
                    "syntax error near unexpected token `{}'",
                    describe_connector(connector)
                )));
            }
            break;
        };
        statements.push(Statement {
            connector,
            kind,
            background: false,
            negated,
        });

        // The token immediately after a statement determines the NEXT connector.
        // parse_single_statement stops without consuming &&/||/;/&.
        match tokens.get(*pos) {
            Some(Token::AndIf) => {
                *pos += 1;
                connector = PipelineConnector::And;
            }
            Some(Token::OrIf) => {
                *pos += 1;
                connector = PipelineConnector::Or;
            }
            Some(Token::Background) => {
                *pos += 1;
                if let Some(last) = statements.last_mut() {
                    last.background = true;
                }
                connector = PipelineConnector::Always;
            }
            _ => {
                connector = PipelineConnector::Always;
            }
        }
    }

    // Check if we have a pending connector but reached a stopper
    if !matches!(connector, PipelineConnector::Always) && *pos >= tokens.len() {
        return Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            describe_connector(connector)
        )));
    }

    Ok(statements)
}

fn describe_connector(connector: PipelineConnector) -> &'static str {
    match connector {
        PipelineConnector::And => "&&",
        PipelineConnector::Or => "||",
        PipelineConnector::Always => "newline",
    }
}

fn is_stop_token(tokens: &[Token], pos: usize, terminators: &[&str]) -> bool {
    match tokens.get(pos) {
        Some(Token::RParen) | Some(Token::RBrace) | Some(Token::DoubleSemi) => true,
        Some(Token::Word(word)) => terminators.iter().any(|keyword| word.is_reserved(keyword)),
        _ => false,
    }
}

/// Parse one statement: a control-flow keyword compound or a pipeline.
/// Stops (without consuming) at &&, ||, ;, \n, &, or a terminator.
fn parse_single_statement(
    tokens: &[Token],
    pos: &mut usize,
    terminators: &[&str],
) -> Result<Option<(StatementKind, bool)>, Error> {
    skip_separators(tokens, pos);
    if *pos >= tokens.len() {
        return Ok(None);
    }

    // `!` negates the statement that follows (a pipeline or a compound).
    if let Token::Word(word) = &tokens[*pos]
        && word.is_reserved("!")
    {
        *pos += 1;
        return match parse_single_statement(tokens, pos, terminators)? {
            Some((kind, negated)) => Ok(Some((kind, !negated))),
            None => Err(Error::Parse(
                "syntax error near unexpected token `!'".to_string(),
            )),
        };
    }

    if let Token::Word(word) = &tokens[*pos]
        && word.is_bare()
    {
        match word.text().as_str() {
            t if terminators.contains(&t) => return Ok(None),
            "if" => {
                *pos += 1;
                return Ok(Some((StatementKind::If(parse_if(tokens, pos)?), false)));
            }
            "while" => {
                *pos += 1;
                return Ok(Some((
                    StatementKind::While(parse_while(tokens, pos, false)?),
                    false,
                )));
            }
            "until" => {
                *pos += 1;
                return Ok(Some((
                    StatementKind::While(parse_while(tokens, pos, true)?),
                    false,
                )));
            }
            "for" => {
                *pos += 1;
                return Ok(Some((StatementKind::For(parse_for(tokens, pos)?), false)));
            }
            "case" => {
                *pos += 1;
                return Ok(Some((StatementKind::Case(parse_case(tokens, pos)?), false)));
            }
            "function" => {
                *pos += 1;
                return Ok(Some((parse_function_keyword(tokens, pos)?, false)));
            }
            other => {
                // `name() { … }` — a function definition.
                if is_valid_name(other)
                    && matches!(tokens.get(*pos + 1), Some(Token::LParen))
                    && matches!(tokens.get(*pos + 2), Some(Token::RParen))
                {
                    let name = other.to_string();
                    *pos += 3;
                    return Ok(Some((
                        StatementKind::Function(parse_function_body(tokens, pos, name)?),
                        false,
                    )));
                }
                if let Some(message) = unsupported_construct(other) {
                    return Err(Error::Parse(message));
                }
            }
        }
    }

    match &tokens[*pos] {
        Token::LParen => {
            *pos += 1;
            let body = parse_statements(tokens, pos, &[])?;
            expect_token(tokens, pos, ")", |token| matches!(token, Token::RParen))?;
            return Ok(Some((StatementKind::Subshell(body), false)));
        }
        Token::LBrace => {
            *pos += 1;
            let body = parse_statements(tokens, pos, &[])?;
            expect_token(tokens, pos, "}", |token| matches!(token, Token::RBrace))?;
            return Ok(Some((StatementKind::Group(body), false)));
        }
        Token::Condition(words) => {
            let expr = parse_condition(words)?;
            *pos += 1;
            return Ok(Some((StatementKind::Cond(expr), false)));
        }
        Token::ArrayAssign(name, append) => {
            let name = name.clone();
            let append = *append;
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_separators(tokens, pos);
                match tokens.get(*pos) {
                    Some(Token::RParen) => {
                        *pos += 1;
                        break;
                    }
                    Some(Token::Word(word)) => {
                        items.push(word.clone());
                        *pos += 1;
                    }
                    _ => {
                        return Err(Error::Parse(
                            "syntax error near unexpected token `('".to_string(),
                        ));
                    }
                }
            }
            return Ok(Some((
                StatementKind::ArrayAssign(ArrayAssign {
                    name,
                    append,
                    items,
                }),
                false,
            )));
        }
        _ => {}
    }

    Ok(parse_pipeline_statement(tokens, pos, terminators)?.map(|kind| (kind, false)))
}

/// Parse a pipeline: one or more commands joined by `|`.
fn parse_pipeline_statement(
    tokens: &[Token],
    pos: &mut usize,
    terminators: &[&str],
) -> Result<Option<StatementKind>, Error> {
    let mut commands: Vec<CommandInvocation> = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut pending_pipe = false;

    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Word(word) => {
                if is_keyword_at(tokens, *pos, terminators) {
                    break;
                }
                // A command-position reserved word this runtime cannot execute
                // fails closed instead of running as an external command
                // (upstream #348). Only the first word of a command is checked,
                // and quoted/escaped words are ordinary command names.
                if words.is_empty()
                    && word.is_bare()
                    && let Some(message) = unsupported_construct(&word.text())
                {
                    return Err(Error::Parse(message));
                }
                words.push(word.clone());
                pending_pipe = false;
                *pos += 1;
            }
            Token::RedirectRead(_)
            | Token::RedirectWrite(_)
            | Token::RedirectAppend(_)
            | Token::RedirectDup(_, _) => {
                let (mode, fd) = match tokens[*pos] {
                    Token::RedirectRead(fd) => (RedirectMode::Read, fd),
                    Token::RedirectWrite(fd) => (RedirectMode::Write, fd),
                    Token::RedirectAppend(fd) => (RedirectMode::Append, fd),
                    Token::RedirectDup(fd, target) => match target {
                        Some(target) => (RedirectMode::Dup(target), fd),
                        None => (RedirectMode::Close, fd),
                    },
                    _ => unreachable!(),
                };
                *pos += 1;
                // Dup/Close carry their target in the token itself.
                if matches!(mode, RedirectMode::Dup(_) | RedirectMode::Close) {
                    redirects.push(Redirect {
                        mode,
                        target: Word::empty(),
                        fd,
                    });
                    pending_pipe = false;
                    continue;
                }
                let Some(Token::Word(target)) = tokens.get(*pos) else {
                    return Err(Error::Parse(
                        "syntax error near unexpected token `newline'".to_string(),
                    ));
                };
                redirects.push(Redirect {
                    mode,
                    target: target.clone(),
                    fd,
                });
                pending_pipe = false;
                *pos += 1;
            }
            Token::RedirectHereString(fd) => {
                let fd = *fd;
                *pos += 1;
                let Some(Token::Word(target)) = tokens.get(*pos) else {
                    return Err(Error::Parse(
                        "syntax error near unexpected token `newline'".to_string(),
                    ));
                };
                redirects.push(Redirect {
                    mode: RedirectMode::HereString,
                    target: target.clone(),
                    fd,
                });
                pending_pipe = false;
                *pos += 1;
            }
            Token::RedirectHeredoc(fd, body, expand) => {
                redirects.push(Redirect {
                    mode: RedirectMode::Heredoc {
                        body: body.clone(),
                        expand: *expand,
                    },
                    target: Word::empty(),
                    fd: *fd,
                });
                pending_pipe = false;
                *pos += 1;
            }
            // `{`, `}` and `[[ … ]]` are reserved words only in command
            // position; as arguments they are ordinary words.
            Token::LBrace | Token::RBrace => {
                if words.is_empty() {
                    break;
                }
                let text = if matches!(tokens[*pos], Token::LBrace) {
                    "{"
                } else {
                    "}"
                };
                words.push(Word::literal(text));
                pending_pipe = false;
                *pos += 1;
            }
            Token::Condition(condition_words) => {
                if words.is_empty() {
                    break;
                }
                words.push(Word::literal("[["));
                words.extend(condition_words.iter().cloned());
                words.push(Word::literal("]]"));
                pending_pipe = false;
                *pos += 1;
            }
            Token::Pipe => {
                if words.is_empty() && redirects.is_empty() && commands.is_empty() {
                    // Skip leading pipe; continue parsing the next command
                    *pos += 1;
                    continue;
                }
                if words.is_empty() && redirects.is_empty() {
                    return Err(Error::Parse(
                        "syntax error near unexpected token `|'".to_string(),
                    ));
                }
                push_command_to(&mut commands, &mut words, &mut redirects);
                pending_pipe = true;
                *pos += 1;
            }
            Token::AndIf
            | Token::OrIf
            | Token::Separator
            | Token::Background
            | Token::DoubleSemi
            | Token::RParen
            | Token::LParen
            | Token::ArrayAssign(_, _) => {
                if pending_pipe {
                    return Err(Error::Parse(format!(
                        "syntax error near unexpected token `{}'",
                        describe_token(&tokens[*pos])
                    )));
                }
                if matches!(tokens[*pos], Token::LParen) && !words.is_empty() {
                    // `echo (` is a syntax error in bash, not a subshell.
                    return Err(Error::Parse(
                        "syntax error near unexpected token `('".to_string(),
                    ));
                }
                break;
            }
        }
    }

    if pending_pipe {
        return Err(Error::Parse(
            "syntax error near unexpected token `newline'".to_string(),
        ));
    }

    push_command_to(&mut commands, &mut words, &mut redirects);

    if commands.is_empty() {
        Ok(None)
    } else {
        Ok(Some(StatementKind::Pipeline(commands)))
    }
}

fn parse_if(tokens: &[Token], pos: &mut usize) -> Result<IfStatement, Error> {
    let condition = parse_statements(tokens, pos, &["then"])?;
    expect_keyword(tokens, pos, "then")?;
    let body = parse_statements(tokens, pos, &["elif", "else", "fi"])?;

    let mut elif_clauses = Vec::new();
    loop {
        skip_separators(tokens, pos);
        if !is_keyword_at(tokens, *pos, &["elif"]) {
            break;
        }
        *pos += 1; // consume "elif"
        let elif_cond = parse_statements(tokens, pos, &["then"])?;
        expect_keyword(tokens, pos, "then")?;
        let elif_body = parse_statements(tokens, pos, &["elif", "else", "fi"])?;
        elif_clauses.push((elif_cond, elif_body));
    }

    let else_body = {
        skip_separators(tokens, pos);
        if is_keyword_at(tokens, *pos, &["else"]) {
            *pos += 1; // consume "else"
            Some(parse_statements(tokens, pos, &["fi"])?)
        } else {
            None
        }
    };

    expect_keyword(tokens, pos, "fi")?;
    Ok(IfStatement {
        condition,
        body,
        elif_clauses,
        else_body,
    })
}

fn parse_while(tokens: &[Token], pos: &mut usize, until: bool) -> Result<WhileStatement, Error> {
    let condition = parse_statements(tokens, pos, &["do"])?;
    expect_keyword(tokens, pos, "do")?;
    let body = parse_statements(tokens, pos, &["done"])?;
    expect_keyword(tokens, pos, "done")?;
    Ok(WhileStatement {
        until,
        condition,
        body,
    })
}

fn parse_for(tokens: &[Token], pos: &mut usize) -> Result<ForStatement, Error> {
    skip_separators(tokens, pos);

    let var = match tokens.get(*pos) {
        Some(Token::Word(word)) => {
            let text = word.text();
            *pos += 1;
            text
        }
        _ => {
            return Err(Error::Parse(
                "syntax error: expected variable name after 'for'".to_string(),
            ));
        }
    };

    skip_separators(tokens, pos);

    let mut items = Vec::new();
    // `for x; do …` iterates the positional parameters, so `in` is optional.
    if is_keyword_at(tokens, *pos, &["in"]) {
        *pos += 1;
        while *pos < tokens.len() {
            match &tokens[*pos] {
                Token::Word(word) if word.is_reserved("do") => break,
                Token::Word(word) => {
                    items.push(word.clone());
                    *pos += 1;
                }
                Token::Separator => {
                    *pos += 1;
                    break;
                }
                _ => break,
            }
        }
    }

    // expect "do" (possibly after separator)
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(Token::Word(word)) if word.is_reserved("do") => *pos += 1,
        _ => {
            return Err(Error::Parse(
                "syntax error: expected 'do' in for loop".to_string(),
            ));
        }
    }

    let body = parse_statements(tokens, pos, &["done"])?;
    expect_keyword(tokens, pos, "done")?;
    Ok(ForStatement { var, items, body })
}

/// `case WORD in [ ( ] PAT|PAT ) list ;; … esac`
fn parse_case(tokens: &[Token], pos: &mut usize) -> Result<CaseStatement, Error> {
    skip_separators(tokens, pos);
    let word = match tokens.get(*pos) {
        Some(Token::Word(word)) => {
            let word = word.clone();
            *pos += 1;
            word
        }
        _ => {
            return Err(Error::Parse(
                "syntax error: expected word after 'case'".to_string(),
            ));
        }
    };

    skip_separators(tokens, pos);
    if !is_keyword_at(tokens, *pos, &["in"]) {
        return Err(Error::Parse(
            "syntax error: expected 'in' after case word".to_string(),
        ));
    }
    *pos += 1;

    let mut clauses = Vec::new();
    loop {
        skip_separators(tokens, pos);
        if is_keyword_at(tokens, *pos, &["esac"]) {
            *pos += 1;
            break;
        }
        if *pos >= tokens.len() {
            return Err(Error::Parse(
                "syntax error: unexpected end of file, expected 'esac'".to_string(),
            ));
        }
        // An optional `(` may open the pattern list.
        if matches!(tokens.get(*pos), Some(Token::LParen)) {
            *pos += 1;
        }
        let mut patterns = Vec::new();
        loop {
            match tokens.get(*pos) {
                Some(Token::Word(pattern)) => {
                    patterns.push(pattern.clone());
                    *pos += 1;
                }
                _ => {
                    return Err(Error::Parse("syntax error in case pattern".to_string()));
                }
            }
            match tokens.get(*pos) {
                Some(Token::Pipe) => {
                    *pos += 1;
                }
                Some(Token::RParen) => {
                    *pos += 1;
                    break;
                }
                _ => {
                    return Err(Error::Parse(
                        "syntax error: expected ')' in case pattern".to_string(),
                    ));
                }
            }
        }
        let body = parse_statements(tokens, pos, &["esac"])?;
        if matches!(tokens.get(*pos), Some(Token::DoubleSemi)) {
            *pos += 1;
        }
        clauses.push(CaseClause { patterns, body });
    }

    Ok(CaseStatement { word, clauses })
}

/// `function NAME [()] compound`
fn parse_function_keyword(tokens: &[Token], pos: &mut usize) -> Result<StatementKind, Error> {
    skip_separators(tokens, pos);
    let name = match tokens.get(*pos) {
        Some(Token::Word(word)) => {
            let name = word.text();
            *pos += 1;
            name
        }
        _ => {
            return Err(Error::Parse(
                "syntax error: expected function name".to_string(),
            ));
        }
    };
    if !is_valid_name(&name) {
        return Err(Error::Parse(format!(
            "syntax error near unexpected token `{name}'"
        )));
    }
    if matches!(tokens.get(*pos), Some(Token::LParen)) {
        *pos += 1;
        expect_token(tokens, pos, ")", |token| matches!(token, Token::RParen))?;
    }
    Ok(StatementKind::Function(parse_function_body(
        tokens, pos, name,
    )?))
}

/// The body of a function: a brace group or any single compound command.
fn parse_function_body(
    tokens: &[Token],
    pos: &mut usize,
    name: String,
) -> Result<FunctionDef, Error> {
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(Token::LBrace) => {
            *pos += 1;
            let body = parse_statements(tokens, pos, &[])?;
            expect_token(tokens, pos, "}", |token| matches!(token, Token::RBrace))?;
            Ok(FunctionDef { name, body })
        }
        Some(Token::LParen) => {
            *pos += 1;
            let body = parse_statements(tokens, pos, &[])?;
            expect_token(tokens, pos, ")", |token| matches!(token, Token::RParen))?;
            Ok(FunctionDef {
                name,
                body: vec![Statement {
                    connector: PipelineConnector::Always,
                    kind: StatementKind::Subshell(body),
                    background: false,
                    negated: false,
                }],
            })
        }
        _ => match parse_single_statement(tokens, pos, &[])? {
            Some((kind, negated)) => Ok(FunctionDef {
                name,
                body: vec![Statement {
                    connector: PipelineConnector::Always,
                    kind,
                    background: false,
                    negated,
                }],
            }),
            None => Err(Error::Parse(format!(
                "syntax error near unexpected token `{}'",
                tokens
                    .get(*pos)
                    .map(describe_token)
                    .unwrap_or_else(|| "newline".to_string())
            ))),
        },
    }
}

fn expect_token(
    tokens: &[Token],
    pos: &mut usize,
    expected: &str,
    predicate: impl Fn(&Token) -> bool,
) -> Result<(), Error> {
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(token) if predicate(token) => {
            *pos += 1;
            Ok(())
        }
        Some(token) => Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            describe_token(token)
        ))),
        None => Err(Error::Parse(format!(
            "syntax error: unexpected end of file, expected '{expected}'"
        ))),
    }
}

fn expect_keyword(tokens: &[Token], pos: &mut usize, keyword: &str) -> Result<(), Error> {
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(Token::Word(word)) if word.is_reserved(keyword) => {
            *pos += 1;
            Ok(())
        }
        Some(Token::Word(word)) => Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            word.text()
        ))),
        Some(token) => Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            describe_token(token)
        ))),
        None => Err(Error::Parse(format!(
            "syntax error: unexpected end of file, expected '{keyword}'"
        ))),
    }
}

fn skip_separators(tokens: &[Token], pos: &mut usize) {
    while matches!(tokens.get(*pos), Some(Token::Separator)) {
        *pos += 1;
    }
}

/// Constructs bash accepts but this runtime does not implement.
///
/// Recognizing them and failing closed is deliberately safer than letting them
/// fall through to "command not found" and silently continuing with the rest of
/// the script (upstream #348).
fn unsupported_construct(word: &str) -> Option<String> {
    const UNSUPPORTED: &[&str] = &[
        "select", "coproc", "time", "in", "then", "else", "elif", "fi", "do", "done", "esac", "]]",
    ];
    if UNSUPPORTED.contains(&word) {
        Some(format!("syntax error: unsupported construct `{word}'"))
    } else {
        None
    }
}

fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `n>` only names a descriptor when the preceding token is entirely unquoted
/// digits directly adjacent to the operator; `a2>f` is the word `a2` followed by
/// a plain stdout redirect.
fn take_fd_prefix(current: &mut Word, current_started: &mut bool, default_fd: u32) -> u32 {
    if !*current_started || current.quoted {
        return default_fd;
    }
    let text = current.text();
    if !text.is_empty()
        && text.chars().all(|ch| ch.is_ascii_digit())
        && let Ok(fd) = text.parse::<u32>()
    {
        current.parts.clear();
        *current_started = false;
        return fd;
    }
    default_fd
}

/// Whether the upcoming characters are `>&N` / `>&-` rather than a plain `>`.
fn dup_follows(chars: &std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut lookahead = chars.clone();
    if lookahead.next() != Some('&') {
        return false;
    }
    matches!(lookahead.next(), Some('-') | Some('0'..='9'))
}

/// Read the descriptor named after `>&` / `<&`.
fn lex_dup_target(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    fd: u32,
) -> Result<Token, Error> {
    let mut digits = String::new();
    while let Some(next) = chars.peek().copied() {
        if next.is_ascii_digit() {
            digits.push(next);
            chars.next();
        } else {
            break;
        }
    }
    // `n>&m-` means "duplicate m, then close m".
    if chars.peek() == Some(&'-') {
        chars.next();
    }
    if digits.is_empty() {
        // `n>&-`: close descriptor n.
        return Ok(Token::RedirectDup(fd, None));
    }
    match digits.parse::<u32>() {
        Ok(target) => Ok(Token::RedirectDup(fd, Some(target))),
        Err(_) => Err(Error::Parse(format!(
            "syntax error near unexpected token `{digits}'"
        ))),
    }
}

fn is_keyword_at(tokens: &[Token], pos: usize, keywords: &[&str]) -> bool {
    match tokens.get(pos) {
        Some(Token::Word(word)) => keywords.iter().any(|keyword| word.is_reserved(keyword)),
        _ => false,
    }
}

fn push_command_to(
    commands: &mut Vec<CommandInvocation>,
    words: &mut Vec<Word>,
    redirects: &mut Vec<Redirect>,
) {
    if !words.is_empty() || !redirects.is_empty() {
        commands.push(CommandInvocation {
            words: std::mem::take(words),
            redirects: std::mem::take(redirects),
        });
    }
}

/// Consume a command substitution body (after the opening `$(` has been
/// consumed). Returns the inner script text without the enclosing `$(` / `)`.
fn consume_command_substitution(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    max_depth: usize,
) -> Result<String, Error> {
    let mut output = String::new();
    let mut depth = 1usize;
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(Quote::Single) if ch == '\'' => {
                output.push(ch);
                quote = None;
            }

            Some(Quote::Double) if ch == '"' => {
                output.push(ch);
                quote = None;
            }
            Some(Quote::Single) => output.push(ch),
            Some(Quote::Double) => {
                output.push(ch);
                if ch == '\\'
                    && let Some(next) = chars.next()
                {
                    output.push(next);
                }
            }
            None => match ch {
                '\'' => {
                    output.push(ch);
                    quote = Some(Quote::Single);
                }
                '"' => {
                    output.push(ch);
                    quote = Some(Quote::Double);
                }
                '\\' => {
                    output.push(ch);
                    if let Some(next) = chars.next() {
                        output.push(next);
                    }
                }
                '$' => {
                    output.push(ch);
                    if chars.next_if_eq(&'(').is_some() {
                        output.push('(');
                        depth += 1;
                        if depth > max_depth {
                            return Err(Error::Parse(format!(
                                "command substitution nesting exceeds limit ({max_depth})"
                            )));
                        }
                    }
                }
                // Any other unquoted `(` opens a level too: a subshell or a
                // process substitution inside `$( … )` must not let its `)`
                // terminate the substitution early.
                '(' => {
                    output.push(ch);
                    depth += 1;
                    if depth > max_depth {
                        return Err(Error::Parse(format!(
                            "command substitution nesting exceeds limit ({max_depth})"
                        )));
                    }
                }
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        // Return inner content without the closing ')'
                        return Ok(output);
                    }
                    output.push(ch);
                }
                _ => output.push(ch),
            },
        }
    }

    Err(Error::Parse(
        "unexpected EOF while looking for matching `)`".to_string(),
    ))
}

/// Consume an arithmetic expansion body after `$((` and stop at the matching `))`.
fn consume_arithmetic_substitution(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> Result<String, Error> {
    let mut output = String::new();
    let mut depth = 0usize;
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(Quote::Single) if ch == '\'' => {
                output.push(ch);
                quote = None;
            }
            Some(Quote::Double) if ch == '"' => {
                output.push(ch);
                quote = None;
            }
            Some(Quote::Single) | Some(Quote::Double) => output.push(ch),
            None => match ch {
                '\'' => {
                    output.push(ch);
                    quote = Some(Quote::Single);
                }
                '"' => {
                    output.push(ch);
                    quote = Some(Quote::Double);
                }
                '\\' => {
                    output.push(ch);
                    if let Some(next) = chars.next() {
                        output.push(next);
                    }
                }
                '(' => {
                    depth += 1;
                    output.push(ch);
                }
                ')' => {
                    if depth > 0 {
                        depth -= 1;
                        output.push(ch);
                    } else if chars.next_if_eq(&')').is_some() {
                        return Ok(output);
                    } else {
                        output.push(ch);
                    }
                }
                _ => output.push(ch),
            },
        }
    }

    Err(Error::Parse(
        "unexpected EOF while looking for matching '))'".to_string(),
    ))
}
// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline_cmds(script: &Script, idx: usize) -> &[CommandInvocation] {
        match &script.statements[idx].kind {
            StatementKind::Pipeline(cmds) => cmds,
            other => panic!("expected Pipeline at {idx}, got {other:?}"),
        }
    }

    #[test]
    fn parses_pipelines_and_redirects() {
        let script = parse_script("cat < in.txt | grep hi > out.txt; echo done >> log").unwrap();
        assert_eq!(script.statements.len(), 2);
        assert_eq!(script.statements[0].connector, PipelineConnector::Always);
        let cmds0 = pipeline_cmds(&script, 0);
        assert_eq!(cmds0.len(), 2);
        assert_eq!(cmds0[0].words[0].text(), "cat");
        assert_eq!(cmds0[0].redirects[0].mode, RedirectMode::Read);
        assert_eq!(cmds0[1].words[0].text(), "grep");
        assert_eq!(cmds0[1].words[1].text(), "hi");
        assert_eq!(cmds0[1].redirects[0].mode, RedirectMode::Write);
        let cmds1 = pipeline_cmds(&script, 1);
        assert_eq!(cmds1[0].redirects[0].mode, RedirectMode::Append);
    }

    #[test]
    fn parses_and_or_connectors() {
        let script = parse_script("false || echo fallback && echo done").unwrap();
        assert_eq!(script.statements.len(), 3);
        assert_eq!(script.statements[0].connector, PipelineConnector::Always);
        assert_eq!(script.statements[1].connector, PipelineConnector::Or);
        assert_eq!(script.statements[2].connector, PipelineConnector::And);
    }

    #[test]
    fn preserves_quoted_expansion_rules() {
        let script = parse_script("echo '$NOPE' \"$YES\" pre'$NO'-$YES").unwrap();
        let words = &pipeline_cmds(&script, 0)[0].words;
        assert_eq!(words[1].parts[0].text, "$NOPE");
        assert_eq!(words[1].parts[0].kind, WordPartKind::Literal);
        assert_eq!(words[2].parts[0].text, "$YES");
        assert_eq!(words[2].parts[0].kind, WordPartKind::Variable);
        assert_eq!(words[3].parts.len(), 3);
        assert_eq!(words[3].parts[0].text, "pre");
        assert_eq!(words[3].parts[0].kind, WordPartKind::Variable);
        assert_eq!(words[3].parts[1].text, "$NO");
        assert_eq!(words[3].parts[1].kind, WordPartKind::Literal);
        assert_eq!(words[3].parts[2].text, "-$YES");
        assert_eq!(words[3].parts[2].kind, WordPartKind::Variable);
    }

    #[test]
    fn preserves_empty_quoted_words() {
        let script = parse_script(r#"echo '' """#).unwrap();
        let words = &pipeline_cmds(&script, 0)[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[1].text(), "");
        assert_eq!(words[2].text(), "");
    }

    #[test]
    fn treats_hash_as_comment_only_at_word_start() {
        let script = parse_script("echo foo#bar # comment\necho https://example/#frag").unwrap();
        assert_eq!(script.statements.len(), 2);
        assert_eq!(pipeline_cmds(&script, 0)[0].words[1].text(), "foo#bar");
        assert_eq!(
            pipeline_cmds(&script, 1)[0].words[1].text(),
            "https://example/#frag"
        );
    }

    #[test]
    fn rejects_missing_pipeline_commands() {
        // "echo &&" should error (missing command after &&)
        let error = parse_script("echo &&").unwrap_err();
        assert!(
            error.to_string().contains("&&"),
            "expected token '&&' in error: {error}"
        );

        // "echo || || echo" should error (empty right side of first ||)
        let error = parse_script("echo || || echo").unwrap_err();
        assert!(
            error.to_string().contains("||"),
            "expected token '||' in error: {error}"
        );

        // Note: "| echo" is now accepted as just "echo" (leading pipe ignored)
        // This is a design choice in the recursive-descent parser
        let script = parse_script("| echo").unwrap();
        assert_eq!(script.statements.len(), 1);
    }

    #[test]
    fn rejects_missing_redirect_target() {
        let error = parse_script("echo hi >").unwrap_err();
        assert_eq!(
            error.to_string(),
            "syntax error near unexpected token `newline'"
        );
    }

    #[test]
    fn keeps_command_substitutions_as_command_sub_parts() {
        let script = parse_script("echo $(printf 'a|b;c') | cat && echo done").unwrap();
        assert_eq!(script.statements.len(), 2);
        let cmds = pipeline_cmds(&script, 0);
        assert_eq!(cmds.len(), 2);
        let sub_word = &cmds[0].words[1];
        assert_eq!(sub_word.parts.len(), 1);
        assert_eq!(sub_word.parts[0].kind, WordPartKind::CommandSub);
        assert_eq!(sub_word.parts[0].text, "printf 'a|b;c'");
    }

    #[test]
    fn keeps_arithmetic_substitution_body_without_trailing_paren() {
        let script = parse_script("echo $((i + 2 * 3))").unwrap();
        let cmds = pipeline_cmds(&script, 0);
        let sub_word = &cmds[0].words[1];
        assert_eq!(sub_word.parts.len(), 1);
        // `$((…))` is distinguished from `$(…)` at lex time, so a bare word is
        // never mistaken for an arithmetic expression.
        assert_eq!(sub_word.parts[0].kind, WordPartKind::Arithmetic);
        assert_eq!(sub_word.parts[0].text, "i + 2 * 3");
    }

    #[test]
    fn bare_word_command_substitution_is_not_arithmetic() {
        let script = parse_script("echo $(mycommand)").unwrap();
        let cmds = pipeline_cmds(&script, 0);
        let sub_word = &cmds[0].words[1];
        assert_eq!(sub_word.parts[0].kind, WordPartKind::CommandSub);
        assert_eq!(sub_word.parts[0].text, "mycommand");
    }

    #[test]
    fn errors_on_unclosed_command_substitution() {
        let error = parse_script("echo $(printf hi").unwrap_err();
        assert_eq!(
            error.to_string(),
            "unexpected EOF while looking for matching `)`"
        );
    }

    #[test]
    fn rejects_excessive_command_substitution_nesting() {
        let limits = ParseLimits {
            max_command_substitution_depth: 2,
        };
        let error =
            parse_script_with_limits("echo $(printf $(printf $(printf hi)))", limits).unwrap_err();
        assert_eq!(
            error.to_string(),
            "command substitution nesting exceeds limit (2)"
        );
    }

    #[test]
    fn parses_if_elif_else_fi() {
        let script =
            parse_script("if true; then echo yes; elif false; then echo mid; else echo no; fi")
                .unwrap();
        assert_eq!(script.statements.len(), 1);
        let StatementKind::If(if_stmt) = &script.statements[0].kind else {
            panic!("expected If");
        };
        assert_eq!(if_stmt.condition.len(), 1);
        assert_eq!(if_stmt.body.len(), 1);
        assert_eq!(if_stmt.elif_clauses.len(), 1);
        assert!(if_stmt.else_body.is_some());
    }

    #[test]
    fn parses_while_loop() {
        let script = parse_script("while false; do echo loop; done").unwrap();
        let StatementKind::While(w) = &script.statements[0].kind else {
            panic!("expected While");
        };
        assert_eq!(w.condition.len(), 1);
        assert_eq!(w.body.len(), 1);
    }

    #[test]
    fn parses_for_loop() {
        let script = parse_script("for x in a b c; do echo $x; done").unwrap();
        let StatementKind::For(f) = &script.statements[0].kind else {
            panic!("expected For");
        };
        assert_eq!(f.var, "x");
        assert_eq!(f.items.len(), 3);
        assert_eq!(f.body.len(), 1);
    }
}

/// Tests for escaped/quoted word handling and fail-closed parsing
/// (upstream #347, #348).
#[cfg(test)]
mod escape_and_reserved_word_tests {
    use super::*;

    fn pipeline(script: &Script, index: usize) -> &[CommandInvocation] {
        match &script.statements[index].kind {
            StatementKind::Pipeline(commands) => commands,
            other => panic!("expected Pipeline, got {other:?}"),
        }
    }

    fn first_kind(script: &str) -> StatementKind {
        parse_script(script).unwrap().statements[0].kind.clone()
    }

    #[test]
    fn escaped_reserved_word_is_an_ordinary_command() {
        // `\if` must not open an if-statement: it is the command name `if`.
        let script = parse_script("\\if true").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[0].text(), "if");
        assert!(!commands[0].words[0].is_bare());
        assert_eq!(commands[0].words[1].text(), "true");

        // Consequently the matching `then` is a stray reserved word in command
        // position, which bash also rejects (verified: syntax error, exit 2).
        let error = parse_script("\\if true; then echo yes; fi").unwrap_err();
        assert!(
            error.to_string().contains("unsupported construct `then'"),
            "{error}"
        );
    }

    #[test]
    fn quoted_reserved_word_is_an_ordinary_command() {
        for source in ["'if' x", "\"if\" x"] {
            let script = parse_script(source).unwrap();
            let commands = pipeline(&script, 0);
            assert_eq!(commands[0].words[0].text(), "if", "{source}");
            assert!(!commands[0].words[0].is_bare(), "{source}");
        }
    }

    #[test]
    fn partially_quoted_reserved_word_is_not_reserved() {
        for source in ["i\\f x", "i\"f\" x", "\"i\"f x"] {
            let script = parse_script(source).unwrap();
            let commands = pipeline(&script, 0);
            assert_eq!(commands[0].words[0].text(), "if", "{source}");
        }
    }

    #[test]
    fn unquoted_reserved_word_still_opens_a_statement() {
        assert!(matches!(
            first_kind("if true; then echo y; fi"),
            StatementKind::If(_)
        ));
        assert!(matches!(
            first_kind("while false; do :; done"),
            StatementKind::While(_)
        ));
        assert!(matches!(
            first_kind("for i in 1; do :; done"),
            StatementKind::For(_)
        ));
    }

    #[test]
    fn backslash_newline_joins_and_keeps_reserved_status() {
        // `ti\<newline>me` joins into the *unquoted* word `time`, so it is still
        // a reserved word. This runtime has no timing prefix, so it fails
        // closed rather than running `time` as an external command.
        let error = parse_script("ti\\\nme echo").unwrap_err();
        assert!(
            error.to_string().contains("unsupported construct `time'"),
            "{error}"
        );

        // The escaped form stays a non-reserved, ordinary command word.
        let escaped = parse_script("\\time echo").unwrap();
        let commands = pipeline(&escaped, 0);
        assert_eq!(commands[0].words[0].text(), "time");
        assert!(!commands[0].words[0].is_bare());
    }

    #[test]
    fn backslash_newline_joins_ordinary_words() {
        let script = parse_script("echo a\\\nb").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "ab");
    }

    #[test]
    fn escaped_characters_become_literal_parts() {
        let script = parse_script("echo \\$HOME").unwrap();
        let commands = pipeline(&script, 0);
        let word = &commands[0].words[1];
        assert_eq!(word.parts[0].kind, WordPartKind::Escaped);
        assert_eq!(word.parts[0].text, "$");
        assert_eq!(word.text(), "$HOME");
    }

    #[test]
    fn double_quote_escape_set_is_narrow() {
        // Only $, `, " and \ are escapable inside double quotes.
        let script = parse_script("echo \"a\\qb\"").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "a\\qb");

        let script = parse_script("echo \"a\\\"b\"").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "a\"b");
    }

    #[test]
    fn single_quotes_keep_every_character_verbatim() {
        let script = parse_script("echo 'a\\$b'").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "a\\$b");
        assert_eq!(commands[0].words[1].parts[0].kind, WordPartKind::Literal);
    }

    #[test]
    fn unsupported_constructs_fail_closed() {
        for source in [
            "select x; do echo q; done",
            "coproc echo hi",
            "echo input | coproc",
            "time echo hi",
            "! select",
            "then echo hi",
            "esac",
            "done",
        ] {
            let error = parse_script(source).unwrap_err();
            assert!(
                error.to_string().contains("unsupported construct"),
                "{source} -> {error}"
            );
        }
    }

    #[test]
    fn reserved_words_remain_valid_as_arguments() {
        let script = parse_script("echo select coproc").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "select");
        assert_eq!(commands[0].words[2].text(), "coproc");
    }

    #[test]
    fn parses_fd_prefixes_and_duplication() {
        let script = parse_script("cmd 2> err.txt 1>&2 3>&- < in.txt").unwrap();
        let commands = pipeline(&script, 0);
        let redirects = &commands[0].redirects;
        assert_eq!(redirects[0].mode, RedirectMode::Write);
        assert_eq!(redirects[0].fd, 2);
        assert_eq!(redirects[0].target.text(), "err.txt");
        assert_eq!(redirects[1].mode, RedirectMode::Dup(2));
        assert_eq!(redirects[1].fd, 1);
        assert_eq!(redirects[2].mode, RedirectMode::Close);
        assert_eq!(redirects[2].fd, 3);
        assert_eq!(redirects[3].mode, RedirectMode::Read);
        assert_eq!(redirects[3].fd, 0);
    }

    #[test]
    fn digit_ending_words_are_not_descriptors() {
        let script = parse_script("echo a2>f").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].words[1].text(), "a2");
        assert_eq!(commands[0].redirects[0].fd, 1);
    }

    #[test]
    fn append_redirection_keeps_its_descriptor() {
        let script = parse_script("cmd 2>> log").unwrap();
        let commands = pipeline(&script, 0);
        assert_eq!(commands[0].redirects[0].mode, RedirectMode::Append);
        assert_eq!(commands[0].redirects[0].fd, 2);
    }
}
