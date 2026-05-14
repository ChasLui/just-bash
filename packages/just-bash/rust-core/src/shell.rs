use std::collections::BTreeMap;

use crate::fs::{BashError, InMemoryFs, normalize_absolute, parent_dir};
use crate::parser::{CommandInvocation, PipelineConnector, RedirectMode, Word, parse_script};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BashOptions {
    pub files: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Bash {
    fs: InMemoryFs,
    env: BTreeMap<String, String>,
    cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

impl Bash {
    pub fn new(options: BashOptions) -> Result<Self, BashError> {
        let cwd = normalize_absolute(options.cwd.as_deref().unwrap_or("/home/user"));
        let mut fs = InMemoryFs::with_files(options.files)?;
        fs.create_dir_all(&cwd)?;
        let mut env = options.env;
        env.entry("HOME".to_string())
            .or_insert_with(|| "/home/user".to_string());
        env.entry("PWD".to_string()).or_insert_with(|| cwd.clone());
        Ok(Self { fs, env, cwd })
    }

    pub fn exec(&mut self, script: &str) -> BashExecResult {
        let parsed = match parse_script(script) {
            Ok(parsed) => parsed,
            Err(error) => {
                return BashExecResult {
                    stdout: String::new(),
                    stderr: format!("bash: {error}\n"),
                    exit_code: 2,
                    cwd: self.cwd.clone(),
                };
            }
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;

        for pipeline in parsed.pipelines {
            let should_run = match pipeline.connector {
                PipelineConnector::Always => true,
                PipelineConnector::And => exit_code == 0,
                PipelineConnector::Or => exit_code != 0,
            };
            if !should_run {
                continue;
            }

            let mut stdin = String::new();
            let command_count = pipeline.commands.len();
            for (index, command) in pipeline.commands.iter().enumerate() {
                let is_last = index + 1 == command_count;
                let result = self.run_invocation(command, &stdin);
                stderr.push_str(&result.stderr);
                exit_code = result.exit_code;
                if is_last {
                    stdout.push_str(&result.stdout);
                } else {
                    stdin = result.stdout;
                }
                if command
                    .words
                    .first()
                    .is_some_and(|word| self.expand_word(word) == "exit")
                {
                    return BashExecResult {
                        stdout,
                        stderr,
                        exit_code,
                        cwd: self.cwd.clone(),
                    };
                }
            }
        }

        BashExecResult {
            stdout,
            stderr,
            exit_code,
            cwd: self.cwd.clone(),
        }
    }

    pub fn fs(&self) -> &InMemoryFs {
        &self.fs
    }

    fn run_invocation(
        &mut self,
        command: &CommandInvocation,
        pipeline_stdin: &str,
    ) -> CommandOutput {
        let mut stdin = pipeline_stdin.to_string();
        let mut stdout_redirect = None;

        for redirect in &command.redirects {
            let target = self.resolve_path(&self.expand_word(&redirect.target));
            match redirect.mode {
                RedirectMode::Read => match self.fs.read_file(&target) {
                    Ok(contents) => stdin = contents.to_string(),
                    Err(error) => {
                        return CommandOutput {
                            stdout: String::new(),
                            stderr: format!("bash: {error}\n"),
                            exit_code: 1,
                        };
                    }
                },
                RedirectMode::Write | RedirectMode::Append => {
                    stdout_redirect = Some((redirect.mode, target))
                }
            }
        }

        let words = command
            .words
            .iter()
            .map(|word| self.expand_word(word))
            .collect::<Vec<_>>();
        let mut result = self.run_command(&words, &stdin);

        if let Some((mode, target)) = stdout_redirect {
            let write_result = match mode {
                RedirectMode::Write => self.fs.write_file(&target, &result.stdout),
                RedirectMode::Append => self.fs.append_file(&target, &result.stdout),
                RedirectMode::Read => unreachable!(),
            };
            match write_result {
                Ok(()) => result.stdout.clear(),
                Err(error) => {
                    result.stderr.push_str(&format!("bash: {error}\n"));
                    result.exit_code = 1;
                }
            }
        }

        result
    }

    fn run_command(&mut self, words: &[String], stdin: &str) -> CommandOutput {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;
        if words.is_empty() {
            return CommandOutput {
                stdout,
                stderr,
                exit_code,
            };
        }
        if words.iter().all(|word| is_assignment(word)) {
            for assignment in words {
                let (key, value) = assignment
                    .split_once('=')
                    .expect("assignment contains equals");
                self.env.insert(key.to_string(), value.to_string());
            }
            return CommandOutput {
                stdout,
                stderr,
                exit_code,
            };
        }

        let command = words[0].as_str();
        let args = &words[1..];

        match command {
            "true" => exit_code = 0,
            "false" => exit_code = 1,
            "exit" => {
                exit_code = args
                    .first()
                    .and_then(|arg| arg.parse::<i32>().ok())
                    .unwrap_or(0)
            }
            "echo" => {
                let (newline, values) = if args.first().is_some_and(|arg| arg == "-n") {
                    (false, &args[1..])
                } else {
                    (true, args)
                };
                stdout.push_str(&values.join(" "));
                if newline {
                    stdout.push('\n');
                }
            }
            "printf" => {
                if let Some(format) = args.first() {
                    stdout.push_str(&format.replace("\\n", "\n"));
                }
            }
            "pwd" => {
                stdout.push_str(&self.cwd);
                stdout.push('\n');
            }
            "cd" => {
                let target = args.first().map(String::as_str).unwrap_or_else(|| {
                    self.env
                        .get("HOME")
                        .map(String::as_str)
                        .unwrap_or("/home/user")
                });
                let path = self.resolve_path(target);
                if self.fs.is_dir(&path) {
                    self.cwd = path.clone();
                    self.env.insert("PWD".to_string(), path);
                } else {
                    stderr.push_str(&format!("cd: {target}: No such file or directory\n"));
                    exit_code = 1;
                }
            }
            "cat" => {
                if args.is_empty() {
                    stdout.push_str(stdin);
                }
                for arg in args {
                    match self.fs.read_file(&self.resolve_path(arg)) {
                        Ok(contents) => stdout.push_str(contents),
                        Err(error) => {
                            stderr.push_str(&format!("cat: {error}\n"));
                            exit_code = 1;
                        }
                    }
                }
            }
            "grep" => {
                if args.is_empty() {
                    stderr.push_str("grep: usage: grep PATTERN [FILE...]\n");
                    exit_code = 2;
                } else {
                    let pattern = &args[0];
                    let haystacks = if args.len() == 1 {
                        vec![stdin.to_string()]
                    } else {
                        let mut values = Vec::new();
                        for arg in &args[1..] {
                            match self.fs.read_file(&self.resolve_path(arg)) {
                                Ok(contents) => values.push(contents.to_string()),
                                Err(error) => {
                                    stderr.push_str(&format!("grep: {error}\n"));
                                    exit_code = 1;
                                }
                            }
                        }
                        values
                    };
                    if exit_code == 0 {
                        for haystack in haystacks {
                            for line in haystack.lines() {
                                if line.contains(pattern) {
                                    stdout.push_str(line);
                                    stdout.push('\n');
                                }
                            }
                        }
                        if stdout.is_empty() {
                            exit_code = 1;
                        }
                    }
                }
            }
            "wc" => {
                let input = if args.is_empty() {
                    stdin.to_string()
                } else {
                    let mut combined = String::new();
                    for arg in args {
                        match self.fs.read_file(&self.resolve_path(arg)) {
                            Ok(contents) => combined.push_str(contents),
                            Err(error) => {
                                stderr.push_str(&format!("wc: {error}\n"));
                                exit_code = 1;
                            }
                        }
                    }
                    combined
                };
                if exit_code == 0 {
                    let lines = input.lines().count();
                    let words = input.split_whitespace().count();
                    let bytes = input.len();
                    stdout.push_str(&format!("{lines} {words} {bytes}\n"));
                }
            }
            "ls" => {
                let target = args.first().map(String::as_str).unwrap_or(".");
                match self.fs.list_dir(&self.resolve_path(target)) {
                    Ok(names) => {
                        stdout.push_str(&names.join("\n"));
                        if !names.is_empty() {
                            stdout.push('\n');
                        }
                    }
                    Err(error) => {
                        stderr.push_str(&format!("ls: {error}\n"));
                        exit_code = 1;
                    }
                }
            }
            "mkdir" => {
                let mut parents = false;
                let mut saw_path = false;
                for arg in args {
                    if arg == "-p" {
                        parents = true;
                        continue;
                    }
                    saw_path = true;
                    let path = self.resolve_path(arg);
                    let result = if parents {
                        self.fs.create_dir_all(&path)
                    } else if self.fs.exists(&path) {
                        Err(BashError::FileSystem(format!("{path}: File exists")))
                    } else {
                        self.fs.create_dir_all(&path)
                    };
                    if let Err(error) = result {
                        stderr.push_str(&format!("mkdir: {error}\n"));
                        exit_code = 1;
                    }
                }
                if !saw_path {
                    stderr.push_str("mkdir: usage: mkdir [-p] DIRECTORY...\n");
                    exit_code = 1;
                }
            }
            "touch" => {
                for arg in args {
                    let path = self.resolve_path(arg);
                    let existing = self.fs.read_file(&path).unwrap_or("").to_string();
                    if let Err(error) = self.fs.write_file(&path, &existing) {
                        stderr.push_str(&format!("touch: {error}\n"));
                        exit_code = 1;
                    }
                }
            }
            "cp" => {
                if args.len() != 2 {
                    stderr.push_str("cp: usage: cp SOURCE DEST\n");
                    exit_code = 1;
                } else {
                    let source = self.resolve_path(&args[0]);
                    let dest = self.resolve_path(&args[1]);
                    match self.fs.read_file(&source).map(str::to_string) {
                        Ok(contents) => {
                            if let Err(error) = self.fs.write_file(&dest, &contents) {
                                stderr.push_str(&format!("cp: {error}\n"));
                                exit_code = 1;
                            }
                        }
                        Err(error) => {
                            stderr.push_str(&format!("cp: {error}\n"));
                            exit_code = 1;
                        }
                    }
                }
            }
            "rm" => {
                let recursive = args
                    .iter()
                    .any(|arg| matches!(arg.as_str(), "-r" | "-R" | "-rf" | "-fr"));
                for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
                    if let Err(error) = self.fs.remove(&self.resolve_path(arg), recursive) {
                        stderr.push_str(&format!("rm: {error}\n"));
                        exit_code = 1;
                    }
                }
            }
            "export" => {
                for assignment in args {
                    if let Some((key, value)) = assignment.split_once('=') {
                        self.env.insert(key.to_string(), value.to_string());
                    }
                }
            }
            "env" => {
                for (key, value) in &self.env {
                    stdout.push_str(key);
                    stdout.push('=');
                    stdout.push_str(value);
                    stdout.push('\n');
                }
            }
            _ => {
                stderr.push_str(&format!("bash: {command}: command not found\n"));
                exit_code = 127;
            }
        }

        CommandOutput {
            stdout,
            stderr,
            exit_code,
        }
    }

    fn resolve_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            normalize_absolute(path)
        } else if path == "." {
            self.cwd.clone()
        } else if path == ".." {
            parent_dir(&self.cwd)
        } else {
            normalize_absolute(&format!("{}/{}", self.cwd, path))
        }
    }

    fn expand_word(&self, word: &Word) -> String {
        let mut expanded = String::new();
        for part in &word.parts {
            if part.expand {
                expanded.push_str(&self.expand_text(&part.text));
            } else {
                expanded.push_str(&part.text);
            }
        }
        expanded
    }

    fn expand_text(&self, text: &str) -> String {
        let mut expanded = String::new();
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch != '$' {
                expanded.push(ch);
                continue;
            }

            if chars.next_if_eq(&'{').is_some() {
                let mut name = String::new();
                for next in chars.by_ref() {
                    if next == '}' {
                        break;
                    }
                    name.push(next);
                }
                expanded.push_str(self.env.get(&name).map(String::as_str).unwrap_or(""));
                continue;
            }

            let mut name = String::new();
            while let Some(next) = chars.peek().copied() {
                if next == '_' || next.is_ascii_alphanumeric() {
                    name.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                expanded.push('$');
            } else {
                expanded.push_str(self.env.get(&name).map(String::as_str).unwrap_or(""));
            }
        }

        expanded
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((key, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_basic_commands_and_preserves_cwd() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("pwd; mkdir -p work; cd work; pwd; echo hello rust");
        assert_eq!(result.stdout, "/home/user\n/home/user/work\nhello rust\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/home/user/work");
    }

    #[test]
    fn reads_and_writes_virtual_files() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/input.txt".to_string(), "hello\n".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("cat input.txt; cp input.txt copy.txt; ls");
        assert_eq!(result.stdout, "hello\ncopy.txt\ninput.txt\n");
        assert_eq!(result.stderr, "");
        assert_eq!(
            bash.fs().read_file("/home/user/copy.txt").unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn reports_unknown_commands() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("wat");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "bash: wat: command not found\n");
        assert_eq!(result.exit_code, 127);
    }

    #[test]
    fn expands_environment_variables() {
        let mut env = BTreeMap::new();
        env.insert("NAME".to_string(), "rust".to_string());
        let mut bash = Bash::new(BashOptions {
            env,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("echo $NAME");
        assert_eq!(result.stdout, "rust\n");
    }

    #[test]
    fn pipes_stdout_into_next_command() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("echo alpha; echo beta | grep beta | wc");
        assert_eq!(result.stdout, "alpha\n1 1 5\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn supports_input_output_and_append_redirection() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/input.txt".to_string(), "one\ntwo\n".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("grep two < input.txt > out.txt; echo done >> out.txt; cat out.txt");
        assert_eq!(result.stdout, "two\ndone\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            bash.fs().read_file("/home/user/out.txt").unwrap(),
            "two\ndone\n"
        );
    }

    #[test]
    fn honors_and_or_connectors() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec(
            "false && echo nope; false || echo fallback; true && echo done; true || echo skip",
        );
        assert_eq!(result.stdout, "fallback\ndone\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn expands_variables_inside_words_and_braces() {
        let mut env = BTreeMap::new();
        env.insert("NAME".to_string(), "rust".to_string());
        env.insert("SUFFIX".to_string(), "core".to_string());
        let mut bash = Bash::new(BashOptions {
            env,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("echo hello-$NAME-${SUFFIX}");
        assert_eq!(result.stdout, "hello-rust-core\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn single_quotes_suppress_variable_expansion() {
        let mut env = BTreeMap::new();
        env.insert("NAME".to_string(), "rust".to_string());
        let mut bash = Bash::new(BashOptions {
            env,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec(r#"echo '$NAME' "$NAME" pre'$NAME'-$NAME"#);
        assert_eq!(result.stdout, "$NAME rust pre$NAME-rust\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn assignment_words_update_environment() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("NAME=rust; echo $NAME");
        assert_eq!(result.stdout, "rust\n");
        assert_eq!(result.stderr, "");
    }
}
