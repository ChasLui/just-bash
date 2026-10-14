//! A small, dependency-free `sed` implementation.
//!
//! Supported script subset: `s<d>pattern<d>replacement<d>flags` (with `g`, `p`
//! and `i`), `p`, `d`, `q`, and an optional line-number or `$` address on each
//! command. Anything else is a fail-closed error rather than a silent no-op.
//!
//! The module owns argument parsing too, so the shell only has to hand over the
//! command line, standard input, and a callback for reading files from the
//! in-memory filesystem.

use just_bash_regex::Regex;

/// The observable result of one `sed` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedRun {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Which lines a command applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SedAddress {
    Every,
    Line(usize),
    Last,
}

/// One parsed `sed` command.
#[derive(Debug, Clone)]
pub enum SedCommand {
    /// `s<d>pattern<d>replacement<d>flags`
    Substitute {
        regex: Regex,
        /// Replacement in `sed` syntax (`&`, `\1`, `\n`, `\t`).
        replacement: String,
        global: bool,
        print: bool,
        address: SedAddress,
    },
    Print {
        address: SedAddress,
    },
    Delete {
        address: SedAddress,
    },
    Quit {
        address: SedAddress,
    },
}

impl SedCommand {
    pub fn address(&self) -> SedAddress {
        match self {
            SedCommand::Substitute { address, .. }
            | SedCommand::Print { address }
            | SedCommand::Delete { address }
            | SedCommand::Quit { address } => *address,
        }
    }
}

/// What one line's pass through the program produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedOutcome {
    pub line: String,
    pub deleted: bool,
    pub quit: bool,
    /// Extra lines printed by an explicit `p` (or `s///p`).
    pub extra_prints: usize,
}

/// Run `sed` over `stdin`, reading file operands through `read_file`.
pub fn run(args: &[String], stdin: &str, read_file: &dyn Fn(&str) -> Option<String>) -> SedRun {
    let mut stdout = String::new();
    let mut stderr = String::new();

    let mut quiet = false;
    let mut scripts: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-n" | "--quiet" | "--silent" => quiet = true,
            "-E" | "-r" | "--regexp-extended" => {}
            "-e" | "--expression" => {
                index += 1;
                match args.get(index) {
                    Some(script) => scripts.push(script.clone()),
                    None => {
                        stderr.push_str("sed: option requires an argument -- 'e'\n");
                        return SedRun {
                            stdout,
                            stderr,
                            exit_code: 1,
                        };
                    }
                }
            }
            "--" => {
                index += 1;
                while let Some(rest) = args.get(index) {
                    operands.push(rest.clone());
                    index += 1;
                }
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                stderr.push_str(&format!(
                    "sed: invalid option -- '{}'\n",
                    other.chars().nth(1).unwrap_or('?')
                ));
                return SedRun {
                    stdout,
                    stderr,
                    exit_code: 1,
                };
            }
            other => operands.push(other.to_string()),
        }
        index += 1;
    }

    // The first operand is the script unless `-e` already supplied one; after
    // that, every operand is a file.
    let files: Vec<String> = if scripts.is_empty() {
        if operands.is_empty() {
            stderr.push_str("sed: no script specified\n");
            return SedRun {
                stdout,
                stderr,
                exit_code: 1,
            };
        }
        let mut rest = operands.clone();
        rest.remove(0);
        scripts.push(operands[0].clone());
        rest
    } else {
        operands.clone()
    };

    let mut program = Vec::new();
    for script in &scripts {
        match parse_script(script) {
            Ok(commands) => program.extend(commands),
            Err(message) => {
                stderr.push_str(&format!("sed: {message}\n"));
                return SedRun {
                    stdout,
                    stderr,
                    exit_code: 1,
                };
            }
        }
    }

    let mut input = String::new();
    if files.is_empty() {
        input.push_str(stdin);
    } else {
        for file in &files {
            match read_file(file) {
                Some(contents) => input.push_str(&contents),
                None => {
                    stderr.push_str(&format!("sed: {file}: No such file or directory\n"));
                    return SedRun {
                        stdout,
                        stderr,
                        exit_code: 2,
                    };
                }
            }
        }
    }

    let total = input.lines().count();
    for (number, line) in input.lines().enumerate() {
        let outcome = apply_program(&program, line, number + 1, total);
        if outcome.deleted {
            continue;
        }
        if !quiet {
            stdout.push_str(&outcome.line);
            stdout.push('\n');
        }
        for _ in 0..outcome.extra_prints {
            stdout.push_str(&outcome.line);
            stdout.push('\n');
        }
        if outcome.quit {
            break;
        }
    }

    SedRun {
        stdout,
        stderr,
        exit_code: 0,
    }
}

