use std::collections::BTreeMap;

use crate::fs::{Error, InMemoryFs, normalize_absolute, parent_dir};
use crate::parser::{
    CommandInvocation, ForStatement, IfStatement, ParseLimits, PipelineConnector, Redirect,
    RedirectMode, Statement, StatementKind, WhileStatement, Word, WordPartKind,
    parse_script_with_limits,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub max_script_size_bytes: usize,
    pub max_command_count: usize,
    pub max_command_substitution_depth: usize,
    pub max_output_bytes: usize,
    pub max_loop_iterations: usize,
}

impl Default for ExecutionLimits {
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
pub struct Options {
    pub files: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub isolate_exec: bool,
    pub execution_limits: ExecutionLimits,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            env: BTreeMap::new(),
            cwd: None,
            isolate_exec: true,
            execution_limits: ExecutionLimits::default(),
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
    execution_limits: ExecutionLimits,
    last_exit_code: i32,
    output_bytes_used: usize,
    loop_iteration_count: usize,
    /// set -e: exit immediately if a command returns non-zero
    opt_errexit: bool,
    /// set -u: treat unset variables as an error
    opt_nounset: bool,
    /// set -o pipefail: pipeline exit code is last non-zero status
    opt_pipefail: bool,
    /// Pending error from set -u: variable name that was unset
    nounset_error: Option<String>,
    /// Pending error from a parameter-expansion operator (`${x:?msg}`),
    /// stored pre-formatted as the stderr line to report.
    param_error: Option<String>,
    /// Pending `break`/`continue` unwinding state.
    loop_signal: Option<LoopSignal>,
    /// stderr produced while expanding `$(…)` in the current command. Bash
    /// forwards it to the command's stderr, so it is accumulated here and
    /// flushed by [`Bash::run_invocation`].
    expansion_stderr: String,
    /// Set when a command substitution failed to parse: the enclosing command
    /// must fail closed rather than run with an empty substitution.
    expansion_failed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

/// Where a redirected descriptor delivers its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sink {
    /// The command's own stdout buffer.
    Stdout,
    /// The command's own stderr buffer.
    Stderr,
    /// Dropped (`/dev/null`, `n>&-`).
    Discard,
    /// A virtual file.
    File { path: String, append: bool },
}

/// A pending `break` / `continue` raised by the loop builtins.
///
/// The payload is the number of loop levels still to unwind (`break 2` from
/// inside one loop leaves one level for the enclosing loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopSignal {
    Break(usize),
    Continue(usize),
}

/// How a pending [`LoopSignal`] applies to the loop that just ran a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopDirective {
    /// Stop this loop.
    Break,
    /// Start this loop's next iteration.
    Continue,
    /// The signal targets an enclosing loop: stop this loop and keep unwinding.
    Propagate,
}

