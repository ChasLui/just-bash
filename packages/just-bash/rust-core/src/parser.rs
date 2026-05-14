use crate::fs::BashError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub pipelines: Vec<Pipeline>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub connector: PipelineConnector,
    pub commands: Vec<CommandInvocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineConnector {
    Always,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

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
pub struct Word {
    pub parts: Vec<WordPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordPart {
    pub text: String,
    pub expand: bool,
}

impl Word {
    pub fn literal(value: impl Into<String>) -> Self {
        Self {
            parts: vec![WordPart {
                text: value.into(),
                expand: true,
            }],
        }
    }

    pub fn text(&self) -> String {
        self.parts.iter().map(|part| part.text.as_str()).collect()
    }
}

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
enum TokenKind {
    Pipe,
    AndIf,
    OrIf,
}

impl TokenKind {
    fn display(self) -> &'static str {
        match self {
            Self::Pipe => "|",
            Self::AndIf => "&&",
            Self::OrIf => "||",
        }
    }
}

fn unexpected_token_error(kind: TokenKind) -> BashError {
    BashError::Parse(format!(
        "syntax error near unexpected token `{}`",
        kind.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Single,
    Double,
}

pub fn parse_script(source: &str) -> Result<Script, BashError> {
    let tokens = lex(source)?;
    let mut pipelines = Vec::new();
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut redirects = Vec::new();
    let mut next_connector = PipelineConnector::Always;
    let mut pending_operator: Option<TokenKind> = None;
    let mut index = 0;

    while index < tokens.len() {
        match &tokens[index] {
            Token::Word(word) => {
                words.push(word.clone());
                pending_operator = None;
            }
            Token::RedirectRead | Token::RedirectWrite | Token::RedirectAppend => {
                let mode = match tokens[index] {
                    Token::RedirectRead => RedirectMode::Read,
                    Token::RedirectWrite => RedirectMode::Write,
                    Token::RedirectAppend => RedirectMode::Append,
                    _ => unreachable!(),
                };
                index += 1;
                let Some(Token::Word(target)) = tokens.get(index) else {
                    return Err(BashError::Parse(
                        "syntax error near unexpected token `newline'".to_string(),
                    ));
                };
                redirects.push(Redirect {
                    mode,
                    target: target.clone(),
                });
                pending_operator = None;
            }
            Token::Pipe => {
                if !push_command(&mut commands, &mut words, &mut redirects)? {
                    return Err(unexpected_token_error(TokenKind::Pipe));
                }
                pending_operator = Some(TokenKind::Pipe);
            }
            Token::AndIf | Token::OrIf => {
                let kind = match tokens[index] {
                    Token::AndIf => TokenKind::AndIf,
                    Token::OrIf => TokenKind::OrIf,
                    _ => unreachable!(),
                };
                if !push_pipeline(
                    &mut pipelines,
                    &mut commands,
                    &mut words,
                    &mut redirects,
                    next_connector,
                )? {
                    return Err(unexpected_token_error(kind));
                }
                next_connector = match kind {
                    TokenKind::AndIf => PipelineConnector::And,
                    TokenKind::OrIf => PipelineConnector::Or,
                    _ => unreachable!(),
                };
                pending_operator = Some(kind);
            }
            Token::Separator => {
                if let Some(kind) = pending_operator {
                    return Err(unexpected_token_error(kind));
                }
                push_pipeline(
                    &mut pipelines,
                    &mut commands,
                    &mut words,
                    &mut redirects,
                    next_connector,
                )?;
                next_connector = PipelineConnector::Always;
            }
        }
        index += 1;
    }
    if let Some(kind) = pending_operator {
        return Err(unexpected_token_error(kind));
    }
    push_pipeline(
        &mut pipelines,
        &mut commands,
        &mut words,
        &mut redirects,
        next_connector,
    )?;

    Ok(Script { pipelines })
}

fn push_pipeline(
    pipelines: &mut Vec<Pipeline>,
    commands: &mut Vec<CommandInvocation>,
    words: &mut Vec<Word>,
    redirects: &mut Vec<Redirect>,
    connector: PipelineConnector,
) -> Result<bool, BashError> {
    push_command(commands, words, redirects)?;
    if !commands.is_empty() {
        pipelines.push(Pipeline {
            connector,
            commands: std::mem::take(commands),
        });
        return Ok(true);
    }
    Ok(false)
}

fn push_command(
    commands: &mut Vec<CommandInvocation>,
    words: &mut Vec<Word>,
    redirects: &mut Vec<Redirect>,
) -> Result<bool, BashError> {
    if words.is_empty() && redirects.is_empty() {
        return Ok(false);
    }
    commands.push(CommandInvocation {
        words: std::mem::take(words),
        redirects: std::mem::take(redirects),
    });
    Ok(true)
}

fn lex(source: &str) -> Result<Vec<Token>, BashError> {
    let mut tokens = Vec::new();
    let mut current = Word { parts: Vec::new() };
    let mut current_started = false;
    let mut chars = source.chars().peekable();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '#') => {
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
                push_word(&mut tokens, &mut current, &mut current_started)
            }
            (None, '\n' | ';') => {
                push_word(&mut tokens, &mut current, &mut current_started);
                tokens.push(Token::Separator);
            }
            (None, '|') => {
                push_word(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'|').is_some() {
                    tokens.push(Token::OrIf);
                } else {
                    tokens.push(Token::Pipe);
                }
            }
            (None, '&') => {
                push_word(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'&').is_some() {
                    tokens.push(Token::AndIf);
                } else {
                    return Err(BashError::Parse(
                        "syntax error near unexpected token `&'".to_string(),
                    ));
                }
            }
            (None, '<') => {
                push_word(&mut tokens, &mut current, &mut current_started);
                tokens.push(Token::RedirectRead);
            }
            (None, '>') => {
                push_word(&mut tokens, &mut current, &mut current_started);
                if chars.next_if_eq(&'>').is_some() {
                    tokens.push(Token::RedirectAppend);
                } else {
                    tokens.push(Token::RedirectWrite);
                }
            }
            (Some(Quote::Single), c) => push_part(&mut current, c, false, &mut current_started),
            (_, '\\') => {
                if let Some(next) = chars.next() {
                    push_part(
                        &mut current,
                        next,
                        quote != Some(Quote::Single),
                        &mut current_started,
                    );
                }
            }
            (_, c) => push_part(
                &mut current,
                c,
                quote != Some(Quote::Single),
                &mut current_started,
            ),
        }
    }

    if quote.is_some() {
        return Err(BashError::Parse(
            "unexpected EOF while looking for matching quote".to_string(),
        ));
    }
    push_word(&mut tokens, &mut current, &mut current_started);
    Ok(tokens)
}