/// Apply a parsed program to a single line.
pub fn apply_program(
    program: &[SedCommand],
    line: &str,
    line_number: usize,
    total: usize,
) -> SedOutcome {
    let mut current = line.to_string();
    let mut extra_prints = 0usize;
    let mut deleted = false;
    let mut quit = false;

    for command in program {
        if !address_matches(command.address(), line_number, total) {
            continue;
        }
        match command {
            SedCommand::Substitute {
                regex,
                replacement,
                global,
                print,
                ..
            } => {
                let substituted = if *global {
                    let engine = replacement_to_engine(replacement);
                    let replaced = regex.replace_all(&current, &engine);
                    if replaced != current {
                        current = replaced;
                        true
                    } else {
                        false
                    }
                } else {
                    let (replaced, did) = substitute_once(regex, &current, replacement);
                    if did {
                        current = replaced;
                    }
                    did
                };
                if substituted && *print {
                    extra_prints += 1;
                }
            }
            SedCommand::Print { .. } => extra_prints += 1,
            SedCommand::Delete { .. } => {
                deleted = true;
                break;
            }
            SedCommand::Quit { .. } => {
                quit = true;
                break;
            }
        }
    }

    SedOutcome {
        line: current,
        deleted,
        quit,
        extra_prints,
    }
}

fn address_matches(address: SedAddress, line_number: usize, total: usize) -> bool {
    match address {
        SedAddress::Every => true,
        SedAddress::Line(number) => number == line_number,
        SedAddress::Last => line_number == total,
    }
}

/// Substitute the first match, expanding `sed` replacement escapes.
fn substitute_once(regex: &Regex, line: &str, replacement: &str) -> (String, bool) {
    match regex.captures(line) {
        Some(groups) => {
            let Some((start, end)) = groups.first().copied().flatten() else {
                return (line.to_string(), false);
            };
            let mut output = String::new();
            output.push_str(&line[..start]);
            output.push_str(&expand_replacement(replacement, line, &groups));
            output.push_str(&line[end..]);
            (output, true)
        }
        None => (line.to_string(), false),
    }
}

/// Expand `&`, `\0`..`\9`, `\n` and `\t` in a replacement.
fn expand_replacement(replacement: &str, text: &str, groups: &[Option<(usize, usize)>]) -> String {
    let mut output = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '&' => {
                if let Some((start, end)) = groups.first().copied().flatten() {
                    output.push_str(&text[start..end]);
                }
            }
            '\\' => match chars.next() {
                Some(digit @ '0'..='9') => {
                    let index = digit.to_digit(10).unwrap_or(0) as usize;
                    if let Some((start, end)) = groups.get(index).copied().flatten() {
                        output.push_str(&text[start..end]);
                    }
                }
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some(other) => output.push(other),
                None => output.push('\\'),
            },
            other => output.push(other),
        }
    }
    output
}

/// Rewrite a `sed` replacement into the regex engine's `$N` syntax.
fn replacement_to_engine(replacement: &str) -> String {
    let mut output = String::new();
    let mut chars = replacement.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            // `&` means the whole match in both syntaxes.
            '&' => output.push('&'),
            '\\' => match chars.next() {
                Some(digit @ '0'..='9') => {
                    output.push('$');
                    output.push(digit);
                }
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('\\') => output.push_str("\\\\"),
                Some(other) => output.push(other),
                None => output.push('\\'),
            },
            other => output.push(other),
        }
    }
    output
}