impl Bash {
    pub fn new(options: Options) -> Result<Self, Error> {
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
            opt_errexit: false,
            opt_nounset: false,
            opt_pipefail: false,
            nounset_error: None,
            param_error: None,
            loop_signal: None,
            expansion_stderr: String::new(),
            expansion_failed: false,
        })
    }

    pub fn exec(&mut self, script: &str) -> ExecResult {
        if script.len() > self.execution_limits.max_script_size_bytes {
            return ExecResult {
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
            let saved_errexit = self.opt_errexit;
            let saved_nounset = self.opt_nounset;
            let saved_pipefail = self.opt_pipefail;
            self.env = self.initial_env.clone();
            self.cwd = self.initial_cwd.clone();
            self.env.insert("PWD".to_string(), self.cwd.clone());
            self.output_bytes_used = 0;
            self.loop_iteration_count = 0;
            self.opt_errexit = false;
            self.opt_nounset = false;
            self.opt_pipefail = false;
            self.nounset_error = None;
            self.param_error = None;
            let result = self.exec_in_current_state(script);
            self.env = saved_env;
            self.cwd = saved_cwd;
            self.output_bytes_used = saved_output;
            self.loop_iteration_count = saved_loop_count;
            self.opt_errexit = saved_errexit;
            self.opt_nounset = saved_nounset;
            self.opt_pipefail = saved_pipefail;
            self.nounset_error = None;
            self.param_error = None;
            result
        } else {
            self.exec_in_current_state(script)
        }
    }

    fn exec_in_current_state(&mut self, script: &str) -> ExecResult {
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
                return ExecResult {
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
            return ExecResult {
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
                return ExecResult {
                    stdout,
                    stderr,
                    exit_code: 2,
                    cwd: self.cwd.clone(),
                };
            }

            // set -e: abort on non-zero exit (but not when connector is || or &&)
            if self.opt_errexit
                && exit_code != 0
                && statement.connector == PipelineConnector::Always
            {
                self.last_exit_code = exit_code;
                return ExecResult {
                    stdout,
                    stderr,
                    exit_code,
                    cwd: self.cwd.clone(),
                };
            }
        }

        // A break/continue outside any loop has nothing to unwind to: it is
        // simply a no-op that reports success.
        self.loop_signal = None;

        self.last_exit_code = exit_code;
        ExecResult {
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
            StatementKind::Pipeline(commands) => self.exec_pipeline(commands),
            StatementKind::If(if_stmt) => self.exec_if(if_stmt),
            StatementKind::While(while_stmt) => self.exec_while(while_stmt),
            StatementKind::For(for_stmt) => self.exec_for(for_stmt),
        }
    }

    fn exec_pipeline(&mut self, commands: &[CommandInvocation]) -> (String, String, i32) {
        let mut stdin = String::new();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;
        // For pipefail: track all non-zero exit codes in the pipeline
        let mut pipeline_last_failure: i32 = 0;

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
            if is_pipeline && result.exit_code != 0 {
                pipeline_last_failure = result.exit_code;
            }

            if is_last {
                stdout.push_str(&result.stdout);
            } else {
                stdin = result.stdout;
            }

            if !is_pipeline
                && command
                    .words
                    .first()
                    .map(|w| self.expand_word(w))
                    .as_deref()
                    == Some("exit")
            {
                self.last_exit_code = exit_code;
                return (stdout, stderr, exit_code);
            }
        }

        // set -o pipefail: use last non-zero exit code from the pipeline
        if self.opt_pipefail && is_pipeline && pipeline_last_failure != 0 {
            exit_code = pipeline_last_failure;
        }

        self.last_exit_code = exit_code;
        (stdout, stderr, exit_code)
    }

    fn exec_if(&mut self, if_stmt: &IfStatement) -> (String, String, i32) {
        let (_, _, cond_exit) = self.exec_statement_list(&if_stmt.condition);
        if cond_exit == 0 {
            return self.exec_statement_list(&if_stmt.body);
        }

        for (elif_cond, elif_body) in &if_stmt.elif_clauses {
            let (_, _, elif_exit) = self.exec_statement_list(elif_cond);
            if elif_exit == 0 {
                return self.exec_statement_list(elif_body);
            }
        }

        if let Some(else_body) = &if_stmt.else_body {
            self.exec_statement_list(else_body)
        } else {
            (String::new(), String::new(), 0)
        }
    }

    fn exec_while(&mut self, while_stmt: &WhileStatement) -> (String, String, i32) {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;

        loop {
            self.loop_iteration_count += 1;
            if self.loop_iteration_count > self.execution_limits.max_loop_iterations {
                stderr.push_str(&format!(
                    "bash: loop iteration limit exceeded ({})\n",
                    self.execution_limits.max_loop_iterations
                ));
                return (stdout, stderr, 2);
            }

            let (cond_stdout, cond_stderr, cond_exit) =
                self.exec_statement_list(&while_stmt.condition);
            stdout.push_str(&cond_stdout);
            stderr.push_str(&cond_stderr);

            // A break/continue in the condition targets this loop.
            match self.loop_directive() {
                Some(LoopDirective::Break) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    break;
                }
                Some(LoopDirective::Continue) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    continue;
                }
                Some(LoopDirective::Propagate) => return (stdout, stderr, 0),
                None => {}
            }

            if cond_exit != 0 {
                break;
            }

            let (body_stdout, body_stderr, body_exit) = self.exec_statement_list(&while_stmt.body);
            stdout.push_str(&body_stdout);
            stderr.push_str(&body_stderr);
            exit_code = body_exit;

            self.output_bytes_used += body_stdout.len() + body_stderr.len();
            if let Err(_) = self.check_output_limit(&mut stderr) {
                return (stdout, stderr, 2);
            }

            match self.loop_directive() {
                Some(LoopDirective::Break) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    break;
                }
                Some(LoopDirective::Continue) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    continue;
                }
                Some(LoopDirective::Propagate) => return (stdout, stderr, 0),
                None => {}
            }
        }

        (stdout, stderr, exit_code)
    }

    fn exec_for(&mut self, for_stmt: &ForStatement) -> (String, String, i32) {
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;

        let items = if for_stmt.items.is_empty() {
            self.env
                .get("@")
                .map(|s| s.as_str())
                .unwrap_or("")
                .split_whitespace()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        } else {
            for_stmt
                .items
                .iter()
                .map(|w| self.expand_word(w))
                .collect::<Vec<_>>()
        };

        for item in items {
            self.loop_iteration_count += 1;
            if self.loop_iteration_count > self.execution_limits.max_loop_iterations {
                stderr.push_str(&format!(
                    "bash: loop iteration limit exceeded ({})\n",
                    self.execution_limits.max_loop_iterations
                ));
                return (stdout, stderr, 2);
            }

            self.env.insert(for_stmt.var.clone(), item);

            let (body_stdout, body_stderr, body_exit) = self.exec_statement_list(&for_stmt.body);
            stdout.push_str(&body_stdout);
            stderr.push_str(&body_stderr);
            exit_code = body_exit;

            self.output_bytes_used += body_stdout.len() + body_stderr.len();
            if let Err(_) = self.check_output_limit(&mut stderr) {
                return (stdout, stderr, 2);
            }

            match self.loop_directive() {
                Some(LoopDirective::Break) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    break;
                }
                Some(LoopDirective::Continue) => {
                    self.adopt_loop_status();
                    exit_code = 0;
                    continue;
                }
                Some(LoopDirective::Propagate) => return (stdout, stderr, 0),
                None => {}
            }
        }

        (stdout, stderr, exit_code)
    }

    /// Consume a pending `break`/`continue` for the innermost loop.
    fn loop_directive(&mut self) -> Option<LoopDirective> {
        match self.loop_signal.take() {
            Some(LoopSignal::Break(levels)) => {
                if levels <= 1 {
                    Some(LoopDirective::Break)
                } else {
                    self.loop_signal = Some(LoopSignal::Break(levels - 1));
                    Some(LoopDirective::Propagate)
                }
            }
            Some(LoopSignal::Continue(levels)) => {
                if levels <= 1 {
                    Some(LoopDirective::Continue)
                } else {
                    self.loop_signal = Some(LoopSignal::Continue(levels - 1));
                    Some(LoopDirective::Propagate)
                }
            }
            None => None,
        }
    }

    /// Adopt a break/continue status: the loop reports success and `$?` becomes
    /// 0 for the next command (upstream #017a911d).
    fn adopt_loop_status(&mut self) {
        self.last_exit_code = 0;
    }

    fn exec_statement_list(&mut self, statements: &[Statement]) -> (String, String, i32) {
        let mut stdout = String::new();
        let mut stderr = String::new();
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

            let (stmt_stdout, stmt_stderr, code) = self.exec_statement(statement);
            stdout.push_str(&stmt_stdout);
            stderr.push_str(&stmt_stderr);
            exit_code = code;

            self.output_bytes_used += stmt_stdout.len() + stmt_stderr.len();
            if let Err(_) = self.check_output_limit(&mut stderr) {
                return (stdout, stderr, 2);
            }

            // `break`/`continue` stops the rest of this statement list so the
            // signal can unwind to the enclosing loop.
            if self.loop_signal.is_some() {
                break;
            }
        }

        (stdout, stderr, exit_code)
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
        self.expansion_stderr.clear();
        self.expansion_failed = false;

        // Each descriptor points at a sink; `2>&1` makes fd 2 share fd 1's sink
        // (a duplicate descriptor), while two redirects to the same path are
        // independent opens. Redirects are applied in written order, so
        // `2>&1 > f` leaves stderr on the caller while `> f 2>&1` sends both to
        // the file (upstream #cb2b583b).
        let mut fd1 = Sink::Stdout;
        let mut fd2 = Sink::Stderr;

        for redirect in &command.redirects {
            match redirect.mode {
                RedirectMode::Read => {
                    let target = self.expand_redirect_target(redirect);
                    // `< /dev/null` is an empty stream, not a file lookup.
                    if target == "/dev/null" {
                        stdin.clear();
                        continue;
                    }
                    match self.fs.read_file(&target) {
                        Ok(contents) => stdin = contents.to_string(),
                        Err(error) => {
                            return CommandOutput {
                                stdout: String::new(),
                                stderr: format!("bash: {error}\n"),
                                exit_code: 1,
                            };
                        }
                    }
                }
                RedirectMode::Write | RedirectMode::Append => {
                    let target = self.expand_redirect_target(redirect);
                    // `/dev/null` is a discard device, not a virtual file: it is
                    // never created and never truncates anything.
                    if target == "/dev/null" {
                        match redirect.fd {
                            1 => fd1 = Sink::Discard,
                            2 => fd2 = Sink::Discard,
                            _ => {}
                        }
                        continue;
                    }
                    let append = matches!(redirect.mode, RedirectMode::Append);
                    // Open/truncate eagerly: the file exists even if a later
                    // redirect re-points the descriptor.
                    let opened = if append {
                        self.fs.append_file(&target, "")
                    } else {
                        self.fs.write_file(&target, "")
                    };
                    if let Err(error) = opened {
                        return CommandOutput {
                            stdout: String::new(),
                            stderr: format!("bash: {error}\n"),
                            exit_code: 1,
                        };
                    }
                    let sink = Sink::File {
                        path: target,
                        append,
                    };
                    match redirect.fd {
                        1 => fd1 = sink,
                        2 => fd2 = sink,
                        // This runtime models only descriptors 0-2.
                        _ => {}
                    }
                }
                RedirectMode::Dup(source) => match (redirect.fd, source) {
                    // `1>&2`: send stdout wherever stderr currently goes.
                    (1, 2) => fd1 = fd2.clone(),
                    // `2>&1`: send stderr wherever stdout currently goes.
                    (2, 1) => fd2 = fd1.clone(),
                    // `2>&2` / `1>&1` are no-ops rather than errors.
                    _ => {}
                },
                RedirectMode::Close => match redirect.fd {
                    1 => fd1 = Sink::Discard,
                    2 => fd2 = Sink::Discard,
                    _ => {}
                },
            }
        }

        let words = command
            .words
            .iter()
            .map(|word| self.expand_word(word))
            .collect::<Vec<_>>();

        // stderr produced by command substitutions during expansion is part of
        // this command's output.
        let expansion_stderr = std::mem::take(&mut self.expansion_stderr);

        // A command substitution that failed to parse aborts the command; it
        // must never run with a silently-empty substitution (upstream #348).
        if self.expansion_failed {
            self.expansion_failed = false;
            let mut stderr = expansion_stderr;
            stderr.push_str("bash: command substitution failed\n");
            return CommandOutput {
                stdout: String::new(),
                stderr,
                exit_code: 2,
            };
        }

        // set -u: abort if any variable expansion flagged an unset variable
        if let Some(ref varname) = self.nounset_error.take() {
            return CommandOutput {
                stdout: String::new(),
                stderr: format!("{expansion_stderr}bash: {varname}: unbound variable\n"),
                exit_code: 1,
            };
        }
        // `${x:?message}` and friends abort the command with exit status 1.
        if let Some(message) = self.param_error.take() {
            return CommandOutput {
                stdout: String::new(),
                stderr: format!("{expansion_stderr}{message}"),
                exit_code: 1,
            };
        }

        let mut result = self.run_command(&words, &stdin);
        if !expansion_stderr.is_empty() {
            result.stderr = format!("{expansion_stderr}{}", result.stderr);
        }

        // Deliver both streams exactly once, after the whole redirect list.
        if let Err(error) = self.deliver(&mut result, &fd1, &fd2) {
            result.stderr.push_str(&format!("bash: {error}\n"));
            result.exit_code = 1;
        }

        result
    }

    /// Route the command's stdout/stderr through the descriptor sinks.
    fn deliver(&mut self, result: &mut CommandOutput, fd1: &Sink, fd2: &Sink) -> Result<(), Error> {
        let stdout = std::mem::take(&mut result.stdout);
        let stderr = std::mem::take(&mut result.stderr);
        if stdout.is_empty() && stderr.is_empty() {
            return Ok(());
        }

        // When both descriptors share one file (a `2>&1` dup), the two streams
        // are written by a single descriptor: stdout first, then stderr.
        if fd1 == fd2
            && let Sink::File { path, append } = fd1
        {
            let combined = format!("{stdout}{stderr}");
            return self.write_sink(path, *append, &combined);
        }

        match fd1 {
            Sink::Stdout => result.stdout.push_str(&stdout),
            Sink::Stderr => result.stderr.push_str(&stdout),
            Sink::Discard => {}
            Sink::File { path, append } => self.write_sink(path, *append, &stdout)?,
        }
        match fd2 {
            Sink::Stderr => result.stderr.push_str(&stderr),
            Sink::Stdout => result.stdout.push_str(&stderr),
            Sink::Discard => {}
            Sink::File { path, append } => self.write_sink(path, *append, &stderr)?,
        }
        Ok(())
    }

    fn write_sink(&mut self, path: &str, append: bool, contents: &str) -> Result<(), Error> {
        if append {
            self.fs.append_file(path, contents)
        } else {
            self.fs.write_file(path, contents)
        }
    }

    /// Expand a redirection target word to an absolute virtual path.
    fn expand_redirect_target(&mut self, redirect: &Redirect) -> String {
        let word = redirect.target.clone();
        let expanded = self.expand_word(&word);
        self.resolve_path(&expanded)
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
            "true" | ":" => exit_code = 0,
            "false" => exit_code = 1,
            "break" | "continue" => {
                let levels = args
                    .first()
                    .and_then(|arg| arg.parse::<usize>().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(1);
                self.loop_signal = Some(if command == "break" {
                    LoopSignal::Break(levels)
                } else {
                    LoopSignal::Continue(levels)
                });
            }
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
                let (options, operands) = match parse_cat_options(args) {
                    Ok(parsed) => parsed,
                    Err(message) => {
                        stderr.push_str(&message);
                        exit_code = 1;
                        (CatOptions::default(), Vec::new())
                    }
                };
                if exit_code == 0 {
                    let mut input = String::new();
                    if operands.is_empty() {
                        input.push_str(stdin);
                    }
                    for arg in &operands {
                        match self.fs.read_file(&self.resolve_path(arg)) {
                            Ok(contents) => input.push_str(contents),
                            Err(error) => {
                                stderr.push_str(&format!("cat: {error}\n"));
                                exit_code = 1;
                            }
                        }
                    }
                    if options.is_plain() {
                        stdout.push_str(&input);
                    } else {
                        stdout.push_str(&apply_cat_options(input.as_bytes(), &options));
                    }
                }
            }
            "grep" => {
                match parse_grep_args(args) {
                    Err(message) => {
                        stderr.push_str(&message);
                        exit_code = 2;
                    }
                    Ok(options) => {
                        // Patterns come from `-e` operands, then each `-f` file in
                        // argument order (upstream #314, #327).
                        let mut patterns = options.patterns.clone();
                        let mut pattern_error = false;
                        for pattern_file in &options.pattern_files {
                            match self.read_pattern_file(pattern_file) {
                                Ok(mut file_patterns) => patterns.append(&mut file_patterns),
                                Err(message) => {
                                    stderr.push_str(&message);
                                    pattern_error = true;
                                }
                            }
                        }
                        if pattern_error {
                            exit_code = 2;
                        } else {
                            match self.run_grep(
                                &options,
                                &patterns,
                                stdin,
                                &mut stdout,
                                &mut stderr,
                            ) {
                                Ok(true) => exit_code = 0,
                                Ok(false) => exit_code = 1,
                                Err(()) => exit_code = 2,
                            }
                        }
                    }
                }
            }
            "wc" => {
                let mut flag_l = false;
                let mut flag_w = false;
                let mut flag_c = false;
                let mut file_start = 0usize;
                for (i, arg) in args.iter().enumerate() {
                    if arg.starts_with('-') && !arg.starts_with("--") {
                        for ch in arg.chars().skip(1) {
                            match ch {
                                'l' => flag_l = true,
                                'w' => flag_w = true,
                                'c' => flag_c = true,
                                _ => {}
                            }
                        }
                        file_start = i + 1;
                    } else {
                        file_start = i;
                        break;
                    }
                }
                // No flags means print all three columns
                if !flag_l && !flag_w && !flag_c {
                    flag_l = true;
                    flag_w = true;
                    flag_c = true;
                }
                let input = if file_start >= args.len() {
                    stdin.to_string()
                } else {
                    let mut combined = String::new();
                    for arg in &args[file_start..] {
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
                    let mut parts = Vec::new();
                    if flag_l {
                        parts.push(
                            input
                                .bytes()
                                .filter(|byte| *byte == b'\n')
                                .count()
                                .to_string(),
                        );
                    }
                    if flag_w {
                        parts.push(input.split_whitespace().count().to_string());
                    }
                    if flag_c {
                        parts.push(input.len().to_string());
                    }
                    stdout.push_str(&parts.join(" "));
                    stdout.push('\n');
                }
            }
            "head" => {
                let (line_count, file_args) = parse_line_count_args(args);
                match self.read_inputs_for_text_tool(file_args, stdin, "head", &mut stderr) {
                    Ok(input) => {
                        for line in input.lines().take(line_count) {
                            stdout.push_str(line);
                            stdout.push('\n');
                        }
                    }
                    Err(()) => exit_code = 1,
                }
            }
            "tail" => {
                let (line_count, file_args) = parse_line_count_args(args);
                match self.read_inputs_for_text_tool(file_args, stdin, "tail", &mut stderr) {
                    Ok(input) => {
                        let lines: Vec<&str> = input.lines().collect();
                        let start = lines.len().saturating_sub(line_count);
                        for line in &lines[start..] {
                            stdout.push_str(line);
                            stdout.push('\n');
                        }
                    }
                    Err(()) => exit_code = 1,
                }
            }
            "sort" => match self.read_inputs_for_text_tool(args, stdin, "sort", &mut stderr) {
                Ok(input) => {
                    let mut lines: Vec<&str> = input.lines().collect();
                    lines.sort_unstable();
                    for line in lines {
                        stdout.push_str(line);
                        stdout.push('\n');
                    }
                }
                Err(()) => exit_code = 1,
            },
            "uniq" => match self.read_inputs_for_text_tool(args, stdin, "uniq", &mut stderr) {
                Ok(input) => {
                    let mut last: Option<&str> = None;
                    for line in input.lines() {
                        if last != Some(line) {
                            stdout.push_str(line);
                            stdout.push('\n');
                            last = Some(line);
                        }
                    }
                }
                Err(()) => exit_code = 1,
            },
            "cut" => {
                let mut delimiter = ',';
                let mut field_index: Option<usize> = None;
                let mut file_start = 0usize;
                let mut index = 0usize;
                while index < args.len() {
                    match args[index].as_str() {
                        "-d" => {
                            let Some(value) = args.get(index + 1) else {
                                stderr.push_str("cut: usage: cut -d DELIM -f FIELD [FILE...]\n");
                                exit_code = 1;
                                break;
                            };
                            delimiter = value.chars().next().unwrap_or(',');
                            index += 2;
                            file_start = index;
                        }
                        "-f" => {
                            let Some(value) = args.get(index + 1) else {
                                stderr.push_str("cut: usage: cut -d DELIM -f FIELD [FILE...]\n");
                                exit_code = 1;
                                break;
                            };
                            field_index =
                                value.parse::<usize>().ok().and_then(|v| v.checked_sub(1));
                            index += 2;
                            file_start = index;
                        }
                        _ => {
                            file_start = index;
                            break;
                        }
                    }
                }
                if exit_code == 0 {
                    if let Some(field_index) = field_index {
                        match self.read_inputs_for_text_tool(
                            &args[file_start..],
                            stdin,
                            "cut",
                            &mut stderr,
                        ) {
                            Ok(input) => {
                                for line in input.lines() {
                                    let fields: Vec<&str> = line.split(delimiter).collect();
                                    if let Some(field) = fields.get(field_index) {
                                        stdout.push_str(field);
                                    }
                                    stdout.push('\n');
                                }
                            }
                            Err(()) => exit_code = 1,
                        }
                    } else {
                        stderr.push_str("cut: usage: cut -d DELIM -f FIELD [FILE...]\n");
                        exit_code = 1;
                    }
                }
            }
            "tr" => {
                if args.len() < 2 {
                    stderr.push_str("tr: usage: tr SET1 SET2\n");
                    exit_code = 1;
                } else {
                    let set1: Vec<char> = args[0].chars().collect();
                    let set2: Vec<char> = args[1].chars().collect();
                    if set1.is_empty() || set2.is_empty() {
                        stderr.push_str("tr: SET1 and SET2 must be non-empty\n");
                        exit_code = 1;
                    } else {
                        for ch in stdin.chars() {
                            if let Some(pos) = set1.iter().position(|candidate| *candidate == ch) {
                                let mapped = set2
                                    .get(pos)
                                    .copied()
                                    .unwrap_or(*set2.last().expect("set2 is non-empty"));
                                stdout.push(mapped);
                            } else {
                                stdout.push(ch);
                            }
                        }
                    }
                }
            }
            "basename" => {
                if let Some(path) = args.first() {
                    let normalized = normalize_absolute(path);
                    let name = normalized.rsplit('/').next().unwrap_or("");
                    stdout.push_str(name);
                    stdout.push('\n');
                } else {
                    stderr.push_str("basename: usage: basename PATH\n");
                    exit_code = 1;
                }
            }
            "dirname" => {
                if let Some(path) = args.first() {
                    let normalized = self.resolve_path(path);
                    stdout.push_str(&parent_dir(&normalized));
                    stdout.push('\n');
                } else {
                    stderr.push_str("dirname: usage: dirname PATH\n");
                    exit_code = 1;
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
                    } else {
                        // `export NAME` marks an existing (or empty) variable.
                        self.env.entry(assignment.clone()).or_default();
                    }
                }
            }
            "unset" => {
                let mut unset_vars = true;
                for arg in args {
                    match arg.as_str() {
                        "-v" => unset_vars = true,
                        "-f" => unset_vars = false,
                        "-fv" | "-vf" => unset_vars = true,
                        other => {
                            if !unset_vars {
                                continue;
                            }
                            self.env.remove(other);
                            // Positional parameters are unset via `set --`.
                            if other.chars().all(|ch| ch.is_ascii_digit()) {
                                self.env.insert("#".to_string(), "0".to_string());
                            }
                        }
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
            "mv" => {
                if args.len() != 2 {
                    stderr.push_str("mv: usage: mv SOURCE DEST\n");
                    exit_code = 1;
                } else {
                    let source = self.resolve_path(&args[0]);
                    let dest = self.resolve_path(&args[1]);
                    if self.fs.is_file(&source) {
                        match self.fs.read_file(&source).map(str::to_string) {
                            Ok(contents) => {
                                if let Err(error) = self.fs.write_file(&dest, &contents) {
                                    stderr.push_str(&format!("mv: {error}\n"));
                                    exit_code = 1;
                                } else if let Err(error) = self.fs.remove(&source, false) {
                                    stderr.push_str(&format!("mv: {error}\n"));
                                    exit_code = 1;
                                }
                            }
                            Err(error) => {
                                stderr.push_str(&format!("mv: {error}\n"));
                                exit_code = 1;
                            }
                        }
                    } else if self.fs.is_dir(&source) {
                        // Move directory: copy all entries under dest
                        match self.fs.entries_under(&source) {
                            Ok(entries) => {
                                let source_prefix = if source.ends_with('/') {
                                    source.clone()
                                } else {
                                    format!("{source}/")
                                };
                                let dest_norm = dest.clone();
                                for (path, contents) in entries {
                                    let rel = path.strip_prefix(&source_prefix).unwrap_or(&path);
                                    let new_path = if rel.is_empty() {
                                        dest_norm.clone()
                                    } else {
                                        format!("{dest_norm}/{rel}")
                                    };
                                    match contents {
                                        None => {
                                            if let Err(error) = self.fs.create_dir_all(&new_path) {
                                                stderr.push_str(&format!("mv: {error}\n"));
                                                exit_code = 1;
                                            }
                                        }
                                        Some(c) => {
                                            if let Err(error) = self.fs.write_file(&new_path, &c) {
                                                stderr.push_str(&format!("mv: {error}\n"));
                                                exit_code = 1;
                                            }
                                        }
                                    }
                                }
                                if exit_code == 0 {
                                    if let Err(error) = self.fs.remove(&source, true) {
                                        stderr.push_str(&format!("mv: {error}\n"));
                                        exit_code = 1;
                                    }
                                }
                            }
                            Err(error) => {
                                stderr.push_str(&format!("mv: {error}\n"));
                                exit_code = 1;
                            }
                        }
                    } else {
                        stderr.push_str(&format!("mv: {}: No such file or directory\n", args[0]));
                        exit_code = 1;
                    }
                }
            }
            "read" => {
                // read [-r] VAR [VAR2 ...]
                let mut raw_mode = false;
                let mut var_args = args;
                if var_args.first().map(String::as_str) == Some("-r") {
                    raw_mode = true;
                    var_args = &var_args[1..];
                }
                let line = stdin.lines().next().unwrap_or("").to_string();
                let unescaped = if raw_mode {
                    line.clone()
                } else {
                    // interpret backslash-newline continuation (simple form)
                    line.replace("\\\n", "")
                };
                if var_args.is_empty() {
                    // No variable name: store in REPLY
                    self.env.insert("REPLY".to_string(), unescaped);
                } else if var_args.len() == 1 {
                    self.env.insert(var_args[0].clone(), unescaped);
                } else {
                    // Split on IFS (default: space/tab/newline)
                    let ifs = self.env.get("IFS").map(String::as_str).unwrap_or(" \t\n");
                    let mut parts: Vec<&str> = unescaped
                        .splitn(var_args.len(), |c| ifs.contains(c))
                        .collect();
                    // Pad with empty strings if fewer parts than variables
                    while parts.len() < var_args.len() {
                        parts.push("");
                    }
                    for (var, val) in var_args.iter().zip(parts.iter()) {
                        self.env.insert(var.clone(), val.to_string());
                    }
                }
            }
            "set" => {
                let mut i = 0;
                while i < args.len() {
                    let arg = args[i].as_str();
                    let arg_chars: Vec<char> = arg.chars().collect();
                    let enable = match arg_chars.first() {
                        Some('-') => true,
                        Some('+') => false,
                        _ => {
                            // First non-option word: everything from here on is a
                            // positional parameter.
                            break;
                        }
                    };
                    if arg == "--" {
                        i += 1;
                        break;
                    }
                    if arg.len() == 1 || arg_chars.get(1) == Some(&arg_chars[0]) {
                        // A lone `-`/`+` or `--`/`++`: handled above, otherwise stop.
                        i += 1;
                        continue;
                    }
                    if arg_chars.get(1) == Some(&'-') || arg_chars.get(1) == Some(&'+') {
                        i += 1;
                        continue;
                    }

                    // A `-o`/`+o` inside a cluster consumes the next word as an
                    // option name, unless that word is itself an option
                    // (upstream #c9904dea). `consumed` counts words already taken
                    // by earlier `o`s in the same cluster.
                    let mut consumed = 0usize;
                    for ch in arg_chars.iter().skip(1) {
                        match ch {
                            'e' => self.opt_errexit = enable,
                            'u' => self.opt_nounset = enable,
                            'o' => {
                                let candidate = args.get(i + 1 + consumed).map(String::as_str);
                                match candidate {
                                    Some(name)
                                        if !name.starts_with('-') && !name.starts_with('+') =>
                                    {
                                        consumed += 1;
                                        if let Err(message) = self.set_long_option(name, enable) {
                                            stderr.push_str(&message);
                                            exit_code = 1;
                                        }
                                    }
                                    // No option name follows: bash prints the
                                    // option listing rather than failing.
                                    _ => {
                                        stdout.push_str(&self.format_options(!enable));
                                    }
                                }
                            }
                            other => {
                                stderr.push_str(&format!(
                                    "set: -{other}: invalid option\nset: usage: set [-e] [-u] [-o option]\n"
                                ));
                                exit_code = 1;
                            }
                        }
                    }
                    i += 1 + consumed;
                }

                // Remaining words become the positional parameters.
                let positional: Vec<&String> = args[i..].iter().collect();
                if !positional.is_empty() {
                    let joined = positional
                        .iter()
                        .map(|value| value.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.env.insert("@".to_string(), joined);
                    for (index, value) in positional.iter().enumerate() {
                        self.env.insert((index + 1).to_string(), (*value).clone());
                    }
                    self.env
                        .insert("#".to_string(), positional.len().to_string());
                } else if i > 0 && args.last().map(|arg| arg == "--").unwrap_or(false) {
                    // `set --` with no trailing words clears the positionals.
                    self.env.remove("@");
                    self.env.insert("#".to_string(), "0".to_string());
                }
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

    fn read_inputs_for_text_tool(
        &self,
        args: &[String],
        stdin: &str,
        cmd: &str,
        stderr: &mut String,
    ) -> Result<String, ()> {
        if args.is_empty() {
            return Ok(stdin.to_string());
        }
        let mut combined = String::new();
        let mut had_error = false;
        for arg in args {
            match self.fs.read_file(&self.resolve_path(arg)) {
                Ok(contents) => combined.push_str(contents),
                Err(error) => {
                    stderr.push_str(&format!("{cmd}: {error}\n"));
                    had_error = true;
                }
            }
        }
        if had_error { Err(()) } else { Ok(combined) }
    }

    fn run_test(&self, args: &[String]) -> bool {
        if args.is_empty() {
            return false;
        }

        // Remove trailing ] if present (for [ ... ] syntax)
        let args = if args.last().map(|s| s.as_str()) == Some("]") {
            &args[..args.len() - 1]
        } else {
            args
        };

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
                // Quoted and backslash-escaped text is copied verbatim: neither
                // `$` nor parameter operators are interpreted (upstream #347).
                WordPartKind::Literal | WordPartKind::Escaped => {
                    expanded.push_str(&part.text);
                }
                WordPartKind::Variable => {
                    expanded.push_str(&self.expand_text(&part.text));
                }
                WordPartKind::CommandSub => {
                    let result = self.exec_command_substitution(&part.text);
                    expanded.push_str(&result);
                }
                // `$((…))` is evaluated arithmetically; a malformed expression
                // is reported rather than silently becoming empty.
                WordPartKind::Arithmetic => match eval_arithmetic_expr(&part.text, &self.env) {
                    Ok(value) => expanded.push_str(&value.to_string()),
                    Err(()) => {
                        self.param_error =
                            Some(format!("bash: {}: syntax error in expression\n", part.text));
                    }
                },
            }
        }
        self.output_bytes_used += expanded.len();
        expanded
    }

    /// Evaluate the contents of a `${…}` expansion, including the POSIX
    /// parameter operators `${p-word}`, `${p:-word}`, `${p=word}`, `${p:=word}`,
    /// `${p?word}`, `${p:?word}`, `${p+word}` and `${p:+word}`.
    ///
    /// Operators never trigger `set -u`: an unset parameter is exactly the case
    /// they exist to handle (upstream #4af0dc0f).
    fn expand_braced(&mut self, content: &str) -> String {
        let (parameter, rest) = split_parameter(content);

        // `${#name}` is the length of `name`, not a parameter called `#name`.
        if parameter == "#" && !rest.is_empty() {
            let (inner, inner_rest) = split_parameter(rest);
            if inner_rest.is_empty() && !inner.is_empty() {
                return self.expand_length(inner);
            }
        }

        let is_set = self.parameter_is_set(parameter);
        let value = self.parameter_value(parameter);

        let (colon, operator, word) = match rest.chars().next() {
            Some(':') => match rest.chars().nth(1) {
                Some(op @ ('-' | '=' | '?' | '+')) => (true, op, &rest[2..]),
                _ => (false, '\0', rest),
            },
            Some(op @ ('-' | '=' | '?' | '+')) => (false, op, &rest[1..]),
            _ => (false, '\0', rest),
        };

        // A bare `${name}` (or `${?}`, `${#}`, …): plain value, nounset applies.
        if operator == '\0' {
            match content {
                "?" => return self.last_exit_code.to_string(),
                "#" => return self.positional_count().to_string(),
                "@" | "*" => {
                    return self.env.get("@").cloned().unwrap_or_default();
                }
                "0" => return "bash".to_string(),
                _ => {}
            }
            if let Some(value) = value {
                return value;
            }
            if self.opt_nounset {
                self.nounset_error = Some(parameter.to_string());
            }
            return String::new();
        }

        // `word` is expanded lazily, but every operator that can use it needs
        // the expansion, and command substitutions inside it must still run.
        let examine = if colon {
            // `:` variants treat an empty value as unset.
            !is_set || value.as_deref() == Some("")
        } else {
            !is_set
        };

        match operator {
            // Use the default word when the parameter is unset/empty.
            '-' => {
                if examine {
                    self.expand_operator_word(word)
                } else {
                    value.unwrap_or_default()
                }
            }
            // Assign the default word to the parameter, then use it.
            '=' => {
                if examine {
                    assert_default_assignment_target(parameter)
                        .map_err(|message| self.param_error = Some(message))
                        .ok();
                    let assigned = self.expand_operator_word(word);
                    if self.param_error.is_none() {
                        self.assign_parameter(parameter, &assigned);
                    }
                    assigned
                } else {
                    value.unwrap_or_default()
                }
            }
            // Report an error when the parameter is unset/empty.
            '?' => {
                if examine {
                    let message = self.expand_operator_word(word);
                    let message = if message.is_empty() {
                        format!("bash: {parameter}: parameter null or not set\n")
                    } else {
                        format!("bash: {message}\n")
                    };
                    self.param_error = Some(message);
                    String::new()
                } else {
                    value.unwrap_or_default()
                }
            }
            // Use the alternative word when the parameter IS set/non-empty.
            '+' => {
                if examine {
                    String::new()
                } else {
                    self.expand_operator_word(word)
                }
            }
            _ => unreachable!("operator is one of - = ? +"),
        }
    }

    /// `${#name}` — character length, honoring `set -u` like a bare `${name}`.
    fn expand_length(&mut self, parameter: &str) -> String {
        match self.parameter_value(parameter) {
            Some(value) => value.chars().count().to_string(),
            None => {
                if self.opt_nounset {
                    self.nounset_error = Some(parameter.to_string());
                }
                "0".to_string()
            }
        }
    }

    /// Expand the `word` side of a parameter operator.
    ///
    /// The word may itself contain `$VAR`, `${NESTED:-x}` and `$(…)`, so it is
    /// recursively expanded through [`Bash::expand_text`].
    fn expand_operator_word(&mut self, word: &str) -> String {
        self.expand_text(word)
    }

    /// Whether `parameter` currently has a value (`${p-word}` semantics).
    fn parameter_is_set(&self, parameter: &str) -> bool {
        match parameter {
            "?" => true,
            "#" => true,
            "0" => true,
            "@" | "*" => true,
            _ => self.env.contains_key(parameter),
        }
    }

    fn parameter_value(&self, parameter: &str) -> Option<String> {
        match parameter {
            "?" => Some(self.last_exit_code.to_string()),
            "#" => Some(self.positional_count().to_string()),
            "0" => Some("bash".to_string()),
            "@" | "*" => Some(self.env.get("@").cloned().unwrap_or_default()),
            _ => self.env.get(parameter).cloned(),
        }
    }

    fn assign_parameter(&mut self, parameter: &str, value: &str) {
        self.env.insert(parameter.to_string(), value.to_string());
    }

    /// Number of positional parameters, tracked under the `#` key by `set --`.
    fn positional_count(&self) -> usize {
        self.env
            .get("#")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
    }

    /// Apply a `set -o NAME` / `set +o NAME` long option.
    fn set_long_option(&mut self, name: &str, enable: bool) -> Result<(), String> {
        match name {
            "errexit" => self.opt_errexit = enable,
            "nounset" => self.opt_nounset = enable,
            "pipefail" => self.opt_pipefail = enable,
            other => {
                return Err(format!(
                    "bash: set: {other}: invalid option name\nset: usage: set [-e] [-u] [-o option]\n"
                ));
            }
        }
        Ok(())
    }

    /// Render the shell option listing for a bare `-o` / `+o`.
    fn format_options(&self, reset_form: bool) -> String {
        let options = [
            ("errexit", self.opt_errexit),
            ("nounset", self.opt_nounset),
            ("pipefail", self.opt_pipefail),
        ];
        let mut output = String::new();
        for (name, enabled) in options {
            if reset_form {
                output.push_str(&format!(
                    "set {}o {name}\n",
                    if enabled { "-" } else { "+" }
                ));
            } else {
                output.push_str(&format!(
                    "{:<16}{}\n",
                    name,
                    if enabled { "on" } else { "off" }
                ));
            }
        }
        output
    }

    fn exec_command_substitution(&mut self, script: &str) -> String {
        let mut child = self.clone();
        match child.exec_in_current_state(script) {
            result => {
                self.fs = child.fs;
                self.env = child.env;
                self.cwd = child.cwd;
                if !result.stderr.is_empty() {
                    self.expansion_stderr.push_str(&result.stderr);
                }
                // A substitution that failed to parse cannot be recovered from:
                // the enclosing command must fail closed (upstream #348).
                if result.exit_code == 2 {
                    self.expansion_failed = true;
                }
                result.stdout.trim_end_matches('\n').to_string()
            }
        }
    }

    fn expand_text(&mut self, text: &str) -> String {
        let mut expanded = String::new();
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch != '$' {
                expanded.push(ch);
                continue;
            }

            if chars.next_if_eq(&'{').is_some() {
                let mut name = String::new();
                let mut depth = 1usize;
                for next in chars.by_ref() {
                    if next == '{' {
                        depth += 1;
                    } else if next == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    name.push(next);
                }
                expanded.push_str(&self.expand_braced(&name));
                continue;
            }

            // Check for special single-character variables first ($?, $#, etc)
            if let Some(&next) = chars.peek() {
                if next == '?' {
                    chars.next();
                    expanded.push_str(&self.last_exit_code.to_string());
                    continue;
                }
                if next == '#' {
                    chars.next();
                    expanded.push_str(&self.positional_count().to_string());
                    continue;
                }
                if next == '@' || next == '*' {
                    chars.next();
                    if let Some(val) = self.env.get("@") {
                        expanded.push_str(val);
                    }
                    continue;
                }
                if next.is_ascii_digit() {
                    let digit = next.to_string();
                    chars.next();
                    if digit == "0" {
                        expanded.push_str("bash"); // $0 is the script name
                    } else {
                        expanded.push_str(self.env.get(&digit).map(String::as_str).unwrap_or(""));
                    }
                    continue;
                }
            }

            // Otherwise, extract alphanumeric variable name
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
            } else if let Some(val) = self.env.get(&name) {
                expanded.push_str(val);
            } else if self.opt_nounset {
                self.nounset_error = Some(name.clone());
            }
        }

        expanded
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

fn parse_line_count_args(args: &[String]) -> (usize, &[String]) {
    if args.len() >= 2 && args[0] == "-n" {
        let count = args[1].parse::<usize>().ok().unwrap_or(10);
        (count, &args[2..])
    } else {
        (10, args)
    }
}

fn eval_arithmetic_expr(expr: &str, env: &BTreeMap<String, String>) -> Result<i64, ()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tok<'a> {
        Num(i64),
        Ident(&'a str),
        Plus,
        Minus,
        Mul,
        Div,
        Mod,
        LParen,
        RParen,
    }

    fn precedence(tok: Tok<'_>) -> i32 {
        match tok {
            Tok::Plus | Tok::Minus => 1,
            Tok::Mul | Tok::Div | Tok::Mod => 2,
            _ => 0,
        }
    }

    fn apply_op(values: &mut Vec<i64>, op: Tok<'_>) -> Result<(), ()> {
        let rhs = values.pop().ok_or(())?;
        let lhs = values.pop().ok_or(())?;
        let value = match op {
            Tok::Plus => lhs + rhs,
            Tok::Minus => lhs - rhs,
            Tok::Mul => lhs * rhs,
            Tok::Div => {
                if rhs == 0 {
                    return Err(());
                }
                lhs / rhs
            }
            Tok::Mod => {
                if rhs == 0 {
                    return Err(());
                }
                lhs % rhs
            }
            _ => return Err(()),
        };
        values.push(value);
        Ok(())
    }

    let mut tokens = Vec::new();
    let mut index = 0usize;
    let bytes = expr.as_bytes();
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if ch.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if ch.is_ascii_digit() {
            let start = index;
            while index < bytes.len() && (bytes[index] as char).is_ascii_digit() {
                index += 1;
            }
            let value = expr[start..index].parse::<i64>().map_err(|_| ())?;
            tokens.push(Tok::Num(value));
            continue;
        }
        if ch == '_' || ch.is_ascii_alphabetic() {
            let start = index;
            while index < bytes.len() {
                let c = bytes[index] as char;
                if c == '_' || c.is_ascii_alphanumeric() {
                    index += 1;
                } else {
                    break;
                }
            }
            tokens.push(Tok::Ident(&expr[start..index]));
            continue;
        }
        let tok = match ch {
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '*' => Tok::Mul,
            '/' => Tok::Div,
            '%' => Tok::Mod,
            '(' => Tok::LParen,
            ')' => Tok::RParen,
            _ => return Err(()),
        };
        tokens.push(tok);
        index += 1;
    }

    let mut values: Vec<i64> = Vec::new();
    let mut ops: Vec<Tok<'_>> = Vec::new();

    for token in tokens {
        match token {
            Tok::Num(value) => values.push(value),
            Tok::Ident(name) => {
                let value = env
                    .get(name)
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .unwrap_or(0);
                values.push(value);
            }
            Tok::LParen => ops.push(token),
            Tok::RParen => {
                while let Some(op) = ops.pop() {
                    if op == Tok::LParen {
                        break;
                    }
                    apply_op(&mut values, op)?;
                }
            }
            Tok::Plus | Tok::Minus | Tok::Mul | Tok::Div | Tok::Mod => {
                while let Some(&op) = ops.last() {
                    if op == Tok::LParen || precedence(op) < precedence(token) {
                        break;
                    }
                    let op = ops.pop().ok_or(())?;
                    apply_op(&mut values, op)?;
                }
                ops.push(token);
            }
        }
    }

    while let Some(op) = ops.pop() {
        if op == Tok::LParen {
            return Err(());
        }
        apply_op(&mut values, op)?;
    }
    if values.len() == 1 {
        Ok(values[0])
    } else {
        Err(())
    }
}

/// Split the inside of `${…}` into its parameter name and the remainder (which
/// begins with an operator such as `:-` when one is present).
fn split_parameter(content: &str) -> (&str, &str) {
    let mut chars = content.char_indices();
    match chars.next() {
        Some((_, ch)) if ch == '_' || ch.is_ascii_alphabetic() => {
            let mut end = 1;
            for (index, next) in chars {
                if next == '_' || next.is_ascii_alphanumeric() {
                    end = index + next.len_utf8();
                } else {
                    break;
                }
            }
            (&content[..end], &content[end..])
        }
        Some((_, ch)) if ch.is_ascii_digit() => {
            let mut end = 1;
            for (index, next) in chars {
                if next.is_ascii_digit() {
                    end = index + 1;
                } else {
                    break;
                }
            }
            (&content[..end], &content[end..])
        }
        Some((_, ch)) if matches!(ch, '@' | '*' | '#' | '?' | '$' | '!' | '-') => {
            let end = ch.len_utf8();
            (&content[..end], &content[end..])
        }
        _ => ("", content),
    }
}

/// `${1:=x}` / `${@:=x}` and friends are rejected by bash: only named
/// parameters can be assigned this way (upstream #701f8e5b).
fn assert_default_assignment_target(parameter: &str) -> Result<(), String> {
    let positional = !parameter.is_empty() && parameter.chars().all(|ch| ch.is_ascii_digit());
    let special = matches!(parameter, "@" | "*" | "#" | "?" | "$" | "!" | "-");
    if positional || special {
        return Err(format!("bash: ${parameter}: cannot assign in this way\n"));
    }
    Ok(())
}

/// Parsed `grep` invocation.
///
/// This runtime has no regex engine, so every pattern is a literal substring
/// needle OR-combined with the others; the regex-only upstream behaviors
/// (BRE/ERE alternation, `-w`/`-x` anchoring by wrapping, `-P`) are expressed
/// directly against the matched line instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GrepOptions {
    /// Patterns given with `-e` (repeatable) or as the first operand.
    patterns: Vec<String>,
    /// Pattern files given with `-f` (repeatable).
    pattern_files: Vec<String>,
    /// File operands, in order; `-` means standard input.
    files: Vec<String>,
    ignore_case: bool,
    invert: bool,
    count_only: bool,
    line_number: bool,
    files_with_matches: bool,
    files_without_match: bool,
    quiet: bool,
    whole_line: bool,
    word_match: bool,
    no_filename: bool,
}

