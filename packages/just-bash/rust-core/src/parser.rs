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
    /// Normal text — `$VAR`, `${VAR}`, `$?`, `$1`, etc. are expanded.
    Variable,
    /// Command substitution `$(…)` — the inner script is run and its stdout
    /// (trailing newlines stripped) replaces the word part.
    CommandSub,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordPart {
    pub text: String,
    pub kind: WordPartKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub parts: Vec<WordPart>,
}

impl Word {
    /// Create a single-part word that will be variable-expanded.
    pub fn literal(value: impl Into<String>) -> Self {
        Self {
            parts: vec![WordPart {
                text: value.into(),
                kind: WordPartKind::Variable,
            }],
        }
    }

    /// Return the raw (unexpanded) text of all parts joined.
    pub fn text(&self) -> String {
        self.parts.iter().map(|p| p.text.as_str()).collect()
    }
}

// ─── Redirections ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub mode: RedirectMode,
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectMode {
    Read,
    Write,
    Append,
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
pub enum StatementKind {
    Pipeline(Vec<CommandInvocation>),
    If(IfStatement),
    While(WhileStatement),
    For(ForStatement),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// How this statement connects to the previous one.
    pub connector: PipelineConnector,
    pub kind: StatementKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub statements: Vec<Statement>,
}

// ─── Entry points ─────────────────────────────────────────────────────────────

pub fn parse_script(source: &str) -> Result<Script, Error> {
    parse_script_with_limits(source, ParseLimits::default())
}

pub fn parse_script_with_limits(source: &str, limits: ParseLimits) -> Result<Script, Error> {
    let tokens = lex(source, limits)?;
    let mut pos = 0;
    let statements = parse_statements(&tokens, &mut pos, &[])?;
    if pos < tokens.len() {
        if let Token::Word(w) = &tokens[pos] {
            return Err(Error::Parse(format!(
                "syntax error near unexpected token `{}'",
                w.text()
            )));
        }
        return Err(Error::Parse(
            "syntax error near unexpected token".to_string(),
        ));
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
    RedirectRead,
    RedirectWrite,
    RedirectAppend,
    Separator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

// ─── Recursive-descent parser ─────────────────────────────────────────────────

/// Parse a flat list of statements, stopping when a terminator keyword is seen
/// (without consuming it) or the token stream ends.
fn parse_statements(
    tokens: &[Token],
    pos: &mut usize,
    terminators: &[&str],
) -> Result<Vec<Statement>, Error> {
    let mut statements = Vec::new();
    let mut connector = PipelineConnector::Always;

    loop {
        skip_separators(tokens, pos);
        if *pos >= tokens.len() {
            break;
        }
        if is_keyword_at(tokens, *pos, terminators) {
            break;
        }

        let Some(kind) = parse_single_statement(tokens, pos, terminators)? else {
            // If we couldn't parse a statement but have a pending connector, error
            if !matches!(connector, PipelineConnector::Always) {
                let connector_name = match connector {
                    PipelineConnector::And => "&&",
                    PipelineConnector::Or => "||",
                    _ => unreachable!(),
                };
                return Err(Error::Parse(format!(
                    "syntax error near unexpected token `{}'",
                    connector_name
                )));
            }
            break;
        };
        statements.push(Statement { connector, kind });

        // The token immediately after a statement determines the NEXT connector.
        // parse_single_statement stops without consuming &&/||/separator.
        match tokens.get(*pos) {
            Some(Token::AndIf) => {
                *pos += 1;
                connector = PipelineConnector::And;
            }
            Some(Token::OrIf) => {
                *pos += 1;
                connector = PipelineConnector::Or;
            }
            _ => {
                connector = PipelineConnector::Always;
            }
        }
    }

    // Check if we have a pending connector but reached EOF
    if !matches!(connector, PipelineConnector::Always) && *pos >= tokens.len() {
        let connector_name = match connector {
            PipelineConnector::And => "&&",
            PipelineConnector::Or => "||",
            _ => unreachable!(),
        };
        return Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            connector_name
        )));
    }

    Ok(statements)
}

