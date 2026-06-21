use std::collections::BTreeMap;

use crate::fs::{BashError, InMemoryFs, normalize_absolute, parent_dir};
use crate::parser::{
    CommandInvocation, ParseLimits, PipelineConnector, RedirectMode, Word, WordPartKind, Statement, StatementKind,
    IfStatement, WhileStatement, ForStatement, parse_script_with_limits,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BashExecutionLimits {
    pub max_script_size_bytes: usize,
    pub max_command_count: usize,
    pub max_command_substitution_depth: usize,
    pub max_output_bytes: usize,
    pub max_loop_iterations: usize,
}

impl Default for BashExecutionLimits {
    fn default() -> Self {
        Self {
            max_script_size_bytes: 1_048_576,
            max_command_count: 10_000,
            max_command_substitution_depth: 50,
            max_output_bytes: 10_485_760, // 10 MiB
            max_loop_iterations: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashOptions {
    pub files: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub isolate_exec: bool,
    pub execution_limits: BashExecutionLimits,
}

impl Default for BashOptions {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            env: BTreeMap::new(),
            cwd: None,
            isolate_exec: true,
            execution_limits: BashExecutionLimits::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Bash {
    fs: InMemoryFs,
    env: BTreeMap<String, String>,
    cwd: String,
    initial_env: BTreeMap<String, String>,
    initial_cwd: String,
    isolate_exec: bool,
    execution_limits: BashExecutionLimits,
    last_exit_code: i32,
    output_bytes_used: usize,
    loop_iteration_count: usize,
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
        Ok(Self {
            fs,
            env: env.clone(),
            cwd: cwd.clone(),
            initial_env: env,
            initial_cwd: cwd,
            isolate_exec: options.isolate_exec,
            execution_limits: options.execution_limits,
            last_exit_code: 0,
            output_bytes_used: 0,
            loop_iteration_count: 0,
        })
    }

    pub fn exec(&mut self, script: &str) -> BashExecResult {
        if script.len() > self.execution_limits.max_script_size_bytes {
            return BashExecResult {
                stdout: String::new(),
                stderr: format!(
                    "bash: script exceeds limit ({} bytes)\n",
                    self.execution_limits.max_script_size_bytes
                ),
                exit_code: 2,
                cwd: if self.isolate_exec {
                    self.initial_cwd.clone()
                } else {
                    self.cwd.clone()
                },
            };
        }

        if self.isolate_exec {
            let saved_env = self.env.clone();
            let saved_cwd = self.cwd.clone();
            let saved_output = self.output_bytes_used;
            let saved_loop_count = self.loop_iteration_count;
            self.env = self.initial_env.clone();
            self.cwd = self.initial_cwd.clone();
            self.env.insert("PWD".to_string(), self.cwd.clone());
            self.output_bytes_used = 0;
            self.loop_iteration_count = 0;
            let result = self.exec_in_current_state(script);
            self.env = saved_env;
            self.cwd = saved_cwd;
            self.output_bytes_used = saved_output;
            self.loop_iteration_count = saved_loop_count;
            result
        } else {
            self.exec_in_current_state(script)
        }
    }

    fn exec_in_current_state(&mut self, script: &str) -> BashExecResult {
        let parsed = match parse_script_with_limits(
            script,
            ParseLimits {
                max_command_substitution_depth: self
                    .execution_limits
                    .max_command_substitution_depth,
            },
        ) {
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

        let command_count: usize = parsed
            .statements
            .iter()
            .map(|stmt| self.count_commands_in_statement(stmt))
            .sum();

        if command_count > self.execution_limits.max_command_count {
            return BashExecResult {
                stdout: String::new(),
                stderr: format!(
                    "bash: command count exceeds limit ({})\n",
                    self.execution_limits.max_command_count
                ),
                exit_code: 2,
                cwd: self.cwd.clone(),
            };
        }

        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;

        for statement in parsed.statements {
            let should_run = match statement.connector {
                PipelineConnector::Always => true,
                PipelineConnector::And => exit_code == 0,
                PipelineConnector::Or => exit_code != 0,
            };
            if !should_run {
                continue;
            }

            let exec_result = self.exec_statement(&statement);
            stdout.push_str(&exec_result.0);
            stderr.push_str(&exec_result.1);
            exit_code = exec_result.2;

            if let Err(_) = self.check_output_limit(&mut stderr) {
                return BashExecResult {
                    stdout,
                    stderr,
                    exit_code: 2,
                    cwd: self.cwd.clone(),
                };
            }
        }

        self.last_exit_code = exit_code;
        BashExecResult {
            stdout,
            stderr,
            exit_code,
            cwd: self.cwd.clone(),
        }
    }

    fn count_commands_in_statement(&self, stmt: &Statement) -> usize {
        match &stmt.kind {
            StatementKind::Pipeline(commands) => commands.len(),
            StatementKind::If(if_stmt) => {
                let mut count = 0;
                for s in &if_stmt.condition {
                    count += self.count_commands_in_statement(s);
                }
                for s in &if_stmt.body {
                    count += self.count_commands_in_statement(s);
                }
                for (cond, body) in &if_stmt.elif_clauses {
                    for s in cond {
                        count += self.count_commands_in_statement(s);
                    }
                    for s in body {
                        count += self.count_commands_in_statement(s);
                    }
                }
                if let Some(else_body) = &if_stmt.else_body {
                    for s in else_body {
                        count += self.count_commands_in_statement(s);
                    }
                }
                count
            }
            StatementKind::While(while_stmt) => {
                let mut count = 0;
                for s in &while_stmt.condition {
                    count += self.count_commands_in_statement(s);
                }
                for s in &while_stmt.body {
                    count += self.count_commands_in_statement(s);
                }
                count
            }
            StatementKind::For(for_stmt) => {
                let mut count = 0;
                for s in &for_stmt.body {
                    count += self.count_commands_in_statement(s);
                }
                count
            }
        }
    }

    fn exec_statement(&mut self, statement: &Statement) -> (String, String, i32) {
        match &statement.kind {
            StatementKind::Pipeline(commands) => {
                self.exec_pipeline(commands)
            }
            StatementKind::If(if_stmt) => {
                self.exec_if(if_stmt)
            }
            StatementKind::While(while_stmt) => {
                self.exec_while(while_stmt)
            }
            StatementKind::For(for_stmt) => {
                self.exec_for(for_stmt)
            }
        }
    }

    fn exec_pipeline(&mut self, commands: &[CommandInvocation]) -> (String, String, i32) {
        let mut stdin = String::new();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;

        let command_count = commands.len();
        let is_pipeline = command_count > 1;

        for (index, command) in commands.iter().enumerate() {
            let is_last = index + 1 == command_count;
            let result = if is_pipeline {
                let mut child = self.clone();
                let result = child.run_invocation(command, &stdin);
                self.fs = child.fs;
                self.env = child.env;
                // Note: cwd is NOT copied back for pipeline commands (pipeline isolation)
                result
            } else {
                self.run_invocation(command, &stdin)
            };

            stderr.push_str(&result.stderr);
            exit_code = result.exit_code;

            if is_last {
                stdout.push_str(&result.stdout);
            } else {
                stdin = result.stdout;
            }

            if !is_pipeline && command.words.first().map(|w| self.expand_word(w)).as_deref() == Some("exit") {
                self.last_exit_code = exit_code;
                return (stdout, stderr, exit_code);
            }
        }

        self.last_exit_code = exit_code;
        (stdout, stderr, exit_code)
    }

    fn exec_if(&mut self, if_stmt: &IfStatement) -> (String, String, i32) {
        let cond_exit = self.exec_statement_list(&if_stmt.condition);
        if cond_exit == 0 {
            let result = self.exec_statement_list(&if_stmt.body);
            return (String::new(), String::new(), result);
        }

        for (elif_cond, elif_body) in &if_stmt.elif_clauses {
            let elif_exit = self.exec_statement_list(elif_cond);
            if elif_exit == 0 {
                let result = self.exec_statement_list(elif_body);
                return (String::new(), String::new(), result);
            }
        }

        let result = if let Some(else_body) = &if_stmt.else_body {
            self.exec_statement_list(else_body)
        } else {
            0
        };
        (String::new(), String::new(), result)
    }

    fn exec_while(&mut self, while_stmt: &WhileStatement) -> (String, String, i32) {
        let mut stderr = String::new();
        let mut exit_code = 0;

        while self.exec_statement_list(&while_stmt.condition) == 0 {
            self.loop_iteration_count += 1;
            if self.loop_iteration_count > self.execution_limits.max_loop_iterations {
                stderr.push_str(&format!(
                    "bash: loop iteration limit exceeded ({})\n",
                    self.execution_limits.max_loop_iterations
                ));
                return (String::new(), stderr, 2);
            }

            exit_code = self.exec_statement_list(&while_stmt.body);
        }

        (String::new(), stderr, exit_code)
    }

    fn exec_for(&mut self, for_stmt: &ForStatement) -> (String, String, i32) {
        let mut stderr = String::new();
        let mut exit_code = 0;

        let items = if for_stmt.items.is_empty() {
            self.env.get("@").map(|s| s.as_str()).unwrap_or("")
                .split_whitespace()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        } else {
            for_stmt.items.iter().map(|w| self.expand_word(w)).collect::<Vec<_>>()
        };

        for item in items {
            self.loop_iteration_count += 1;
            if self.loop_iteration_count > self.execution_limits.max_loop_iterations {
                stderr.push_str(&format!(
                    "bash: loop iteration limit exceeded ({})\n",
                    self.execution_limits.max_loop_iterations
                ));
                return (String::new(), stderr, 2);
            }

            self.env.insert(for_stmt.var.clone(), item);
            exit_code = self.exec_statement_list(&for_stmt.body);
        }

        (String::new(), stderr, exit_code)
    }

    fn exec_statement_list(&mut self, statements: &[Statement]) -> i32 {
        let mut exit_code = 0;

        for statement in statements {
            let should_run = match statement.connector {
                PipelineConnector::Always => true,
                PipelineConnector::And => exit_code == 0,
                PipelineConnector::Or => exit_code != 0,
            };
            if !should_run {
                continue;
            }

            let (_, _, code) = self.exec_statement(statement);
            exit_code = code;
        }

        exit_code
    }

    fn check_output_limit(&mut self, stderr: &mut String) -> Result<(), ()> {
        if self.output_bytes_used > self.execution_limits.max_output_bytes {
            stderr.push_str(&format!(
                "bash: output exceeds limit ({} bytes)\n",
                self.execution_limits.max_output_bytes
            ));
            Err(())
        } else {
            Ok(())
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
            let expanded_target = self.expand_word(&redirect.target);
            let target = self.resolve_path(&expanded_target);
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
                RedirectMode::Write => {
                    if let Err(error) = self.fs.write_file(&target, "") {
                        return CommandOutput {
                            stdout: String::new(),
                            stderr: format!("bash: {error}\n"),
                            exit_code: 1,
                        };
                    }
                    stdout_redirect = Some((redirect.mode, target));
                }
                RedirectMode::Append => {
                    if let Err(error) = self.fs.append_file(&target, "") {
                        return CommandOutput {
                            stdout: String::new(),
                            stderr: format!("bash: {error}\n"),
                            exit_code: 1,
                        };
                    }
                    stdout_redirect = Some((redirect.mode, target));
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
                    stdout.push_str(&format_printf(format, &args[1..]));
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
                    let lines = input.bytes().filter(|byte| *byte == b'\n').count();
                    let words = input.split_whitespace().count();
                    let bytes = input.len();
                    stdout.push_str(&format!("{lines} {words} {bytes}\n"));
                }
            }
            "ls" => {
                let target = args.first().map(String::as_str).unwrap_or(".");
                let path = self.resolve_path(target);
                if self.fs.is_file(&path) {
                    stdout.push_str(target);
                    stdout.push('\n');
                } else {
                    match self.fs.list_dir(&path) {
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
                    } else {
                        self.fs.create_dir(&path)
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
                let mut recursive = false;
                let mut operands = Vec::new();
                for arg in args {
                    if arg.starts_with('-') && arg != "-" {
                        if arg.chars().skip(1).all(|ch| matches!(ch, 'r' | 'R' | 'f')) {
                            recursive |= arg.chars().any(|ch| matches!(ch, 'r' | 'R'));
                        } else {
                            stderr.push_str(&format!("rm: unsupported option: {arg}\n"));
                            exit_code = 1;
                        }
                    } else {
                        operands.push(arg);
                    }
                }
                if exit_code == 0 && operands.is_empty() {
                    stderr.push_str(
                        "rm: missing operand
",
                    );
                    exit_code = 1;
                }
                if exit_code == 0 {
                    for arg in operands {
                        if let Err(error) = self.fs.remove(&self.resolve_path(arg), recursive) {
                            stderr.push_str(&format!("rm: {error}\n"));
                            exit_code = 1;
                        }
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
            "test" | "[" => {
                exit_code = if self.run_test(args) { 0 } else { 1 };
            }
            _ => {
                stderr.push_str(&format!("bash: {command}: command not found\n"));
                exit_code = 127;
            }
        }

        self.output_bytes_used += stdout.len() + stderr.len();
        CommandOutput {
            stdout,
            stderr,
            exit_code,
        }
    }

    fn run_test(&self, args: &[String]) -> bool {
        if args.is_empty() {
            return false;
        }

        if args[0] == "!" {
            return !self.run_test(&args[1..]);
        }

        if args.len() == 1 {
            return !args[0].is_empty();
        }

        if args.len() == 2 {
            match args[0].as_str() {
                "-z" => return args[1].is_empty(),
                "-n" => return !args[1].is_empty(),
                "-f" => return self.fs.is_file(&self.resolve_path(&args[1])),
                "-d" => return self.fs.is_dir(&self.resolve_path(&args[1])),
                "-e" => {
                    let path = self.resolve_path(&args[1]);
                    return self.fs.is_file(&path) || self.fs.is_dir(&path);
                }
                _ => {}
            }
        }

        if args.len() == 3 {
            match args[1].as_str() {
                "=" => return args[0] == args[2],
                "!=" => return args[0] != args[2],
                "-eq" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a == b;
                    }
                }
                "-ne" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a != b;
                    }
                }
                "-lt" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a < b;
                    }
                }
                "-gt" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a > b;
                    }
                }
                "-le" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a <= b;
                    }
                }
                "-ge" => {
                    if let (Ok(a), Ok(b)) = (args[0].parse::<i32>(), args[2].parse::<i32>()) {
                        return a >= b;
                    }
                }
                _ => {}
            }
        }

        false
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

    fn expand_word(&mut self, word: &Word) -> String {
        let mut expanded = String::new();
        for part in &word.parts {
            match part.kind {
                WordPartKind::Literal => {
                    expanded.push_str(&part.text);
                }
                WordPartKind::Variable => {
                    expanded.push_str(&self.expand_text(&part.text));
                }
                WordPartKind::CommandSub => {
                    let result = self.exec_command_substitution(&part.text);
                    expanded.push_str(&result);
                }
            }
        }
        self.output_bytes_used += expanded.len();
        expanded
    }

    fn exec_command_substitution(&mut self, script: &str) -> String {
        let mut child = self.clone();
        match child.exec_in_current_state(script) {
            result => {
                self.fs = child.fs;
                self.env = child.env;
                self.cwd = child.cwd;
                result.stdout.trim_end_matches('\n').to_string()
            }
        }
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
                expanded.push_str(self.expand_special_var(&name));
                continue;
            }

            let mut name = String::new();
            while let Some(&next) = chars.peek() {
                if next == '_' || next.is_ascii_alphanumeric() {
                    name.push(next);
                    chars.next();
                } else {
                    break;
                }
            }

            if name.is_empty() {
                expanded.push('$');
            } else if name == "?" {
                expanded.push_str(&self.last_exit_code.to_string());
            } else if name == "#" {
                if let Ok(argv) = std::env::var("ARGV") {
                    expanded.push_str(&argv.split_whitespace().count().to_string());
                } else {
                    expanded.push('0');
                }
            } else if name == "@" || name == "*" {
                if let Some(val) = self.env.get("@") {
                    expanded.push_str(val);
                }
            } else {
                expanded.push_str(self.env.get(&name).map(String::as_str).unwrap_or(""));
            }
        }

        expanded
    }

    fn expand_special_var(&self, name: &str) -> &str {
        if name == "?" {
            if self.last_exit_code == 0 {
                "0"
            } else {
                "1"
            }
        } else {
            self.env.get(name).map(String::as_str).unwrap_or("")
        }
    }
}