impl GrepOptions {
    /// A single source needs no `file:` prefix.
    fn prefix_needed(&self, source_count: usize) -> bool {
        source_count > 1 && !self.no_filename && !self.quiet
    }
}

/// Parse grep arguments, honoring `--` as an option terminator.
fn parse_grep_args(args: &[String]) -> Result<GrepOptions, String> {
    let mut options = GrepOptions::default();
    let mut operands: Vec<String> = Vec::new();
    let mut options_ended = false;
    let mut saw_pattern_option = false;

    let take_value =
        |index: &mut usize, rest: &str, args: &[String], short: char| -> Result<String, String> {
            if !rest.is_empty() {
                return Ok(rest.to_string());
            }
            *index += 1;
            match args.get(*index) {
                Some(value) => Ok(value.clone()),
                None => Err(format!("grep: option requires an argument -- '{short}'\n")),
            }
        };

    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].clone();
        if options_ended || arg == "-" || !arg.starts_with('-') {
            operands.push(arg);
            index += 1;
            continue;
        }
        if arg == "--" {
            options_ended = true;
            index += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let need_value = |index: &mut usize| -> Result<String, String> {
                if let Some(value) = &inline {
                    return Ok(value.clone());
                }
                *index += 1;
                match args.get(*index) {
                    Some(value) => Ok(value.clone()),
                    None => Err(format!("grep: option '--{name}' requires an argument\n")),
                }
            };
            match name {
                "regexp" => {
                    options.patterns.push(need_value(&mut index)?);
                    saw_pattern_option = true;
                }
                "file" => {
                    options.pattern_files.push(need_value(&mut index)?);
                    saw_pattern_option = true;
                }
                "ignore-case" => options.ignore_case = true,
                "invert-match" => options.invert = true,
                "count" => options.count_only = true,
                "line-number" => options.line_number = true,
                "files-with-matches" => options.files_with_matches = true,
                "files-without-match" => options.files_without_match = true,
                "quiet" | "silent" => options.quiet = true,
                "line-regexp" => options.whole_line = true,
                "word-regexp" => options.word_match = true,
                "no-filename" => options.no_filename = true,
                "fixed-strings" => {}
                other => {
                    return Err(format!("grep: unrecognized option '--{other}'\n"));
                }
            }
            index += 1;
            continue;
        }

        let chars: Vec<char> = arg.chars().skip(1).collect();
        let mut position = 0usize;
        while position < chars.len() {
            let flag = chars[position];
            let rest: String = chars[position + 1..].iter().collect();
            match flag {
                'e' => {
                    options
                        .patterns
                        .push(take_value(&mut index, &rest, args, 'e')?);
                    saw_pattern_option = true;
                    break;
                }
                'f' => {
                    options
                        .pattern_files
                        .push(take_value(&mut index, &rest, args, 'f')?);
                    saw_pattern_option = true;
                    break;
                }
                'i' | 'y' => options.ignore_case = true,
                'v' => options.invert = true,
                'c' => options.count_only = true,
                'n' => options.line_number = true,
                'l' => options.files_with_matches = true,
                'L' => options.files_without_match = true,
                'q' => options.quiet = true,
                'x' => options.whole_line = true,
                'w' => options.word_match = true,
                'h' => options.no_filename = true,
                // Literal matching is this engine's native mode.
                'F' => {}
                other => return Err(format!("grep: invalid option -- '{other}'\n")),
            }
            position += 1;
        }
        index += 1;
    }

    // A positional operand supplies the pattern only when no -e/-f was given
    // (upstream #314).
    if !saw_pattern_option && let Some(first) = operands.first().cloned() {
        options.patterns.push(first);
        operands.remove(0);
    }
    options.files = operands;
    Ok(options)
}