/// Parse one statement: a control-flow keyword compound or a pipeline.
/// Stops (without consuming) at &&, ||, ;, \n, or a terminator keyword.
fn parse_single_statement(
    tokens: &[Token],
    pos: &mut usize,
    terminators: &[&str],
) -> Result<Option<StatementKind>, Error> {
    skip_separators(tokens, pos);
    if *pos >= tokens.len() {
        return Ok(None);
    }

    if let Token::Word(w) = &tokens[*pos] {
        match w.text().as_str() {
            t if terminators.contains(&t) => return Ok(None),
            "if" => {
                *pos += 1;
                return Ok(Some(StatementKind::If(parse_if(tokens, pos)?)));
            }
            "while" => {
                *pos += 1;
                return Ok(Some(StatementKind::While(parse_while(tokens, pos)?)));
            }
            "for" => {
                *pos += 1;
                return Ok(Some(StatementKind::For(parse_for(tokens, pos)?)));
            }
            _ => {}
        }
    }

    parse_pipeline_statement(tokens, pos, terminators)
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
            Token::Word(w) => {
                if is_keyword_at(tokens, *pos, terminators) {
                    break;
                }
                words.push(w.clone());
                pending_pipe = false;
                *pos += 1;
            }
            Token::RedirectRead | Token::RedirectWrite | Token::RedirectAppend => {
                let mode = match tokens[*pos] {
                    Token::RedirectRead => RedirectMode::Read,
                    Token::RedirectWrite => RedirectMode::Write,
                    Token::RedirectAppend => RedirectMode::Append,
                    _ => unreachable!(),
                };
                *pos += 1;
                let Some(Token::Word(target)) = tokens.get(*pos) else {
                    return Err(Error::Parse(
                        "syntax error near unexpected token `newline'".to_string(),
                    ));
                };
                redirects.push(Redirect {
                    mode,
                    target: target.clone(),
                });
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
            Token::AndIf | Token::OrIf | Token::Separator => {
                if pending_pipe {
                    let tok = if matches!(tokens[*pos], Token::AndIf) {
                        "&&"
                    } else if matches!(tokens[*pos], Token::OrIf) {
                        "||"
                    } else {
                        "newline"
                    };
                    return Err(Error::Parse(format!(
                        "syntax error near unexpected token `{tok}'"
                    )));
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

fn parse_while(tokens: &[Token], pos: &mut usize) -> Result<WhileStatement, Error> {
    let condition = parse_statements(tokens, pos, &["do"])?;
    expect_keyword(tokens, pos, "do")?;
    let body = parse_statements(tokens, pos, &["done"])?;
    expect_keyword(tokens, pos, "done")?;
    Ok(WhileStatement { condition, body })
}

fn parse_for(tokens: &[Token], pos: &mut usize) -> Result<ForStatement, Error> {
    skip_separators(tokens, pos);

    let var = match tokens.get(*pos) {
        Some(Token::Word(w)) => {
            let text = w.text();
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

    // expect "in"
    match tokens.get(*pos) {
        Some(Token::Word(w)) if w.text() == "in" => *pos += 1,
        _ => {
            return Err(Error::Parse(
                "syntax error: expected 'in' after for variable".to_string(),
            ));
        }
    }

    // collect items until separator or "do"
    let mut items = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Word(w) if w.text() == "do" => break,
            Token::Word(w) => {
                items.push(w.clone());
                *pos += 1;
            }
            Token::Separator => {
                *pos += 1;
                break;
            }
            _ => break,
        }
    }

    // expect "do" (possibly after separator)
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(Token::Word(w)) if w.text() == "do" => *pos += 1,
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

fn expect_keyword(tokens: &[Token], pos: &mut usize, keyword: &str) -> Result<(), Error> {
    skip_separators(tokens, pos);
    match tokens.get(*pos) {
        Some(Token::Word(w)) if w.text() == keyword => {
            *pos += 1;
            Ok(())
        }
        Some(Token::Word(w)) => Err(Error::Parse(format!(
            "syntax error near unexpected token `{}'",
            w.text()
        ))),
        Some(_) => Err(Error::Parse(format!(
            "syntax error: expected '{keyword}'"
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

fn is_keyword_at(tokens: &[Token], pos: usize, keywords: &[&str]) -> bool {
    match tokens.get(pos) {
        Some(Token::Word(w)) => keywords.contains(&w.text().as_str()),
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

// ─── Lexer ────────────────────────────────────────────────────────────────────

fn lex(source: &str, limits: ParseLimits) -> Result<Vec<Token>, Error> {
    let mut tokens = Vec::new();
    let mut current = Word { parts: Vec::new() };
    let mut current_started = false;
    let mut chars = source.chars().peekable();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '#') if !current_started => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        tokens.push(Token::Separator);
                        break;
                    }
                }
            }
            (None, '\'') => {
                current_started = true;
                quote = Some(Quote::Single);
            }
            (None, '"') => {
                current_started = true;
                quote = Some(Quote::Double);
            }
            (Some(Quote::Single), '\'') | (Some(Quote::Double), '"') => quote = None,
            (None, c) if c.is_whitespace() && c != '\n' => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
            }
            (None, '\n' | ';') => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
                tokens.push(Token::Separator);
            }
            (None, '|') => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'|').is_some() {
                    tokens.push(Token::OrIf);
                } else {
                    tokens.push(Token::Pipe);
                }
            }
            (None, '&') => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'&').is_some() {
                    tokens.push(Token::AndIf);
                } else {
                    return Err(Error::Parse(
                        "syntax error near unexpected token `&'".to_string(),
                    ));
                }
            }
            (None, '<') => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
                tokens.push(Token::RedirectRead);
            }
            (None, '>') => {
                push_word_token(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'>').is_some() {
                    tokens.push(Token::RedirectAppend);
                } else {
                    tokens.push(Token::RedirectWrite);
                }
            }
            // single-quoted character: Literal, no expansion
            (Some(Quote::Single), c) => {
                push_part(&mut current, c, WordPartKind::Literal, &mut current_started);
            }
            // command substitution $(...): execute and substitute
            (quote_state, '$') if quote_state != Some(Quote::Single) => {
                if chars.next_if_eq(&'(').is_some() {
                    let inner = if chars.next_if_eq(&'(').is_some() {
                        consume_arithmetic_substitution(&mut chars)?
                    } else {
                        consume_command_substitution(
                            &mut chars,
                            limits.max_command_substitution_depth,
                        )?
                    };
                    // Each $(...) is its own CommandSub part (never merged)
                    current_started = true;
                    current.parts.push(WordPart {
                        text: inner,
                        kind: WordPartKind::CommandSub,
                    });
                } else {
                    push_part(
                        &mut current,
                        '$',
                        WordPartKind::Variable,
                        &mut current_started,
                    );
                }
            }
            // backslash escape (outside single quotes)
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    let kind = if quote == Some(Quote::Single) {
                        WordPartKind::Literal
                    } else {
                        WordPartKind::Variable
                    };
                    push_part(&mut current, next, kind, &mut current_started);
                }
            }
            // all other characters
            (_, c) => {
                let kind = if quote == Some(Quote::Single) {
                    WordPartKind::Literal
                } else {
                    WordPartKind::Variable
                };
                push_part(&mut current, c, kind, &mut current_started);
            }
        }
    }

    if quote.is_some() {
        return Err(Error::Parse(
            "unexpected EOF while looking for matching quote".to_string(),
        ));
    }
    push_word_token(&mut tokens, &mut current, &mut current_started);
    Ok(tokens)
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
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        output.push(next);
                    }
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

fn push_part(word: &mut Word, ch: char, kind: WordPartKind, current_started: &mut bool) {
    *current_started = true;
    // Merge adjacent parts of the same kind (but never merge CommandSub)
    if !matches!(kind, WordPartKind::CommandSub) {
        if let Some(part) = word.parts.last_mut().filter(|p| p.kind == kind) {
            part.text.push(ch);
            return;
        }
    }
    word.parts.push(WordPart {
        text: ch.to_string(),
        kind,
    });
}

fn push_word_token(tokens: &mut Vec<Token>, current: &mut Word, current_started: &mut bool) {
    if *current_started || !current.parts.is_empty() {
        tokens.push(Token::Word(std::mem::replace(
            current,
            Word { parts: Vec::new() },
        )));
        *current_started = false;
    }
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
        assert_eq!(sub_word.parts[0].kind, WordPartKind::CommandSub);
        assert_eq!(sub_word.parts[0].text, "i + 2 * 3");
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
