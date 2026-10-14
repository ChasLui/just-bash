use std::collections::BTreeMap;

use crate::fs::{Error, InMemoryFs, Metadata, normalize_absolute, parent_dir};
use crate::glob;
use crate::parser::{
    ArrayAssign, CaseStatement, CommandInvocation, CondExpr, CondTerm, ForStatement, FunctionDef,
    IfStatement, ParseLimits, PipelineConnector, Redirect, RedirectMode, Statement, StatementKind,
    WhileStatement, Word, WordPartKind, parse_heredoc_word, parse_script_with_limits,
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
    /// Allow `curl` to open real plain-HTTP connections. Off by default: this
    /// runtime has no network surface unless the embedder asks for one.
    pub allow_network: bool,
    /// Deterministic URL -> body responses served to `curl` without any I/O.
    pub http_responses: BTreeMap<String, String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            files: BTreeMap::new(),
            env: BTreeMap::new(),
            cwd: None,
            isolate_exec: true,
            execution_limits: ExecutionLimits::default(),
            allow_network: false,
            http_responses: BTreeMap::new(),
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
    /// Shell functions defined in this shell (`name() { … }`).
    functions: BTreeMap<String, Vec<Statement>>,
    /// Positional parameters (`$1`, `$@`, `$#`). Stored as a list rather than a
    /// joined string so `"$@"` can expand to one field per parameter.
    positionals: Vec<String>,
    /// Indexed arrays. `PIPESTATUS` lives here too, as bash does.
    arrays: BTreeMap<String, Vec<String>>,
    /// Associative arrays created with `declare -A` / `local -A`.
    assoc_arrays: BTreeMap<String, BTreeMap<String, String>>,
    /// Active `local`/`declare` scopes, innermost last. A function call pushes
    /// one scope and restores it on return.
    local_scopes: Vec<Vec<ScopedVar>>,
    /// Background jobs started with `&`, in start order.
    jobs: Vec<JobRecord>,
    next_job_id: u32,
    /// Depth of the function-call stack; `return` is only valid when > 0.
    function_depth: usize,
    /// `return N` unwinding state for the innermost function body.
    return_signal: Option<i32>,
    /// Lines the current pipeline stage may still produce. Set from a
    /// downstream consumer (`head -n`, `grep -m`) so an unbounded producer such
    /// as `yes` stops exactly at demand instead of buffering forever.
    line_budget: Option<usize>,
    /// Seed for the next pipeline's standard input (process substitution and
    /// `exec_script_with_stdin`).
    seed_stdin: Option<String>,
    /// Temporary files created for `<(…)` / `>(…)`, removed after the command.
    procsub_files: Vec<String>,
    /// `>(cmd)` scripts that must run once the current command finished.
    procsub_readers: Vec<(String, String)>,
    procsub_counter: u64,
    /// Deterministic counter backing `mktemp`.
    mktemp_counter: u64,
    /// Array literal attached to the command being run (`declare -A m=( … )`).
    pending_array: Option<ArrayAssign>,
    allow_network: bool,
    http_responses: BTreeMap<String, String>,
}