impl Bash {
    /// Read a `-f` pattern file: newline-separated, with a single trailing
    /// newline acting as a terminator rather than an empty pattern.
    fn read_pattern_file(&self, path: &str) -> Result<Vec<String>, String> {
        if path == "-" {
            // `-f -` reads patterns from standard input; this runtime has no
            // separate pattern input, so it is treated as empty.
            return Ok(Vec::new());
        }
        let resolved = self.resolve_path(path);
        let contents = self
            .fs
            .read_file(&resolved)
            .map_err(|_| format!("grep: {path}: No such file or directory\n"))?;
        Ok(split_pattern_file(contents))
    }

    /// Search every input source and write the selected output.
    ///
    /// Returns `Ok(true)` when at least one line was selected. A read error
    /// yields `Err(())`, which maps to grep's exit status 2 (upstream #332).
    fn run_grep(
        &mut self,
        options: &GrepOptions,
        patterns: &[String],
        stdin: &str,
        stdout: &mut String,
        stderr: &mut String,
    ) -> Result<bool, ()> {
        // `-f emptyfile` selects nothing and short-circuits before any input is
        // opened, except under -v which selects everything.
        let patterns_empty = patterns.is_empty();
        if patterns_empty && !options.invert {
            return Ok(false);
        }

        // Resolve input sources. No file operands means standard input.
        let mut sources: Vec<(String, String)> = Vec::new();
        if options.files.is_empty() {
            sources.push(("(standard input)".to_string(), stdin.to_string()));
        } else {
            let mut stdin_used = false;
            for operand in &options.files {
                if operand == "-" {
                    // Only the first `-` sees the stream; later ones see EOF.
                    let contents = if stdin_used {
                        String::new()
                    } else {
                        stdin_used = true;
                        stdin.to_string()
                    };
                    sources.push(("(standard input)".to_string(), contents));
                    continue;
                }
                match self.fs.read_file(&self.resolve_path(operand)) {
                    Ok(contents) => sources.push((operand.clone(), contents.to_string())),
                    Err(_) => {
                        stderr.push_str(&format!("grep: {operand}: No such file or directory\n"));
                    }
                }
            }
        }

        let had_error = !stderr.is_empty();
        let prefix = options.prefix_needed(sources.len());
        let mut selected_any = false;

        for (name, contents) in &sources {
            let mut matched_lines: Vec<(usize, &str)> = Vec::new();
            for (number, line) in contents.lines().enumerate() {
                if line_matches(line, patterns, options) {
                    matched_lines.push((number + 1, line));
                }
            }
            let matched = !matched_lines.is_empty();
            selected_any |= matched;

            if options.quiet {
                if matched {
                    return Ok(true);
                }
                continue;
            }
            if options.files_with_matches {
                if matched {
                    stdout.push_str(name);
                    stdout.push('\n');
                }
                continue;
            }
            if options.files_without_match {
                if !matched {
                    stdout.push_str(name);
                    stdout.push('\n');
                }
                continue;
            }
            if options.count_only {
                if prefix {
                    stdout.push_str(name);
                    stdout.push(':');
                }
                stdout.push_str(&matched_lines.len().to_string());
                stdout.push('\n');
                continue;
            }
            for (number, line) in matched_lines {
                if prefix {
                    stdout.push_str(name);
                    stdout.push(':');
                }
                if options.line_number {
                    stdout.push_str(&format!("{number}:"));
                }
                stdout.push_str(line);
                stdout.push('\n');
            }
        }

        if had_error {
            // An unreadable operand outranks "nothing matched".
            return Err(());
        }
        // `-L` prints names but its status still reflects whether a line was
        // selected (upstream #332).
        Ok(selected_any)
    }
}