fn push_part(word: &mut Word, ch: char, expand: bool, current_started: &mut bool) {
    *current_started = true;
    if let Some(part) = word.parts.last_mut().filter(|part| part.expand == expand) {
        part.text.push(ch);
    } else {
        word.parts.push(WordPart {
            text: ch.to_string(),
            expand,
        });
    }
}

fn push_word(tokens: &mut Vec<Token>, current: &mut Word, current_started: &mut bool) {
    if *current_started || !current.parts.is_empty() {
        tokens.push(Token::Word(std::mem::replace(
            current,
            Word { parts: Vec::new() },
        )));
        *current_started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pipelines_and_redirects() {
        let script = parse_script("cat < in.txt | grep hi > out.txt; echo done >> log").unwrap();
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(script.pipelines[0].connector, PipelineConnector::Always);
        assert_eq!(script.pipelines[0].commands.len(), 2);
        assert_eq!(script.pipelines[0].commands[0].words[0].text(), "cat");
        assert_eq!(
            script.pipelines[0].commands[0].redirects[0].mode,
            RedirectMode::Read
        );
        assert_eq!(script.pipelines[0].commands[1].words[0].text(), "grep");
        assert_eq!(script.pipelines[0].commands[1].words[1].text(), "hi");
        assert_eq!(
            script.pipelines[0].commands[1].redirects[0].mode,
            RedirectMode::Write
        );
        assert_eq!(
            script.pipelines[1].commands[0].redirects[0].mode,
            RedirectMode::Append
        );
    }

    #[test]
    fn parses_and_or_connectors() {
        let script = parse_script("false || echo fallback && echo done").unwrap();
        assert_eq!(script.pipelines.len(), 3);
        assert_eq!(script.pipelines[0].connector, PipelineConnector::Always);
        assert_eq!(script.pipelines[1].connector, PipelineConnector::Or);
        assert_eq!(script.pipelines[2].connector, PipelineConnector::And);
    }

    #[test]
    fn preserves_quoted_expansion_rules() {
        let script = parse_script("echo '$NOPE' \"$YES\" pre'$NO'-$YES").unwrap();
        let words = &script.pipelines[0].commands[0].words;
        assert_eq!(words[1].parts[0].text, "$NOPE");
        assert!(!words[1].parts[0].expand);
        assert_eq!(words[2].parts[0].text, "$YES");
        assert!(words[2].parts[0].expand);
        assert_eq!(words[3].parts.len(), 3);
        assert_eq!(words[3].parts[0].text, "pre");
        assert!(words[3].parts[0].expand);
        assert_eq!(words[3].parts[1].text, "$NO");
        assert!(!words[3].parts[1].expand);
        assert_eq!(words[3].parts[2].text, "-$YES");
        assert!(words[3].parts[2].expand);
    }

    #[test]
    fn preserves_empty_quoted_words() {
        let script = parse_script(r#"echo '' """#).unwrap();
        let words = &script.pipelines[0].commands[0].words;
        assert_eq!(words.len(), 3);
        assert_eq!(words[1].text(), "");
        assert_eq!(words[2].text(), "");
    }

    #[test]
    fn rejects_missing_pipeline_commands() {
        for (source, token) in [
            ("| echo", "|"),
            ("echo |", "|"),
            ("echo &&", "&&"),
            ("echo || || echo", "||"),
        ] {
            let error = parse_script(source).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("syntax error near unexpected token `{token}`")
            );
        }
    }

    #[test]
    fn rejects_missing_redirect_target() {
        let error = parse_script("echo hi >").unwrap_err();
        assert_eq!(
            error.to_string(),
            "syntax error near unexpected token `newline'"
        );
    }
}