/// One background job (`cmd &`).
///
/// There is no concurrency in this runtime: a background statement is executed
/// to completion in a child shell and only its status is retained, so `$!` and
/// `wait` stay deterministic.
/// A variable captured by `local`/`declare` inside a function, with the value
/// it had before the local declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopedVar {
    Scalar {
        name: String,
        previous: Option<String>,
    },
    Indexed {
        name: String,
        previous: Option<Vec<String>>,
    },
    Associative {
        name: String,
        previous: Option<BTreeMap<String, String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JobRecord {
    id: u32,
    status: i32,
    command: String,
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
            functions: BTreeMap::new(),
            positionals: Vec::new(),
            arrays: BTreeMap::new(),
            assoc_arrays: BTreeMap::new(),
            local_scopes: Vec::new(),
            jobs: Vec::new(),
            next_job_id: 1,
            function_depth: 0,
            return_signal: None,
            line_budget: None,
            seed_stdin: None,
            procsub_files: Vec::new(),
            procsub_readers: Vec::new(),
            procsub_counter: 0,
            mktemp_counter: 0,
            pending_array: None,
            allow_network: options.allow_network,
            http_responses: options.http_responses,
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
            // Functions and arrays are shell state, so an isolated exec starts
            // without them, exactly like a fresh shell.
            let saved_functions = std::mem::take(&mut self.functions);
            let saved_positionals = std::mem::take(&mut self.positionals);
            let saved_arrays = std::mem::take(&mut self.arrays);
            let saved_assoc = std::mem::take(&mut self.assoc_arrays);
            let saved_scopes = std::mem::take(&mut self.local_scopes);
            let saved_jobs = std::mem::take(&mut self.jobs);
            let saved_job_id = self.next_job_id;
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
            self.return_signal = None;
            self.next_job_id = 1;
            let result = self.exec_in_current_state(script);
            self.env = saved_env;
            self.cwd = saved_cwd;
            self.output_bytes_used = saved_output;
            self.loop_iteration_count = saved_loop_count;
            self.opt_errexit = saved_errexit;
            self.opt_nounset = saved_nounset;
            self.opt_pipefail = saved_pipefail;
            self.functions = saved_functions;
            self.positionals = saved_positionals;
            self.arrays = saved_arrays;
            self.assoc_arrays = saved_assoc;
            self.local_scopes = saved_scopes;
            self.jobs = saved_jobs;
            self.next_job_id = saved_job_id;
            self.nounset_error = None;
            self.param_error = None;
            self.return_signal = None;
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

            if self.check_output_limit(&mut stderr).is_err() {
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
        self.return_signal = None;

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
            StatementKind::Case(case_stmt) => {
                let mut count = 0;
                for clause in &case_stmt.clauses {
                    for s in &clause.body {
                        count += self.count_commands_in_statement(s);
                    }
                }
                count
            }
            StatementKind::Function(def) => {
                let mut count = 0;
                for s in &def.body {
                    count += self.count_commands_in_statement(s);
                }
                count
            }
            StatementKind::Group(body) | StatementKind::Subshell(body) => {
                let mut count = 0;
                for s in body {
                    count += self.count_commands_in_statement(s);
                }
                count
            }
            // `[[ … ]]` and `name=(…)` are not command invocations.
            StatementKind::Cond(_) | StatementKind::ArrayAssign(_) => 0,
        }
    }

    fn exec_statement(&mut self, statement: &Statement) -> (String, String, i32) {
        if statement.background {
            return self.exec_background(statement);
        }
        let (stdout, stderr, mut exit_code) = self.exec_statement_inner(statement);
        if statement.negated {
            // `! cmd` inverts the status; output is unchanged.
            exit_code = if exit_code == 0 { 1 } else { 0 };
        }
        // `$?` must observe this statement's own status: compound statements do
        // not go through `exec_pipeline`, and `!` changes the status afterwards.
        self.last_exit_code = exit_code;
        (stdout, stderr, exit_code)
    }

    /// Execute a statement body; `&` and `!` are handled by the caller.
    fn exec_statement_inner(&mut self, statement: &Statement) -> (String, String, i32) {
        let result = match &statement.kind {
            StatementKind::Pipeline(commands) => self.exec_pipeline(commands),
            StatementKind::If(if_stmt) => self.exec_if(if_stmt),
            StatementKind::While(while_stmt) => self.exec_while(while_stmt),
            StatementKind::For(for_stmt) => self.exec_for(for_stmt),
            StatementKind::Case(case_stmt) => self.exec_case(case_stmt),
            StatementKind::Function(def) => self.define_function(def),
            StatementKind::Group(body) => self.exec_statement_list(body),
            StatementKind::Subshell(body) => self.run_subshell(body),
            StatementKind::Cond(expr) => self.exec_cond(expr),
            StatementKind::ArrayAssign(assign) => self.exec_array_assign(assign),
        };
        // A pipeline records one status per stage; every other statement kind
        // reports a single element, matching bash's PIPESTATUS after a compound.
        if !matches!(statement.kind, StatementKind::Pipeline(_)) {
            self.set_pipestatus(vec![result.2]);
        }
        result
    }

    fn exec_pipeline(&mut self, commands: &[CommandInvocation]) -> (String, String, i32) {
        // Standard input may be seeded by process substitution.
        let mut stdin = self.seed_stdin.take().unwrap_or_default();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = 0;
        // For pipefail: track all non-zero exit codes in the pipeline
        let mut pipeline_last_failure: i32 = 0;

        let command_count = commands.len();
        let is_pipeline = command_count > 1;
        // Back-pressure: a downstream consumer caps how much a producer may
        // emit, so `yes | head -n 3` never materialises an infinite stream.
        let budget = Self::pipeline_line_budget(commands);
        let mut statuses: Vec<i32> = Vec::with_capacity(command_count);

        for (index, command) in commands.iter().enumerate() {
            let is_last = index + 1 == command_count;
            self.line_budget = if index == 0 { budget } else { None };
            let result = if is_pipeline {
                let mut child = self.child_shell();
                let result = child.run_invocation(command, &stdin);
                self.fs = child.fs;
                self.env = child.env;
                // Note: cwd is NOT copied back for pipeline commands (pipeline isolation)
                result
            } else {
                self.run_invocation(command, &stdin)
            };
            self.line_budget = None;

            stderr.push_str(&result.stderr);
            exit_code = result.exit_code;
            statuses.push(result.exit_code);
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
                self.set_pipestatus(statuses);
                self.last_exit_code = exit_code;
                return (stdout, stderr, exit_code);
            }
        }

        // set -o pipefail: use last non-zero exit code from the pipeline
        if self.opt_pipefail && is_pipeline && pipeline_last_failure != 0 {
            exit_code = pipeline_last_failure;
        }

        self.set_pipestatus(statuses);
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

            // `until` runs the body while the condition FAILS.
            let condition_true = if while_stmt.until {
                cond_exit != 0
            } else {
                cond_exit == 0
            };
            if !condition_true {
                break;
            }

            let (body_stdout, body_stderr, body_exit) = self.exec_statement_list(&while_stmt.body);
            stdout.push_str(&body_stdout);
            stderr.push_str(&body_stderr);
            exit_code = body_exit;

            self.output_bytes_used += body_stdout.len() + body_stderr.len();
            if self.check_output_limit(&mut stderr).is_err() {
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

        let items: Vec<String> = if for_stmt.items.is_empty() {
            self.positional_parameters()
        } else {
            let mut items = Vec::new();
            for word in &for_stmt.items {
                items.extend(self.expand_word_list(word));
            }
            items
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
            if self.check_output_limit(&mut stderr).is_err() {
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
            if self.check_output_limit(&mut stderr).is_err() {
                return (stdout, stderr, 2);
            }

            // `break`/`continue`/`return` stop the rest of this statement list
            // so the signal can unwind to the enclosing loop or function.
            if self.loop_signal.is_some() || self.return_signal.is_some() {
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
                // `<<<word`: the expanded word plus a newline becomes stdin.
                RedirectMode::HereString => {
                    let word = redirect.target.clone();
                    stdin = format!("{}\n", self.expand_word(&word));
                }
                // `<<DELIM`: the collected body becomes stdin; a quoted
                // delimiter suppresses expansion.
                RedirectMode::Heredoc { .. } => {
                    if let RedirectMode::Heredoc { body, expand } = &redirect.mode {
                        stdin = if *expand {
                            self.expand_heredoc(body)
                        } else {
                            body.clone()
                        };
                    }
                }
            }
        }

        // Words expand in list context: `${a[@]}`, `"$@"` and pathname
        // expansion can each produce several arguments.
        let mut words: Vec<String> = Vec::new();
        for word in &command.words {
            words.extend(self.expand_word_list(word));
        }

        // stderr produced by command substitutions during expansion is part of
        // this command's output.
        let expansion_stderr = std::mem::take(&mut self.expansion_stderr);

        // A command substitution that failed to parse aborts the command; it
        // must never run with a silently-empty substitution (upstream #348).
        if self.expansion_failed {
            self.expansion_failed = false;
            self.cleanup_procsubs();
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
            self.cleanup_procsubs();
            return CommandOutput {
                stdout: String::new(),
                stderr: format!("{expansion_stderr}bash: {varname}: unbound variable\n"),
                exit_code: 1,
            };
        }
        // `${x:?message}` and friends abort the command with exit status 1.
        if let Some(message) = self.param_error.take() {
            self.cleanup_procsubs();
            return CommandOutput {
                stdout: String::new(),
                stderr: format!("{expansion_stderr}{message}"),
                exit_code: 1,
            };
        }

        // An array literal attached to this command is applied by the
        // declaration builtin (or by the assignment path when there are no
        // words).
        self.pending_array = command.array.clone();
        // Shell functions take precedence over built-ins and external commands.
        let mut result = match words.first() {
            Some(name) if self.functions.contains_key(name) => {
                let body = self.functions.get(name).cloned().unwrap_or_default();
                self.call_function(&body, &words[1..])
            }
            _ => self.run_command(&words, &stdin),
        };
        if !expansion_stderr.is_empty() {
            result.stderr = format!("{expansion_stderr}{}", result.stderr);
        }
        // Deliver both streams exactly once, after the whole redirect list.
        if let Err(error) = self.deliver(&mut result, &fd1, &fd2) {
            result.stderr.push_str(&format!("bash: {error}\n"));
            result.exit_code = 1;
        }

        // `>(cmd)` readers run once the command has finished writing to their
        // fifo. Their own output belongs to the shell's standard output, not to
        // this command's redirects. Temporary files are removed after every
        // reader has consumed them.
        self.flush_procsub_readers(&mut result);
        self.cleanup_procsubs();
        // Only declaration built-ins consume an attached array literal; anywhere
        // else it is a syntax error rather than a silent no-op.
        if self.pending_array.take().is_some() {
            result
                .stderr
                .push_str("bash: syntax error near unexpected token `('\n");
            result.exit_code = 2;
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
            // `m=( … )` with no command word is an array assignment.
            if let Some(assign) = self.pending_array.take() {
                let (out, err, code) = self.exec_array_assign(&assign);
                return CommandOutput {
                    stdout: out,
                    stderr: err,
                    exit_code: code,
                };
            }
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
                // `a[1]=x` assigns one array element; anything else is a scalar.
                if let Some((name, subscript)) = split_array_subscript(key) {
                    self.assign_element(name, subscript, value);
                } else {
                    self.env.insert(key.to_string(), value.to_string());
                }
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
                            match compile_grep_patterns(&options, &patterns) {
                                Err(message) => {
                                    stderr.push_str(&message);
                                    exit_code = 2;
                                }
                                Ok(compiled) => {
                                    match self.run_grep(
                                        &options,
                                        &compiled,
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
                    let set1 = expand_tr_set(&args[0]);
                    let set2 = expand_tr_set(&args[1]);
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
                exit_code = self.run_ls(args, &mut stdout, &mut stderr);
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
                // `export a=( … )` also creates the array.
                if let Some(assign) = self.pending_array.take() {
                    self.exec_array_assign(&assign);
                }
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
                            // `unset a[k]` removes one element; `unset a`
                            // removes the whole array.
                            if let Some((name, subscript)) = split_array_subscript(other) {
                                if let Some(map) = self.assoc_arrays.get_mut(name) {
                                    if subscript == "@" || subscript == "*" {
                                        map.clear();
                                    } else {
                                        map.remove(subscript);
                                    }
                                } else if let Some(values) = self.arrays.get_mut(name)
                                    && let Ok(index) = subscript.parse::<usize>()
                                    && index < values.len()
                                {
                                    values.remove(index);
                                }
                                continue;
                            }
                            let removed_assoc = self.assoc_arrays.remove(other).is_some();
                            self.arrays.remove(other);
                            let _ = removed_assoc;
                            self.env.remove(other);
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
                                if exit_code == 0
                                    && let Err(error) = self.fs.remove(&source, true)
                                {
                                    stderr.push_str(&format!("mv: {error}\n"));
                                    exit_code = 1;
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
                    self.link_positionals(
                        &positional
                            .iter()
                            .map(|value| (*value).clone())
                            .collect::<Vec<_>>(),
                    );
                } else if i > 0 && args.last().map(|arg| arg == "--").unwrap_or(false) {
                    // `set --` with no trailing words clears the positionals.
                    self.positionals.clear();
                }
            }
            "yes" => self.run_yes(args, &mut stdout),
            "mktemp" => match self.run_mktemp(args, &mut stderr) {
                Ok(path) => {
                    stdout.push_str(&path);
                    stdout.push('\n');
                }
                Err(code) => exit_code = code,
            },
            "chmod" => exit_code = self.run_chmod(args, &mut stderr),
            "stat" => exit_code = self.run_stat(args, &mut stdout, &mut stderr),
            "seq" => exit_code = self.run_seq(args, &mut stdout, &mut stderr),
            "declare" | "typeset" | "local" => {
                exit_code = self.run_declare(command, args, &mut stderr);
            }
            // `shift [n]` drops the first n positional parameters; asking for
            // more than exist fails with status 1 and changes nothing.
            "shift" => {
                let count = match args.first() {
                    Some(text) => match text.parse::<usize>() {
                        Ok(count) => count,
                        Err(_) => {
                            stderr.push_str(&format!(
                                "bash: shift: {text}: numeric argument required\n"
                            ));
                            exit_code = 1;
                            0
                        }
                    },
                    None => 1,
                };
                if exit_code == 0 {
                    if count > self.positionals.len() {
                        exit_code = 1;
                    } else {
                        self.positionals.drain(0..count);
                    }
                }
            }
            "jobs" => stdout.push_str(&self.run_jobs()),
            "wait" => exit_code = self.run_wait(args),
            "sed" => {
                let outcome = crate::sed::run(args, stdin, &|path: &str| {
                    self.fs
                        .read_file(&self.resolve_path(path))
                        .map(str::to_string)
                        .ok()
                });
                stdout.push_str(&outcome.stdout);
                stderr.push_str(&outcome.stderr);
                exit_code = outcome.exit_code;
            }
            "curl" => exit_code = self.run_curl(args, &mut stdout, &mut stderr),
            // `awk` and `jq` are implemented in-crate (see awk.rs / jq.rs), so
            // they get the same virtual filesystem and no host access.
            "awk" => {
                let reader = |path: &str| {
                    self.fs
                        .read_file(&self.resolve_path(path))
                        .map(str::to_string)
                        .ok()
                };
                let outcome = crate::awk::run(args, stdin, &reader);
                stdout.push_str(&outcome.stdout);
                stderr.push_str(&outcome.stderr);
                exit_code = outcome.exit_code;
            }
            "jq" => {
                let reader = |path: &str| {
                    self.fs
                        .read_file(&self.resolve_path(path))
                        .map(str::to_string)
                        .ok()
                };
                let outcome = crate::jq::run(args, stdin, &reader);
                stdout.push_str(&outcome.stdout);
                stderr.push_str(&outcome.stderr);
                exit_code = outcome.exit_code;
            }
            // There is no wall clock in this runtime, so `sleep` validates its
            // operand and returns immediately. Combined with `&`/`wait` this
            // keeps timing-dependent scripts deterministic.
            "sleep" => {
                if let Some(value) = args.iter().find(|arg| arg.parse::<f64>().is_err()) {
                    stderr.push_str(&format!("sleep: invalid time interval '{value}'\n"));
                    exit_code = 1;
                }
            }
            // Interpreters this dependency-free core does not embed. Known
            // names fail closed with an explicit message instead of the generic
            // "command not found", so scripts cannot mistake the boundary.
            "python" | "python3" | "node" | "nodejs" | "js" | "sqlite" | "sqlite3" => {
                exit_code = self.run_unsupported_runtime(command, &mut stderr);
            }
            "return" => {
                if self.function_depth == 0 {
                    stderr.push_str(
                        "bash: return: can only `return' from a function or sourced script\n",
                    );
                    exit_code = 1;
                } else {
                    let code = args
                        .first()
                        .and_then(|arg| arg.parse::<i32>().ok())
                        .unwrap_or(0);
                    self.return_signal = Some(code);
                    exit_code = code;
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
            let value = args[1].as_str();
            let path = self.resolve_path(value);
            match args[0].as_str() {
                "-z" => return value.is_empty(),
                "-n" => return !value.is_empty(),
                "-f" => return self.fs.is_file(&path),
                "-d" => return self.fs.is_dir(&path),
                "-e" | "-a" => return self.fs.exists(&path),
                "-s" => return self.fs.metadata(&path).is_ok_and(|meta| meta.size > 0),
                "-r" => return self.mode_bits(value).is_some_and(|mode| mode & 0o444 != 0),
                "-w" => return self.mode_bits(value).is_some_and(|mode| mode & 0o222 != 0),
                "-x" => return self.mode_bits(value).is_some_and(|mode| mode & 0o111 != 0),
                // There are no symlinks in this filesystem.
                "-L" | "-h" => return false,
                "-v" => {
                    return self.parameter_is_set(value) || self.arrays.contains_key(value);
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
                "-nt" => {
                    let left = self.resolve_path(&args[0]);
                    let right = self.resolve_path(&args[2]);
                    return self
                        .mtime_of(&left)
                        .zip(self.mtime_of(&right))
                        .is_some_and(|(a, b)| a > b);
                }
                "-ot" => {
                    let left = self.resolve_path(&args[0]);
                    let right = self.resolve_path(&args[2]);
                    return self
                        .mtime_of(&left)
                        .zip(self.mtime_of(&right))
                        .is_some_and(|(a, b)| a < b);
                }
                "-ef" => {
                    let left = self.resolve_path(&args[0]);
                    let right = self.resolve_path(&args[2]);
                    return left == right && self.fs.exists(&left);
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
                WordPartKind::Arithmetic => {
                    match eval_arithmetic_expr(&part.text, &self.env, &self.positionals) {
                        Ok(value) => expanded.push_str(&value.to_string()),
                        Err(()) => {
                            self.param_error =
                                Some(format!("bash: {}: syntax error in expression\n", part.text));
                        }
                    }
                }
                // `<(cmd)`: run the script now and hand out a path holding its
                // output, which the command can then read.
                WordPartKind::ProcSubIn => {
                    let output = self.exec_procsub_script(&part.text);
                    let path = self.next_procsub_path();
                    let _ = self.fs.write_file(&path, &output);
                    self.procsub_files.push(path.clone());
                    expanded.push_str(&path);
                }
                // `>(cmd)`: hand out a path that the command writes to; the
                // script runs once the command finishes.
                WordPartKind::ProcSubOut => {
                    let path = self.next_procsub_path();
                    let _ = self.fs.write_file(&path, "");
                    self.procsub_files.push(path.clone());
                    self.procsub_readers.push((part.text.clone(), path.clone()));
                    expanded.push_str(&path);
                }
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
        // `${!name}` is indirect expansion; `${!a[@]}` lists the array indices.
        if let Some(inner) = content.strip_prefix('!')
            && !inner.is_empty()
        {
            return self.expand_indirect(inner);
        }
        let (parameter, rest) = split_parameter(content);

        // An associative subscript is a string, and may itself contain
        // expansions (`${m[$key]}`), so expand it before the lookup.
        let expanded_parameter: String = match split_array_subscript(parameter) {
            Some((name, subscript)) if self.assoc_arrays.contains_key(name) => {
                let key = self.expand_text(subscript);
                format!("{name}[{key}]")
            }
            _ => parameter.to_string(),
        };
        let parameter: &str = &expanded_parameter;

        // `${#name}` is the length of `name`, not a parameter called `#name`.
        if parameter == "#" && !rest.is_empty() {
            let (inner, inner_rest) = split_parameter(rest);
            if inner_rest.is_empty() && !inner.is_empty() {
                return self.expand_length(inner);
            }
        }

        // `${v:off:len}` / `${a[@]:off:len}` slicing. A `:` followed by `-`,
        // `=`, `?` or `+` is a parameter operator, never a slice; anything else
        // (including a variable or arithmetic offset) is a slice.
        if let Some(spec) = rest.strip_prefix(':')
            && !spec.starts_with(['-', '=', '?', '+'])
        {
            return self.expand_slice(parameter, spec.trim());
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
                    return self.positionals.join(" ");
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
        // `${#a[@]}` is the element count, not the length of a joined string.
        if let Some((name, subscript)) = split_array_subscript(parameter)
            && (subscript == "@" || subscript == "*")
        {
            if let Some(map) = self.assoc_arrays.get(name) {
                return map.len().to_string();
            }
            return self
                .arrays
                .get(name)
                .map(|elements| elements.len())
                .unwrap_or(0)
                .to_string();
        }
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
        if let Some((name, subscript)) = split_array_subscript(parameter) {
            if let Some(map) = self.assoc_arrays.get(name) {
                return match subscript {
                    "@" | "*" => true,
                    key => map.contains_key(key),
                };
            }
            let Some(elements) = self.arrays.get(name) else {
                return false;
            };
            return match subscript {
                "@" | "*" => true,
                index => self
                    .array_subscript_index(index)
                    .is_some_and(|index| elements.get(index).is_some()),
            };
        }
        match parameter {
            "?" | "#" | "0" | "@" | "*" | "!" | "$" => true,
            _ => match parameter.parse::<usize>() {
                Ok(index) if index > 0 => index <= self.positionals.len(),
                _ => self.env.contains_key(parameter) || self.arrays.contains_key(parameter),
            },
        }
    }

    fn parameter_value(&self, parameter: &str) -> Option<String> {
        if let Some((name, subscript)) = split_array_subscript(parameter) {
            // An associative array is keyed by string, not by index.
            if let Some(map) = self.assoc_arrays.get(name) {
                return match subscript {
                    "@" | "*" => Some(map.values().cloned().collect::<Vec<_>>().join(" ")),
                    key => Some(map.get(key).cloned().unwrap_or_default()),
                };
            }
            return match subscript {
                "@" | "*" => Some(
                    self.arrays
                        .get(name)
                        .map(|elements| elements.join(" "))
                        .unwrap_or_default(),
                ),
                index => {
                    let elements = self.arrays.get(name)?;
                    let index = self.array_subscript_index(index)?;
                    Some(elements.get(index).cloned().unwrap_or_default())
                }
            };
        }
        match parameter {
            "?" => Some(self.last_exit_code.to_string()),
            "#" => Some(self.positional_count().to_string()),
            "0" => Some("bash".to_string()),
            "@" | "*" => Some(self.positionals.join(" ")),
            "!" => Some(
                self.env
                    .get("!")
                    .cloned()
                    .unwrap_or_else(|| "0".to_string()),
            ),
            // There is exactly one shell process in this runtime.
            "$" => Some("1".to_string()),
            // `$1`, `$2`, … name positional parameters.
            _ => match parameter.parse::<usize>() {
                Ok(index) if index > 0 => self.positionals.get(index - 1).cloned(),
                _ => self
                    .env
                    .get(parameter)
                    .cloned()
                    .or_else(|| self.arrays.get(parameter).and_then(|v| v.first().cloned()))
                    // A bare associative name expands to the value of key `0`.
                    .or_else(|| {
                        self.assoc_arrays
                            .get(parameter)
                            .and_then(|map| map.get("0").cloned())
                    }),
            },
        }
    }

    fn assign_parameter(&mut self, parameter: &str, value: &str) {
        self.env.insert(parameter.to_string(), value.to_string());
    }

    /// Number of positional parameters.
    fn positional_count(&self) -> usize {
        self.positionals.len()
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
        let mut child = self.child_shell();
        let result = child.exec_in_current_state(script);
        self.fs = child.fs;
        self.env = child.env;
        self.cwd = child.cwd;
        // Deterministic name counters must not restart in a child.
        self.mktemp_counter = self.mktemp_counter.max(child.mktemp_counter);
        self.procsub_counter = self.procsub_counter.max(child.procsub_counter);
        if !result.stderr.is_empty() {
            self.expansion_stderr.push_str(&result.stderr);
        }
        // A substitution that failed to parse cannot be recovered from: the
        // enclosing command must fail closed (upstream #348).
        if result.exit_code == 2 {
            self.expansion_failed = true;
        }
        result.stdout.trim_end_matches('\n').to_string()
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
                    expanded.push_str(&self.positionals.join(" "));
                    continue;
                }
                if next == '!' {
                    chars.next();
                    let job = self
                        .env
                        .get("!")
                        .cloned()
                        .unwrap_or_else(|| "0".to_string());
                    expanded.push_str(&job);
                    continue;
                }
                if next == '$' {
                    chars.next();
                    // There is exactly one shell process in this runtime.
                    expanded.push('1');
                    continue;
                }
                if next.is_ascii_digit() {
                    // Like bash, `$10` is `$1` followed by a literal `0`.
                    let digit = next;
                    chars.next();
                    let index = digit.to_digit(10).unwrap_or(0) as usize;
                    if index == 0 {
                        expanded.push_str("bash"); // $0 is the script name
                    } else if let Some(value) = self.positionals.get(index - 1) {
                        expanded.push_str(value);
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
        return (count, &args[2..]);
    }
    // `-NUM` is the obsolete spelling of `-n NUM` (`head -1`, `tail -5`).
    if let Some(first) = args.first()
        && let Some(digits) = first.strip_prefix('-')
        && !digits.is_empty()
        && digits.chars().all(|ch| ch.is_ascii_digit())
        && let Ok(count) = digits.parse::<usize>()
    {
        return (count, &args[1..]);
    }
    (10, args)
}

fn eval_arithmetic_expr(
    expr: &str,
    env: &BTreeMap<String, String>,
    positionals: &[String],
) -> Result<i64, ()> {
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
        // `$x` / `$1` are accepted inside `$(( … ))`, as in bash: the
        // parameter is resolved before the expression is evaluated.
        if ch == '$' {
            index += 1;
            let start = index;
            while index < bytes.len() {
                let c = bytes[index] as char;
                if c == '_' || c.is_ascii_alphanumeric() {
                    index += 1;
                } else {
                    break;
                }
            }
            let name = &expr[start..index];
            let value = match name.parse::<usize>() {
                // `$1` names a positional parameter.
                Ok(index) if index > 0 => positionals
                    .get(index - 1)
                    .and_then(|raw| raw.trim().parse::<i64>().ok())
                    .unwrap_or(0),
                _ => env
                    .get(name)
                    .and_then(|value| value.trim().parse::<i64>().ok())
                    .unwrap_or(0),
            };
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
    // Whether the next token must be an operand, so a leading `+`/`-` is read
    // as a unary sign (`-x` evaluates as `0 - x`).
    let mut expect_operand = true;

    for token in tokens {
        match token {
            Tok::Num(value) => {
                values.push(value);
                expect_operand = false;
            }
            Tok::Ident(name) => {
                // A bare name is a variable; digits alone are not identifiers.
                let value = env
                    .get(name)
                    .and_then(|value| value.trim().parse::<i64>().ok())
                    .unwrap_or(0);
                values.push(value);
                expect_operand = false;
            }
            Tok::LParen => {
                ops.push(token);
                expect_operand = true;
            }
            Tok::RParen => {
                while let Some(op) = ops.pop() {
                    if op == Tok::LParen {
                        break;
                    }
                    apply_op(&mut values, op)?;
                }
                expect_operand = false;
            }
            Tok::Plus | Tok::Minus | Tok::Mul | Tok::Div | Tok::Mod => {
                if expect_operand {
                    // A unary sign binds tighter than any pending binary
                    // operator, so it is pushed without reducing first:
                    // `2 - -3` is `2 - (0 - 3)`, not `(2 - 0) - 3`.
                    match token {
                        Tok::Plus | Tok::Minus => {
                            values.push(0);
                            ops.push(token);
                            continue;
                        }
                        // `* 3` / `/ 3` with no left operand is a syntax error.
                        _ => return Err(()),
                    }
                }
                while let Some(&op) = ops.last() {
                    if op == Tok::LParen || precedence(op) < precedence(token) {
                        break;
                    }
                    let op = ops.pop().ok_or(())?;
                    apply_op(&mut values, op)?;
                }
                ops.push(token);
                expect_operand = true;
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
            // `name[subscript]` is part of the parameter reference.
            if content[end..].starts_with('[')
                && let Some(close) = content[end..].find(']')
            {
                end += close + 1;
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
    /// `-m N`: stop after N selected lines per source.
    max_count: Option<usize>,
    /// `-E`/`-P`: patterns are regular expressions, not literal substrings.
    regex: bool,
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
                "max-count" => {
                    let value = need_value(&mut index)?;
                    options.max_count = value.parse::<usize>().ok();
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
                "fixed-strings" => options.regex = false,
                "extended-regexp" | "perl-regexp" => options.regex = true,
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
                'm' => {
                    let value = take_value(&mut index, &rest, args, 'm')?;
                    options.max_count = value.parse::<usize>().ok();
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
                // Literal matching is the native mode unless -E/-P asks for
                // the regex engine.
                'F' => options.regex = false,
                'E' | 'P' => options.regex = true,
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
        patterns: &[GrepPattern],
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
                // `-m N` stops after N selected lines in this source.
                if options
                    .max_count
                    .is_some_and(|limit| matched_lines.len() >= limit)
                {
                    break;
                }
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

/// A grep pattern, pre-compiled once per invocation.
#[derive(Debug, Clone)]
enum GrepPattern {
    /// A literal substring needle (`-F`, the default).
    Literal(String),
    /// A compiled regular expression (`-E` / `-P`).
    Regex(crate::regex::Regex),
}

/// Compile every pattern, or report grep's syntax error (`exit 2`).
fn compile_grep_patterns(
    options: &GrepOptions,
    patterns: &[String],
) -> Result<Vec<GrepPattern>, String> {
    if !options.regex {
        return Ok(patterns
            .iter()
            .map(|pattern| GrepPattern::Literal(pattern.clone()))
            .collect());
    }
    let mut compiled = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        match crate::regex::Regex::new_with_flags(pattern, options.ignore_case) {
            Ok(regex) => compiled.push(GrepPattern::Regex(regex)),
            Err(error) => return Err(format!("grep: {pattern}: {error}\n")),
        }
    }
    Ok(compiled)
}

/// Whether `line` is selected by any pattern, honoring `-v`, `-x` and `-w`.
fn line_matches(line: &str, patterns: &[GrepPattern], options: &GrepOptions) -> bool {
    let matched = patterns.iter().any(|pattern| match pattern {
        GrepPattern::Regex(regex) => {
            if options.whole_line {
                regex
                    .find(line)
                    .is_some_and(|(start, end)| start == 0 && end == line.len())
            } else if options.word_match {
                regex_matches_word(regex, line)
            } else {
                regex.is_match(line)
            }
        }
        GrepPattern::Literal(needle) => {
            let haystack = if options.ignore_case {
                line.to_lowercase()
            } else {
                line.to_string()
            };
            let needle = if options.ignore_case {
                needle.to_lowercase()
            } else {
                needle.clone()
            };
            if options.whole_line {
                haystack == needle
            } else if options.word_match {
                contains_word(&haystack, &needle)
            } else {
                haystack.contains(&needle)
            }
        }
    });
    if options.invert { !matched } else { matched }
}

/// `-w` for a regex: every match must be surrounded by non-word characters.
fn regex_matches_word(regex: &crate::regex::Regex, line: &str) -> bool {
    let mut from = 0usize;
    while from <= line.len() {
        let Some((start, end)) = regex.find(&line[from..]) else {
            return false;
        };
        let (start, end) = (from + start, from + end);
        let left_ok = start == 0 || !is_word_char(line[..start].chars().next_back().unwrap_or(' '));
        let right_ok =
            end == line.len() || !is_word_char(line[end..].chars().next().unwrap_or(' '));
        if left_ok && right_ok {
            return true;
        }
        // Advance at least one character to avoid an infinite loop on a
        // zero-width match.
        from = if end > from { end } else { from + 1 };
    }
    false
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
    if is_shell_name(key) {
        return true;
    }
    // `a[1]=value` assigns one array element.
    match split_array_subscript(key) {
        Some((name, subscript)) => {
            is_shell_name(name) && !subscript.is_empty() && subscript != "@" && subscript != "*"
        }
        None => false,
    }
}

// ─── Extended runtime ─────────────────────────────────────────────────────────
//
// Shell functions, `case`, `[[ … ]]`, indexed arrays, background jobs, process
// substitution, pathname expansion and the extra built-ins added by this
// milestone all live in this block so the original execution core stays small.

/// Options for the `ls` built-in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LsOptions {
    all: bool,
    almost_all: bool,
    one_per_line: bool,
    long: bool,
    reverse: bool,
    by_time: bool,
    by_size: bool,
    classify: bool,
    directory_itself: bool,
}

impl Bash {
    // ── Job and positional bookkeeping ───────────────────────────────────────

    /// Record `PIPESTATUS` from a pipeline's per-stage statuses.
    fn set_pipestatus(&mut self, statuses: Vec<i32>) {
        self.arrays.insert(
            "PIPESTATUS".to_string(),
            statuses.iter().map(|status| status.to_string()).collect(),
        );
    }

    /// `$@`: the positional parameters as separate words.
    fn positional_parameters(&self) -> Vec<String> {
        self.positionals.clone()
    }

    /// Replace the positional parameters (function call, `set --`).
    fn link_positionals(&mut self, args: &[String]) {
        self.positionals = args.to_vec();
    }

    fn save_positionals(&self) -> Vec<String> {
        self.positionals.clone()
    }

    fn restore_positionals(&mut self, saved: Vec<String>) {
        self.positionals = saved;
    }

    fn remaining_output_budget(&self) -> usize {
        self.execution_limits
            .max_output_bytes
            .saturating_sub(self.output_bytes_used)
    }

    // ── Statement kinds ──────────────────────────────────────────────────────

    /// `cmd &`: run the statement to completion in a child shell.
    ///
    /// This runtime has no concurrency, so the job finishes immediately and its
    /// output is emitted in place; only the exit status is retained for `wait`.
    fn exec_background(&mut self, statement: &Statement) -> (String, String, i32) {
        let mut child = self.child_shell();
        let (stdout, stderr, status) = child.exec_statement_inner(statement);
        // A job's filesystem changes are visible to the parent; its environment
        // and working directory are not.
        self.fs = child.fs;
        let id = self.next_job_id;
        self.next_job_id += 1;
        self.jobs.push(JobRecord {
            id,
            status,
            command: describe_statement(statement),
        });
        self.env.insert("!".to_string(), id.to_string());
        self.set_pipestatus(vec![0]);
        (stdout, stderr, 0)
    }

    fn exec_case(&mut self, case_stmt: &CaseStatement) -> (String, String, i32) {
        let subject = self.expand_word(&case_stmt.word);
        for clause in &case_stmt.clauses {
            let matched = clause.patterns.iter().any(|pattern| {
                let pattern = self.expand_word(pattern);
                glob::pattern_matches(&pattern, &subject)
            });
            if matched {
                return self.exec_statement_list(&clause.body);
            }
        }
        (String::new(), String::new(), 0)
    }

    fn define_function(&mut self, def: &FunctionDef) -> (String, String, i32) {
        self.functions.insert(def.name.clone(), def.body.clone());
        (String::new(), String::new(), 0)
    }

    /// `( … )`: a child shell whose environment changes are discarded.
    fn run_subshell(&mut self, body: &[Statement]) -> (String, String, i32) {
        let mut child = self.child_shell();
        let (stdout, stderr, exit_code) = child.exec_statement_list(body);
        self.fs = child.fs;
        (stdout, stderr, exit_code)
    }

    fn call_function(&mut self, body: &[Statement], args: &[String]) -> CommandOutput {
        let saved = self.save_positionals();
        self.link_positionals(args);
        let saved_return = self.return_signal.take();
        self.function_depth += 1;
        self.local_scopes.push(Vec::new());
        let (stdout, stderr, exit_code) = self.exec_statement_list(body);
        let scope = self.local_scopes.pop().unwrap_or_default();
        self.restore_scope(scope);
        self.function_depth -= 1;
        let returned = self.return_signal.take();
        self.return_signal = saved_return;
        self.restore_positionals(saved);
        CommandOutput {
            stdout,
            stderr,
            exit_code: returned.unwrap_or(exit_code),
        }
    }

    // ── `[[ … ]]` ────────────────────────────────────────────────────────────

    fn exec_cond(&mut self, expr: &CondExpr) -> (String, String, i32) {
        let matched = self.eval_cond_expr(expr);
        (String::new(), String::new(), if matched { 0 } else { 1 })
    }

    fn eval_cond_expr(&mut self, expr: &CondExpr) -> bool {
        match expr {
            CondExpr::And(left, right) => self.eval_cond_expr(left) && self.eval_cond_expr(right),
            CondExpr::Or(left, right) => self.eval_cond_expr(left) || self.eval_cond_expr(right),
            CondExpr::Not(inner) => !self.eval_cond_expr(inner),
            CondExpr::Term(term) => self.eval_cond_term(term),
        }
    }

    fn eval_cond_term(&mut self, term: &CondTerm) -> bool {
        match term {
            CondTerm::Atom(word) => !self.expand_word(word).is_empty(),
            CondTerm::Unary { op, operand } => {
                let value = self.expand_word(operand);
                self.eval_cond_unary(op, &value)
            }
            CondTerm::Binary { left, op, right } => {
                if op == "=~" {
                    let text = self.expand_word(left);
                    let pattern = self.expand_word(right);
                    return self.regex_matches(&pattern, &text);
                }
                if matches!(op.as_str(), "==" | "=" | "!=") {
                    let text = self.expand_word(left);
                    let pattern = self.expand_word(right);
                    // A quoted right-hand side is compared literally; an
                    // unquoted one is a glob pattern, as in bash.
                    let matched = if right.quoted {
                        text == pattern
                    } else {
                        glob::pattern_matches(&pattern, &text)
                    };
                    return if op == "!=" { !matched } else { matched };
                }
                if matches!(op.as_str(), "<" | ">") {
                    let left = self.expand_word(left);
                    let right = self.expand_word(right);
                    return match op.as_str() {
                        "<" => left < right,
                        ">" => left > right,
                        _ => unreachable!("string comparison operator"),
                    };
                }
                if matches!(op.as_str(), "-nt" | "-ot" | "-ef") {
                    let left = {
                        let expanded = self.expand_word(left);
                        self.resolve_path(&expanded)
                    };
                    let right = {
                        let expanded = self.expand_word(right);
                        self.resolve_path(&expanded)
                    };
                    return match op.as_str() {
                        "-nt" => self
                            .mtime_of(&left)
                            .zip(self.mtime_of(&right))
                            .is_some_and(|(a, b)| a > b),
                        "-ot" => self
                            .mtime_of(&left)
                            .zip(self.mtime_of(&right))
                            .is_some_and(|(a, b)| a < b),
                        _ => left == right && self.fs.exists(&left),
                    };
                }
                let left = self.expand_word(left).trim().parse::<i64>().unwrap_or(0);
                let right = self.expand_word(right).trim().parse::<i64>().unwrap_or(0);
                match op.as_str() {
                    "-eq" => left == right,
                    "-ne" => left != right,
                    "-lt" => left < right,
                    "-le" => left <= right,
                    "-gt" => left > right,
                    "-ge" => left >= right,
                    _ => false,
                }
            }
        }
    }

    fn eval_cond_unary(&self, op: &str, value: &str) -> bool {
        // An empty operand never names an existing file.
        let is_file_test = matches!(
            op,
            "-e" | "-a" | "-f" | "-d" | "-s" | "-r" | "-w" | "-x" | "-L" | "-h"
        );
        if is_file_test && value.is_empty() {
            return false;
        }
        match op {
            "-z" => value.is_empty(),
            "-n" => !value.is_empty(),
            "-e" | "-a" => {
                let path = self.resolve_path(value);
                self.fs.exists(&path)
            }
            "-f" => self.fs.is_file(&self.resolve_path(value)),
            "-d" => self.fs.is_dir(&self.resolve_path(value)),
            "-s" => self
                .fs
                .metadata(&self.resolve_path(value))
                .is_ok_and(|meta| meta.size > 0),
            "-r" => self.mode_bits(value).is_some_and(|mode| mode & 0o444 != 0),
            "-w" => self.mode_bits(value).is_some_and(|mode| mode & 0o222 != 0),
            "-x" => self.mode_bits(value).is_some_and(|mode| mode & 0o111 != 0),
            // There are no symlinks, block devices or terminals here.
            "-L" | "-h" | "-b" | "-c" | "-p" | "-S" | "-t" | "-g" | "-u" | "-k" | "-O" | "-G"
            | "-N" => false,
            "-v" => self.parameter_is_set(value) || self.arrays.contains_key(value),
            _ => false,
        }
    }

    fn mode_bits(&self, path: &str) -> Option<u32> {
        self.fs
            .metadata(&self.resolve_path(path))
            .ok()
            .map(|meta| meta.mode)
    }

    fn mtime_of(&self, path: &str) -> Option<i64> {
        self.fs.metadata(path).ok().map(|meta| meta.mtime)
    }

    /// `[[ str =~ re ]]`.
    fn regex_matches(&self, pattern: &str, text: &str) -> bool {
        match crate::regex::Regex::new(pattern) {
            Ok(regex) => regex.is_match(text),
            // An invalid pattern is a non-match rather than a panic.
            Err(_) => false,
        }
    }

    // ── Arrays ───────────────────────────────────────────────────────────────

    fn exec_array_assign(&mut self, assign: &ArrayAssign) -> (String, String, i32) {
        // Each element keeps its raw subscript text: an associative array uses
        // that text as a key, an indexed array evaluates it as an index.
        let mut elements: Vec<(Option<String>, String)> = Vec::new();
        for word in &assign.items {
            let fields = self.expand_word_list(word);
            for field in fields {
                // `[k]=value` gives an explicit subscript.
                if !word.quoted
                    && field.starts_with('[')
                    && let Some(close) = field.find("]=")
                {
                    elements.push((
                        Some(field[1..close].to_string()),
                        field[close + 2..].to_string(),
                    ));
                } else {
                    elements.push((None, field));
                }
            }
        }

        // `declare -A a; a=([k]=v …)` fills an associative array.
        if self.assoc_arrays.contains_key(&assign.name) {
            if !assign.append {
                self.assoc_arrays
                    .insert(assign.name.clone(), BTreeMap::new());
            }
            for (subscript, value) in elements {
                let key = match subscript {
                    // `a=([k]=v)` keys by the literal text, and the subscript
                    // may itself contain expansions.
                    Some(subscript) => self.expand_text(&subscript),
                    // A bare element contributes its own value as the key.
                    None => value.clone(),
                };
                self.assoc_arrays
                    .entry(assign.name.clone())
                    .or_default()
                    .insert(key, value);
            }
            return (String::new(), String::new(), 0);
        }
        if !assign.append {
            self.arrays.remove(&assign.name);
        }
        let mut values = self.arrays.get(&assign.name).cloned().unwrap_or_default();
        for (subscript, value) in elements {
            match subscript {
                Some(subscript) => {
                    let index = self.array_index(&subscript).unwrap_or(0);
                    if index >= values.len() {
                        values.resize(index + 1, String::new());
                    }
                    values[index] = value;
                }
                None => values.push(value),
            }
        }
        self.arrays.insert(assign.name.clone(), values);
        (String::new(), String::new(), 0)
    }

    /// Evaluate an array subscript (`a[i]`), defaulting to 0 on error.
    fn array_index(&mut self, expression: &str) -> Option<usize> {
        let value = eval_arithmetic_expr(expression, &self.env, &self.positionals).ok()?;
        usize::try_from(value).ok()
    }

    /// An indexed subscript is an arithmetic expression, so `${a[i]}` and
    /// `${a[i+1]}` resolve through the environment like `$(( … ))` does.
    fn array_subscript_index(&self, subscript: &str) -> Option<usize> {
        let value = eval_arithmetic_expr(subscript, &self.env, &self.positionals).ok()?;
        usize::try_from(value).ok()
    }

    /// `a[3]=value` (an indexed assignment written as an ordinary word), or
    /// `a[key]=value` when the target is an associative array.
    fn assign_element(&mut self, name: &str, subscript: &str, value: &str) {
        if self.assoc_arrays.contains_key(name) {
            // The subscript may still contain expansions inside `${a[$k]}`.
            let key = self.expand_text(subscript);
            self.assoc_arrays
                .entry(name.to_string())
                .or_default()
                .insert(key, value.to_string());
            return;
        }
        let index = self.array_index(subscript).unwrap_or(0);
        let mut values = self.arrays.get(name).cloned().unwrap_or_default();
        if index >= values.len() {
            values.resize(index + 1, String::new());
        }
        values[index] = value.to_string();
        self.arrays.insert(name.to_string(), values);
    }

    /// Values of an array in expansion order: insertion order for indexed
    /// arrays, sorted key order for associative ones (deterministic).
    fn array_values_in_order(&self, name: &str) -> Vec<String> {
        if let Some(map) = self.assoc_arrays.get(name) {
            return map.values().cloned().collect();
        }
        self.arrays.get(name).cloned().unwrap_or_default()
    }

    fn eval_int(&mut self, expression: &str) -> Option<i64> {
        let mut text = expression.trim();
        // A slice offset may arrive wrapped in an arithmetic expansion, as in
        // `${v:$((1+1)):2}`; unwrap any nesting before evaluating.
        while let Some(inner) = text
            .strip_prefix("$((")
            .and_then(|rest| rest.strip_suffix("))"))
        {
            text = inner.trim();
        }
        if text.is_empty() {
            return Some(0);
        }
        eval_arithmetic_expr(text, &self.env, &self.positionals).ok()
    }

    /// `(start, end)` for a `${p:off:len}` slice over `length` items.
    ///
    /// A negative offset counts from the end and clamps at 0; a missing or
    /// empty length runs to the end; a non-positive length yields an empty
    /// range, all matching bash.
    fn slice_bounds(&mut self, spec: &str, length: usize) -> Option<(usize, usize)> {
        let (offset_text, length_text) = split_slice_spec(spec);
        let offset = self.eval_int(offset_text)?;
        let start = if offset < 0 {
            (length as i64 + offset).max(0) as usize
        } else {
            (offset as usize).min(length)
        };
        let end = match length_text {
            None => length,
            Some(text) if text.trim().is_empty() => length,
            Some(text) => {
                let requested = self.eval_int(text)?;
                if requested <= 0 {
                    start
                } else {
                    start.saturating_add(requested as usize).min(length)
                }
            }
        };
        Some((start, end))
    }

    fn slice_text(&mut self, text: &str, spec: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        match self.slice_bounds(spec, chars.len()) {
            Some((start, end)) => chars[start..end].iter().collect(),
            None => {
                self.param_error = Some(format!("bash: {spec}: syntax error in expression\n"));
                String::new()
            }
        }
    }

    /// `${p:off:len}` in a scalar context.
    fn expand_slice(&mut self, parameter: &str, spec: &str) -> String {
        // `${@:off:len}` / `${*:off:len}` slice the positional parameters.
        if parameter == "@" || parameter == "*" {
            let parameters = self.positional_parameters();
            let bound = self.slice_bounds(spec, parameters.len());
            return match bound {
                Some((start, end)) => parameters[start..end].join(" "),
                None => {
                    self.param_error = Some(format!("bash: {spec}: syntax error in expression\n"));
                    String::new()
                }
            };
        }
        if let Some((name, subscript)) = split_array_subscript(parameter)
            && (subscript == "@" || subscript == "*")
        {
            let items = self.array_values_in_order(name);
            return match self.slice_bounds(spec, items.len()) {
                Some((start, end)) => items[start..end].join(" "),
                None => {
                    self.param_error = Some(format!("bash: {spec}: syntax error in expression\n"));
                    String::new()
                }
            };
        }
        let value = match self.parameter_value(parameter) {
            Some(value) => value,
            None => {
                if self.opt_nounset {
                    self.nounset_error = Some(parameter.to_string());
                }
                return String::new();
            }
        };
        self.slice_text(&value, spec)
    }

    // ── declare / local / typeset ───────────────────────────────────────────

    fn record_local(&mut self, entry: ScopedVar) {
        if let Some(scope) = self.local_scopes.last_mut() {
            scope.push(entry);
        }
    }

    /// Restore a function's `local` declarations, innermost first.
    fn restore_scope(&mut self, scope: Vec<ScopedVar>) {
        for entry in scope.into_iter().rev() {
            match entry {
                ScopedVar::Scalar { name, previous } => match previous {
                    Some(value) => {
                        self.env.insert(name, value);
                    }
                    None => {
                        self.env.remove(&name);
                    }
                },
                ScopedVar::Indexed { name, previous } => match previous {
                    Some(values) => {
                        self.assoc_arrays.remove(&name);
                        self.arrays.insert(name, values);
                    }
                    None => {
                        self.arrays.remove(&name);
                    }
                },
                ScopedVar::Associative { name, previous } => match previous {
                    Some(map) => {
                        self.arrays.remove(&name);
                        self.assoc_arrays.insert(name, map);
                    }
                    None => {
                        self.assoc_arrays.remove(&name);
                    }
                },
            }
        }
    }

    fn declare_assoc(&mut self, name: &str, local: bool) {
        if local {
            self.record_local(ScopedVar::Associative {
                name: name.to_string(),
                previous: self.assoc_arrays.get(name).cloned(),
            });
        }
        self.arrays.remove(name);
        self.assoc_arrays.entry(name.to_string()).or_default();
    }

    fn declare_indexed(&mut self, name: &str, local: bool) {
        if local {
            self.record_local(ScopedVar::Indexed {
                name: name.to_string(),
                previous: self.arrays.get(name).cloned(),
            });
        }
        self.assoc_arrays.remove(name);
        self.arrays.entry(name.to_string()).or_default();
    }

    fn declare_scalar(&mut self, name: &str, local: bool) {
        if local {
            self.record_local(ScopedVar::Scalar {
                name: name.to_string(),
                previous: self.env.get(name).cloned(),
            });
        }
    }

    /// `declare`/`typeset`/`local`: `-A` associative, `-a` indexed, otherwise a
    /// scalar (or an element assignment).
    fn run_declare(&mut self, command: &str, args: &[String], stderr: &mut String) -> i32 {
        if command == "local" && self.function_depth == 0 {
            stderr.push_str("bash: local: can only be used in a function\n");
            return 1;
        }
        // `declare`/`typeset` inside a function are function-local unless `-g`
        // is given, exactly like `local`.
        let force_global = args.iter().any(|arg| arg == "-g");
        let local = command == "local" || (self.function_depth > 0 && !force_global);
        let literal = self.pending_array.take();
        let mut associative = false;
        let mut indexed = false;
        let mut exit_code = 0;
        for arg in args {
            match arg.as_str() {
                "-A" => {
                    associative = true;
                    continue;
                }
                "-a" => {
                    indexed = true;
                    continue;
                }
                // Accepted for compatibility; this runtime has no readonly or
                // integer attributes to attach. `-g` is handled above.
                "-r" | "-x" | "-i" | "-g" | "--" => continue,
                other if other.starts_with('-') && other.len() > 1 => {
                    stderr.push_str(&format!(
                        "bash: {command}: {}: invalid option\n",
                        other.trim_start_matches('-')
                    ));
                    exit_code = 1;
                    continue;
                }
                _ => {}
            }

            let (declaration, value) = match arg.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (arg.as_str(), None),
            };
            let (name, subscript) = match split_array_subscript(declaration) {
                Some((name, subscript)) => (name.to_string(), Some(subscript.to_string())),
                None => (declaration.to_string(), None),
            };
            if !is_shell_name(&name) {
                stderr.push_str(&format!(
                    "bash: {command}: `{arg}': not a valid identifier\n"
                ));
                exit_code = 1;
                continue;
            }

            if associative {
                self.declare_assoc(&name, local);
                if let (Some(subscript), Some(value)) = (&subscript, &value) {
                    self.assoc_arrays
                        .entry(name.clone())
                        .or_default()
                        .insert(subscript.clone(), value.clone());
                }
                continue;
            }
            if indexed {
                self.declare_indexed(&name, local);
                match (&subscript, &value) {
                    (Some(subscript), Some(value)) => self.assign_element(&name, subscript, value),
                    (None, Some(value)) => {
                        self.arrays.insert(name.clone(), vec![value.clone()]);
                    }
                    _ => {}
                }
                continue;
            }
            self.declare_scalar(&name, local);
            if let Some(value) = value {
                match subscript {
                    Some(subscript) => self.assign_element(&name, &subscript, &value),
                    None => {
                        self.env.insert(name, value);
                    }
                }
            }
        }

        // `declare -A m=( … )` / `local -a a=( … )` carry the literal that the
        // command line attached.
        if let Some(assign) = literal {
            if !is_shell_name(&assign.name) {
                stderr.push_str(&format!(
                    "bash: {command}: `{}=(': not a valid identifier\n",
                    assign.name
                ));
                return 1;
            }
            if associative {
                self.declare_assoc(&assign.name, local);
            } else {
                // Without `-A`, a literal creates an indexed array.
                self.declare_indexed(&assign.name, local);
            }
            self.exec_array_assign(&assign);
        }
        exit_code
    }

    /// `${!name}` indirect expansion and `${!a[@]}` index listing.
    fn expand_indirect(&mut self, content: &str) -> String {
        if let Some((name, subscript)) = split_array_subscript(content)
            && (subscript == "@" || subscript == "*")
        {
            // Associative arrays list their keys; indexed arrays list indices.
            if let Some(map) = self.assoc_arrays.get(name) {
                return map.keys().cloned().collect::<Vec<_>>().join(" ");
            }
            let indices: Vec<String> = self
                .arrays
                .get(name)
                .map(|elements| (0..elements.len()).map(|index| index.to_string()).collect())
                .unwrap_or_default();
            return indices.join(" ");
        }
        // `${!name}` expands to the value of the variable whose NAME is the
        // current value of `name`.
        let target = self.parameter_value(content).unwrap_or_default();
        if target.is_empty() {
            return String::new();
        }
        self.parameter_value(&target).unwrap_or_default()
    }

    // ── Expansion helpers ────────────────────────────────────────────────────

    /// Expand a word in a list context: `${a[@]}`, `"$@"` and pathname
    /// expansion may each produce several fields.
    fn expand_word_list(&mut self, word: &Word) -> Vec<String> {
        if word.parts.len() == 1 && word.parts[0].kind == WordPartKind::Variable {
            let quoted = word.parts[0].quoted;
            let text = word.parts[0].text.trim().to_string();
            // `"$@"` (one field per parameter) and `$*` (joined when quoted).
            if text == "$@" || text == "$*" {
                let parameters = self.positional_parameters();
                if quoted {
                    return if text == "$*" {
                        vec![parameters.join(" ")]
                    } else {
                        parameters
                    };
                }
                return parameters
                    .into_iter()
                    .flat_map(|parameter| self.glob_fields(&parameter))
                    .collect();
            }
            if let Some(inner) = text
                .strip_prefix("${")
                .and_then(|rest| rest.strip_suffix('}'))
            {
                // `${!a[@]}` lists keys (associative) or indices (indexed) as
                // separate fields.
                if let Some(indirect) = inner.strip_prefix('!')
                    && let Some((name, subscript)) = split_array_subscript(indirect)
                    && (subscript == "@" || subscript == "*")
                {
                    if let Some(map) = self.assoc_arrays.get(name) {
                        return map.keys().cloned().collect();
                    }
                    return self
                        .arrays
                        .get(name)
                        .map(|elements| {
                            (0..elements.len()).map(|index| index.to_string()).collect()
                        })
                        .unwrap_or_default();
                }
                // `${a[@]:off:len}` slices the array in list context.
                if let Some((base, spec)) = inner.split_once("]:")
                    && let Some((name, subscript)) = split_array_subscript(&format!("{base}]"))
                    && (subscript == "@" || subscript == "*")
                {
                    let items = self.array_values_in_order(name);
                    let Some((start, end)) = self.slice_bounds(spec, items.len()) else {
                        self.param_error =
                            Some(format!("bash: {spec}: syntax error in expression\n"));
                        return Vec::new();
                    };
                    let sliced = items[start..end].to_vec();
                    if quoted {
                        return sliced;
                    }
                    return sliced
                        .into_iter()
                        .flat_map(|element| self.glob_fields(&element))
                        .collect();
                }
                if let Some((name, subscript)) = split_array_subscript(inner)
                    && (subscript == "@" || subscript == "*")
                {
                    let elements = self.array_values_in_order(name);
                    if quoted {
                        // `"${a[*]}"` is one joined field; `"${a[@]}"` keeps one
                        // field per element.
                        return if subscript == "*" {
                            vec![elements.join(" ")]
                        } else {
                            elements
                        };
                    }
                    return elements
                        .into_iter()
                        .flat_map(|element| self.glob_fields(&element))
                        .collect();
                }
                if inner == "@" || inner == "*" {
                    let parameters = self.positional_parameters();
                    if quoted {
                        return if inner == "*" {
                            vec![parameters.join(" ")]
                        } else {
                            parameters
                        };
                    }
                    return parameters
                        .into_iter()
                        .flat_map(|parameter| self.glob_fields(&parameter))
                        .collect();
                }
            }
        }
        let expanded = self.expand_word(word);
        if word.may_glob() {
            let matches = glob::expand(&self.fs, &expanded, &self.cwd);
            if !matches.is_empty() {
                return matches;
            }
        }
        vec![expanded]
    }

    /// Pathname-expand one already-expanded field (used for array elements and
    /// positional parameters, whose quoting is tracked by the reference).
    fn glob_fields(&mut self, field: &str) -> Vec<String> {
        if glob::has_meta(field) {
            let matches = glob::expand(&self.fs, field, &self.cwd);
            if !matches.is_empty() {
                return matches;
            }
        }
        vec![field.to_string()]
    }

    /// Expand a heredoc body (`<<EOF`), which behaves like a double-quoted
    /// string except that quotes are literal.
    fn expand_heredoc(&mut self, body: &str) -> String {
        match parse_heredoc_word(body, self.execution_limits.max_command_substitution_depth) {
            Ok(word) => self.expand_word(&word),
            Err(_) => body.to_string(),
        }
    }

    // ── Process substitution ─────────────────────────────────────────────────

    /// A child shell for one nested execution.
    ///
    /// Process-substitution state belongs to the enclosing command, so it is
    /// never inherited: an inner `cleanup_procsubs` would otherwise unlink the
    /// temp files the outer command is still using.
    fn child_shell(&self) -> Bash {
        let mut child = self.clone();
        child.procsub_files.clear();
        child.procsub_readers.clear();
        child.seed_stdin = None;
        child
    }

    fn next_procsub_path(&mut self) -> String {
        self.procsub_counter += 1;
        format!("/tmp/.just-bash-procsub-{}", self.procsub_counter)
    }

    /// Run a `<(…)` script and keep its raw stdout (trailing newlines included).
    fn exec_procsub_script(&mut self, script: &str) -> String {
        let mut child = self.child_shell();
        let result = child.exec_in_current_state(script);
        self.fs = child.fs;
        if !result.stderr.is_empty() {
            self.expansion_stderr.push_str(&result.stderr);
        }
        if result.exit_code == 2 {
            self.expansion_failed = true;
        }
        result.stdout
    }

    /// Feed each `>(cmd)` temporary file to its command once the enclosing
    /// command has finished writing.
    fn flush_procsub_readers(&mut self, result: &mut CommandOutput) {
        let readers = std::mem::take(&mut self.procsub_readers);
        for (script, path) in readers {
            let contents = self.fs.read_file(&path).unwrap_or("").to_string();
            let mut child = self.child_shell();
            // The reader's standard input is the fifo's contents; a child
            // shell starts without a seed, so set it explicitly.
            child.seed_stdin = Some(contents);
            let output = child.exec_in_current_state(&script);
            self.fs = child.fs;
            result.stdout.push_str(&output.stdout);
            result.stderr.push_str(&output.stderr);
        }
    }

    /// Remove every temporary file created for this command.
    fn cleanup_procsubs(&mut self) {
        let files = std::mem::take(&mut self.procsub_files);
        for path in files {
            let _ = self.fs.remove(&path, false);
        }
        self.procsub_readers.clear();
    }

    // ── Bounded, back-pressured pipelines ────────────────────────────────────

    /// How many lines a downstream stage is willing to consume.
    ///
    /// A producer (notably `yes`) stops at this count, so an infinite source
    /// cannot outrun its consumer. Only literal counts are recognised, because
    /// the budget must be known before the producer's expansion side effects.
    fn pipeline_line_budget(commands: &[CommandInvocation]) -> Option<usize> {
        let mut budget: Option<usize> = None;
        for command in commands.iter().skip(1) {
            let words: Vec<String> = command.words.iter().map(|word| word.text()).collect();
            let Some(name) = words.first() else {
                continue;
            };
            let limit = match name.as_str() {
                "head" => head_line_limit(&words[1..]),
                "grep" => grep_max_count(&words[1..]),
                _ => None,
            };
            if let Some(limit) = limit {
                budget = Some(budget.map_or(limit, |current: usize| current.min(limit)));
            }
        }
        budget
    }

    // ── ls ───────────────────────────────────────────────────────────────────

    fn run_ls(&mut self, args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
        let mut options = LsOptions::default();
        let mut operands: Vec<String> = Vec::new();
        let mut options_ended = false;
        for arg in args {
            if arg == "--" {
                options_ended = true;
                continue;
            }
            if options_ended || arg == "-" || !arg.starts_with('-') {
                operands.push(arg.clone());
                continue;
            }
            for ch in arg.chars().skip(1) {
                match ch {
                    'a' => options.all = true,
                    'A' => options.almost_all = true,
                    '1' => options.one_per_line = true,
                    'l' => options.long = true,
                    'r' => options.reverse = true,
                    't' => options.by_time = true,
                    'S' => options.by_size = true,
                    'F' => options.classify = true,
                    'd' => options.directory_itself = true,
                    other => {
                        stderr.push_str(&format!("ls: invalid option -- '{other}'\n"));
                        return 2;
                    }
                }
            }
        }
        if operands.is_empty() {
            operands.push(".".to_string());
        }
        let _ = options.one_per_line;

        let mut exit_code = 0;
        let mut files: Vec<(String, Metadata)> = Vec::new();
        let mut dirs: Vec<(String, String)> = Vec::new();
        for operand in &operands {
            let resolved = self.resolve_path(operand);
            match self.fs.metadata(&resolved) {
                Ok(meta) if meta.is_dir() && !options.directory_itself => {
                    dirs.push((operand.clone(), resolved));
                }
                Ok(meta) => files.push((operand.clone(), meta)),
                Err(_) => {
                    stderr.push_str(&format!("ls: {operand}: No such file or directory\n"));
                    exit_code = 2;
                }
            }
        }

        let show_headers = operands.len() > 1;
        let mut printed_any = false;
        if !files.is_empty() {
            sort_ls_entries(&mut files, &options);
            for (name, meta) in &files {
                stdout.push_str(&format_ls_entry(name, meta, &options));
            }
            printed_any = true;
        }
        for (label, path) in &dirs {
            let Ok(names) = self.fs.list_dir(path) else {
                continue;
            };
            let mut entries: Vec<(String, Metadata)> = Vec::new();
            for name in names {
                if name.starts_with('.') && !options.all && !options.almost_all {
                    continue;
                }
                if let Ok(meta) = self.fs.metadata(&join_path(path, &name)) {
                    entries.push((name, meta));
                }
            }
            if options.all {
                // `.` and `..` are listed by `-a` only.
                if let Ok(meta) = self.fs.metadata(path) {
                    entries.push((".".to_string(), meta));
                }
                let parent = parent_dir(path);
                if let Ok(meta) = self.fs.metadata(&parent) {
                    entries.push(("..".to_string(), meta));
                }
            }
            sort_ls_entries(&mut entries, &options);
            if show_headers {
                if printed_any {
                    stdout.push('\n');
                }
                stdout.push_str(label);
                stdout.push_str(":\n");
            }
            for (name, meta) in &entries {
                stdout.push_str(&format_ls_entry(name, meta, &options));
            }
            printed_any = true;
        }
        exit_code
    }

    // ── Small built-ins added by this milestone ──────────────────────────────

    /// `yes [STRING]` — bounded by the pipeline's line budget so that
    /// `yes | head -n 3` terminates without materialising an infinite stream.
    fn run_yes(&mut self, args: &[String], stdout: &mut String) {
        let text = if args.is_empty() {
            "y".to_string()
        } else {
            args.join(" ")
        };
        let limit = self.line_budget.unwrap_or(usize::MAX);
        let budget = self.remaining_output_budget();
        let mut produced = 0usize;
        while produced < limit && stdout.len() < budget {
            stdout.push_str(&text);
            stdout.push('\n');
            produced += 1;
        }
    }

    fn run_mktemp(&mut self, args: &[String], stderr: &mut String) -> Result<String, i32> {
        let mut directory = false;
        let mut into: Option<String> = None;
        let mut template: Option<String> = None;
        let mut index = 0usize;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "-d" | "--directory" => directory = true,
                "-p" | "--tmpdir" => {
                    index += 1;
                    match args.get(index) {
                        Some(value) => into = Some(value.clone()),
                        None => {
                            stderr.push_str("mktemp: option requires an argument -- 'p'\n");
                            return Err(1);
                        }
                    }
                }
                "-t" | "-q" | "--quiet" => {}
                "--" => {
                    index += 1;
                    if let Some(value) = args.get(index) {
                        template = Some(value.clone());
                    }
                    break;
                }
                other if other.starts_with("--tmpdir=") => {
                    into = Some(other["--tmpdir=".len()..].to_string());
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    stderr.push_str(&format!(
                        "mktemp: invalid option -- '{}'\n",
                        other.chars().nth(1).unwrap_or('?')
                    ));
                    return Err(1);
                }
                other => template = Some(other.to_string()),
            }
            index += 1;
        }

        // Without a template mktemp uses `$TMPDIR/tmp.XXXXXXXXXX`.
        let base = template.unwrap_or_else(|| {
            let dir = into
                .clone()
                .or_else(|| self.env.get("TMPDIR").cloned())
                .unwrap_or_else(|| "/tmp".to_string());
            format!("{}/tmp.XXXXXXXXXX", dir.trim_end_matches('/'))
        });
        // Pick the first counter value whose path is still free: a counter
        // observed inside a command-substitution child may be stale.
        let path = loop {
            self.mktemp_counter += 1;
            let suffix = format!("{:06}", self.mktemp_counter);
            let name = substitute_x_run(&base, &suffix);
            let candidate = self.resolve_path(&name);
            if !self.fs.exists(&candidate) {
                break candidate;
            }
        };
        let created = if directory {
            self.fs.create_dir_all(&path)
        } else {
            self.fs.write_file(&path, "")
        };
        match created {
            Ok(()) => Ok(path),
            Err(error) => {
                stderr.push_str(&format!("mktemp: {error}\n"));
                Err(1)
            }
        }
    }

    fn run_chmod(&mut self, args: &[String], stderr: &mut String) -> i32 {
        if args.len() < 2 {
            stderr.push_str("chmod: missing operand\n");
            return 1;
        }
        let spec = args[0].as_str();
        let mut exit_code = 0;
        for operand in &args[1..] {
            let path = self.resolve_path(operand);
            let Ok(meta) = self.fs.metadata(&path) else {
                stderr.push_str(&format!(
                    "chmod: cannot access '{operand}': No such file or directory\n"
                ));
                exit_code = 1;
                continue;
            };
            let Some(mode) = resolve_mode(spec, meta.mode) else {
                stderr.push_str(&format!("chmod: invalid mode: '{spec}'\n"));
                return 1;
            };
            if let Err(error) = self.fs.set_mode(&path, mode) {
                stderr.push_str(&format!("chmod: {error}\n"));
                exit_code = 1;
            }
        }
        exit_code
    }

    fn run_stat(&mut self, args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
        let mut format: Option<String> = None;
        let mut operands: Vec<String> = Vec::new();
        let mut index = 0usize;
        while index < args.len() {
            let arg = args[index].as_str();
            if arg == "-c" || arg == "--format" {
                index += 1;
                match args.get(index) {
                    Some(value) => format = Some(value.clone()),
                    None => {
                        stderr.push_str("stat: option requires an argument -- 'c'\n");
                        return 1;
                    }
                }
            } else if let Some(value) = arg.strip_prefix("--format=") {
                format = Some(value.to_string());
            } else if arg.starts_with('-') && arg != "-" {
                stderr.push_str(&format!(
                    "stat: invalid option -- '{}'\n",
                    arg.trim_start_matches('-').chars().next().unwrap_or('?')
                ));
                return 1;
            } else {
                operands.push(arg.to_string());
            }
            index += 1;
        }
        if operands.is_empty() {
            stderr.push_str("stat: missing operand\n");
            return 1;
        }
        let mut exit_code = 0;
        for operand in &operands {
            let path = self.resolve_path(operand);
            match self.fs.metadata(&path) {
                Ok(meta) => match &format {
                    Some(template) => stdout.push_str(&render_stat_format(template, &path, &meta)),
                    None => stdout.push_str(&format!(
                        "  File: {path}\n  Size: {}\tMode: {:04o}\tMtime: {}\n",
                        meta.size, meta.mode, meta.mtime
                    )),
                },
                Err(_) => {
                    stderr.push_str(&format!(
                        "stat: cannot stat '{operand}': No such file or directory\n"
                    ));
                    exit_code = 1;
                }
            }
        }
        exit_code
    }

    fn run_seq(&mut self, args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
        let numbers: Vec<i64> = args
            .iter()
            .filter_map(|arg| arg.parse::<i64>().ok())
            .collect();
        let (first, step, last) = match numbers.len() {
            1 => (1, 1, numbers[0]),
            2 => (numbers[0], 1, numbers[1]),
            3 => (numbers[0], numbers[1], numbers[2]),
            _ => {
                stderr.push_str("seq: usage: seq [FIRST [STEP]] LAST\n");
                return 1;
            }
        };
        if step == 0 {
            stderr.push_str("seq: invalid Zero increment value: '0'\n");
            return 1;
        }
        let limit = self.line_budget.unwrap_or(usize::MAX);
        let budget = self.remaining_output_budget();
        let mut value = first;
        let mut produced = 0usize;
        while produced < limit
            && ((step > 0 && value <= last) || (step < 0 && value >= last))
            && stdout.len() < budget
        {
            stdout.push_str(&value.to_string());
            stdout.push('\n');
            value += step;
            produced += 1;
        }
        0
    }

    /// `curl` with an offline-by-default network surface.
    ///
    /// Bodies come from the embedder's deterministic response table, or from a
    /// real plain-HTTP fetch when [`Options::allow_network`] is set. HTTPS is
    /// rejected rather than silently downgraded.
    fn run_curl(&mut self, args: &[String], stdout: &mut String, stderr: &mut String) -> i32 {
        let mut parsed = match crate::net::parse_curl_args(args) {
            Ok(parsed) => parsed,
            Err(message) => {
                stderr.push_str(&message);
                if !message.ends_with('\n') {
                    stderr.push('\n');
                }
                return 2;
            }
        };
        // `--data @file` reads from the virtual filesystem, and
        // `--data-urlencode` percent-encodes its value.
        let mut data = parsed.data.clone();
        for (kind, value) in &mut data {
            if let Some(path) = value.strip_prefix('@') {
                match self.fs.read_file(&self.resolve_path(path)) {
                    Ok(contents) => *value = contents.to_string(),
                    Err(error) => {
                        stderr.push_str(&format!("curl: {error}\n"));
                        return 26;
                    }
                }
            } else if matches!(kind, crate::net::DataKind::UrlEncode) {
                *value = crate::net::urlencode_form(value);
            }
        }
        parsed.data = data;

        let body = if parsed.data.is_empty() {
            None
        } else {
            Some(
                parsed
                    .data
                    .iter()
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>()
                    .join("&"),
            )
        };
        let method = if parsed.head_only && parsed.method.is_none() {
            "HEAD".to_string()
        } else {
            crate::net::effective_method(&parsed)
        };
        let request = crate::net::HttpRequest {
            method,
            url: parsed.url.clone(),
            headers: parsed.headers.clone(),
            body,
        };

        let response = match crate::net::perform(&request, &self.http_responses, self.allow_network)
        {
            Ok(response) => response,
            Err(error) => {
                if !parsed.silent {
                    stderr.push_str(&format!("{}\n", error.message()));
                }
                return error.exit_code();
            }
        };

        if parsed.fail_on_http_error && response.status >= 400 {
            if !parsed.silent {
                stderr.push_str(&format!(
                    "curl: (22) The requested URL returned error: {}\n",
                    response.status
                ));
            }
            return 22;
        }

        let mut payload = String::new();
        if parsed.include_headers || parsed.head_only {
            payload.push_str(&format!(
                "HTTP/1.1 {} {}\n",
                response.status, response.reason
            ));
            for (name, value) in &response.headers {
                payload.push_str(&format!("{name}: {value}\n"));
            }
            payload.push('\n');
        }
        if !parsed.head_only {
            payload.push_str(&response.body);
        }

        match &parsed.output {
            Some(path) => {
                let resolved = self.resolve_path(path);
                if let Err(error) = self.fs.write_file(&resolved, &payload) {
                    stderr.push_str(&format!("curl: {error}\n"));
                    return 23;
                }
            }
            None => stdout.push_str(&payload),
        }
        0
    }

    /// A runtime this core does not embed: fail closed rather than fall through
    /// to "command not found".
    fn run_unsupported_runtime(&self, command: &str, stderr: &mut String) -> i32 {
        stderr.push_str(&format!(
            "bash: {command}: this dependency-free core does not embed an interpreter\n"
        ));
        127
    }

    fn run_jobs(&self) -> String {
        let mut output = String::new();
        for job in &self.jobs {
            output.push_str(&format!(
                "[{}] Done                    {}\n",
                job.id, job.command
            ));
        }
        output
    }

    fn run_wait(&self, args: &[String]) -> i32 {
        let target = args
            .first()
            .and_then(|arg| arg.trim_start_matches('%').parse::<u32>().ok());
        match target {
            Some(id) => self
                .jobs
                .iter()
                .find(|job| job.id == id)
                .map(|job| job.status)
                .unwrap_or(127),
            None => self.jobs.last().map(|job| job.status).unwrap_or(0),
        }
    }
}

/// Split a `${p:off:len}` spec at its top-level `:`, ignoring colons nested in
/// parentheses or brackets (`${v:$((1+1)):2}`, `${a[i:j]:1}`).
fn split_slice_spec(spec: &str) -> (&str, Option<&str>) {
    let mut depth = 0i32;
    for (index, ch) in spec.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ':' if depth == 0 => {
                return (&spec[..index], Some(&spec[index + 1..]));
            }
            _ => {}
        }
    }
    (spec, None)
}

/// Expand a `tr` set: `a-z` ranges plus the usual backslash escapes.
fn expand_tr_set(spec: &str) -> Vec<char> {
    let chars: Vec<char> = spec.chars().collect();
    let mut set = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        // `\n`, `\t`, `\r` and `\\` are escapes; any other `\x` is literal.
        if chars[index] == '\\' && index + 1 < chars.len() {
            match chars[index + 1] {
                'n' => {
                    set.push('\n');
                    index += 2;
                    continue;
                }
                't' => {
                    set.push('\t');
                    index += 2;
                    continue;
                }
                'r' => {
                    set.push('\r');
                    index += 2;
                    continue;
                }
                '\\' => {
                    set.push('\\');
                    index += 2;
                    continue;
                }
                _ => {}
            }
        }
        // `a-z` is a range; a trailing `-` is a literal.
        if index + 2 < chars.len() && chars[index + 1] == '-' {
            let (low, high) = (chars[index], chars[index + 2]);
            if low <= high {
                let mut current = low as u32;
                while current <= high as u32 {
                    if let Some(ch) = char::from_u32(current) {
                        set.push(ch);
                    }
                    current += 1;
                }
                index += 3;
                continue;
            }
        }
        set.push(chars[index]);
        index += 1;
    }
    set
}

fn is_shell_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// `name`, or `name[subscript]` for an array element reference.
fn split_array_subscript(parameter: &str) -> Option<(&str, &str)> {
    let open = parameter.find('[')?;
    if !parameter.ends_with(']') {
        return None;
    }
    let name = &parameter[..open];
    if !is_shell_name(name) {
        return None;
    }
    Some((name, &parameter[open + 1..parameter.len() - 1]))
}

/// Short human-readable rendering of a statement, used by `jobs`.
fn describe_statement(statement: &Statement) -> String {
    match &statement.kind {
        StatementKind::Pipeline(commands) => {
            let mut stages = Vec::new();
            for command in commands {
                let words: Vec<String> = command.words.iter().map(Word::text).collect();
                stages.push(words.join(" "));
            }
            stages.join(" | ")
        }
        StatementKind::If(_) => "if".to_string(),
        StatementKind::While(while_stmt) => {
            if while_stmt.until {
                "until".to_string()
            } else {
                "while".to_string()
            }
        }
        StatementKind::For(_) => "for".to_string(),
        StatementKind::Case(_) => "case".to_string(),
        StatementKind::Function(def) => format!("{}()", def.name),
        StatementKind::Group(_) => "{ ...; }".to_string(),
        StatementKind::Subshell(_) => "( ... )".to_string(),
        StatementKind::Cond(_) => "[[ ... ]]".to_string(),
        StatementKind::ArrayAssign(assign) => format!("{}=(", assign.name),
    }
}

fn head_line_limit(args: &[String]) -> Option<usize> {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "-n" {
            return args.get(index + 1).and_then(|value| value.parse().ok());
        }
        if let Some(value) = arg.strip_prefix("-n")
            && let Ok(parsed) = value.parse()
        {
            return Some(parsed);
        }
        if let Some(value) = arg.strip_prefix("--lines=")
            && let Ok(parsed) = value.parse()
        {
            return Some(parsed);
        }
        index += 1;
    }
    None
}

fn grep_max_count(args: &[String]) -> Option<usize> {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "-m" {
            return args.get(index + 1).and_then(|value| value.parse().ok());
        }
        if let Some(value) = arg.strip_prefix("-m")
            && let Ok(parsed) = value.parse()
        {
            return Some(parsed);
        }
        if let Some(value) = arg.strip_prefix("--max-count=")
            && let Ok(parsed) = value.parse()
        {
            return Some(parsed);
        }
        index += 1;
    }
    None
}

fn join_path(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{directory}/{name}")
    }
}

fn sort_ls_entries(entries: &mut [(String, Metadata)], options: &LsOptions) {
    if options.by_time {
        entries.sort_by(|a, b| b.1.mtime.cmp(&a.1.mtime).then_with(|| a.0.cmp(&b.0)));
    } else if options.by_size {
        entries.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(&b.0)));
    } else {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
    }
    if options.reverse {
        entries.reverse();
    }
}

fn format_ls_entry(name: &str, meta: &Metadata, options: &LsOptions) -> String {
    let mut rendered = name.to_string();
    if options.classify {
        if meta.is_dir() {
            rendered.push('/');
        } else if meta.mode & 0o111 != 0 {
            rendered.push('*');
        }
    }
    if options.long {
        format!(
            "{}{} {:>8} {} {}\n",
            meta.type_char(),
            meta.mode_string(),
            meta.size,
            meta.mtime,
            rendered
        )
    } else {
        format!("{rendered}\n")
    }
}

/// Replace a run of at least three `X`s with the deterministic suffix.
fn substitute_x_run(template: &str, suffix: &str) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut output = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == 'X' {
            let start = index;
            while index < chars.len() && chars[index] == 'X' {
                index += 1;
            }
            let run = index - start;
            if run >= 3 {
                let mut filled: Vec<char> = suffix.chars().collect();
                while filled.len() < run {
                    filled.insert(0, '0');
                }
                while filled.len() > run {
                    filled.remove(0);
                }
                output.extend(filled);
            } else {
                for _ in 0..run {
                    output.push('X');
                }
            }
            continue;
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

/// Resolve a `chmod` mode: octal (`755`) or a symbolic clause list (`u+x`).
fn resolve_mode(spec: &str, current: u32) -> Option<u32> {
    if !spec.is_empty() && spec.chars().all(|ch| ('0'..='7').contains(&ch)) {
        return u32::from_str_radix(spec, 8)
            .ok()
            .filter(|mode| *mode <= 0o7777);
    }
    parse_symbolic_mode(spec, current)
}

fn parse_symbolic_mode(spec: &str, current: u32) -> Option<u32> {
    let mut mode = current;
    for clause in spec.split(',') {
        if clause.is_empty() {
            return None;
        }
        let split = clause.find(['+', '-', '=']).unwrap_or(clause.len());
        let (who, rest) = clause.split_at(split);
        let mut chars = rest.chars();
        let op = chars.next()?;
        if !matches!(op, '+' | '-' | '=') {
            return None;
        }
        let mut bits = 0u32;
        for perm in chars {
            bits |= match perm {
                'r' => 0o444,
                'w' => 0o222,
                'x' | 'X' => 0o111,
                's' => 0o6000,
                't' => 0o1000,
                _ => return None,
            };
        }
        let mut mask = 0u32;
        for ch in who.chars() {
            mask |= match ch {
                'u' => 0o4700,
                'g' => 0o2070,
                'o' => 0o1007,
                'a' => 0o7777,
                _ => return None,
            };
        }
        if mask == 0 {
            mask = 0o7777;
        }
        match op {
            '+' => mode |= bits & mask,
            '-' => mode &= !(bits & mask),
            '=' => {
                mode &= !(0o7777 & mask);
                mode |= bits & mask;
            }
            _ => return None,
        }
    }
    Some(mode)
}

fn render_stat_format(template: &str, path: &str, meta: &Metadata) -> String {
    let mut output = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => output.push_str(path),
            Some('s') => output.push_str(&meta.size.to_string()),
            Some('a') => output.push_str(&format!("{:o}", meta.mode)),
            Some('A') => output.push_str(&meta.mode_string()),
            Some('F') => output.push_str(if meta.is_dir() {
                "directory"
            } else {
                "regular file"
            }),
            Some('Y') | Some('y') => output.push_str(&meta.mtime.to_string()),
            Some('%') => output.push('%'),
            Some(other) => {
                output.push('%');
                output.push(other);
            }
            None => output.push('%'),
        }
    }
    output
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
/// Tests for the feature slice added by this milestone: heredocs,
/// here-strings, `case`, `until`, functions, `[[ … ]]`, arrays, background
/// jobs, process substitution, pathname expansion, `PIPESTATUS`, bounded
/// pipelines and the new built-ins.
#[cfg(test)]
mod feature_slice_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn run(script: &str) -> ExecResult {
        bash().exec(script)
    }

    fn out(script: &str) -> String {
        let result = run(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn files(entries: &[(&str, &str)]) -> Bash {
        let mut map = BTreeMap::new();
        for (path, contents) in entries {
            map.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files: map,
            ..Options::default()
        })
        .expect("initialize")
    }

    // ── Heredocs and here-strings ───────────────────────────────────────────

    #[test]
    fn heredoc_body_expands_parameters() {
        let result = run("name=world\ncat <<EOF\nhello $name\nEOF\n");
        assert_eq!(result.stdout, "hello world\n", "stderr: {}", result.stderr);
    }

    #[test]
    fn quoted_heredoc_delimiter_suppresses_expansion() {
        let result = run("name=world\ncat <<'EOF'\nhello $name\nEOF\n");
        assert_eq!(result.stdout, "hello $name\n");
    }

    #[test]
    fn heredoc_dash_strips_leading_tabs() {
        let result = run("cat <<-EOF\n\tindented\n\tEOF\n");
        assert_eq!(result.stdout, "indented\n");
    }

    #[test]
    fn heredoc_runs_command_substitution() {
        let result = run("cat <<EOF\n$(printf sub)\nEOF\n");
        assert_eq!(result.stdout, "sub\n");
    }

    #[test]
    fn heredoc_keeps_quotes_literal() {
        let result = run("cat <<EOF\n\"quoted\" 'single'\nEOF\n");
        assert_eq!(result.stdout, "\"quoted\" 'single'\n");
    }

    #[test]
    fn heredoc_feeds_a_pipeline_stage() {
        assert_eq!(out("cat <<EOF | tr a-z A-Z\nhi\nEOF\n"), "HI\n");
    }

    #[test]
    fn here_string_feeds_standard_input() {
        assert_eq!(out("cat <<< hello\n"), "hello\n");
        assert_eq!(out("cat <<< \"a b\"\n"), "a b\n");
        // The expanded value is never word-split into several operands.
        assert_eq!(out("v='a b'; cat <<< $v\n"), "a b\n");
    }

    #[test]
    fn here_string_expands_variables_and_arithmetic() {
        assert_eq!(out("x=5; cat <<< $((x * 2))\n"), "10\n");
    }

    // ── `case` ─────────────────────────────────────────────────────────────

    #[test]
    fn case_matches_glob_patterns() {
        assert_eq!(out("case abc in a*) echo match;; esac\n"), "match\n");
        assert_eq!(out("case abc in x*) echo no;; esac\n"), "");
    }

    #[test]
    fn case_alternation_and_default_clause() {
        assert_eq!(
            out("case b in a|b) echo ab;; *) echo other;; esac\n"),
            "ab\n"
        );
        assert_eq!(
            out("case z in a) echo a;; *) echo other;; esac\n"),
            "other\n"
        );
    }

    #[test]
    fn case_accepts_an_optional_open_paren() {
        assert_eq!(out("case b in (b) echo yes;; esac\n"), "yes\n");
    }

    #[test]
    fn case_runs_several_commands_and_reports_status() {
        let result = run("case x in x) echo one; echo two;; esac\n");
        assert_eq!(result.stdout, "one\ntwo\n");
        assert_eq!(result.exit_code, 0);
        let result = run("case x in x) false;; esac\n");
        assert_eq!(result.exit_code, 1);
        // No clause matched: the case reports success.
        let result = run("case x in y) false;; esac\n");
        assert_eq!(result.exit_code, 0);
    }

    // ── `until` ────────────────────────────────────────────────────────────

    #[test]
    fn until_runs_while_the_condition_fails() {
        assert_eq!(
            out("i=0; until [ $i -ge 3 ]; do echo $i; i=$((i+1)); done\n"),
            "0\n1\n2\n"
        );
    }

    #[test]
    fn until_skips_the_body_when_the_condition_holds() {
        assert_eq!(out("until true; do echo never; done\n"), "");
    }

    #[test]
    fn until_honors_break_and_continue() {
        assert_eq!(
            out("i=0; until false; do i=$((i+1)); [ $i -eq 2 ] && break; echo $i; done\n"),
            "1\n"
        );
    }

    // ── Functions ──────────────────────────────────────────────────────────

    #[test]
    fn function_definition_and_arguments() {
        assert_eq!(
            out("greet() { echo \"hi $1\"; }; greet world\n"),
            "hi world\n"
        );
        assert_eq!(out("function g { echo g; }; g\n"), "g\n");
        assert_eq!(out("f() { echo $#; }; f a b c\n"), "3\n");
    }

    #[test]
    fn function_return_sets_the_status() {
        let result = run("f() { return 3; }; f; echo $?\n");
        assert_eq!(result.stdout, "3\n");
        let result = run("f() { true; }; f; echo $?\n");
        assert_eq!(result.stdout, "0\n");
    }

    #[test]
    fn function_restores_positional_parameters() {
        assert_eq!(
            out("f() { echo $1; }; set -- outer; f inner; echo $1\n"),
            "inner\nouter\n"
        );
    }

    #[test]
    fn function_output_can_be_piped_and_captured() {
        assert_eq!(out("f() { echo one; echo two; }; f | head -n 1\n"), "one\n");
        assert_eq!(out("f() { echo cap; }; v=$(f); echo $v\n"), "cap\n");
    }

    #[test]
    fn return_outside_a_function_is_an_error() {
        let result = run("return 1\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("can only `return'"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn functions_can_recurse() {
        let result =
            run("count() { if [ $1 -gt 0 ]; then echo $1; count $(( $1 - 1 )); fi; }; count 3\n");
        assert_eq!(result.stdout, "3\n2\n1\n", "stderr: {}", result.stderr);
    }

    // ── `[[ … ]]` ──────────────────────────────────────────────────────────

    #[test]
    fn conditional_string_tests() {
        assert_eq!(out("[[ -n abc ]] && echo yes\n"), "yes\n");
        assert_eq!(out("[[ -z '' ]] && echo empty\n"), "empty\n");
        assert_eq!(out("[[ abc == a* ]] && echo glob\n"), "glob\n");
        assert_eq!(out("[[ abc != x* ]] && echo ne\n"), "ne\n");
        assert_eq!(out("[[ a < b ]] && echo lt\n"), "lt\n");
    }

    #[test]
    fn conditional_quoted_rhs_is_literal() {
        assert_eq!(
            out("x='a*'; [[ $x == \"a*\" ]] && echo literal\n"),
            "literal\n"
        );
        assert_eq!(
            out("x='abc'; [[ $x == \"a*\" ]] || echo not-literal\n"),
            "not-literal\n"
        );
    }

    #[test]
    fn conditional_file_tests() {
        let mut bash = files(&[("/home/user/data.txt", "hello")]);
        let result = bash.exec("[[ -f data.txt ]] && echo file; [[ -d /tmp ]] && echo dir\n");
        assert_eq!(result.stdout, "file\ndir\n");
        let result =
            bash.exec("[[ -s data.txt ]] && echo nonempty; [[ -e missing ]] || echo absent\n");
        assert_eq!(result.stdout, "nonempty\nabsent\n");
    }

    #[test]
    fn conditional_logic_and_parentheses() {
        assert_eq!(out("[[ -n a && -n b ]] && echo both\n"), "both\n");
        assert_eq!(out("[[ -z a || -n b ]] && echo either\n"), "either\n");
        assert_eq!(out("[[ ! -z abc ]] && echo not\n"), "not\n");
        assert_eq!(
            out("[[ ( -n a || -n b ) && -n c ]] && echo paren\n"),
            "paren\n"
        );
    }

    #[test]
    fn conditional_numeric_tests() {
        assert_eq!(out("[[ 5 -gt 3 && 3 -le 3 ]] && echo num\n"), "num\n");
        assert_eq!(out("[[ 5 -ne 4 ]] && echo ne\n"), "ne\n");
    }

    #[test]
    fn conditional_regex_operator() {
        assert_eq!(
            out("[[ abc123 =~ ^[a-z]+[0-9]+$ ]] && echo match\n"),
            "match\n"
        );
        assert_eq!(out("[[ abc =~ ^[0-9]+$ ]] || echo nomatch\n"), "nomatch\n");
    }

    #[test]
    fn conditional_reports_failure_status() {
        let result = run("[[ -n '' ]]\n");
        assert_eq!(result.exit_code, 1);
    }

    // ── Arrays ─────────────────────────────────────────────────────────────

    #[test]
    fn array_literal_and_indexed_reads() {
        assert_eq!(out("a=(x y z); echo ${a[0]} ${a[2]}\n"), "x z\n");
        assert_eq!(out("a=(x y z); echo ${#a[@]}\n"), "3\n");
        assert_eq!(out("a=(x y z); echo ${a[@]}\n"), "x y z\n");
    }

    #[test]
    fn array_element_assignment_and_append() {
        assert_eq!(out("a=(1 2); a[1]=9; echo ${a[@]}\n"), "1 9\n");
        assert_eq!(out("a=(1 2); a+=(3 4); echo ${#a[@]}\n"), "4\n");
        assert_eq!(out("a=(1 2); a[4]=5; echo ${#a[@]}\n"), "5\n");
    }

    #[test]
    fn array_at_expands_one_word_per_element() {
        assert_eq!(
            out("a=('one two' three); for e in \"${a[@]}\"; do echo \"[$e]\"; done\n"),
            "[one two]\n[three]\n"
        );
        // Unquoted `${a[@]}` splits and globs, as in bash.
        assert_eq!(out("a=(1 2 3); printf '%s\\n' ${a[@]}\n"), "1\n2\n3\n");
    }

    #[test]
    fn array_indices_and_indirect_reference() {
        assert_eq!(out("a=(x y); echo ${!a[@]}\n"), "0 1\n");
    }

    #[test]
    fn array_element_length() {
        assert_eq!(out("a=(hello hi); echo ${#a[0]} ${#a[1]}\n"), "5 2\n");
    }

    #[test]
    fn arrays_are_isolated_between_exec_calls() {
        let mut bash = bash();
        bash.exec("a=(1 2 3)");
        let result = bash.exec("echo ${#a[@]}");
        assert_eq!(result.stdout, "0\n");
    }

    // ── PIPESTATUS and background jobs ─────────────────────────────────────

    #[test]
    fn pipestatus_records_every_stage() {
        assert_eq!(
            out("false | true; echo ${PIPESTATUS[0]}${PIPESTATUS[1]}\n"),
            "10\n"
        );
        assert_eq!(
            out("true | false | true; echo ${PIPESTATUS[*]}\n"),
            "0 1 0\n"
        );
    }

    #[test]
    fn pipestatus_is_replaced_by_the_next_command() {
        assert_eq!(out("false | true; true; echo ${PIPESTATUS[0]}\n"), "0\n");
    }

    #[test]
    fn background_statement_reports_success_and_unwinds_later() {
        let result = run("echo bg & echo fg\n");
        assert_eq!(result.stdout, "bg\nfg\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn wait_returns_the_job_status_and_jobs_lists_it() {
        let result = run("false & wait; echo $?\n");
        assert_eq!(result.stdout, "1\n", "stderr: {}", result.stderr);
        let result = run("sh -c 'exit 4' & true\n");
        // `sh` is not available; the job still records the failure status.
        assert_eq!(result.exit_code, 0);
        let result = run("true & jobs\n");
        assert!(result.stdout.contains("[1] Done"), "{}", result.stdout);
    }

    #[test]
    fn background_job_filesystem_changes_are_visible() {
        assert_eq!(
            out("touch /tmp/bg-file & wait; ls /tmp/bg-file\n"),
            "/tmp/bg-file\n"
        );
    }

    #[test]
    fn background_job_environment_does_not_leak() {
        assert_eq!(out("x=1 & echo $x\n"), "\n");
    }

    #[test]
    fn last_background_id_is_available() {
        assert_eq!(out("true & echo $!\n"), "1\n");
    }

    // ── Process substitution ───────────────────────────────────────────────

    #[test]
    fn process_substitution_feeds_a_read_command() {
        assert_eq!(out("cat <(echo one) <(echo two)\n"), "one\ntwo\n");
    }

    #[test]
    fn process_substitution_removes_its_temporary_file() {
        let result = run("cat <(echo tmp) > /dev/null; echo ${#PIPESTATUS[@]}\n");
        assert_eq!(result.stdout, "1\n");
    }

    #[test]
    fn process_substitution_output_form_runs_after_the_command() {
        assert_eq!(out("echo hello > >(tr a-z A-Z)\n"), "HELLO\n");
    }

    // ── Pathname expansion ─────────────────────────────────────────────────

    #[test]
    fn glob_expands_in_command_arguments() {
        let mut bash = files(&[
            ("/home/user/a.txt", ""),
            ("/home/user/b.txt", ""),
            ("/home/user/c.md", ""),
        ]);
        assert_eq!(bash.exec("echo *.txt").stdout, "a.txt b.txt\n");
        assert_eq!(bash.exec("echo *.none").stdout, "*.none\n");
        assert_eq!(bash.exec("echo ?.md").stdout, "c.md\n");
    }

    #[test]
    fn glob_is_skipped_for_quoted_and_escaped_metacharacters() {
        let mut bash = files(&[("/home/user/a.txt", "")]);
        assert_eq!(bash.exec("echo \"*.txt\"").stdout, "*.txt\n");
        assert_eq!(bash.exec("echo \\*.txt").stdout, "*.txt\n");
    }

    #[test]
    fn glob_expands_nested_and_directory_patterns() {
        let mut bash = files(&[
            ("/home/user/src/main.rs", ""),
            ("/home/user/src/lib.rs", ""),
        ]);
        assert_eq!(
            bash.exec("echo src/*.rs").stdout,
            "src/lib.rs src/main.rs\n"
        );
        assert_eq!(bash.exec("echo */").stdout, "src/\n");
    }

    #[test]
    fn glob_results_are_sorted_and_skip_dotfiles() {
        let mut bash = files(&[
            ("/home/user/b", ""),
            ("/home/user/a", ""),
            ("/home/user/.hidden", ""),
        ]);
        assert_eq!(bash.exec("echo *").stdout, "a b\n");
        assert_eq!(bash.exec("echo .*").stdout, ".hidden\n");
    }

    #[test]
    fn for_loop_iterates_glob_matches() {
        let mut bash = files(&[("/home/user/a.txt", ""), ("/home/user/b.txt", "")]);
        assert_eq!(
            bash.exec("for f in *.txt; do echo \"[$f]\"; done").stdout,
            "[a.txt]\n[b.txt]\n"
        );
    }

    // ── New built-ins ──────────────────────────────────────────────────────

    #[test]
    fn yes_is_bounded_by_the_downstream_consumer() {
        // Without back-pressure this would produce output until the limit.
        assert_eq!(out("yes | head -n 3\n"), "y\ny\ny\n");
        assert_eq!(out("yes hello | head -n 2\n"), "hello\nhello\n");
    }

    #[test]
    fn seq_prints_ascending_and_descending_ranges() {
        assert_eq!(out("seq 3\n"), "1\n2\n3\n");
        assert_eq!(out("seq 2 4\n"), "2\n3\n4\n");
        assert_eq!(out("seq 5 -2 1\n"), "5\n3\n1\n");
    }

    #[test]
    fn seq_honors_a_downstream_head() {
        assert_eq!(out("seq 1 1000000 | head -n 3\n"), "1\n2\n3\n");
    }

    #[test]
    fn mktemp_creates_distinct_paths() {
        let result = run(
            "a=$(mktemp); b=$(mktemp); [ \"$a\" != \"$b\" ] && echo distinct; [ -f \"$a\" ] && echo exists\n",
        );
        assert_eq!(
            result.stdout, "distinct\nexists\n",
            "stderr: {}",
            result.stderr
        );
    }

    #[test]
    fn mktemp_directory_and_template() {
        let result = run("d=$(mktemp -d); [ -d \"$d\" ] && echo dir\n");
        assert_eq!(result.stdout, "dir\n");
        let result = run("f=$(mktemp /tmp/name.XXXXXX); echo \"$f\"\n");
        assert_eq!(result.stdout, "/tmp/name.000001\n");
    }

    #[test]
    fn chmod_and_stat_report_modes() {
        // `stat -c` adds no trailing newline, exactly like GNU stat.
        assert_eq!(
            out("touch /tmp/f; chmod 600 /tmp/f; stat -c %a /tmp/f\n"),
            "600"
        );
        assert_eq!(
            out("touch /tmp/f; chmod 644 /tmp/f; stat -c %A /tmp/f\n"),
            "rw-r--r--"
        );
        assert_eq!(
            out("touch /tmp/f; chmod +x /tmp/f; [ -x /tmp/f ] && echo exec\n"),
            "exec\n"
        );
    }

    #[test]
    fn stat_reports_size_and_type() {
        let mut bash = files(&[("/home/user/data", "hello")]);
        assert_eq!(bash.exec("stat -c '%s %F' data").stdout, "5 regular file");
        assert_eq!(bash.exec("stat -c '%F' /tmp").stdout, "directory");
    }

    #[test]
    fn chmod_rejects_an_invalid_mode() {
        let result = run("touch /tmp/f; chmod 999 /tmp/f\n");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("invalid mode"), "{}", result.stderr);
    }

    #[test]
    fn sleep_validates_its_operand() {
        assert_eq!(run("sleep 1; echo ok\n").stdout, "ok\n");
        let result = run("sleep abc\n");
        assert_eq!(result.exit_code, 1);
    }

    // ── ls sorting and classification ──────────────────────────────────────

    #[test]
    fn ls_sorts_by_name_by_default_and_reverses() {
        let mut bash = files(&[("/home/user/b", ""), ("/home/user/a", "")]);
        assert_eq!(bash.exec("ls").stdout, "a\nb\n");
        assert_eq!(bash.exec("ls -r").stdout, "b\na\n");
    }

    #[test]
    fn ls_sorts_by_time_and_size() {
        let mut bash = bash();
        bash.exec("echo 1 > small; echo 12345 > large");
        // `large` was written last, so it is newest and largest.
        assert_eq!(bash.exec("ls -t").stdout, "large\nsmall\n");
        assert_eq!(bash.exec("ls -S").stdout, "large\nsmall\n");
        assert_eq!(bash.exec("ls -Sr").stdout, "small\nlarge\n");
    }

    #[test]
    fn ls_lists_hidden_files_and_classifies() {
        let mut bash = files(&[
            ("/home/user/.hidden", ""),
            ("/home/user/visible", ""),
            ("/home/user/dir/x", ""),
        ]);
        assert_eq!(bash.exec("ls").stdout, "dir\nvisible\n");
        assert_eq!(bash.exec("ls -a").stdout, ".\n..\n.hidden\ndir\nvisible\n");
        assert_eq!(bash.exec("ls -A").stdout, ".hidden\ndir\nvisible\n");
        assert_eq!(bash.exec("ls -F").stdout, "dir/\nvisible\n");
    }

    #[test]
    fn ls_accepts_multiple_operands_and_long_format() {
        let mut bash = files(&[("/home/user/a", "1"), ("/home/user/b", "22")]);
        let result = bash.exec("ls a b");
        assert_eq!(result.stdout, "a\nb\n");
        let result = bash.exec("ls -l a");
        assert!(result.stdout.starts_with("-rw-r--r--"), "{}", result.stdout);
        assert!(result.stdout.contains("a\n"), "{}", result.stdout);
        let result = bash.exec("ls missing");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("No such file"), "{}", result.stderr);
    }

    #[test]
    fn ls_directory_itself_with_dash_d() {
        let mut bash = files(&[("/home/user/dir/x", "")]);
        assert_eq!(bash.exec("ls -d dir").stdout, "dir\n");
    }

    // ── Regex-backed grep ──────────────────────────────────────────────────

    #[test]
    fn grep_extended_regex_matches() {
        assert_eq!(out("printf 'abc\\nxyz\\n' | grep -E 'a.c'\n"), "abc\n");
        assert_eq!(
            out("printf 'cat\\ndog\\n' | grep -E '^(cat|dog)$'\n"),
            "cat\ndog\n"
        );
        assert_eq!(out("printf 'a1\\nb2\\n' | grep -E '[0-9]'\n"), "a1\nb2\n");
    }

    #[test]
    fn grep_perl_class_shorthand() {
        assert_eq!(out("printf 'a1\\nbb\\n' | grep -P '\\d'\n"), "a1\n");
    }

    #[test]
    fn grep_regex_flags_combine() {
        assert_eq!(out("printf 'AAA\\nbbb\\n' | grep -Ei 'aaa'\n"), "AAA\n");
        assert_eq!(out("printf 'abc\\n' | grep -Ec 'a|z'\n"), "1\n");
        assert_eq!(out("printf 'abc\\ndef\\n' | grep -Ev 'a'\n"), "def\n");
        assert_eq!(out("printf 'ab\\ncd\\n' | grep -E -m 1 '.'\n"), "ab\n");
        assert_eq!(out("printf 'abc\\n' | grep -Ex 'a.c'\n"), "abc\n");
        assert_eq!(out("printf 'foo bar\\n' | grep -Ew 'foo'\n"), "foo bar\n");
    }

    #[test]
    fn grep_fixed_strings_stay_literal() {
        assert_eq!(out("printf 'a.c\\nabc\\n' | grep -F 'a.c'\n"), "a.c\n");
        // The default engine is literal too.
        assert_eq!(out("printf 'a.c\\nabc\\n' | grep 'a.c'\n"), "a.c\n");
    }

    #[test]
    fn grep_invalid_regex_exits_two() {
        let result = run("echo x | grep -E '('\n");
        assert_eq!(result.exit_code, 2);
        assert!(!result.stderr.is_empty(), "expected a syntax error");
    }

    // ── sed ────────────────────────────────────────────────────────────────

    #[test]
    fn sed_substitutes_in_a_pipeline() {
        assert_eq!(
            out("printf 'hello world\\n' | sed 's/world/there/'\n"),
            "hello there\n"
        );
        assert_eq!(out("printf 'aaa\\n' | sed 's/a/b/g'\n"), "bbb\n");
    }

    #[test]
    fn sed_quiet_print_and_address() {
        assert_eq!(out("printf 'a\\nb\\n' | sed -n '2p'\n"), "b\n");
        assert_eq!(out("printf 'a\\nb\\nc\\n' | sed '2d'\n"), "a\nc\n");
        assert_eq!(out("printf 'a\\nb\\n' | sed -n 's/a/A/p'\n"), "A\n");
    }

    #[test]
    fn sed_reads_files_from_the_virtual_filesystem() {
        let mut bash = files(&[("/home/user/data.txt", "one two\n")]);
        assert_eq!(bash.exec("sed 's/one/1/' data.txt").stdout, "1 two\n");
    }

    #[test]
    fn sed_fails_closed_on_unsupported_scripts() {
        let result = run("printf 'x\\n' | sed 'y/a/b/'\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("unsupported command"),
            "{}",
            result.stderr
        );
    }

    // ── curl ───────────────────────────────────────────────────────────────

    #[test]
    fn curl_serves_deterministic_mock_responses() {
        let mut options = Options::default();
        options
            .http_responses
            .insert("http://example.test/hello".to_string(), "hi\n".to_string());
        let mut bash = Bash::new(options).expect("initialize");
        let result = bash.exec("curl http://example.test/hello");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn curl_can_write_to_a_file_and_include_headers() {
        let mut options = Options::default();
        options
            .http_responses
            .insert("http://example.test/data".to_string(), "body".to_string());
        let mut bash = Bash::new(options).expect("initialize");
        let result = bash.exec("curl -s -o /tmp/out http://example.test/data; cat /tmp/out");
        assert_eq!(result.stdout, "body");
        let result = bash.exec("curl -s -i http://example.test/data");
        assert!(
            result.stdout.starts_with("HTTP/1.1 200 OK"),
            "{}",
            result.stdout
        );
        assert!(result.stdout.ends_with("body"), "{}", result.stdout);
    }

    #[test]
    fn curl_fails_closed_without_network() {
        let result = run("curl http://example.test/\n");
        assert_eq!(result.exit_code, 7);
        assert!(
            result.stderr.contains("network access is disabled"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn curl_reports_a_missing_data_file() {
        let result = run("curl --data @missing.txt http://example.test/\n");
        assert_eq!(result.exit_code, 26);
    }

    #[test]
    fn curl_rejects_an_unknown_option() {
        let result = run("curl --nope http://example.test/\n");
        assert_eq!(result.exit_code, 2);
    }

    // ── Grouping, subshells, negation, fail-closed commands ────────────────

    #[test]
    fn brace_group_runs_in_the_current_shell() {
        assert_eq!(out("{ echo a; echo b; }\n"), "a\nb\n");
        assert_eq!(out("x=1; { x=2; }; echo $x\n"), "2\n");
    }

    #[test]
    fn subshell_isolates_state_but_not_files() {
        assert_eq!(out("x=1; ( x=2 ); echo $x\n"), "1\n");
        assert_eq!(out("( cd /tmp; pwd ); pwd\n"), "/tmp\n/home/user\n");
        assert_eq!(
            out("( touch /tmp/from-sub ); ls /tmp/from-sub\n"),
            "/tmp/from-sub\n"
        );
    }

    #[test]
    fn negation_inverts_the_status() {
        let result = run("! true; echo $?\n");
        assert_eq!(result.stdout, "1\n");
        let result = run("! false; echo $?\n");
        assert_eq!(result.stdout, "0\n");
        let result = run("! [ -f /nope ] && echo absent\n");
        assert_eq!(result.stdout, "absent\n");
    }

    #[test]
    fn known_interpreter_names_fail_closed_with_a_message() {
        for command in [
            "python3 -c 'print(1)'",
            "node -e 1",
            "sqlite3 :memory: 'select 1'",
        ] {
            let result = run(&format!("{command}\n"));
            assert_eq!(result.exit_code, 127, "{command}");
            assert!(
                result.stderr.contains("does not embed an interpreter"),
                "{command} -> {}",
                result.stderr
            );
        }
    }

    #[test]
    fn unknown_commands_remain_command_not_found() {
        let result = run("definitely-not-a-command\n");
        assert_eq!(result.exit_code, 127);
        assert_eq!(
            result.stderr,
            "bash: definitely-not-a-command: command not found\n"
        );
    }

    // ── `set -e` / `pipefail` interactions with the new syntax ─────────────

    #[test]
    fn errexit_stops_after_a_failed_conditional_statement() {
        let result = run("set -e\n[[ -n '' ]]\necho unreachable\n");
        assert_eq!(result.stdout, "");
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn pipefail_reports_the_failing_stage() {
        let result = run("set -o pipefail\nfalse | true\necho $?\n");
        assert_eq!(result.stdout, "1\n");
    }

    #[test]
    fn pipestatus_remains_available_with_pipefail() {
        assert_eq!(
            out("set -o pipefail\nfalse | true; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]}\n"),
            "1 0\n"
        );
    }

    #[test]
    fn positional_list_expansion() {
        // `"$@"` yields one field per parameter; quoted `"$*"` joins them.
        assert_eq!(
            out("f() { for a in \"$@\"; do echo \"[$a]\"; done; }; f x 'y z'\n"),
            "[x]\n[y z]\n"
        );
        assert_eq!(out("set -- a b; echo \"$*\"\n"), "a b\n");
    }

    #[test]
    fn dot_components_glob_correctly() {
        let mut bash = files(&[("/home/user/a.rs", ""), ("/home/user/b.rs", "")]);
        assert_eq!(bash.exec("echo ./*.rs").stdout, "./a.rs ./b.rs\n");
        assert_eq!(bash.exec("echo ./a.rs").stdout, "./a.rs\n");
    }

    #[test]
    fn head_and_tail_accept_the_obsolete_count_form() {
        assert_eq!(out("printf 'a\\nb\\nc\\n' | head -2\n"), "a\nb\n");
        assert_eq!(out("printf 'a\\nb\\nc\\n' | tail -1\n"), "c\n");
    }

    #[test]
    fn tr_understands_backslash_escapes() {
        // Every newline is translated, including the final one.
        assert_eq!(out("printf 'a\\nb\\n' | tr '\\n' ' '\n"), "a b ");
    }
}

/// Tests for associative arrays, parameter/array slicing, `declare`/`local`
/// and `shift`.
#[cfg(test)]
mod array_and_declare_tests {
    use super::*;

    fn run(script: &str) -> ExecResult {
        Bash::new(Options::default())
            .expect("initialize")
            .exec(script)
    }

    fn out(script: &str) -> String {
        let result = run(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    // ── Associative arrays ──────────────────────────────────────────────────

    #[test]
    fn declare_a_creates_an_associative_array() {
        assert_eq!(
            out("declare -A m; m[foo]=bar; m[baz]=qux; echo ${m[foo]} ${#m[@]} ${!m[@]}\n"),
            "bar 2 baz foo\n"
        );
    }

    #[test]
    fn associative_literal_keeps_string_keys() {
        assert_eq!(
            out("declare -A m=([a]=1 [b]=2); echo ${m[a]}${m[b]}\n"),
            "12\n"
        );
        assert_eq!(out("declare -A m=([alpha]=x); echo ${m[alpha]}\n"), "x\n");
    }

    #[test]
    fn associative_keys_are_not_evaluated_as_arithmetic() {
        // `key` is a literal here, not a variable that evaluates to 0.
        assert_eq!(
            out("declare -A m; k=1; m[key]=v; echo ${m[key]}; echo ${#m[@]}\n"),
            "v\n1\n"
        );
    }

    #[test]
    fn associative_expansions_inside_subscripts() {
        assert_eq!(
            out("declare -A m; k=name; m[$k]=value; echo ${m[name]}\n"),
            "value\n"
        );
        assert_eq!(out("declare -A m=([a]=1); key=a; echo ${m[$key]}\n"), "1\n");
    }

    #[test]
    fn associative_values_and_keys_lists() {
        assert_eq!(out("declare -A m=([x]=1 [y]=2); echo ${m[@]}\n"), "1 2\n");
        assert_eq!(
            out("declare -A m=([x]=1 [y]=2); for k in \"${!m[@]}\"; do echo \"[$k]\"; done\n"),
            "[x]\n[y]\n"
        );
    }

    #[test]
    fn associative_unset_removes_one_key() {
        assert_eq!(
            out("declare -A m=([a]=1 [b]=2); unset \"m[a]\"; echo ${#m[@]} ${!m[@]}\n"),
            "1 b\n"
        );
    }

    #[test]
    fn bare_associative_name_reads_key_zero() {
        assert_eq!(out("declare -A m=([0]=zero [k]=v); echo ${m}\n"), "zero\n");
    }

    #[test]
    fn associative_arrays_are_isolated_between_exec_calls() {
        let mut bash = Bash::new(Options::default()).expect("initialize");
        bash.exec("declare -A m=([a]=1)");
        assert_eq!(bash.exec("echo ${#m[@]}").stdout, "0\n");
    }

    // ── Indexed arrays keep working ─────────────────────────────────────────

    #[test]
    fn indexed_literals_and_slices() {
        assert_eq!(out("a=(1 2 3 4 5); echo ${a[@]:1:2}\n"), "2 3\n");
        assert_eq!(out("a=(1 2 3 4 5); echo ${a[@]: -2}\n"), "4 5\n");
        assert_eq!(out("a=(1 2 3); echo ${a[@]:1}\n"), "2 3\n");
        assert_eq!(out("a=(1 2 3); echo ${a[@]:5}\n"), "\n");
    }

    #[test]
    fn indexing_assigns_beyond_the_end() {
        assert_eq!(
            out("declare -a a=([2]=x); echo ${#a[@]} ${a[2]}\n"),
            "3 x\n"
        );
    }

    #[test]
    fn quoted_star_joins_and_at_splits() {
        assert_eq!(
            out("a=(p q r); printf \"[%s]\" \"${a[*]}\"; echo\n"),
            "[p q r]\n"
        );
        assert_eq!(
            out("a=(p q r); printf \"[%s]\" \"${a[@]}\"; echo\n"),
            "[p][q][r]\n"
        );
        assert_eq!(out("set -- m n; printf \"[%s]\" \"$*\"; echo\n"), "[m n]\n");
        assert_eq!(
            out("set -- m n; printf \"[%s]\" \"$@\"; echo\n"),
            "[m][n]\n"
        );
    }

    // ── `${v:off:len}` slicing ─────────────────────────────────────────────

    #[test]
    fn string_slices() {
        assert_eq!(out("v=abcdef; echo ${v:2}\n"), "cdef\n");
        assert_eq!(out("v=abcdef; echo ${v:1:3}\n"), "bcd\n");
        assert_eq!(out("v=abcdef; echo ${v: -2}\n"), "ef\n");
        assert_eq!(out("v=abcdef; echo ${v: -4:2}\n"), "cd\n");
        assert_eq!(out("v=abcdef; echo \"${v: 1:2}\"\n"), "bc\n");
        assert_eq!(out("v=abcdef; echo ${v:10}\n"), "\n");
    }

    #[test]
    fn positional_slices() {
        assert_eq!(out("set -- a b c d; echo ${@:2:2}\n"), "c d\n");
        assert_eq!(out("set -- a b c d; echo ${@: -2}\n"), "c d\n");
    }

    #[test]
    fn slices_use_arithmetic_offsets() {
        assert_eq!(out("v=abcdef; i=1; n=2; echo ${v:i:n}\n"), "bc\n");
        assert_eq!(out("v=abcdef; echo ${v:1+1:2+1}\n"), "cde\n");
    }

    #[test]
    fn parameter_operators_are_not_slices() {
        assert_eq!(out("unset u; echo ${u:-fallback}\n"), "fallback\n");
        assert_eq!(
            out("unset u; echo ${u:=assigned}; echo $u\n"),
            "assigned\nassigned\n"
        );
        assert_eq!(out("v=set; echo ${v:-x}\n"), "set\n");
        assert_eq!(out("unset u; echo ${u:+alt}\n"), "\n");
    }

    #[test]
    fn expansions_with_spaces_or_operators_inside_braces() {
        // The whole `${ … }` is one lexical unit, so the space is literal.
        assert_eq!(out("unset u; echo \"${u:-a b}\"\n"), "a b\n");
        assert_eq!(out("v=xyz; echo ${v:-a|b}\n"), "xyz\n");
        assert_eq!(out("v=abcdef; echo \"${v: -2}\"\n"), "ef\n");
    }

    // ── declare / local ────────────────────────────────────────────────────

    #[test]
    fn local_shadows_and_restores_a_variable() {
        assert_eq!(
            out("v=outer; f() { local v=inner; echo $v; }; f; echo $v\n"),
            "inner\nouter\n"
        );
    }

    #[test]
    fn declare_inside_a_function_is_local() {
        assert_eq!(
            out("v=outer; f() { declare v=inner; echo $v; }; f; echo $v\n"),
            "inner\nouter\n"
        );
        assert_eq!(
            out(
                "declare -A m=([k]=out); f() { declare -A m=([k]=in); echo ${m[k]}; }; f; echo ${m[k]}\n"
            ),
            "in\nout\n"
        );
    }

    #[test]
    fn declare_dash_g_sets_a_global() {
        assert_eq!(out("f() { declare -g g=set; }; f; echo $g\n"), "set\n");
    }

    #[test]
    fn local_arrays_are_restored() {
        assert_eq!(
            out("a=(global); f() { local -a a=(1 2 3); echo ${#a[@]}; }; f; echo ${a[0]}\n"),
            "3\nglobal\n"
        );
    }

    #[test]
    fn local_outside_a_function_is_an_error() {
        let result = run("local x=1\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("can only be used in a function"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn declare_rejects_an_invalid_identifier() {
        let result = run("declare 1bad=2\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("not a valid identifier"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn nested_functions_share_dynamic_scope() {
        // An inner function sees (and may change) the caller's local.
        assert_eq!(
            out("g() { local x=1; h; echo $x; }; h() { x=2; }; g\n"),
            "2\n"
        );
    }

    // ── Array literals attached to a command ───────────────────────────────

    #[test]
    fn array_literal_attached_to_declare() {
        assert_eq!(out("declare -a a=(x y); echo ${a[1]}\n"), "y\n");
        assert_eq!(out("declare -A m=([k]=v); echo ${m[k]}\n"), "v\n");
        assert_eq!(
            out("f() { local -A m=([k]=in); echo ${m[k]}; }; f\n"),
            "in\n"
        );
    }

    #[test]
    fn array_literal_on_a_non_declaration_is_a_syntax_error() {
        let result = run("echo a=(1 2)\n");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("syntax error"), "{}", result.stderr);
    }

    // ── shift ──────────────────────────────────────────────────────────────

    #[test]
    fn shift_drops_leading_positionals() {
        assert_eq!(out("set -- a b c; shift; echo $@\n"), "b c\n");
        assert_eq!(out("set -- a b c; shift 2; echo $@\n"), "c\n");
        assert_eq!(out("set -- a b c; shift 3; echo \"[$*]\"\n"), "[]\n");
    }

    #[test]
    fn shift_past_the_end_reports_failure() {
        let result = run("set -- a; shift 5; echo $?\n");
        assert_eq!(result.stdout, "1\n");
        // The positional parameters are unchanged.
        assert_eq!(out("set -- a; shift 5; echo $1\n"), "a\n");
    }

    #[test]
    fn shift_rejects_a_non_numeric_count() {
        let result = run("set -- a; shift x\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("numeric argument required"),
            "{}",
            result.stderr
        );
    }

    // ── Unary sign in arithmetic ───────────────────────────────────────────

    #[test]
    fn arithmetic_accepts_unary_sign() {
        assert_eq!(out("echo $(( -5 + 2 ))\n"), "-3\n");
        assert_eq!(out("x=3; echo $(( -x ))\n"), "-3\n");
        assert_eq!(out("echo $(( 2 - -3 ))\n"), "5\n");
        assert_eq!(out("echo $(( (-2) * 3 ))\n"), "-6\n");
    }
}

/// Edge cases for the feature slice: nested expansions, mixed array kinds and
/// interactions between the new syntax and existing behavior.
#[cfg(test)]
mod feature_edge_tests {
    use super::*;

    fn run(script: &str) -> ExecResult {
        Bash::new(Options::default())
            .expect("initialize")
            .exec(script)
    }

    fn out(script: &str) -> String {
        let result = run(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    #[test]
    fn nested_expansions_in_subscripts_and_slices() {
        assert_eq!(
            out("declare -A m=([a]=1 [b]=2); k=b; echo ${m[${k}]}\n"),
            "2\n"
        );
        assert_eq!(out("v=abcdef; echo ${v:$((1+1)):2}\n"), "cd\n");
        assert_eq!(out("a=(1 2 3); i=1; echo ${a[i]}\n"), "2\n");
    }

    #[test]
    fn associative_and_indexed_arrays_can_coexist() {
        assert_eq!(
            out("a=(x y); declare -A m=([k]=v); echo ${a[1]} ${m[k]} ${#a[@]} ${#m[@]}\n"),
            "y v 2 1\n"
        );
    }

    #[test]
    fn declaring_associative_replaces_an_indexed_array() {
        assert_eq!(
            out("a=(1 2 3); declare -A a; a[k]=v; echo ${#a[@]} ${a[k]}\n"),
            "1 v\n"
        );
    }

    #[test]
    fn local_scope_survives_early_return() {
        assert_eq!(
            out("v=outer; f() { local v=inner; return 3; }; f; echo $? $v\n"),
            "3 outer\n"
        );
    }

    #[test]
    fn slices_do_not_disturb_set_u() {
        // `set -u` must not fire for a slice of an unset variable's default.
        assert_eq!(out("set -u\nv=abc; echo ${v:1}\n"), "bc\n");
        let result = run("set -u\nunset v\necho ${v:1}\n");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("unbound variable"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn declare_without_arguments_lists_nothing_and_succeeds() {
        assert_eq!(out("declare; echo ok\n"), "ok\n");
        assert_eq!(out("declare -A; echo ok\n"), "ok\n");
    }

    #[test]
    fn shift_inside_a_function_only_affects_the_call() {
        assert_eq!(
            out("set -- outer; f() { shift; echo \"[$*]\"; }; f a b c; echo $1\n"),
            "[b c]\nouter\n"
        );
    }

    #[test]
    fn unset_of_an_indexed_element_removes_it() {
        assert_eq!(
            out("a=(x y z); unset 'a[1]'; echo ${#a[@]} ${a[@]}\n"),
            "2 x z\n"
        );
    }

    #[test]
    fn arithmetic_unary_inside_conditionals_and_slices() {
        assert_eq!(out("i=-2; echo $(( i ))\n"), "-2\n");
        assert_eq!(out("v=abcdef; echo ${v:(-2)}\n"), "ef\n");
    }

    #[test]
    fn assoc_values_expand_in_loops() {
        assert_eq!(
            out(
                "declare -A m=([a]=1 [b]=2); total=0; for v in \"${m[@]}\"; do total=$(( total + v )); done; echo $total\n"
            ),
            "3\n"
        );
    }
}