/// Split a pattern file into patterns: interior blank lines are patterns, and
/// only a *single* trailing newline is treated as a terminator.
fn split_pattern_file(contents: &str) -> Vec<String> {
    if contents.is_empty() {
        return Vec::new();
    }
    let mut patterns: Vec<String> = contents.split('\n').map(str::to_string).collect();
    if patterns.last().is_some_and(|last| last.is_empty()) {
        patterns.pop();
    }
    patterns
}

/// Whether `line` is selected by any needle, honoring `-v`, `-x` and `-w`.
fn line_matches(line: &str, patterns: &[String], options: &GrepOptions) -> bool {
    let haystack = if options.ignore_case {
        line.to_lowercase()
    } else {
        line.to_string()
    };
    let matched = patterns.iter().any(|pattern| {
        let needle = if options.ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };
        if options.whole_line {
            haystack == needle
        } else if options.word_match {
            contains_word(&haystack, &needle)
        } else {
            haystack.contains(&needle)
        }
    });
    if options.invert { !matched } else { matched }
}

/// Substring match that additionally requires non-word characters (or the
/// string edges) on both sides.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .map(|ch| !is_word_char(ch))
            .unwrap_or(true);
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map(|ch| !is_word_char(ch))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len().max(1);
        if from > haystack.len() {
            break;
        }
    }
    false
}

fn is_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

/// GNU `cat` display options (upstream #6130334f).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CatOptions {
    number_all: bool,
    number_nonblank: bool,
    show_ends: bool,
    show_tabs: bool,
    show_nonprinting: bool,
    squeeze_blank: bool,
}

impl CatOptions {
    /// Whether the output is a byte-for-byte copy of the input.
    fn is_plain(&self) -> bool {
        !self.number_all
            && !self.number_nonblank
            && !self.show_ends
            && !self.show_tabs
            && !self.show_nonprinting
            && !self.squeeze_blank
    }

    /// Resolve the `-A`/`-e`/`-t` aliases into their `-vET` components.
    fn resolve_aliases(mut self) -> Self {
        if self.show_ends && self.show_tabs && self.show_nonprinting {
            // Nothing to do; already fully expanded.
        }
        self.number_all &= !self.number_nonblank;
        self
    }
}

/// Parse GNU-style `cat` flags. Returns the options and the file operands.
fn parse_cat_options(args: &[String]) -> Result<(CatOptions, Vec<String>), String> {
    let mut options = CatOptions::default();
    let mut all = false;
    let mut ends = false;
    let mut tabs = false;
    let mut nonprinting = false;
    let mut operands = Vec::new();
    let mut options_ended = false;

    let apply_short = |ch: char, options: &mut CatOptions| -> Result<(), String> {
        match ch {
            'n' => options.number_all = true,
            'b' => options.number_nonblank = true,
            'E' => options.show_ends = true,
            'T' => options.show_tabs = true,
            'v' => options.show_nonprinting = true,
            's' => options.squeeze_blank = true,
            // Accepted by GNU cat but has no effect on a buffered runtime.
            'u' => {}
            other => return Err(format!("cat: invalid option -- '{other}'\n")),
        }
        Ok(())
    };

    for arg in args {
        if options_ended || arg == "-" || !arg.starts_with('-') {
            operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "number" => options.number_all = true,
                "number-nonblank" => options.number_nonblank = true,
                "show-ends" => options.show_ends = true,
                "show-tabs" => options.show_tabs = true,
                "show-nonprinting" => options.show_nonprinting = true,
                "show-all" => all = true,
                "squeeze-blank" => options.squeeze_blank = true,
                other => return Err(format!("cat: unrecognized option '--{other}'\n")),
            }
            continue;
        }
        for ch in arg.chars().skip(1) {
            match ch {
                'A' => all = true,
                'e' => {
                    ends = true;
                    nonprinting = true;
                }
                't' => {
                    tabs = true;
                    nonprinting = true;
                }
                other => apply_short(other, &mut options)?,
            }
        }
    }

    // `-A` is exactly `-vET`; `-e`/`-t` contribute their parts.
    options.show_ends |= ends || all;
    options.show_tabs |= tabs || all;
    options.show_nonprinting |= nonprinting || all;

    Ok((options.resolve_aliases(), operands))
}

/// Render the concatenated byte stream through the requested display options.
///
/// Numbering and `-s` state run across the whole stream (and across file
/// boundaries), matching GNU cat.
fn apply_cat_options(input: &[u8], options: &CatOptions) -> String {
    let mut output = String::new();
    let mut line_number = 0usize;
    let mut previous_blank = false;

    let mut segments: Vec<&[u8]> = input.split(|byte| *byte == b'\n').collect();
    let ends_with_newline = input.last() == Some(&b'\n');
    if ends_with_newline {
        // The trailing empty segment is the terminator of the last line, not a
        // line of its own.
        segments.pop();
    }

    for (index, segment) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        let unterminated = is_last && !ends_with_newline;
        let blank = segment.is_empty();

        if options.squeeze_blank && blank && previous_blank {
            // A squeezed line is dropped entirely and does not reset the
            // squeeze state.
            continue;
        }
        previous_blank = blank;

        let numbered = if options.number_nonblank {
            !blank
        } else {
            options.number_all
        };
        if numbered {
            line_number += 1;
            output.push_str(&format!("{line_number:>6}\t"));
        }

        if options.show_nonprinting || options.show_tabs {
            for byte in segment.iter() {
                push_cat_byte(&mut output, *byte, options);
            }
        } else {
            output.push_str(&String::from_utf8_lossy(segment));
        }

        if !unterminated {
            if options.show_ends {
                output.push_str("$\n");
            } else {
                output.push('\n');
            }
        }
    }

    output
}