fn format_printf(format: &str, args: &[String]) -> String {
    let mut output = String::new();
    let mut arg_index = 0;
    let repeats = if args.is_empty() { 1 } else { args.len() };

    while arg_index < repeats {
        let consumed = append_printf_once(&mut output, format, &args[arg_index..]);
        if args.is_empty() {
            break;
        }
        arg_index += consumed.max(1);
    }

    output
}

fn append_printf_once(output: &mut String, format: &str, args: &[String]) -> usize {
    let mut consumed = 0;
    let mut chars = format.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some('\\') => output.push('\\'),
                Some(other) => output.push(other),
                None => output.push('\\'),
            }
            continue;
        }
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => output.push('%'),
            Some('s') | Some('b') | Some('d') | Some('i') => {
                output.push_str(args.get(consumed).map(String::as_str).unwrap_or(""));
                consumed += 1;
            }
            Some(other) => {
                output.push('%');
                output.push(other);
            }
            None => output.push('%'),
        }
    }

    consumed
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
    fn rejects_writes_to_directories_and_missing_parents() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("echo hi > /tmp; touch /tmp; echo hi > missing/file; ls /tmp");
        assert_eq!(result.stdout, "");
        assert_eq!(
            result.stderr,
            "bash: /tmp: Is a directory\ntouch: /tmp: Is a directory\nbash: /home/user/missing: No such file or directory\n"
        );
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn mkdir_without_parents_fails_for_missing_parent() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("mkdir nested/child; ls");
        assert_eq!(result.stdout, "");
        assert_eq!(
            result.stderr,
            "mkdir: /home/user/nested: No such file or directory\n"
        );
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn pipeline_commands_do_not_mutate_parent_shell_state() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("cd /tmp | cat; pwd; exit 7 | true; echo after");
        assert_eq!(result.stdout, "/home/user\nafter\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/home/user");
    }

    #[test]
    fn wc_counts_newline_bytes_for_lines() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("echo -n x | wc");
        assert_eq!(result.stdout, "0 1 1\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn output_redirection_truncates_before_command_runs() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/f".to_string(), "old\n".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("cat f > f; wc f");
        assert_eq!(result.stdout, "0 0 0\n");
        assert_eq!(result.stderr, "");
        assert_eq!(bash.fs().read_file("/home/user/f").unwrap(), "");
    }

    #[test]
    fn ls_lists_file_operands() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/file.txt".to_string(), "data".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("ls file.txt");
        assert_eq!(result.stdout, "file.txt\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn printf_formats_arguments() {
        let mut bash = Bash::new(BashOptions::default()).unwrap();
        let result = bash.exec("printf '%s\\n' a b");
        assert_eq!(result.stdout, "a\nb\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn rm_rejects_unsupported_options_without_deleting() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/file.txt".to_string(), "data".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("rm -i file.txt; cat file.txt");
        assert_eq!(result.stdout, "data");
        assert_eq!(result.stderr, "rm: unsupported option: -i\n");
        assert_eq!(result.exit_code, 0);
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

    #[test]
    fn isolates_environment_and_cwd_between_exec_calls_by_default() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/seed.txt".to_string(), "ok\n".to_string());
        let mut bash = Bash::new(BashOptions {
            files,
            ..BashOptions::default()
        })
        .unwrap();
        let first = bash
            .exec("NAME=rust; cd /tmp; cp /home/user/seed.txt /home/user/out.txt; echo $NAME; pwd");
        assert_eq!(first.stdout, "rust\n/tmp\n");
        let second = bash.exec("echo $NAME; pwd; cat out.txt");
        assert_eq!(second.stdout, "\n/home/user\nok\n");
        assert_eq!(second.stderr, "");
    }

    #[test]
    fn can_opt_out_of_exec_isolation() {
        let mut bash = Bash::new(BashOptions {
            isolate_exec: false,
            ..BashOptions::default()
        })
        .unwrap();
        let first = bash.exec("NAME=rust; cd /tmp");
        assert_eq!(first.exit_code, 0);
        let second = bash.exec("echo $NAME; pwd");
        assert_eq!(second.stdout, "rust\n/tmp\n");
    }

    #[test]
    fn rejects_scripts_larger_than_limit() {
        let mut bash = Bash::new(BashOptions {
            execution_limits: BashExecutionLimits {
                max_script_size_bytes: 8,
                ..BashExecutionLimits::default()
            },
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("echo hello");
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stderr, "bash: script exceeds limit (8 bytes)\n");
    }

    #[test]
    fn rejects_too_many_commands() {
        let mut bash = Bash::new(BashOptions {
            execution_limits: BashExecutionLimits {
                max_command_count: 2,
                ..BashExecutionLimits::default()
            },
            ..BashOptions::default()
        })
        .unwrap();
        let result = bash.exec("echo a; echo b; echo c");
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stderr, "bash: command count exceeds limit (2)\n");
    }
}