/// Parse a `;`/newline-separated script into commands.
pub fn parse_script(script: &str) -> Result<Vec<SedCommand>, String> {
    let chars: Vec<char> = script.chars().collect();
    let mut commands = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        while index < chars.len() && (chars[index].is_whitespace() || chars[index] == ';') {
            index += 1;
        }
        if index >= chars.len() {
            break;
        }
        let (command, next) = parse_command(&chars, index)?;
        commands.push(command);
        index = next;
    }
    if commands.is_empty() {
        return Err("empty script".to_string());
    }
    Ok(commands)
}

fn parse_command(chars: &[char], start: usize) -> Result<(SedCommand, usize), String> {
    let mut index = start;
    let mut address = SedAddress::Every;
    if chars.get(index) == Some(&'$') {
        address = SedAddress::Last;
        index += 1;
    } else {
        let digits_start = index;
        while matches!(chars.get(index), Some(ch) if ch.is_ascii_digit()) {
            index += 1;
        }
        if index > digits_start {
            let text: String = chars[digits_start..index].iter().collect();
            match text.parse::<usize>() {
                Ok(number) => address = SedAddress::Line(number),
                Err(_) => return Err(format!("invalid address `{text}'")),
            }
        }
    }

    let Some(&command) = chars.get(index) else {
        return Err("missing command".to_string());
    };
    index += 1;
    match command {
        's' => parse_substitute(chars, index, address),
        'p' => Ok((SedCommand::Print { address }, index)),
        'd' => Ok((SedCommand::Delete { address }, index)),
        'q' => Ok((SedCommand::Quit { address }, index)),
        other => Err(format!("unsupported command `{other}'")),
    }
}