/// Render one byte with cat's `-v`/`-T` notation.
fn push_cat_byte(output: &mut String, byte: u8, options: &CatOptions) {
    if byte == b'\t' {
        if options.show_tabs {
            output.push_str("^I");
        } else {
            output.push('\t');
        }
        return;
    }
    if !options.show_nonprinting {
        push_cat_plain(output, byte);
        return;
    }
    match byte {
        32..=126 => push_cat_plain(output, byte),
        127 => output.push_str("^?"),
        128..=255 => {
            let low = byte - 128;
            if low >= 32 {
                if low == 127 {
                    output.push_str("M-^?");
                } else {
                    output.push_str("M-");
                    push_cat_plain(output, low);
                }
            } else {
                output.push_str("M-^");
                output.push((low + 64) as char);
            }
        }
        _ => {
            output.push('^');
            output.push((byte + 64) as char);
        }
    }
}

fn push_cat_plain(output: &mut String, byte: u8) {
    output.push(byte as char);
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
        let mut bash = Bash::new(Options::default()).unwrap();
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
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
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
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("wat");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "bash: wat: command not found\n");
        assert_eq!(result.exit_code, 127);
    }

    #[test]
    fn expands_environment_variables() {
        let mut env = BTreeMap::new();
        env.insert("NAME".to_string(), "rust".to_string());
        let mut bash = Bash::new(Options {
            env,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("echo $NAME");
        assert_eq!(result.stdout, "rust\n");
    }

    #[test]
    fn pipes_stdout_into_next_command() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo alpha; echo beta | grep beta | wc");
        assert_eq!(result.stdout, "alpha\n1 1 5\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn supports_input_output_and_append_redirection() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/input.txt".to_string(), "one\ntwo\n".to_string());
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
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
        let mut bash = Bash::new(Options::default()).unwrap();
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
        let mut bash = Bash::new(Options {
            env,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("echo hello-$NAME-${SUFFIX}");
        assert_eq!(result.stdout, "hello-rust-core\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn rejects_writes_to_directories_and_missing_parents() {
        let mut bash = Bash::new(Options::default()).unwrap();
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
        let mut bash = Bash::new(Options::default()).unwrap();
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
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("cd /tmp | cat; pwd; exit 7 | true; echo after");
        assert_eq!(result.stdout, "/home/user\nafter\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.cwd, "/home/user");
    }

    #[test]
    fn wc_counts_newline_bytes_for_lines() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo -n x | wc");
        assert_eq!(result.stdout, "0 1 1\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn output_redirection_truncates_before_command_runs() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/f".to_string(), "old\n".to_string());
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
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
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("ls file.txt");
        assert_eq!(result.stdout, "file.txt\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn printf_formats_arguments() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("printf '%s\\n' a b");
        assert_eq!(result.stdout, "a\nb\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn supports_head_tail_sort_uniq() {
        let mut files = BTreeMap::new();
        files.insert(
            "/home/user/input.txt".to_string(),
            "b\na\na\nc\n".to_string(),
        );
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec(
            "head -n 2 input.txt; tail -n 2 input.txt; sort input.txt; sort input.txt | uniq",
        );
        assert_eq!(result.stdout, "b\na\na\nc\na\na\nb\nc\na\nb\nc\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn supports_cut_and_tr() {
        let mut files = BTreeMap::new();
        files.insert(
            "/home/user/csv.txt".to_string(),
            "name,lang\nalice,rust\nbob,go\n".to_string(),
        );
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("cut -d , -f 2 csv.txt | tr og OG");
        assert_eq!(result.stdout, "lanG\nrust\nGO\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn supports_basename_and_dirname() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("basename /tmp/hello.txt; dirname /tmp/hello.txt");
        assert_eq!(result.stdout, "hello.txt\n/tmp\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn text_tool_usage_errors_are_reported() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("cut -d , missing");
        assert_eq!(result.stdout, "");
        assert_eq!(
            result.stderr,
            "cut: usage: cut -d DELIM -f FIELD [FILE...]\n"
        );
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn rm_rejects_unsupported_options_without_deleting() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/file.txt".to_string(), "data".to_string());
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
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
        let mut bash = Bash::new(Options {
            env,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec(r#"echo '$NAME' "$NAME" pre'$NAME'-$NAME"#);
        assert_eq!(result.stdout, "$NAME rust pre$NAME-rust\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn assignment_words_update_environment() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("NAME=rust; echo $NAME");
        assert_eq!(result.stdout, "rust\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn isolates_environment_and_cwd_between_exec_calls_by_default() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/seed.txt".to_string(), "ok\n".to_string());
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
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
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        let first = bash.exec("NAME=rust; cd /tmp");
        assert_eq!(first.exit_code, 0);
        let second = bash.exec("echo $NAME; pwd");
        assert_eq!(second.stdout, "rust\n/tmp\n");
    }

    #[test]
    fn rejects_scripts_larger_than_limit() {
        let mut bash = Bash::new(Options {
            execution_limits: ExecutionLimits {
                max_script_size_bytes: 8,
                ..ExecutionLimits::default()
            },
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("echo hello");
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stderr, "bash: script exceeds limit (8 bytes)\n");
    }

    #[test]
    fn rejects_too_many_commands() {
        let mut bash = Bash::new(Options {
            execution_limits: ExecutionLimits {
                max_command_count: 2,
                ..ExecutionLimits::default()
            },
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("echo a; echo b; echo c");
        assert_eq!(result.exit_code, 2);
        assert_eq!(result.stderr, "bash: command count exceeds limit (2)\n");
    }

    // ─── Control-flow tests ───────────────────────────────────────────────

    #[test]
    fn executes_if_statements() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("if true; then echo yes; fi");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "yes\n");
    }

    #[test]
    fn executes_if_else_statements() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("if false; then echo yes; else echo no; fi");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "no\n");
    }

    #[test]
    fn executes_if_elif_statements() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("if false; then echo a; elif true; then echo b; else echo c; fi");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "b\n");
    }

    #[test]
    fn executes_while_loops() {
        let mut bash = Bash::new(Options {
            execution_limits: ExecutionLimits {
                max_loop_iterations: 5,
                ..ExecutionLimits::default()
            },
            ..Options::default()
        })
        .unwrap();
        // Simple while loop that terminates when the limit is hit
        let result = bash.exec("while true; do echo x; done");
        assert_eq!(result.exit_code, 2); // Error code for loop limit
        assert!(result.stderr.contains("loop iteration"));
    }

    #[test]
    fn executes_for_loops() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("for i in a b c; do echo $i; done");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "a\nb\nc\n");
    }

    // ─── Command substitution tests ────────────────────────────────────────

    #[test]
    fn executes_command_substitution() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo $(echo hello)");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "hello\n");
    }

    #[test]
    fn command_substitution_strips_trailing_newlines() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo $(echo -e \"a\\nb\\nc\")");
        assert_eq!(result.exit_code, 0);
        // Trailing newlines are stripped but internal ones are preserved
        assert!(
            result.stdout.contains("a")
                && result.stdout.contains("b")
                && result.stdout.contains("c")
        );
    }

    #[test]
    fn nested_command_substitutions() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo $(echo $(echo nested))");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "nested\n");
    }

    #[test]
    fn supports_arithmetic_expansion() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("i=1; i=$((i + 2 * 3)); echo $i");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "7\n");
    }

    #[test]
    fn while_loop_with_arithmetic_counter() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("i=0; while [ \"$i\" -lt 3 ]; do echo $i; i=$((i + 1)); done");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n1\n2\n");
    }

    // ─── Special variables tests ───────────────────────────────────────────

    #[test]
    fn special_variable_exit_code() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("true; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn special_variable_exit_code_after_failure() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("false; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "1\n");
    }

    #[test]
    fn special_variable_script_name() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo $0");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "bash\n");
    }

    // ─── Test builtin tests ──────────────────────────────────────────────

    #[test]
    fn test_builtin_string_empty() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test -z \"\"; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn test_builtin_string_not_empty() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test -z \"hello\"; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "1\n");
    }

    #[test]
    fn test_builtin_string_not_null() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test -n \"hello\"; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn test_builtin_string_equal() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test \"a\" = \"a\"; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn test_builtin_numeric_equal() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test 5 -eq 5; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn test_builtin_numeric_less_than() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("test 3 -lt 5; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn bracket_test_builtin() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("[ \"a\" = \"a\" ]; echo $?");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "0\n");
    }

    // ─── Execution limits tests ────────────────────────────────────────────

    #[test]
    fn enforces_output_size_limit() {
        let mut bash = Bash::new(Options {
            execution_limits: ExecutionLimits {
                max_output_bytes: 50,
                ..ExecutionLimits::default()
            },
            ..Options::default()
        })
        .unwrap();
        // Generate output that exceeds the limit (50 bytes limit)
        let result = bash.exec("echo this is a very long string that exceeds fifty bytes limit");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("output exceeds limit"));
    }

    #[test]
    fn enforces_loop_iteration_limit() {
        let mut bash = Bash::new(Options {
            execution_limits: ExecutionLimits {
                max_loop_iterations: 10,
                ..ExecutionLimits::default()
            },
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("i=0; while [ 1 ]; do i=$((i + 1)); done");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("loop iteration"));
    }

    // ─── Complex scenarios ────────────────────────────────────────────────

    #[test]
    fn complex_script_with_conditionals_and_substitution() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result =
            bash.exec("result=$(echo \"test\"); if [ \"$result\" = \"test\" ]; then echo pass; fi");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, "pass\n");
    }

    #[test]
    fn for_loop_with_command_substitution() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("for i in $(echo -e \"1\\n2\\n3\"); do echo $i; done");
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("1"));
        assert!(result.stdout.contains("2"));
        assert!(result.stdout.contains("3"));
    }

    // ─── mv ───────────────────────────────────────────────────────────────

    #[test]
    fn mv_renames_a_file() {
        let mut files = BTreeMap::new();
        files.insert("/home/user/old.txt".to_string(), "content\n".to_string());
        let mut bash = Bash::new(Options {
            files,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("mv old.txt new.txt; cat new.txt");
        assert_eq!(result.stdout, "content\n");
        assert_eq!(result.stderr, "");
        assert!(bash.fs().read_file("/home/user/old.txt").is_err());
        assert_eq!(
            bash.fs().read_file("/home/user/new.txt").unwrap(),
            "content\n"
        );
    }

    #[test]
    fn mv_fails_for_missing_source() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("mv missing.txt dest.txt");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("No such file or directory"));
    }

    // ─── wc flags ─────────────────────────────────────────────────────────

    #[test]
    fn wc_flag_l_counts_lines_only() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("printf 'a\\nb\\nc\\n' | wc -l");
        assert_eq!(result.stdout, "3\n");
    }

    #[test]
    fn wc_flag_w_counts_words_only() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo 'hello world' | wc -w");
        assert_eq!(result.stdout, "2\n");
    }

    #[test]
    fn wc_combined_flags() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("echo 'hello world' | wc -lw");
        assert_eq!(result.stdout, "1 2\n");
    }

    // ─── read builtin ─────────────────────────────────────────────────────

    #[test]
    fn read_assigns_line_to_variable() {
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        bash.exec("echo hello | read LINE; echo $LINE");
        // In a pipeline the child env doesn't propagate; test via here-string style
        let result = bash.exec("read NAME <<< 'world'; echo $NAME");
        // <<< is not yet supported, so use a simpler approach: write to file then read
        let _ = result;
        let mut bash2 = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        bash2.exec("echo 'alice' > /home/user/name.txt");
        let r = bash2.exec("read NAME < /home/user/name.txt; echo $NAME");
        assert_eq!(r.stdout, "alice\n");
        assert_eq!(r.stderr, "");
    }

    // ─── set options ─────────────────────────────────────────────────────

    #[test]
    fn set_e_aborts_on_nonzero() {
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("set -e; false; echo should_not_run");
        assert_eq!(result.stdout, "");
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn set_u_errors_on_unset_variable() {
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("set -u; echo $UNSET_VAR");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("unbound variable"));
    }

    #[test]
    fn set_o_pipefail_propagates_pipeline_failure() {
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("set -o pipefail; false | echo ok");
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn set_bundled_flags() {
        let mut bash = Bash::new(Options {
            isolate_exec: false,
            ..Options::default()
        })
        .unwrap();
        let result = bash.exec("set -eu; false");
        assert_ne!(result.exit_code, 0);
    }
}

/// Tests mirroring the upstream behaviors ported in the current sync round.
#[cfg(test)]
mod upstream_sync_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).unwrap()
    }

    fn run(script: &str) -> ExecResult {
        bash().exec(script)
    }

    fn stdout_of(script: &str) -> String {
        run(script).stdout
    }

    // ─── Parameter expansion operators (upstream #4af0dc0f, #701f8e5b) ──────

    #[test]
    fn default_value_operator_uses_word_when_unset() {
        assert_eq!(stdout_of("echo ${MISSING:-fallback}"), "fallback\n");
        assert_eq!(stdout_of("echo ${MISSING-fallback}"), "fallback\n");
        assert_eq!(stdout_of("PRESENT=v; echo [${PRESENT:-fallback}]"), "[v]\n");
    }

    #[test]
    fn colon_variants_treat_empty_as_unset() {
        assert_eq!(
            stdout_of("EMPTY=; echo [${EMPTY:-d}] [${EMPTY-d}]"),
            "[d] []\n"
        );
        assert_eq!(stdout_of("EMPTY=; echo [${EMPTY:+alt}]"), "[]\n");
        assert_eq!(stdout_of("SET=v; echo [${SET:+alt}]"), "[alt]\n");
    }

    #[test]
    fn assign_default_writes_the_parameter() {
        assert_eq!(
            stdout_of("echo ${V:=assigned}; echo $V"),
            "assigned\nassigned\n"
        );
        assert_eq!(
            stdout_of("echo ${V=assigned}; echo $V"),
            "assigned\nassigned\n"
        );
    }

    #[test]
    fn assign_default_leaves_an_existing_value_alone() {
        assert_eq!(
            stdout_of("V=keep; echo ${V:=ignored}; echo $V"),
            "keep\nkeep\n"
        );
    }

    #[test]
    fn error_operator_reports_the_message_and_exit_1() {
        let result = run("echo before${MISSING:?boom}after");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "bash: boom\n");
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn operators_do_not_trip_nounset() {
        // `${x:-y}` exists precisely for the unset case, so `set -u` must not
        // abort the command.
        for script in [
            "set -u; echo ${MISSING:-fallback}",
            "set -u; echo [${MISSING-fallback}]",
            "set -u; echo [${MISSING:+alt}]",
            "set -u; echo ${MISSING:=assigned}",
            "set -u; echo ${MISSING=assigned}",
        ] {
            let result = run(script);
            assert_eq!(result.exit_code, 0, "{script} -> {result:?}");
            assert_eq!(result.stderr, "", "{script}");
        }
    }

    #[test]
    fn bare_and_length_forms_still_report_nounset() {
        for script in ["set -u; echo ${MISSING}", "set -u; echo \"${#MISSING}\""] {
            let result = run(script);
            assert_eq!(result.exit_code, 1, "{script}");
            assert_eq!(
                result.stderr, "bash: MISSING: unbound variable\n",
                "{script}"
            );
        }
    }

    #[test]
    fn length_operator_reports_character_count() {
        assert_eq!(stdout_of("V=hello; echo ${#V}"), "5\n");
        assert_eq!(stdout_of("V=; echo ${#V}"), "0\n");
    }

    #[test]
    fn default_word_may_contain_nested_expansions() {
        assert_eq!(stdout_of("echo ${V:=pre${W:-q}post}"), "preqpost\n");
    }

    #[test]
    fn assignment_default_to_positional_parameters_is_rejected() {
        for script in [
            "set --; echo ${1:=x}",
            "set --; echo ${12=fallback}",
            "set --; echo ${@:=fallback}",
            "set --; echo ${*:=fallback}",
        ] {
            let result = run(script);
            assert_eq!(result.stdout, "", "{script}");
            assert_eq!(result.exit_code, 1, "{script}");
            assert!(
                result.stderr.contains("cannot assign in this way"),
                "{script} -> {}",
                result.stderr
            );
        }
    }

    #[test]
    fn positional_parameters_can_still_be_read() {
        assert_eq!(stdout_of("set -- a b; echo \"$# $1 $2\""), "2 a b\n");
        assert_eq!(stdout_of("set -- a b; echo ${1:-none}"), "a\n");
    }

    // ─── set: bundled -o clusters (upstream #c9904dea) ──────────────────────

    #[test]
    fn bundled_o_enables_pipefail() {
        // `false | true` succeeds unless pipefail is on.
        let result = run("set -euo pipefail; echo start; false | true; echo never");
        assert!(!result.stdout.contains("never"), "{result:?}");
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn standalone_pipefail_switch_still_works() {
        let result = run("set -o pipefail; false | true; echo $?");
        assert_eq!(result.stdout, "1\n");
    }

    #[test]
    fn bundled_o_consumes_the_next_word_as_the_option_name() {
        // `-oe pipefail`: the `o` takes `pipefail`, then `e` sets errexit.
        let result = run("set -oe pipefail; false | true; echo never");
        assert!(!result.stdout.contains("never"), "{result:?}");
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn multiple_bundled_os_consume_successive_words() {
        let result = run("set -oo pipefail errexit; false | true; echo never");
        assert!(!result.stdout.contains("never"), "{result:?}");
        assert_ne!(result.exit_code, 0);
    }

    #[test]
    fn bundled_o_can_be_disabled_with_plus() {
        let result = run("set -euo pipefail; set +euo pipefail; false | true; echo done");
        assert_eq!(result.stdout, "done\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn bundled_o_rejects_an_unknown_option_name() {
        let result = run("set -euo bogusoption");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("bogusoption"), "{}", result.stderr);
        assert!(
            result.stderr.contains("invalid option name"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn bare_o_prints_the_option_listing() {
        let result = run("set -o");
        assert!(result.stdout.contains("errexit"), "{}", result.stdout);
        assert!(result.stdout.contains("pipefail"), "{}", result.stdout);
    }

    #[test]
    fn trailing_words_after_the_cluster_become_positionals() {
        assert_eq!(
            stdout_of("set -euo pipefail one two; echo \"$# $1 $2\""),
            "2 one two\n"
        );
    }

    // ─── break / continue status (upstream #017a911d) ───────────────────────

    #[test]
    fn break_reports_success_even_after_a_failed_command() {
        for script in [
            "while :; do false; break; done; echo $?",
            "for i in 1 2; do false; break; done; echo $?",
            "for i in 1; do for j in 1; do false; break 2; done; done; echo $?",
        ] {
            let result = run(script);
            assert_eq!(result.stdout, "0\n", "{script}");
            assert_eq!(result.exit_code, 0, "{script}");
        }
        // A nested break reports 0 to the enclosing body as well.
        let result = run("for i in 1; do for j in 1; do false; break; done; echo after=$?; done");
        assert_eq!(result.stdout, "after=0\n");
    }

    #[test]
    fn continue_reports_success_but_is_not_pinned() {
        // The final `false` runs after the last `continue` was adopted, so the
        // loop's status is 1.
        assert_eq!(
            stdout_of(
                "for i in 1 2; do if [ $i = 1 ]; then true; continue; fi; false; done; echo $?"
            ),
            "1\n"
        );
        assert_eq!(
            stdout_of("for i in 1 2; do false; continue; done; echo $?"),
            "0\n"
        );
    }

    #[test]
    fn continue_adopts_status_zero_for_the_next_iteration() {
        assert_eq!(
            stdout_of("for i in 1 2; do echo \"$i:$?\"; false; continue; done; echo $?"),
            "1:0\n2:0\n0\n"
        );
    }

    #[test]
    fn normal_loop_termination_keeps_the_body_status() {
        assert_eq!(stdout_of("for i in 1 2; do false; done; echo $?"), "1\n");
        assert_eq!(stdout_of("for i in; do false; done; echo $?"), "0\n");
    }

    #[test]
    fn break_stops_the_rest_of_the_statement_list() {
        let result = run("for i in 1; do break; echo unreachable; done; echo done");
        assert_eq!(result.stdout, "done\n");
    }

    // ─── fd-aware redirection (upstream #cb2b583b) ──────────────────────────

    fn files(entries: &[(&str, &str)]) -> Bash {
        let mut map = BTreeMap::new();
        for (path, contents) in entries {
            map.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files: map,
            ..Options::default()
        })
        .unwrap()
    }

    #[test]
    fn dup_sends_stderr_to_the_redirected_stdout() {
        let mut bash = bash();
        let result = bash.exec("nosuchcmd > /home/user/f 2>&1");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 127);
        assert_eq!(
            bash.fs().read_file("/home/user/f").unwrap(),
            "bash: nosuchcmd: command not found\n"
        );
    }

    #[test]
    fn dup_before_redirect_leaves_stderr_on_the_caller() {
        let mut bash = bash();
        // `2>&1` captures the *current* stdout target (the caller's), then `> g`
        // moves fd 1 only — so g stays empty.
        let result = bash.exec("nosuchcmd 2>&1 > /home/user/g");
        assert_eq!(result.stdout, "bash: nosuchcmd: command not found\n");
        assert_eq!(result.stderr, "");
        assert_eq!(bash.fs().read_file("/home/user/g").unwrap(), "");
    }

    #[test]
    fn append_dup_appends_both_streams() {
        let mut bash = files(&[("/home/user/log", "x\n")]);
        bash.exec("nosuchcmd >> /home/user/log 2>&1");
        assert_eq!(
            bash.fs().read_file("/home/user/log").unwrap(),
            "x\nbash: nosuchcmd: command not found\n"
        );
    }

    #[test]
    fn later_redirect_repoints_the_descriptor() {
        let mut bash = bash();
        bash.exec("echo hi > /home/user/a > /home/user/b");
        assert_eq!(bash.fs().read_file("/home/user/a").unwrap(), "");
        assert_eq!(bash.fs().read_file("/home/user/b").unwrap(), "hi\n");
    }

    #[test]
    fn dev_null_discards_without_creating_a_file() {
        let mut bash = bash();
        let result = bash.exec("nosuchcmd > /dev/null 2>&1; echo survived");
        assert_eq!(result.stdout, "survived\n");
        assert_eq!(result.stderr, "");
        assert!(bash.fs().read_file("/dev/null").is_err());
    }

    #[test]
    fn stdout_can_be_duplicated_onto_stderr() {
        let mut bash = bash();
        let result = bash.exec("echo hi 2> /home/user/e 1>&2");
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "");
        assert_eq!(bash.fs().read_file("/home/user/e").unwrap(), "hi\n");
    }

    #[test]
    fn plain_word_ending_in_a_digit_is_not_a_descriptor() {
        // `a2>f` is the word `a2`, not a redirect of descriptor 2.
        let mut bash = bash();
        bash.exec("echo a2>/home/user/n");
        assert_eq!(bash.fs().read_file("/home/user/n").unwrap(), "a2\n");
    }

    #[test]
    fn digit_only_word_before_the_operator_is_the_descriptor() {
        let mut bash = bash();
        bash.exec("echo 2>/home/user/m");
        assert_eq!(bash.fs().read_file("/home/user/m").unwrap(), "");
    }

    #[test]
    fn reading_from_dev_null_yields_an_empty_stream() {
        let mut bash = bash();
        let result = bash.exec("cat < /dev/null; echo after");
        assert_eq!(result.stdout, "after\n");
        assert_eq!(result.stderr, "");
        assert!(bash.fs().read_file("/dev/null").is_err());
    }

    #[test]
    fn close_descriptor_discards_the_stream() {
        let result = run("echo hi 2>&-");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.stderr, "");
    }

    // ─── cat display flags (upstream #6130334f) ─────────────────────────────

    fn cat(bytes: &[u8], flags: &str) -> String {
        let args: Vec<String> = flags.split_whitespace().map(str::to_string).collect();
        let (options, operands) = parse_cat_options(&args).unwrap();
        assert!(operands.is_empty());
        apply_cat_options(bytes, &options)
    }

    #[test]
    fn cat_show_ends_marks_terminated_lines_only() {
        assert_eq!(cat(b"a\nb\n", "-E"), "a$\nb$\n");
        assert_eq!(cat(b"a\nb", "-E"), "a$\nb");
    }

    #[test]
    fn cat_show_tabs_and_aliases() {
        assert_eq!(cat(b"a\tb\n", "-T"), "a^Ib\n");
        assert_eq!(cat(b"a\tb\n", "-A"), "a^Ib$\n");
        assert_eq!(cat(b"a\tb\n", "-e"), "a\tb$\n");
        assert_eq!(cat(b"a\tb\n", "-t"), "a^Ib\n");
        assert_eq!(cat(b"a\tb\n", "--show-all"), "a^Ib$\n");
    }

    #[test]
    fn cat_numbers_lines() {
        assert_eq!(cat(b"a\n\nb\n", "-n"), "     1\ta\n     2\t\n     3\tb\n");
        assert_eq!(cat(b"a\n\nb\n", "-b"), "     1\ta\n\n     2\tb\n");
        // -b overrides -n
        assert_eq!(cat(b"a\n\nb\n", "-bn"), "     1\ta\n\n     2\tb\n");
        assert_eq!(cat(b"a", "-n"), "     1\ta");
    }

    #[test]
    fn cat_squeezes_before_numbering() {
        assert_eq!(cat(b"a\n\n\n\nb\n", "-s"), "a\n\nb\n");
        assert_eq!(cat(b"\n\n\na\n", "-s"), "\na\n");
        assert_eq!(
            cat(b"a\n\n\nb\n", "-sn"),
            "     1\ta\n     2\t\n     3\tb\n"
        );
    }

    #[test]
    fn cat_renders_nonprinting_bytes_exactly() {
        // Reference vectors from upstream cat.show-nonprinting.test.ts.
        assert_eq!(
            cat(&[0, 1, 7, 8, 27, 31, 32, 126, 127, 10], "-v"),
            "^@^A^G^H^[^_ ~^?\n"
        );
        assert_eq!(
            cat(&[128, 129, 155, 159, 160, 200, 254, 255, 10], "-v"),
            "M-^@M-^AM-^[M-^_M- M-HM-~M-^?\n"
        );
        // UTF-8 multi-byte content is rendered byte-wise.
        assert_eq!(cat("é\n".as_bytes(), "-v"), "M-CM-)\n");
        // TAB is literal under plain -v but rendered under -vT.
        assert_eq!(cat(b"a\tb\n", "-v"), "a\tb\n");
    }

    #[test]
    fn cat_unknown_options_are_reported() {
        let args = vec!["-Z".to_string()];
        let error = parse_cat_options(&args).unwrap_err();
        assert_eq!(error, "cat: invalid option -- 'Z'\n");

        let args = vec!["--bogus".to_string()];
        let error = parse_cat_options(&args).unwrap_err();
        assert_eq!(error, "cat: unrecognized option '--bogus'\n");
    }

    #[test]
    fn cat_double_dash_ends_option_parsing() {
        let args = vec!["--".to_string(), "-E".to_string()];
        let (options, operands) = parse_cat_options(&args).unwrap();
        assert!(options.is_plain());
        assert_eq!(operands, vec!["-E".to_string()]);
    }

    #[test]
    fn cat_numbers_across_file_boundaries() {
        let mut bash = files(&[("/a.txt", "a1\na2\n"), ("/b.txt", "b1\nb2\n")]);
        let result = bash.exec("cat -n /a.txt /b.txt");
        assert_eq!(
            result.stdout,
            "     1\ta1\n     2\ta2\n     3\tb1\n     4\tb2\n"
        );
    }

    #[test]
    fn cat_squeeze_spans_file_boundaries() {
        let mut bash = files(&[("/a.txt", "a\n\n"), ("/b.txt", "\n\nb\n")]);
        let result = bash.exec("cat -s /a.txt /b.txt");
        assert_eq!(result.stdout, "a\n\nb\n");
    }

    // ─── misc parity ────────────────────────────────────────────────────────

    #[test]
    fn colon_is_a_successful_no_op() {
        let result = run(": anything here; echo $?");
        assert_eq!(result.stdout, "0\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn unset_removes_a_variable() {
        assert_eq!(stdout_of("V=1; unset V; echo [${V:-gone}]"), "[gone]\n");
    }

    #[test]
    fn export_without_a_value_creates_an_empty_variable() {
        assert_eq!(
            stdout_of("export NEWVAR; echo [${NEWVAR:-empty}]"),
            "[empty]\n"
        );
    }

    #[test]
    fn command_substitution_stderr_reaches_the_caller() {
        // A failing substitution must not vanish silently.
        let result = run("V=$(nosuchcmd); echo \"[${V}]\"");
        assert!(result.stderr.contains("command not found"), "{result:?}");
    }
}

/// grep option/pattern/status parity (upstream #314, #315, #327, #331, #332).
#[cfg(test)]
mod grep_parity_tests {
    use super::*;

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut map = BTreeMap::new();
        for (path, contents) in entries {
            map.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files: map,
            ..Options::default()
        })
        .unwrap()
    }

    fn haystack() -> Bash {
        with_files(&[
            ("/hay.txt", "apple pie\ncherry\nbanana split\n"),
            ("/pat.txt", "apple\nbanana\n"),
        ])
    }

    #[test]
    fn repeated_e_patterns_are_unioned() {
        let mut bash = with_files(&[("/t.txt", "aaa Q1\nbbb\nccc Q4\n")]);
        let result = bash.exec("grep -e Q1 -e Q4 /t.txt");
        assert_eq!(result.stdout, "aaa Q1\nccc Q4\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn non_option_operands_are_files_when_e_is_present() {
        let mut bash = with_files(&[("/one.txt", "needle one\n"), ("/two.txt", "needle two\n")]);
        let result = bash.exec("grep /one.txt -e needle /two.txt");
        assert_eq!(result.stdout, "/one.txt:needle one\n/two.txt:needle two\n");
    }

    #[test]
    fn pattern_file_is_read_and_union_ordered() {
        let mut bash = haystack();
        for script in [
            "grep -f /pat.txt /hay.txt",
            "grep --file=/pat.txt /hay.txt",
            "grep -f/pat.txt /hay.txt",
        ] {
            let result = bash.exec(script);
            assert_eq!(result.stdout, "apple pie\nbanana split\n", "{script}");
        }
    }

    #[test]
    fn pattern_file_unions_with_e_patterns() {
        let mut bash = haystack();
        let result = bash.exec("grep -f /pat.txt -e cherry /hay.txt");
        assert_eq!(result.stdout, "apple pie\ncherry\nbanana split\n");
    }

    #[test]
    fn empty_pattern_file_short_circuits() {
        let mut bash = with_files(&[("/hay.txt", "apple pie\ncherry\n"), ("/empty.txt", "")]);
        // No output, no count, and unreadable operands are never opened.
        for script in [
            "grep -f /empty.txt /hay.txt",
            "grep -c -f /empty.txt /hay.txt",
            "grep -f /empty.txt /nonexistent.txt",
        ] {
            let result = bash.exec(script);
            assert_eq!(result.stdout, "", "{script}");
            assert_eq!(result.stderr, "", "{script}");
            assert_eq!(result.exit_code, 1, "{script}");
        }
        // -v with an empty pattern list selects everything.
        let result = bash.exec("grep -v -f /empty.txt /hay.txt");
        assert_eq!(result.stdout, "apple pie\ncherry\n");
    }

    #[test]
    fn missing_pattern_file_reports_and_exits_2() {
        let mut bash = haystack();
        let result = bash.exec("grep -f /missing.txt /hay.txt");
        assert_eq!(result.stdout, "");
        assert_eq!(
            result.stderr,
            "grep: /missing.txt: No such file or directory\n"
        );
        assert_eq!(result.exit_code, 2);
    }

    #[test]
    fn missing_pattern_file_argument_exits_2() {
        let mut bash = haystack();
        let short = bash.exec("grep -f");
        assert_eq!(short.exit_code, 2);
        assert_eq!(short.stderr, "grep: option requires an argument -- 'f'\n");

        let long = bash.exec("grep --file");
        assert_eq!(long.exit_code, 2);
        assert_eq!(long.stderr, "grep: option '--file' requires an argument\n");
    }

    #[test]
    fn pattern_file_trailing_newline_is_a_terminator() {
        assert_eq!(
            split_pattern_file("apple\nbanana\n"),
            vec!["apple", "banana"]
        );
        assert_eq!(split_pattern_file("apple\nbanana"), vec!["apple", "banana"]);
        assert_eq!(split_pattern_file(""), Vec::<String>::new());
        // Interior blank lines are patterns.
        assert_eq!(split_pattern_file("foo\n\nbar\n"), vec!["foo", "", "bar"]);
    }

    #[test]
    fn dash_operand_reads_standard_input() {
        let mut bash = haystack();
        let result = bash.exec("printf 'apple\\n' | grep apple -");
        assert_eq!(result.stdout, "apple\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn lone_dash_gets_no_filename_prefix() {
        let mut bash = haystack();
        let result = bash.exec("printf 'apple\\n' | grep -n apple -");
        assert_eq!(result.stdout, "1:apple\n");
    }

    #[test]
    fn repeated_dash_operand_is_drained_after_the_first() {
        let mut bash = haystack();
        let result = bash.exec("printf 'apple\\n' | grep -c apple - -");
        assert_eq!(result.stdout, "(standard input):1\n(standard input):0\n");
    }

    #[test]
    fn dash_still_works_as_the_pattern() {
        let mut bash = with_files(&[("/dash.txt", "a-b\nxy\n")]);
        let result = bash.exec("grep - /dash.txt");
        assert_eq!(result.stdout, "a-b\n");
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let mut bash = with_files(&[("/pt.txt", "match\n-x\n--help\n")]);
        for (script, expected) in [
            ("grep -- match /pt.txt", "match\n"),
            ("grep -- -x /pt.txt", "-x\n"),
            ("grep -- --help /pt.txt", "--help\n"),
        ] {
            let result = bash.exec(script);
            assert_eq!(result.stdout, expected, "{script}");
        }
    }

    #[test]
    fn files_without_match_follows_gnu_status() {
        let mut bash = with_files(&[
            ("/m1.txt", "hello\n"),
            ("/m2.txt", "hello\n"),
            ("/n1.txt", "nope\n"),
            ("/n2.txt", "nope\n"),
        ]);
        // All match: nothing listed, status 0.
        let result = bash.exec("grep -L hello /m1.txt /m2.txt");
        assert_eq!(result.stdout, "");
        assert_eq!(result.exit_code, 0);
        // None match: every name listed, status 1.
        let result = bash.exec("grep -L hello /n1.txt /n2.txt");
        assert_eq!(result.stdout, "/n1.txt\n/n2.txt\n");
        assert_eq!(result.exit_code, 1);
        // Mixed: only the non-matching name, status 0.
        let result = bash.exec("grep -L hello /m1.txt /n1.txt");
        assert_eq!(result.stdout, "/n1.txt\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn unreadable_operand_outranks_a_match_but_still_searches() {
        let mut bash = with_files(&[("/m1.txt", "hello\n")]);
        let result = bash.exec("grep hello /m1.txt /missing.txt");
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(
            result.stderr,
            "grep: /missing.txt: No such file or directory\n"
        );
        assert_eq!(result.exit_code, 2);
    }

    #[test]
    fn quiet_reports_status_without_output() {
        let mut bash = with_files(&[("/m1.txt", "hello\n")]);
        let result = bash.exec("grep -q hello /m1.txt");
        assert_eq!(result.stdout, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn word_and_whole_line_matching_use_literal_needles() {
        assert!(contains_word("foo bar", "foo"));
        assert!(!contains_word("foobar", "foo"));
        assert!(!contains_word("xfoo", "foo"));

        let mut bash = with_files(&[("/t.txt", "foo\nbar\nfoobar\n")]);
        let result = bash.exec("grep -x -e foo -e bar /t.txt");
        assert_eq!(result.stdout, "foo\nbar\n");
    }

    #[test]
    fn ignore_case_and_invert_are_applied() {
        let mut bash = with_files(&[("/t.txt", "Apple\nbanana\n")]);
        assert_eq!(bash.exec("grep -i apple /t.txt").stdout, "Apple\n");
        // -v is case-sensitive on its own, so `Apple` is kept.
        assert_eq!(bash.exec("grep -v apple /t.txt").stdout, "Apple\nbanana\n");
        assert_eq!(bash.exec("grep -v Apple /t.txt").stdout, "banana\n");
        // -i makes `Apple` match, so -v excludes it and keeps `banana`.
        assert_eq!(bash.exec("grep -iv apple /t.txt").stdout, "banana\n");
    }

    #[test]
    fn no_filename_suppresses_only_the_prefix() {
        let mut bash = with_files(&[("/a.txt", "x\n"), ("/b.txt", "x\n")]);
        let result = bash.exec("grep -h x /a.txt /b.txt");
        assert_eq!(result.stdout, "x\nx\n");
    }

    #[test]
    fn count_only_reports_per_file_totals() {
        let mut bash = with_files(&[("/a.txt", "x\nx\ny\n"), ("/b.txt", "y\n")]);
        let result = bash.exec("grep -c x /a.txt /b.txt");
        assert_eq!(result.stdout, "/a.txt:2\n/b.txt:0\n");
        assert_eq!(result.exit_code, 0);
    }
}