fn parse_substitute(
    chars: &[char],
    start: usize,
    address: SedAddress,
) -> Result<(SedCommand, usize), String> {
    let Some(&delimiter) = chars.get(start) else {
        return Err("missing substitution delimiter".to_string());
    };
    let mut index = start + 1;
    let mut fields: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut escaped = false;

    while index < chars.len() && fields.len() < 2 {
        let ch = chars[index];
        if escaped {
            // Keep the escape so the regex engine sees it (`\/` is `/`).
            if ch == delimiter {
                current.push(ch);
            } else {
                current.push('\\');
                current.push(ch);
            }
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            index += 1;
            continue;
        }
        if ch == delimiter {
            fields.push(std::mem::take(&mut current));
            index += 1;
            continue;
        }
        current.push(ch);
        index += 1;
    }

    if fields.len() < 2 {
        return Err("unterminated `s' command".to_string());
    }

    let pattern = fields[0].clone();
    let replacement = fields[1].clone();
    let mut global = false;
    let mut print = false;
    let mut ignore_case = false;
    while index < chars.len() && !chars[index].is_whitespace() && chars[index] != ';' {
        match chars[index] {
            'g' => global = true,
            'p' => print = true,
            'i' | 'I' => ignore_case = true,
            other => return Err(format!("unsupported substitution flag `{other}'")),
        }
        index += 1;
    }

    let regex = Regex::new_with_flags(&pattern, ignore_case)
        .map_err(|error| format!("{pattern}: {error}"))?;
    Ok((
        SedCommand::Substitute {
            regex,
            replacement,
            global,
            print,
            address,
        },
        index,
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn run_script(script: &str, input: &str) -> String {
        let args = vec![script.to_string()];
        let result = run(&args, input, &|_| None);
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        result.stdout
    }

    #[test]
    fn substitutes_the_first_match_by_default() {
        assert_eq!(run_script("s/a/b/", "aaa\n"), "baa\n");
        assert_eq!(run_script("s/a/b/g", "aaa\n"), "bbb\n");
    }

    #[test]
    fn substitution_keeps_unmatched_lines() {
        assert_eq!(run_script("s/x/y/", "one\ntwo\n"), "one\ntwo\n");
    }

    #[test]
    fn ampersand_and_group_references() {
        assert_eq!(run_script("s/b/[&]/", "abc\n"), "a[b]c\n");
        assert_eq!(run_script("s/(a)(b)/\\2\\1/", "ab\n"), "ba\n");
    }

    #[test]
    fn print_and_quiet_modes() {
        assert_eq!(run_script("p", "x\n"), "x\nx\n");
        assert_eq!(run_script("s/x/y/p", "x\n"), "y\ny\n");
        let result = run(&["-n".to_string(), "p".to_string()], "x\n", &|_| None);
        assert_eq!(result.stdout, "x\n");
        let result = run(&["-n".to_string(), "s/x/y/p".to_string()], "x\n", &|_| None);
        assert_eq!(result.stdout, "y\n");
    }

    #[test]
    fn delete_and_quit() {
        assert_eq!(run_script("d", "a\nb\n"), "");
        assert_eq!(run_script("2d", "a\nb\nc\n"), "a\nc\n");
        assert_eq!(run_script("2q", "a\nb\nc\n"), "a\nb\n");
    }

    #[test]
    fn addresses_are_honored() {
        assert_eq!(run_script("2s/a/b/", "a\na\na\n"), "a\nb\na\n");
        assert_eq!(run_script("$s/a/b/", "a\na\na\n"), "a\na\nb\n");
        let result = run(&["-n".to_string(), "1p".to_string()], "a\nb\n", &|_| None);
        assert_eq!(result.stdout, "a\n");
    }

    #[test]
    fn case_insensitive_flag() {
        // Without `g` only the first match is replaced.
        assert_eq!(run_script("s/A/b/i", "aA\n"), "bA\n");
        assert_eq!(run_script("s/A/b/gi", "aA\n"), "bb\n");
    }

    #[test]
    fn multiple_expressions_and_separators() {
        assert_eq!(run_script("s/a/b/;s/b/c/", "a\n"), "c\n");
        let result = run(
            &[
                "-e".to_string(),
                "s/a/b/".to_string(),
                "-e".to_string(),
                "s/b/c/".to_string(),
            ],
            "a\n",
            &|_| None,
        );
        assert_eq!(result.stdout, "c\n");
    }

    #[test]
    fn alternate_delimiters() {
        assert_eq!(run_script("s|a|b|", "a\n"), "b\n");
        assert_eq!(run_script("s#/usr#/opt#", "/usr/bin\n"), "/opt/bin\n");
    }

    #[test]
    fn escaped_delimiter_is_literal() {
        assert_eq!(run_script("s/a\\/b/c/", "a/b\n"), "c\n");
    }

    #[test]
    fn script_operand_then_file_operands() {
        let files = |path: &str| match path {
            "in.txt" => Some("hello\n".to_string()),
            _ => None,
        };
        let result = run(
            &["s/hello/world/".to_string(), "in.txt".to_string()],
            "",
            &files,
        );
        assert_eq!(result.stdout, "world\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn missing_file_reports_status_two() {
        let result = run(
            &["s/a/b/".to_string(), "missing.txt".to_string()],
            "",
            &|_| None,
        );
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("No such file"), "{}", result.stderr);
    }

    #[test]
    fn unsupported_commands_fail_closed() {
        for script in ["y/a/b/", "a text", "w out.txt", "s/a/b/x"] {
            let result = run(&[script.to_string()], "a\n", &|_| None);
            assert_eq!(result.exit_code, 1, "{script} should fail");
            assert!(!result.stderr.is_empty(), "{script} needs a message");
        }
    }

    #[test]
    fn missing_script_is_an_error() {
        let result = run(&[], "", &|_| None);
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("no script"), "{}", result.stderr);
    }

    #[test]
    fn invalid_option_is_an_error() {
        let result = run(&["-Z".to_string(), "s/a/b/".to_string()], "a\n", &|_| None);
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("invalid option"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn replacement_newline_and_tab_escapes() {
        assert_eq!(run_script("s/x/\\t/", "x\n"), "\t\n");
    }
}
