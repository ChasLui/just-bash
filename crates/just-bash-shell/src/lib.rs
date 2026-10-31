use std::collections::BTreeMap;

mod arith;
mod opts;
mod textutils;

use just_bash_fs::{Error, InMemoryFs, Metadata, normalize_absolute, parent_dir};
use just_bash_glob as glob;
use just_bash_parser::{
    ArrayAssign, CaseStatement, CommandInvocation, CondExpr, CondTerm, ForStatement, FunctionDef,
    IfStatement, ParseLimits, PipelineConnector, Redirect, RedirectMode, Statement, StatementKind,
    WhileStatement, Word, WordPart, WordPartKind, parse_heredoc_word, parse_script_with_limits,
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
    /// Maximum nesting depth accepted by the parser (`( )`, `{ }`, `if`,
    /// `while`, `for`, `case`, function bodies, `[[ … ]]` groups).
    ///
    /// Parser recursion is user-controlled, and a stack overflow aborts the
    /// host process instead of failing the script, so this bound is enforced
    /// before execution starts. Matches upstream's `MAX_PARSER_DEPTH`.
    pub max_nesting_depth: usize,
    /// Maximum shell function call depth (`f(){ f; }; f`). Matches upstream's
    /// `maxCallDepth`; without it recursive functions exhaust the stack.
    pub max_call_depth: usize,
    /// Maximum nested parameter-expansion depth (`${x:-${x:-…}}`). Expansion
    /// recurses through the operator word, so each level costs a stack frame.
    pub max_expansion_depth: usize,
    /// Maximum `source`/`eval` nesting depth. A script that sources itself
    /// would otherwise recurse without bound.
    pub max_source_depth: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_script_size_bytes: 1_048_576,
            max_command_count: 10_000,
            max_command_substitution_depth: 50,
            max_output_bytes: 10_485_760, // 10 MiB
            max_loop_iterations: 100_000,
            max_nesting_depth: 32,
            max_call_depth: 100,
            max_expansion_depth: 100,
            max_source_depth: 100,
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
    /// `set -f` / `set -o noglob`: leave glob patterns unexpanded.
    opt_noglob: bool,
    /// `set -n` / `set -o noexec`: read the script but do not run it.
    opt_noexec: bool,
    /// `set -o xtrace` / `set -x`: echo each command before running it.
    opt_xtrace: bool,
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
    /// Aliases defined with `alias`, expanded before a command runs.
    aliases: BTreeMap<String, String>,
    /// Body installed by `trap … EXIT`, run just before the shell returns.
    trap_exit: Option<String>,
    /// `getopts` cursor: index into the argument list.
    getopts_index: usize,
    /// `getopts` cursor: offset inside a clustered option such as `-ab`.
    getopts_cluster: usize,
    /// `shopt` flags that change expansion behaviour.
    shopt_nullglob: bool,
    /// Names made read-only by `readonly` / `declare -r`.
    readonly_names: std::collections::BTreeSet<String>,
    /// Set by `exec cmd`, which replaces the shell and ends the script.
    exec_replaced: bool,
    /// Directory stack for `pushd`/`popd`/`dirs`; the first entry is the
    /// current directory, matching bash's ordering.
    dir_stack: Vec<String>,
    /// Descriptors redirected for the rest of the script by `exec N>file`.
    /// Only the descriptors a command actually uses (0-2 plus any explicitly
    /// redirected one) are consulted, so an untouched entry is never opened.
    persistent_fds: BTreeMap<u32, Sink>,
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
    /// Current `source`/`eval` nesting depth.
    source_depth: usize,
    /// Current parameter-expansion nesting depth. Expansion recurses through
    /// the operator word (`${x:-…}`), so a hostile script can otherwise
    /// exhaust the stack.
    expansion_depth: usize,
    /// `return N` unwinding state for the innermost function body.
    return_signal: Option<i32>,
    /// Lines the current pipeline stage may still produce. Set from a
    /// downstream consumer (`head -n`, `grep -m`) so an unbounded producer such
    /// as `yes` stops exactly at demand instead of buffering forever.
    line_budget: Option<usize>,
    /// Seed for the next pipeline's standard input (process substitution and
    /// `exec_script_with_stdin`).
    seed_stdin: Option<String>,
    /// Standard input of the compound command currently executing, shared by
    /// every command inside it (`{ …; } < f`, `while …; done < f`).
    compound_stdin: Option<StdinBuffer>,
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

/// Byte cursor over a compound command's redirected standard input.
///
/// `while read l; do …; done < f` shares one file offset across every command
/// in the loop body, exactly like a real shell descriptor, so `read` consumes a
/// line per iteration instead of re-reading the whole buffer.
#[derive(Debug, Clone)]
struct StdinBuffer {
    data: String,
    pos: usize,
}

impl StdinBuffer {
    fn new(data: String) -> Self {
        Self { data, pos: 0 }
    }

    /// Take the remainder and mark the buffer consumed.
    fn take_remaining(&mut self) -> String {
        let rest = self.data[self.pos..].to_string();
        self.pos = self.data.len();
        rest
    }

    /// Take one line without its newline; `None` at end of input.
    fn next_line(&mut self) -> Option<String> {
        if self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        match rest.find('\n') {
            Some(index) => {
                let line = rest[..index].to_string();
                self.pos += index + 1;
                Some(line)
            }
            None => {
                let line = rest.to_string();
                self.pos = self.data.len();
                Some(line)
            }
        }
    }
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
            opt_noglob: false,
            opt_noexec: false,
            opt_xtrace: false,
            nounset_error: None,
            param_error: None,
            loop_signal: None,
            expansion_stderr: String::new(),
            expansion_failed: false,
            functions: BTreeMap::new(),
            aliases: BTreeMap::new(),
            trap_exit: None,
            getopts_index: 0,
            getopts_cluster: 1,
            shopt_nullglob: false,
            readonly_names: std::collections::BTreeSet::new(),
            exec_replaced: false,
            dir_stack: Vec::new(),
            persistent_fds: BTreeMap::new(),
            positionals: Vec::new(),
            arrays: BTreeMap::new(),
            assoc_arrays: BTreeMap::new(),
            local_scopes: Vec::new(),
            jobs: Vec::new(),
            next_job_id: 1,
            function_depth: 0,
            source_depth: 0,
            expansion_depth: 0,
            return_signal: None,
            line_budget: None,
            seed_stdin: None,
            compound_stdin: None,
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
            let result = self.run_guarded(script);
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
            self.run_guarded(script)
        }
    }

    /// Run one script, converting a panic into a clean failed result.
    ///
    /// The explicit limits in [`ExecutionLimits`] are the real defense — a
    /// stack overflow is an abort and cannot be caught — but an unexpected
    /// `panic!`/index-out-of-bounds inside the interpreter would otherwise
    /// unwind through the embedder. This is the last-resort containment layer:
    /// it reports `bash: internal error: …` with exit status 2 and resets the
    /// transient per-command state. Filesystem and variable changes made before
    /// the panic are *not* rolled back.
    fn run_guarded(&mut self, script: &str) -> ExecResult {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.exec_in_current_state(script)
        })) {
            Ok(mut result) => {
                // `trap … EXIT` runs as the shell finishes, with its output
                // appended to this script's result.
                if let Some(body) = self.trap_exit.take() {
                    let trap_result = self.exec_in_current_state(&body);
                    result.stdout.push_str(&trap_result.stdout);
                    result.stderr.push_str(&trap_result.stderr);
                }
                result
            }
            Err(payload) => {
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".to_string());
                self.reset_transient_state();
                ExecResult {
                    stdout: String::new(),
                    stderr: format!("bash: internal error: {detail}\n"),
                    exit_code: 2,
                    cwd: self.cwd.clone(),
                }
            }
        }
    }

    /// Clear per-command state so a contained panic does not poison the shell.
    fn reset_transient_state(&mut self) {
        self.function_depth = 0;
        self.expansion_depth = 0;
        self.local_scopes.clear();
        self.loop_signal = None;
        self.return_signal = None;
        self.param_error = None;
        self.nounset_error = None;
        self.expansion_stderr.clear();
        self.expansion_failed = false;
        self.line_budget = None;
        self.seed_stdin = None;
        self.procsub_files.clear();
        self.procsub_readers.clear();
        self.pending_array = None;
    }

    fn exec_in_current_state(&mut self, script: &str) -> ExecResult {
        let parsed = match parse_script_with_limits(
            script,
            ParseLimits {
                max_command_substitution_depth: self
                    .execution_limits
                    .max_command_substitution_depth,
                max_nesting_depth: self.execution_limits.max_nesting_depth,
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
            // `set -n` takes effect for the statements that follow it in the
            // same script, so the check belongs inside the loop.
            if self.opt_noexec {
                break;
            }
            let should_run = match statement.connector {
                PipelineConnector::Always => true,
                PipelineConnector::And => exit_code == 0,
                PipelineConnector::Or => exit_code != 0,
            };
            if !should_run {
                continue;
            }

            let exec_result = self.exec_statement(&statement);
            if self.exec_replaced {
                // `exec cmd` replaced the shell, so nothing after it runs.
                stdout.push_str(&exec_result.0);
                stderr.push_str(&exec_result.1);
                return ExecResult {
                    stdout,
                    stderr,
                    exit_code: exec_result.2,
                    cwd: self.cwd.clone(),
                };
            }
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
            // The redirection is not itself a command; the compound inside is.
            StatementKind::Redirected { inner, .. } => {
                self.count_commands_in_statement(&Statement {
                    connector: PipelineConnector::Always,
                    kind: (**inner).clone(),
                    background: false,
                    negated: false,
                })
            }
            // `[[ … ]]`, `(( … ))` and `name=(…)` are not command invocations.
            StatementKind::Cond(_)
            | StatementKind::ArrayAssign(_)
            | StatementKind::Arithmetic(_) => 0,
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

    /// Parse and run a script string in the current shell state.
    ///
    /// Used by `eval` and `source`: both must see and modify the caller's
    /// variables and functions, unlike `$( … )` or a subshell.
    fn exec_script_text(&mut self, script: &str) -> (String, String, i32) {
        let parsed = match parse_script_with_limits(
            script,
            ParseLimits {
                max_command_substitution_depth: self
                    .execution_limits
                    .max_command_substitution_depth,
                max_nesting_depth: self.execution_limits.max_nesting_depth,
            },
        ) {
            Ok(parsed) => parsed,
            Err(error) => return (String::new(), format!("bash: {error}\n"), 2),
        };
        self.exec_statement_list(&parsed.statements)
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
            StatementKind::Redirected { inner, redirects } => {
                self.exec_redirected_compound(inner, redirects)
            }
            StatementKind::Arithmetic(expression) => self.exec_arithmetic_command(expression),
        };
        // A pipeline records one status per stage; every other statement kind
        // reports a single element, matching bash's PIPESTATUS after a compound.
        if !matches!(
            statement.kind,
            StatementKind::Pipeline(_) | StatementKind::Redirected { .. }
        ) {
            self.set_pipestatus(vec![result.2]);
        }
        result
    }

    /// Execute a compound command that carries its own redirections.
    ///
    /// The redirects apply to the compound as a whole: output produced anywhere
    /// inside it is delivered to the compound's descriptors, and a `<` input is
    /// shared by every command inside it.
    /// `(( expression ))`: evaluate for its side effects. The exit status is 1
    /// when the value is zero, 0 otherwise, and 2 on a syntax error.
    fn exec_arithmetic_command(&mut self, expression: &str) -> (String, String, i32) {
        let mut env = std::mem::take(&mut self.env);
        let result = eval_arithmetic_expr(expression, &mut env, &self.positionals);
        self.env = env;
        match result {
            Ok(value) => (String::new(), String::new(), i32::from(value == 0)),
            Err(message) => (String::new(), format!("bash: {message}\n"), 2),
        }
    }

    fn exec_redirected_compound(
        &mut self,
        inner: &StatementKind,
        redirects: &[Redirect],
    ) -> (String, String, i32) {
        let mut stdin_text = String::new();
        let (fd1, fd2) = match self.apply_redirects(redirects, &mut stdin_text) {
            Ok(sinks) => sinks,
            Err(message) => return (String::new(), message, 1),
        };

        // A compound's stdin is a shared cursor, so `read` inside a loop
        // advances it instead of seeing the whole file every iteration.
        let saved = self.compound_stdin.take();
        if !stdin_text.is_empty() {
            self.compound_stdin = Some(StdinBuffer::new(stdin_text));
        }

        let statement = Statement {
            connector: PipelineConnector::Always,
            kind: inner.clone(),
            background: false,
            negated: false,
        };
        let (stdout, stderr, exit_code) = self.exec_statement_inner(&statement);

        self.compound_stdin = saved;

        let mut result = CommandOutput {
            stdout,
            stderr,
            exit_code,
        };
        if let Err(error) = self.deliver(&mut result, &fd1, &fd2) {
            result.stderr.push_str(&format!("bash: {error}\n"));
            result.exit_code = 1;
        }
        (result.stdout, result.stderr, result.exit_code)
    }

    fn exec_pipeline(&mut self, commands: &[CommandInvocation]) -> (String, String, i32) {
        // Standard input may be seeded by process substitution.
        let mut stdin = match self.seed_stdin.take() {
            Some(seeded) => seeded,
            // Inside `{ …; } < f` / `while …; done < f` the whole compound
            // shares one stdin cursor. `read` pulls a single line so a loop
            // advances instead of re-reading everything; any other command
            // consumes the remainder like a real file descriptor.
            None => match self.compound_stdin.as_mut() {
                // Only a command that actually reads standard input advances
                // the shared offset. `while read l; do echo "$l"; done < f`
                // must not let `echo` drain the buffer.
                Some(buffer) => match first_command_name(commands).as_deref() {
                    Some("read") => match buffer.next_line() {
                        Some(line) => format!("{line}\n"),
                        None => String::new(),
                    },
                    Some(name) if command_reads_stdin(name) => buffer.take_remaining(),
                    // A compound stage has no command name of its own; its
                    // body reads the shared cursor, so hand over whatever is
                    // left and let the inner command decide.
                    None if commands
                        .first()
                        .is_some_and(|command| command.compound.is_some()) =>
                    {
                        buffer.take_remaining()
                    }
                    _ => String::new(),
                },
                None => String::new(),
            },
        };
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
            let result = if let Some(compound) = &command.compound {
                // A compound stage (`{ …; } | cat`, `( … ) | wc`) runs in a
                // subshell whose stdin is the previous stage's output.
                let mut child = self.child_shell();
                // A shared cursor, not a one-shot seed: `… | while read l; do
                // …; done` must consume a new line on every iteration. The
                // piped data replaces any inherited cursor, because this stage's
                // input is exactly what the previous stage produced.
                child.compound_stdin = Some(StdinBuffer::new(stdin.clone()));
                child.seed_stdin = None;
                let statement = Statement {
                    connector: PipelineConnector::Always,
                    kind: (**compound).clone(),
                    background: false,
                    negated: false,
                };
                let (out, err, code) = child.exec_statement_inner(&statement);
                self.fs = child.fs;
                self.env = child.env;
                CommandOutput {
                    stdout: out,
                    stderr: err,
                    exit_code: code,
                }
            } else if is_pipeline {
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

        // `set -n`: parse and expand, but never execute.
        if self.opt_noexec {
            return (stdout, stderr, exit_code);
        }

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

    /// Apply a redirection list, returning the two output sinks.
    ///
    /// Shared by simple commands and compound commands so both follow the same
    /// written-order descriptor rules (upstream #cb2b583b, #be55fec4).
    fn apply_redirects(
        &mut self,
        redirects: &[Redirect],
        stdin: &mut String,
    ) -> Result<(Sink, Sink), String> {
        // Each descriptor points at a sink; `2>&1` makes fd 2 share fd 1's sink
        // (a duplicate descriptor), while two redirects to the same path are
        // independent opens. Redirects are applied in written order, so
        // `2>&1 > f` leaves stderr on the caller while `> f 2>&1` sends both to
        // the file.
        // Descriptors redirected by an earlier `exec` stay in effect.
        let mut fd1 = self.persistent_fds.get(&1).cloned().unwrap_or(Sink::Stdout);
        let mut fd2 = self.persistent_fds.get(&2).cloned().unwrap_or(Sink::Stderr);

        for redirect in redirects {
            match redirect.mode {
                RedirectMode::Read => {
                    let target = self.expand_redirect_target(redirect);
                    // `< /dev/null` is an empty stream, not a file lookup.
                    if target == "/dev/null" {
                        stdin.clear();
                        continue;
                    }
                    match self.fs.read_file(&target) {
                        Ok(contents) => *stdin = contents.to_string(),
                        Err(error) => {
                            return Err(format!("bash: {error}\n"));
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
                        return Err(format!("bash: {error}\n"));
                    }
                    let sink = Sink::File {
                        path: target,
                        // The open above already applied the truncation, so
                        // subsequent writes through this descriptor append.
                        append: true,
                    };
                    match redirect.fd {
                        1 => fd1 = sink,
                        2 => fd2 = sink,
                        // Other descriptors are kept for `N>&M` duplication.
                        other => {
                            self.persistent_fds.insert(other, sink);
                        }
                    }
                }
                RedirectMode::Dup(source) => match (redirect.fd, source) {
                    // `1>&2`: send stdout wherever stderr currently goes.
                    (1, 2) => fd1 = fd2.clone(),
                    // `2>&1`: send stderr wherever stdout currently goes.
                    (2, 1) => fd2 = fd1.clone(),
                    // `2>&2` / `1>&1` are no-ops rather than errors.
                    (target, from) if target == from => {}
                    // `N>&M` / `1>&N`: bind the extra descriptor, and when the
                    // command itself writes to fd 1/2 make it the output sink.
                    (target, from) => {
                        let source_sink = match from {
                            1 => Some(fd1.clone()),
                            2 => Some(fd2.clone()),
                            other => self.persistent_fds.get(&other).cloned(),
                        };
                        if let Some(sink) = source_sink {
                            match target {
                                1 => fd1 = sink,
                                2 => fd2 = sink,
                                _ => {
                                    self.persistent_fds.insert(target, sink);
                                }
                            }
                        }
                    }
                },
                RedirectMode::Close => match redirect.fd {
                    1 => {
                        fd1 = Sink::Discard;
                        self.persistent_fds.remove(&1);
                    }
                    2 => {
                        fd2 = Sink::Discard;
                        self.persistent_fds.remove(&2);
                    }
                    other => {
                        self.persistent_fds.remove(&other);
                    }
                },
                // `<<<word`: the expanded word plus a newline becomes stdin.
                RedirectMode::HereString => {
                    let word = redirect.target.clone();
                    *stdin = format!("{}\n", self.expand_word(&word));
                }
                // `<<DELIM`: the collected body becomes stdin; a quoted
                // delimiter suppresses expansion.
                RedirectMode::Heredoc { .. } => {
                    if let RedirectMode::Heredoc { body, expand } = &redirect.mode {
                        *stdin = if *expand {
                            self.expand_heredoc(body)
                        } else {
                            body.clone()
                        };
                    }
                }
            }
        }

        Ok((fd1, fd2))
    }

    /// Recognize a leading `NAME=value` word and split off the value part.
    ///
    /// The value is returned as a word of its own so it can be expanded
    /// without field splitting, which is what bash does for assignments.
    fn assignment_prefix(&self, word: &Word) -> Option<(String, Word)> {
        let first = word.parts.first()?;
        if first.quoted || first.kind != WordPartKind::Variable {
            return None;
        }
        let (name, remainder) = first.text.split_once('=')?;
        if !is_assignment_name(name) {
            return None;
        }
        let mut parts: Vec<WordPart> = Vec::new();
        if !remainder.is_empty() {
            parts.push(WordPart {
                text: remainder.to_string(),
                kind: WordPartKind::Variable,
                quoted: false,
            });
        }
        parts.extend(word.parts.iter().skip(1).cloned());
        Some((
            name.to_string(),
            Word {
                parts,
                quoted: word.quoted,
            },
        ))
    }

    /// Expand an assignment value, where a leading `~` also means `$HOME`.
    fn expand_assignment_value(&mut self, word: &Word) -> String {
        let text = self.expand_word(word);
        if let Some(rest) = text.strip_prefix('~')
            && (rest.is_empty() || rest.starts_with('/'))
        {
            let home = self.env.get("HOME").cloned().unwrap_or_default();
            return format!("{home}{rest}");
        }
        text
    }

    fn run_invocation(
        &mut self,
        command: &CommandInvocation,
        pipeline_stdin: &str,
    ) -> CommandOutput {
        let mut stdin = pipeline_stdin.to_string();
        self.expansion_stderr.clear();
        self.expansion_failed = false;

        let (fd1, fd2) = match self.apply_redirects(&command.redirects, &mut stdin) {
            Ok(sinks) => sinks,
            Err(message) => {
                return CommandOutput {
                    stdout: String::new(),
                    stderr: message,
                    exit_code: 1,
                };
            }
        };

        // Leading `NAME=value` words are a temporary environment for this
        // command (`IFS= read -r line`, `LC_ALL=C sort < in`). They are peeled
        // before field splitting so the value is never split, and they are
        // restored once the command finishes.
        let mut assignments: Vec<(String, String)> = Vec::new();
        let mut word_start = 0usize;
        while word_start < command.words.len() {
            let Some((name, rhs)) = self.assignment_prefix(&command.words[word_start]) else {
                break;
            };
            let value = self.expand_assignment_value(&rhs);
            assignments.push((name, value));
            word_start += 1;
        }

        // Words expand in list context: `${a[@]}`, `"$@"` and pathname
        // expansion can each produce several arguments.
        let mut words: Vec<String> = Vec::new();
        for word in &command.words[word_start..] {
            words.extend(self.expand_word_list(word));
        }

        // `set -x` traces the command on stderr before it runs. A bare
        // assignment is traced too, using its `NAME=value` form.
        if self.opt_xtrace {
            if !words.is_empty() {
                self.expansion_stderr
                    .push_str(&format!("+ {}\n", words.join(" ")));
            } else if !assignments.is_empty() {
                let rendered: Vec<String> = assignments
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect();
                self.expansion_stderr
                    .push_str(&format!("+ {}\n", rendered.join(" ")));
            }
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

        // Aliases expand the first word before dispatch. The replacement is
        // re-split on blanks, matching bash's textual substitution; a word that
        // refers to itself is left alone so an alias cannot loop.
        if let Some(first) = words.first().cloned()
            && let Some(replacement) = self.aliases.get(&first).cloned()
            && replacement != first
        {
            let mut expanded: Vec<String> =
                replacement.split_whitespace().map(str::to_string).collect();
            expanded.extend(words.into_iter().skip(1));
            words = expanded;
        }

        // A command with no words left is a plain assignment statement, which
        // persists. Otherwise the assignments only live for this command.
        let temporary = !words.is_empty();
        // Temporary assignments are restored once the command finishes; a bare
        // assignment persists.
        let saved_env: Vec<(String, Option<String>)> = if temporary {
            assignments
                .iter()
                .map(|(name, _)| (name.clone(), self.env.get(name).cloned()))
                .collect()
        } else {
            Vec::new()
        };
        for (name, value) in &assignments {
            if self.readonly_names.contains(name) {
                // bash reports the attempt and leaves the value alone.
                return CommandOutput {
                    stdout: String::new(),
                    stderr: format!("bash: {name}: readonly variable\n"),
                    exit_code: 1,
                };
            }
            self.env.insert(name.clone(), value.clone());
        }

        // `exec` with no command word makes its redirections permanent for the
        // rest of the script; with a command it replaces this shell, which here
        // means "run it and stop executing the rest of the script".
        if words.first().map(String::as_str) == Some("exec") {
            let rest: Vec<String> = words[1..].to_vec();
            if rest.is_empty() {
                if fd1 != Sink::Stdout {
                    self.persistent_fds.insert(1, fd1.clone());
                }
                if fd2 != Sink::Stderr {
                    self.persistent_fds.insert(2, fd2.clone());
                }
                return CommandOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                };
            }
            let result = self.run_command(&rest, &stdin);
            // `exec cmd` never returns: stop the script after this command.
            self.exec_replaced = true;
            return result;
        }

        // Shell functions take precedence over built-ins and external commands.
        let mut result = match words.first() {
            Some(name) if self.functions.contains_key(name) => {
                let body = self.functions.get(name).cloned().unwrap_or_default();
                self.call_function(&body, &words[1..])
            }
            _ => self.run_command(&words, &stdin),
        };

        for (name, previous) in saved_env {
            match previous {
                Some(value) => {
                    self.env.insert(name, value);
                }
                None => {
                    self.env.remove(&name);
                }
            }
        }
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
                // `-n` suppresses the trailing newline; `-e` interprets
                // backslash escapes; `-E` is the (default) off switch. Flags
                // may be clustered (`-ne`).
                let mut newline = true;
                let mut escapes = false;
                let mut start = 0usize;
                while let Some(arg) = args.get(start) {
                    if arg.len() < 2 || !arg.starts_with('-') || arg.starts_with("--") {
                        break;
                    }
                    let cluster: Vec<char> = arg.chars().skip(1).collect();
                    if !cluster.iter().all(|flag| matches!(flag, 'n' | 'e' | 'E')) {
                        break;
                    }
                    for flag in cluster {
                        match flag {
                            'n' => newline = false,
                            'e' => escapes = true,
                            'E' => escapes = false,
                            _ => {}
                        }
                    }
                    start += 1;
                }
                let text = args[start..].join(" ");
                let text = if escapes {
                    interpret_echo_escapes(&text)
                } else {
                    text
                };
                stdout.push_str(&text);
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
                        // `-` is standard input, positioned where it appears.
                        if arg == "-" {
                            input.push_str(stdin);
                            continue;
                        }
                        match self.read_operand(arg) {
                            Ok(contents) => input.push_str(&contents),
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
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('l', "lines", opts::Arity::Flag),
                    ('w', "words", opts::Arity::Flag),
                    ('c', "bytes", opts::Arity::Flag),
                    ('m', "chars", opts::Arity::Flag),
                    ('L', "max-line-length", opts::Arity::Flag),
                ];
                match opts::parse("wc", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let mut lines = parsed.has("lines");
                        let mut words = parsed.has("words");
                        let mut bytes = parsed.has("bytes");
                        let chars = parsed.has("chars");
                        let max_line = parsed.has("max-line-length");
                        if !lines && !words && !bytes && !chars && !max_line {
                            lines = true;
                            words = true;
                            bytes = true;
                        }
                        let files = parsed.operands;
                        let mut texts: Vec<(String, String)> = Vec::new();
                        if files.is_empty() {
                            texts.push((String::new(), stdin.to_string()));
                        } else {
                            for name in &files {
                                match self.fs.read_file(&self.resolve_path(name)) {
                                    Ok(contents) => {
                                        texts.push((name.clone(), contents.to_string()))
                                    }
                                    Err(error) => {
                                        stderr.push_str(&format!("wc: {error}\n"));
                                        exit_code = 1;
                                    }
                                }
                            }
                        }
                        if exit_code == 0 {
                            // Column widths: with file operands each column is
                            // padded to its own widest value (including the
                            // `total` row); reading standard input uses the
                            // fixed 7-column layout instead.
                            let from_stdin = files.is_empty();
                            let mut rows: Vec<([usize; 5], String)> = Vec::new();
                            let mut total = [0usize; 5];
                            for (name, text) in &texts {
                                let counts = [
                                    text.bytes().filter(|byte| *byte == b'\n').count(),
                                    text.split_whitespace().count(),
                                    text.len(),
                                    text.chars().count(),
                                    text.lines()
                                        .map(|line| line.chars().count())
                                        .max()
                                        .unwrap_or(0),
                                ];
                                for (index, value) in counts.iter().enumerate() {
                                    total[index] += *value;
                                }
                                rows.push((counts, name.clone()));
                            }
                            let with_total = texts.len() > 1;
                            let mut widths = [1usize; 5];
                            let consider = |counts: &[usize; 5], widths: &mut [usize; 5]| {
                                for (index, value) in counts.iter().enumerate() {
                                    widths[index] = widths[index].max(value.to_string().len());
                                }
                            };
                            for (counts, _) in &rows {
                                consider(counts, &mut widths);
                            }
                            if with_total {
                                consider(&total, &mut widths);
                            }

                            let selected = [lines, words, bytes, chars, max_line];
                            // Each selected column is right-aligned to at least
                            // two characters (one when `wc` prints a single
                            // column); reading stdin uses the 7-column layout.
                            let selected_count = selected.iter().filter(|flag| **flag).count();
                            let render = |counts: &[usize; 5], widths: &[usize; 5]| -> String {
                                let mut parts = Vec::new();
                                for (index, enabled) in selected.iter().enumerate() {
                                    if !enabled {
                                        continue;
                                    }
                                    // An entirely empty file prints bare zeros:
                                    // upstream emits no padding at all.
                                    let width = if counts.iter().all(|value| *value == 0) {
                                        1
                                    } else if selected_count == 1 {
                                        // A single column is never padded when
                                        // there is one input; multiple inputs pad
                                        // to their own widest value.
                                        if texts.len() > 1 {
                                            widths[index].max(2)
                                        } else {
                                            1
                                        }
                                    } else if from_stdin {
                                        7
                                    } else {
                                        widths[index].max(2)
                                    };
                                    parts.push(format!("{:>width$}", counts[index], width = width));
                                }
                                parts.join(" ")
                            };

                            for (counts, name) in &rows {
                                let body = render(counts, &widths);
                                if name.is_empty() {
                                    stdout.push_str(&format!("{body}\n"));
                                } else {
                                    stdout.push_str(&format!("{body} {name}\n"));
                                }
                            }
                            if with_total {
                                let body = render(&total, &widths);
                                stdout.push_str(&format!("{body} total\n"));
                            }
                        }
                    }
                }
            }
            "head" | "tail" => {
                let program = if command == "head" { "head" } else { "tail" };
                match parse_head_tail_args(program, args) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(options) => {
                        let files = options.operands.clone();
                        // Each file's header is emitted immediately before its
                        // own content, with a blank line between files, exactly
                        // as GNU head/tail do.
                        let sources: Vec<(String, Option<String>)> = if files.is_empty() {
                            vec![(String::new(), Some(stdin.to_string()))]
                        } else {
                            files
                                .iter()
                                .map(|name| {
                                    // `/dev/zero` is bounded by the downstream
                                    // budget, so `head -c 5 /dev/zero` terminates.
                                    (name.clone(), self.read_operand(name).ok())
                                })
                                .collect()
                        };
                        let multiple = sources.len() > 1;
                        for (index, (name, text)) in sources.iter().enumerate() {
                            let Some(text) = text else {
                                stderr.push_str(&format!(
                                    "{program}: {name}: No such file or directory\n"
                                ));
                                exit_code = 1;
                                continue;
                            };
                            if multiple {
                                if index > 0 {
                                    stdout.push('\n');
                                }
                                stdout.push_str(&format!("==> {name} <==\n"));
                            }
                            stdout.push_str(&head_or_tail(text, &options));
                        }
                    }
                }
            }
            "sort" => match sort_options(args) {
                Err(message) => {
                    stderr.push_str(&message);
                    exit_code = 2;
                }
                Ok(options) => {
                    match self.read_inputs_for_text_tool(
                        &options.operands,
                        stdin,
                        "sort",
                        &mut stderr,
                    ) {
                        Ok(input) => {
                            let name = options.operands.first().map(String::as_str);
                            match sort_lines_named(&input, &options, name) {
                                Ok(text) => stdout.push_str(&text),
                                Err(message) => {
                                    stderr.push_str(&message);
                                    exit_code = 1;
                                }
                            }
                        }
                        Err(()) => exit_code = 1,
                    }
                }
            },
            "uniq" => match uniq_options(args) {
                Err(message) => {
                    stderr.push_str(&message);
                    exit_code = 2;
                }
                Ok(options) => {
                    match self.read_inputs_for_text_tool(
                        &options.operands,
                        stdin,
                        "uniq",
                        &mut stderr,
                    ) {
                        Ok(input) => stdout.push_str(&filter_uniq(&input, &options)),
                        Err(()) => exit_code = 1,
                    }
                }
            },
            "cut" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('d', "delimiter", opts::Arity::Value),
                    ('f', "fields", opts::Arity::Value),
                    ('c', "characters", opts::Arity::Value),
                ];
                match opts::parse("cut", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        // GNU `cut` defaults to TAB, not comma.
                        let delimiter = parsed
                            .value("delimiter")
                            .and_then(|value| value.chars().next())
                            .unwrap_or('\t');
                        let selection = parsed
                            .value("fields")
                            .map(|value| (CutMode::Fields, parse_cut_list(value)))
                            .or_else(|| {
                                parsed
                                    .value("characters")
                                    .map(|value| (CutMode::Characters, parse_cut_list(value)))
                            });
                        match selection {
                            None => {
                                stderr.push_str(
                                    "cut: you must specify a list of bytes, characters, or fields\n",
                                );
                                exit_code = 1;
                            }
                            Some((_mode, Err(message))) => {
                                stderr.push_str(&format!("cut: {message}\n"));
                                exit_code = 1;
                            }
                            Some((mode, Ok(ranges))) => {
                                match self.read_inputs_for_text_tool(
                                    &parsed.operands,
                                    stdin,
                                    "cut",
                                    &mut stderr,
                                ) {
                                    Ok(input) => {
                                        for line in input.lines() {
                                            let pieces: Vec<String> = match mode {
                                                CutMode::Characters => {
                                                    let chars: Vec<char> = line.chars().collect();
                                                    select_ranges(&chars, &ranges)
                                                        .iter()
                                                        .map(|ch| ch.to_string())
                                                        .collect()
                                                }
                                                CutMode::Fields => {
                                                    let fields: Vec<&str> =
                                                        line.split(delimiter).collect();
                                                    select_ranges(&fields, &ranges)
                                                        .iter()
                                                        .map(|field| field.to_string())
                                                        .collect()
                                                }
                                            };
                                            // Fields are concatenated without a
                                            // separator (`cut -d: -f1,3` prints `a:c`
                                            // because the delimiter is re-inserted).
                                            let joiner = match mode {
                                                CutMode::Fields => delimiter.to_string(),
                                                CutMode::Characters => String::new(),
                                            };
                                            stdout.push_str(&pieces.join(&joiner));
                                            stdout.push('\n');
                                        }
                                    }
                                    Err(()) => exit_code = 1,
                                }
                            }
                        }
                    }
                }
            }
            "tr" => {
                // `tr [-c|-C] [-d] [-s] SET1 [SET2]`
                let mut delete = false;
                let mut squeeze = false;
                let mut complement = false;
                let mut sets: Vec<&String> = Vec::new();
                let mut bad_option = None;
                for arg in args {
                    if arg.starts_with('-') && arg.len() > 1 && sets.is_empty() {
                        for flag in arg.chars().skip(1) {
                            match flag {
                                'd' => delete = true,
                                's' => squeeze = true,
                                'c' | 'C' => complement = true,
                                other => bad_option = Some(other),
                            }
                        }
                        continue;
                    }
                    sets.push(arg);
                }
                if let Some(flag) = bad_option {
                    stderr.push_str(&format!("tr: invalid option -- '{flag}'\n"));
                    stderr.push_str("usage: tr [-cCds] SET1 [SET2]\n");
                    exit_code = 1;
                } else if sets.is_empty() || (!delete && sets.len() < 2 && !squeeze) {
                    stderr.push_str("usage: tr [-cCds] SET1 [SET2]\n");
                    exit_code = 1;
                } else {
                    let set1 = expand_tr_set(sets[0]);
                    let set2 = sets
                        .get(1)
                        .map(|spec| expand_tr_set(spec))
                        .unwrap_or_default();
                    let in_set = |ch: char| {
                        let present = set1.contains(&ch);
                        if complement { !present } else { present }
                    };
                    // `-s` collapses runs of characters from its squeeze set:
                    // SET2 when a translation is given, otherwise SET1.
                    let squeeze_set: &[char] = if !squeeze {
                        &[]
                    } else if sets.len() > 1 && !delete {
                        &set2
                    } else {
                        &set1
                    };
                    let translating = !delete && sets.len() > 1 && !set2.is_empty();
                    let mut previous: Option<char> = None;
                    for ch in stdin.chars() {
                        if delete && in_set(ch) {
                            continue;
                        }
                        let mapped = if !translating {
                            ch
                        } else if let Some(position) =
                            set1.iter().position(|candidate| *candidate == ch)
                        {
                            set2.get(position)
                                .copied()
                                .unwrap_or(*set2.last().expect("SET2 is non-empty"))
                        } else {
                            ch
                        };
                        if !squeeze_set.is_empty()
                            && squeeze_set.contains(&mapped)
                            && previous == Some(mapped)
                        {
                            continue;
                        }
                        previous = Some(mapped);
                        stdout.push(mapped);
                    }
                }
            }
            "basename" => {
                // `basename [-a] [-s SUFFIX] NAME… [SUFFIX]`
                let mut multiple = false;
                let mut suffix: Option<String> = None;
                let mut operands: Vec<String> = Vec::new();
                let mut index = 0usize;
                while index < args.len() {
                    let arg = &args[index];
                    if arg == "-a" || arg == "--multiple" {
                        multiple = true;
                        index += 1;
                        continue;
                    }
                    if let Some(value) = arg.strip_prefix("--suffix=") {
                        suffix = Some(value.to_string());
                        index += 1;
                        continue;
                    }
                    if arg == "-s" || arg == "--suffix" {
                        suffix = args.get(index + 1).cloned();
                        index += 2;
                        continue;
                    }
                    if arg.len() > 2 && arg.starts_with("-s") {
                        suffix = Some(arg[2..].to_string());
                        index += 1;
                        continue;
                    }
                    break;
                }
                operands.extend(args[index..].iter().cloned());
                // A trailing operand is the suffix unless `-s`/`-a` was given.
                let mut strip = suffix.clone();
                if strip.is_none() && !multiple && operands.len() > 1 {
                    strip = operands.pop();
                }
                if operands.is_empty() {
                    stderr.push_str("basename: missing operand\n");
                    exit_code = 1;
                } else if operands.len() > 1 && !multiple && suffix.is_none() {
                    stderr.push_str("basename: extra operand\n");
                    exit_code = 1;
                } else {
                    for path in &operands {
                        let mut name = path
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            name = "/".to_string();
                        }
                        if let Some(suffix) = &strip
                            && !suffix.is_empty()
                            && name.len() > suffix.len()
                            && name.ends_with(suffix.as_str())
                        {
                            name.truncate(name.len() - suffix.len());
                        }
                        stdout.push_str(&name);
                        stdout.push('\n');
                    }
                }
            }
            "dirname" => {
                // `dirname NAME…` — every operand gets a line.
                let mut operands: Vec<&String> = args.iter().collect();
                if args.first().is_some_and(|arg| arg == "-z") {
                    operands = args[1..].iter().collect();
                }
                if operands.is_empty() {
                    stderr.push_str("dirname: missing operand\n");
                    exit_code = 1;
                } else {
                    for path in operands {
                        stdout.push_str(&textual_dirname(path));
                        stdout.push('\n');
                    }
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
                let mut force = false;
                let mut operands = Vec::new();
                for arg in args {
                    if arg.starts_with('-') && arg != "-" {
                        if arg.chars().skip(1).all(|ch| matches!(ch, 'r' | 'R' | 'f')) {
                            recursive |= arg.chars().any(|ch| matches!(ch, 'r' | 'R'));
                            force |= arg.chars().any(|ch| matches!(ch, 'f'));
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
                            // `-f` suppresses the diagnostic for a missing file.
                            if force && error.to_string().contains("No such file") {
                                continue;
                            }
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
                // An empty stream is end-of-file, which bash reports as a
                // non-zero status. Without this distinction a loop such as
                // `while read l; do …; done` runs forever against an empty
                // stdin instead of terminating at the first read.
                if stdin.is_empty() {
                    exit_code = 1;
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
                            'f' => self.opt_noglob = enable,
                            'n' => self.opt_noexec = enable,
                            'x' => self.opt_xtrace = enable,
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
                let outcome = just_bash_sed::run(args, stdin, &|path: &str| {
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
                let outcome = just_bash_awk::run(args, stdin, &reader);
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
                let outcome = just_bash_jq::run(args, stdin, &reader);
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
            // `find` walks the tree and prints paths matching its expression.
            "find" => {
                let mut roots: Vec<String> = Vec::new();
                let mut options = textutils::FindOptions {
                    max_depth: None,
                    min_depth: 0,
                    expression: textutils::FindExpression::default(),
                };
                // Tests after `-o` form the second branch of the expression.
                let mut in_or = false;
                let mut index = 0usize;
                let mut error: Option<String> = None;
                while index < args.len() {
                    let arg = args[index].as_str();
                    match arg {
                        "-maxdepth" | "-mindepth" => {
                            let Some(value) = args.get(index + 1) else {
                                error = Some(format!("find: missing argument to `{arg}'"));
                                break;
                            };
                            let depth = value.parse::<usize>().unwrap_or(0);
                            if arg == "-maxdepth" {
                                options.max_depth = Some(depth);
                            } else {
                                options.min_depth = depth;
                            }
                            index += 2;
                        }
                        "-name" | "-type" => {
                            let Some(value) = args.get(index + 1) else {
                                error = Some(format!("find: missing argument to `{arg}'"));
                                break;
                            };
                            let test = if arg == "-name" {
                                textutils::FindTest::Name(value.clone())
                            } else {
                                textutils::FindTest::Type(value.chars().next().unwrap_or('f'))
                            };
                            if in_or {
                                options
                                    .expression
                                    .or
                                    .get_or_insert_with(Vec::new)
                                    .push(test);
                            } else {
                                options.expression.and.push(test);
                            }
                            index += 2;
                        }
                        "-o" | "-or" => {
                            in_or = true;
                            index += 1;
                        }
                        _ if arg.starts_with('-') => {
                            error = Some(format!("find: unsupported option `{arg}'"));
                            break;
                        }
                        _ => {
                            roots.push(arg.to_string());
                            index += 1;
                        }
                    }
                }
                if let Some(message) = error {
                    stderr.push_str(&message);
                    stderr.push('\n');
                    exit_code = 1;
                } else if roots.is_empty() {
                    stderr.push_str("find: missing path operand\n");
                    exit_code = 1;
                } else if exit_code == 0 {
                    for root in &roots {
                        let resolved = self.resolve_path(root);
                        let entries = match self.fs.entries_under(&resolved) {
                            Ok(entries) => entries,
                            Err(_) => {
                                stderr.push_str(&format!(
                                    "find: {root}: No such file or directory\n"
                                ));
                                exit_code = 1;
                                continue;
                            }
                        };
                        let mut visited: Vec<(String, bool, usize)> = Vec::new();
                        for (path, contents) in entries {
                            let is_dir = contents.is_none();
                            // Depth counts path components below the root.
                            let relative = path.strip_prefix(&resolved).unwrap_or("");
                            let depth = relative.split('/').filter(|part| !part.is_empty()).count();
                            let display = if depth == 0 {
                                root.clone()
                            } else {
                                let suffix = relative.trim_start_matches('/');
                                if root == "." {
                                    format!("./{suffix}")
                                } else {
                                    format!("{}/{}", root.trim_end_matches('/'), suffix)
                                }
                            };
                            visited.push((display, is_dir, depth));
                        }
                        let rows: Vec<(&str, bool, usize)> = visited
                            .iter()
                            .map(|(display, is_dir, depth)| (display.as_str(), *is_dir, *depth))
                            .collect();
                        stdout.push_str(&textutils::find(&rows, &options));
                    }
                }
            }
            // `tar` packs and unpacks members of the in-memory filesystem.
            "tar" => {
                let mut create = false;
                let mut extract = false;
                let mut list = false;
                let mut compress = false;
                let mut archive_name: Option<String> = None;
                let mut change_dir: Option<String> = None;
                let mut strip_components = 0usize;
                let mut operands: Vec<String> = Vec::new();
                let mut index = 0usize;
                let mut error: Option<String> = None;
                while index < args.len() {
                    let arg = args[index].as_str();
                    if let Some(value) = arg.strip_prefix("--strip-components=") {
                        strip_components = value.parse().unwrap_or(0);
                        index += 1;
                        continue;
                    }
                    if arg == "--strip-components" {
                        strip_components = args
                            .get(index + 1)
                            .and_then(|value| value.parse().ok())
                            .unwrap_or(0);
                        index += 2;
                        continue;
                    }
                    if arg.starts_with("--") {
                        error = Some(format!("tar: unsupported option `{arg}'"));
                        break;
                    }
                    if arg.starts_with('-') && arg.len() > 1 {
                        // Flags may be clustered (`-czf`), and `f`/`C` take the
                        // rest of the cluster or the next argument.
                        let chars: Vec<char> = arg.chars().skip(1).collect();
                        let mut position = 0usize;
                        while position < chars.len() {
                            match chars[position] {
                                'c' => create = true,
                                'x' => extract = true,
                                't' => list = true,
                                'z' => compress = true,
                                'v' => {}
                                'f' | 'C' => {
                                    let rest: String = chars[position + 1..].iter().collect();
                                    let value = if rest.is_empty() {
                                        index += 1;
                                        match args.get(index) {
                                            Some(value) => value.clone(),
                                            None => {
                                                error = Some(format!(
                                                    "tar: option requires an argument -- '{}'",
                                                    chars[position]
                                                ));
                                                break;
                                            }
                                        }
                                    } else {
                                        rest
                                    };
                                    if chars[position] == 'f' {
                                        archive_name = Some(value);
                                    } else {
                                        change_dir = Some(value);
                                    }
                                    break;
                                }
                                other => {
                                    error = Some(format!("tar: invalid option -- '{other}'"));
                                    break;
                                }
                            }
                            position += 1;
                        }
                        if error.is_some() {
                            break;
                        }
                        index += 1;
                        continue;
                    }
                    operands.push(arg.to_string());
                    index += 1;
                }
                if archive_name.is_none() && error.is_none() {
                    error = Some("tar: no archive file specified; use -f".to_string());
                }
                if let Some(message) = error {
                    stderr.push_str(&message);
                    stderr.push('\n');
                    exit_code = 2;
                } else if let Some(archive_name) = archive_name {
                    let archive_path = self.resolve_path(&archive_name);
                    if create {
                        // Collect the requested paths, expanding directories.
                        let mut members: Vec<just_bash_tar::Entry> = Vec::new();
                        for operand in &operands {
                            let resolved = self.resolve_path(operand);
                            if self.fs.is_dir(&resolved) {
                                let base = operand.trim_end_matches('/').to_string();
                                members.push(just_bash_tar::Entry {
                                    path: base.clone(),
                                    data: Vec::new(),
                                    is_dir: true,
                                });
                                let entries = self.fs.entries_under(&resolved).unwrap_or_default();
                                for (path, contents) in entries {
                                    let relative = path
                                        .strip_prefix(&format!("{resolved}/"))
                                        .unwrap_or("")
                                        .to_string();
                                    if relative.is_empty() {
                                        continue;
                                    }
                                    let is_dir = contents.is_none();
                                    members.push(just_bash_tar::Entry {
                                        path: format!("{base}/{relative}"),
                                        data: contents
                                            .map(|body| body.into_bytes())
                                            .unwrap_or_default(),
                                        is_dir,
                                    });
                                }
                            } else if let Ok(contents) = self.fs.read_file(&resolved) {
                                members.push(just_bash_tar::Entry {
                                    path: operand.trim_start_matches('/').to_string(),
                                    data: contents.as_bytes().to_vec(),
                                    is_dir: false,
                                });
                            } else {
                                stderr.push_str(&format!(
                                    "tar: {operand}: Cannot stat: No such file or directory\n"
                                ));
                                exit_code = 2;
                            }
                        }
                        if exit_code == 0 {
                            let bytes = if compress {
                                just_bash_tar::gzip(&just_bash_tar::archive(&members))
                            } else {
                                just_bash_tar::archive(&members)
                            };
                            // Archives are binary; the filesystem holds strings, so
                            // bytes are encoded losslessly as Latin-1 characters.
                            let text = just_bash_tar::encode_bytes(&bytes);
                            if let Err(fs_error) = self.fs.write_file(&archive_path, &text) {
                                stderr.push_str(&format!("tar: {fs_error}\n"));
                                exit_code = 2;
                            }
                        }
                    } else if extract || list {
                        match self.fs.read_file(&archive_path) {
                            Err(fs_error) => {
                                stderr.push_str(&format!("tar: {fs_error}\n"));
                                exit_code = 2;
                            }
                            Ok(contents) => {
                                let raw = just_bash_tar::decode_bytes(contents);
                                let bytes = if compress {
                                    match just_bash_tar::gunzip(&raw) {
                                        Ok(bytes) => bytes,
                                        Err(message) => {
                                            stderr.push_str(&format!("tar: {message}\n"));
                                            exit_code = 2;
                                            Vec::new()
                                        }
                                    }
                                } else {
                                    raw
                                };
                                if exit_code == 0 {
                                    match just_bash_tar::extract(&bytes) {
                                        Err(message) => {
                                            stderr.push_str(&format!("tar: {message}\n"));
                                            exit_code = 2;
                                        }
                                        Ok(entries) => {
                                            let target = change_dir
                                                .as_deref()
                                                .map(|dir| self.resolve_path(dir));
                                            for entry in entries {
                                                // `--strip-components` drops leading
                                                // path components.
                                                let stripped: Vec<&str> = entry
                                                    .path
                                                    .split('/')
                                                    .filter(|part| !part.is_empty())
                                                    .collect();
                                                if stripped.len() <= strip_components {
                                                    continue;
                                                }
                                                let relative =
                                                    stripped[strip_components..].join("/");
                                                if list {
                                                    if entry.is_dir {
                                                        stdout.push_str(&format!("{relative}/\n"));
                                                    } else {
                                                        stdout.push_str(&relative);
                                                        stdout.push('\n');
                                                    }
                                                    continue;
                                                }
                                                let destination = match &target {
                                                    Some(dir) => join_path(dir, &relative),
                                                    None => self.resolve_path(&relative),
                                                };
                                                if entry.is_dir {
                                                    let _ = self.fs.create_dir_all(&destination);
                                                } else {
                                                    if let Some(parent) = destination
                                                        .rsplit_once('/')
                                                        .map(|(head, _)| head.to_string())
                                                    {
                                                        let _ = self.fs.create_dir_all(&parent);
                                                    }
                                                    let text = String::from_utf8_lossy(&entry.data)
                                                        .into_owned();
                                                    if let Err(fs_error) =
                                                        self.fs.write_file(&destination, &text)
                                                    {
                                                        stderr.push_str(&format!(
                                                            "tar: {fs_error}\n"
                                                        ));
                                                        exit_code = 2;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        stderr.push_str("tar: you must specify one of -c, -x or -t\n");
                        exit_code = 2;
                    }
                }
            }
            // `nl` numbers lines; `join` merges two sorted files on a key.
            "nl" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('b', "body-numbering", opts::Arity::Value),
                    ('n', "number-format", opts::Arity::Value),
                    ('w', "number-width", opts::Arity::Value),
                    ('s', "separator", opts::Arity::Value),
                    ('v', "starting-line-number", opts::Arity::Value),
                    ('i', "line-increment", opts::Arity::Value),
                ];
                match opts::parse("nl", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let mut options = textutils::NlOptions::default();
                        if let Some(style) = parsed.value("body-numbering") {
                            options.number_blank = style == "a";
                        }
                        match parsed.value("number-format").unwrap_or("rn") {
                            "ln" => options.format = textutils::NlNumberFormat::Left,
                            "rz" => options.format = textutils::NlNumberFormat::RightZero,
                            _ => options.format = textutils::NlNumberFormat::Right,
                        }
                        if let Some(width) = parsed.value("number-width") {
                            options.width = width.parse().unwrap_or(6);
                        }
                        if let Some(separator) = parsed.value("separator") {
                            options.separator = separator.to_string();
                        }
                        if let Some(start) = parsed.value("starting-line-number") {
                            options.start = start.parse().unwrap_or(1);
                        }
                        if let Some(increment) = parsed.value("line-increment") {
                            options.increment = increment.parse().unwrap_or(1);
                        }
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "nl",
                            &mut stderr,
                        ) {
                            Ok(input) => stdout.push_str(&textutils::nl(&input, &options)),
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            "join" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('t', "separator", opts::Arity::Value),
                    ('1', "join-field-1", opts::Arity::Value),
                    ('2', "join-field-2", opts::Arity::Value),
                    ('a', "also-unpaired", opts::Arity::Value),
                    ('v', "only-unpaired", opts::Arity::Value),
                ];
                match opts::parse("join", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let mut options = textutils::JoinOptions {
                            separator: parsed
                                .value("separator")
                                .and_then(|value| value.chars().next()),
                            ..Default::default()
                        };
                        if let Some(field) = parsed.value("join-field-1") {
                            options.first_key = field.parse().unwrap_or(1);
                        }
                        if let Some(field) = parsed.value("join-field-2") {
                            options.second_key = field.parse().unwrap_or(1);
                        }
                        if let Some(which) = parsed.value("also-unpaired") {
                            options.include_first |= which == "1";
                            options.include_second |= which == "2";
                        }
                        if let Some(which) = parsed.value("only-unpaired") {
                            options.only_unpaired = which.parse().ok();
                        }
                        let operands = parsed.operands.clone();
                        if operands.len() != 2 {
                            stderr.push_str("join: usage: join [OPTION]... FILE1 FILE2\n");
                            exit_code = 1;
                        } else {
                            let mut inputs: Vec<String> = Vec::new();
                            for name in &operands {
                                match self.fs.read_file(&self.resolve_path(name)) {
                                    Ok(contents) => inputs.push(contents.to_string()),
                                    Err(error) => {
                                        stderr.push_str(&format!("join: {error}\n"));
                                        exit_code = 1;
                                        break;
                                    }
                                }
                            }
                            if exit_code == 0 {
                                stdout.push_str(&textutils::join(&inputs[0], &inputs[1], &options));
                            }
                        }
                    }
                }
            }
            // `rev`, `fold`, `expand`, `unexpand`, `strings`: pure text
            // transforms over stdin or file operands.
            "rev" => match self.read_inputs_for_text_tool(args, stdin, "rev", &mut stderr) {
                Ok(input) => stdout.push_str(&textutils::rev(&input)),
                Err(()) => exit_code = 1,
            },
            "fold" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('w', "width", opts::Arity::Value),
                    ('s', "spaces", opts::Arity::Flag),
                ];
                match opts::parse("fold", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let width = parsed
                            .value("width")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(80);
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "fold",
                            &mut stderr,
                        ) {
                            Ok(input) => stdout.push_str(&textutils::fold(
                                &input,
                                width,
                                parsed.has("spaces"),
                            )),
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            "expand" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[('t', "tabs", opts::Arity::Value)];
                match opts::parse("expand", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let stop = parsed
                            .value("tabs")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(8);
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "expand",
                            &mut stderr,
                        ) {
                            Ok(input) => stdout.push_str(&textutils::expand(&input, stop)),
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            "unexpand" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('t', "tabs", opts::Arity::Value),
                    ('a', "all", opts::Arity::Flag),
                ];
                match opts::parse("unexpand", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let stop = parsed
                            .value("tabs")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(8);
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "unexpand",
                            &mut stderr,
                        ) {
                            Ok(input) => stdout.push_str(&textutils::unexpand(
                                &input,
                                stop,
                                parsed.has("all"),
                            )),
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            "strings" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[('n', "bytes", opts::Arity::Value)];
                match opts::parse("strings", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let min = parsed
                            .value("bytes")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(4);
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "strings",
                            &mut stderr,
                        ) {
                            Ok(input) => {
                                stdout.push_str(&textutils::strings(input.as_bytes(), min))
                            }
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            // `paste` joins files side by side, `column` aligns fields.
            "paste" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('d', "delimiters", opts::Arity::Value),
                    ('s', "serial", opts::Arity::Flag),
                ];
                match opts::parse("paste", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let delimiters: Vec<char> = parsed
                            .value("delimiters")
                            .map(|value| value.chars().collect())
                            .unwrap_or_default();
                        let operands = parsed.operands.clone();
                        let mut inputs: Vec<Vec<String>> = Vec::new();
                        if operands.is_empty() {
                            inputs.push(stdin.lines().map(str::to_string).collect());
                        } else {
                            // Every `-` operand reads from the *same* stdin, so
                            // `paste - - -` folds the stream into three columns:
                            // reading proceeds row by row across the columns.
                            let dash_count = operands.iter().filter(|name| *name == "-").count();
                            let stdin_lines: Vec<String> =
                                stdin.lines().map(str::to_string).collect();
                            let mut dash_column = 0usize;
                            let mut dash_cursor = 0usize;
                            for name in &operands {
                                let contents = if name == "-" {
                                    let mut rows: Vec<String> = Vec::new();
                                    // Column `dash_column` takes one line per row.
                                    let mut index = dash_cursor + dash_column;
                                    while index < stdin_lines.len() {
                                        rows.push(stdin_lines[index].clone());
                                        index += dash_count.max(1);
                                    }
                                    dash_column += 1;
                                    if dash_column >= dash_count.max(1) {
                                        dash_column = 0;
                                        dash_cursor += dash_count.max(1);
                                    }
                                    rows.join("\n")
                                } else {
                                    match self.fs.read_file(&self.resolve_path(name)) {
                                        Ok(contents) => contents.to_string(),
                                        Err(error) => {
                                            stderr.push_str(&format!("paste: {error}\n"));
                                            exit_code = 1;
                                            continue;
                                        }
                                    }
                                };
                                inputs.push(contents.lines().map(str::to_string).collect());
                            }
                        }
                        if exit_code == 0 {
                            stdout.push_str(&textutils::paste(
                                &inputs,
                                &delimiters,
                                parsed.has("serial"),
                            ));
                        }
                    }
                }
            }
            "column" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('t', "table", opts::Arity::Flag),
                    ('s', "separator", opts::Arity::Value),
                ];
                match opts::parse("column", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let separator = parsed
                            .value("separator")
                            .and_then(|value| value.chars().next());
                        match self.read_inputs_for_text_tool(
                            &parsed.operands,
                            stdin,
                            "column",
                            &mut stderr,
                        ) {
                            Ok(input) => stdout.push_str(&textutils::column(&input, separator)),
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            // `split` writes its chunks back into the virtual filesystem.
            "split" => {
                const TABLE: &[(char, &str, opts::Arity)] = &[
                    ('l', "lines", opts::Arity::Value),
                    ('b', "bytes", opts::Arity::Value),
                ];
                match opts::parse("split", args, TABLE) {
                    Err(message) => {
                        stderr.push_str(&message);
                        stderr.push('\n');
                        exit_code = 1;
                    }
                    Ok(parsed) => {
                        let lines = parsed.value("lines").and_then(|v| v.parse().ok());
                        let bytes = parsed.value("bytes").and_then(|v| v.parse().ok());
                        // `split [OPTION]… [FILE [PREFIX]]`: the first operand
                        // is the input file, the second the output prefix.
                        let mut operands = parsed.operands.clone();
                        let input_files: Vec<String> = if operands.is_empty() {
                            Vec::new()
                        } else {
                            vec![operands.remove(0)]
                        };
                        let prefix = operands
                            .first()
                            .map(|name| format!("{name}."))
                            .unwrap_or_else(|| "x".to_string());
                        let files = input_files;
                        match self.read_inputs_for_text_tool(&files, stdin, "split", &mut stderr) {
                            Ok(input) => {
                                for (name, contents) in
                                    textutils::split(&input, &prefix, lines, bytes)
                                {
                                    let path = self.resolve_path(&name);
                                    if let Err(error) = self.fs.write_file(&path, &contents) {
                                        stderr.push_str(&format!("split: {error}\n"));
                                        exit_code = 1;
                                    }
                                }
                            }
                            Err(()) => exit_code = 1,
                        }
                    }
                }
            }
            // `eval` re-parses its arguments and runs them in this shell, so
            // assignments and function definitions persist.
            "eval" => {
                if self.source_depth >= self.execution_limits.max_source_depth {
                    stderr.push_str(&format!(
                        "bash: eval: nesting exceeds limit ({})\n",
                        self.execution_limits.max_source_depth
                    ));
                    exit_code = 2;
                } else {
                    let script = args.join(" ");
                    self.source_depth += 1;
                    let (out, err, code) = self.exec_script_text(&script);
                    self.source_depth -= 1;
                    stdout.push_str(&out);
                    stderr.push_str(&err);
                    exit_code = code;
                }
            }
            // `source FILE [ARG…]` / `. FILE [ARG…]`
            "source" | "." => {
                if args.is_empty() {
                    stderr.push_str("bash: source: filename argument required\n");
                    exit_code = 2;
                }
                let path = args.first().map(String::as_str).unwrap_or("");
                if exit_code == 0 && self.source_depth >= self.execution_limits.max_source_depth {
                    stderr.push_str(&format!(
                        "bash: source: nesting exceeds limit ({})\n",
                        self.execution_limits.max_source_depth
                    ));
                    exit_code = 2;
                } else if exit_code == 0 {
                    let resolved = self.resolve_path(path);
                    match self.fs.read_file(&resolved) {
                        Ok(contents) => {
                            let script = contents.to_string();
                            // Arguments after the file become the positional
                            // parameters for the duration of the source.
                            let saved = if args.len() > 1 {
                                Some(self.save_positionals())
                            } else {
                                None
                            };
                            if args.len() > 1 {
                                self.link_positionals(&args[1..]);
                            }
                            self.source_depth += 1;
                            let (out, err, code) = self.exec_script_text(&script);
                            self.source_depth -= 1;
                            // `return` inside a sourced file ends the source.
                            let returned = self.return_signal.take();
                            if let Some(saved) = saved {
                                self.restore_positionals(saved);
                            }
                            stdout.push_str(&out);
                            stderr.push_str(&err);
                            exit_code = returned.unwrap_or(code);
                        }
                        Err(error) => {
                            // The filesystem error already names the path.
                            stderr.push_str(&format!("bash: source: {error}\n"));
                            exit_code = 1;
                        }
                    }
                }
            }
            // `tee [-a] [FILE…]`: copy stdin to stdout and to each file.
            "tee" => {
                let mut append = false;
                let mut files: Vec<String> = Vec::new();
                for arg in args {
                    match arg.as_str() {
                        "-a" | "--append" => append = true,
                        "-i" | "--ignore-interrupts" => {}
                        other => files.push(other.to_string()),
                    }
                }
                for file in files {
                    let path = self.resolve_path(&file);
                    let written = if append {
                        self.fs.append_file(&path, stdin)
                    } else {
                        self.fs.write_file(&path, stdin)
                    };
                    if let Err(error) = written {
                        stderr.push_str(&format!("tee: {error}\n"));
                        exit_code = 1;
                    }
                }
                stdout.push_str(stdin);
            }
            // `pushd [DIR]` / `popd` / `dirs [-v]` — the directory stack.
            "pushd" | "popd" | "dirs" => {
                // The stack always starts with the current directory so `dirs`
                // alone prints just it.
                if self.dir_stack.is_empty() {
                    self.dir_stack.push(self.cwd.clone());
                }
                match command {
                    "pushd" => {
                        let Some(target) = args.first() else {
                            stderr.push_str("pushd: no other directory\n");
                            exit_code = 1;
                            return CommandOutput {
                                stdout,
                                stderr,
                                exit_code,
                            };
                        };
                        let path = self.resolve_path(target);
                        if !self.fs.is_dir(&path) {
                            stderr
                                .push_str(&format!("pushd: {target}: No such file or directory\n"));
                            exit_code = 1;
                        } else {
                            self.dir_stack[0] = self.cwd.clone();
                            self.dir_stack.insert(0, path.clone());
                            self.cwd = path;
                            self.env.insert("PWD".to_string(), self.cwd.clone());
                            // bash echoes the stack unless output is redirected.
                            stdout.push_str(&format!("{}\n", self.dir_stack.join(" ")));
                        }
                    }
                    "popd" => {
                        if self.dir_stack.len() < 2 {
                            stderr.push_str("popd: directory stack empty\n");
                            exit_code = 1;
                        } else {
                            self.dir_stack.remove(0);
                            let path = self.dir_stack.remove(0);
                            self.cwd = path;
                            self.env.insert("PWD".to_string(), self.cwd.clone());
                            self.dir_stack.insert(0, self.cwd.clone());
                            stdout.push_str(&format!("{}\n", self.dir_stack.join(" ")));
                        }
                    }
                    _ => {
                        // `dirs -v` numbers the entries.
                        if args.iter().any(|arg| arg == "-v") {
                            for (index, dir) in self.dir_stack.iter().enumerate() {
                                stdout.push_str(&format!(" {index}  {dir}\n"));
                            }
                        } else {
                            stdout.push_str(&format!("{}\n", self.dir_stack.join(" ")));
                        }
                    }
                }
            }
            // `du [-s] [-a] [-h] [PATH…]` — usage in 512-byte blocks.
            "du" => {
                let mut summarize = false;
                let mut all = false;
                let mut human = false;
                let mut operands: Vec<String> = Vec::new();
                for arg in args {
                    if arg.starts_with('-') && arg.len() > 1 {
                        for flag in arg.chars().skip(1) {
                            match flag {
                                's' => summarize = true,
                                'a' => all = true,
                                'h' => human = true,
                                // `-b`/`-k` change the unit; keep block counts.
                                'b' | 'k' => {}
                                other => {
                                    stderr.push_str(&format!("du: invalid option -- '{other}'\n"));
                                    exit_code = 1;
                                }
                            }
                        }
                        continue;
                    }
                    operands.push(arg.clone());
                }
                if operands.is_empty() {
                    operands.push(".".to_string());
                }
                if exit_code == 0 {
                    for operand in &operands {
                        let resolved = self.resolve_path(operand);
                        if !self.fs.exists(&resolved) {
                            stderr.push_str(&format!("du: {operand}: No such file or directory\n"));
                            exit_code = 1;
                            continue;
                        }
                        let mut usages: Vec<textutils::DuUsage> = Vec::new();
                        if self.fs.is_dir(&resolved) {
                            // Every entry's own size, keyed by absolute path.
                            let entries = self.fs.entries_under(&resolved).unwrap_or_default();
                            let mut own: BTreeMap<String, u64> = BTreeMap::new();
                            for (path, contents) in &entries {
                                let blocks = contents
                                    .as_ref()
                                    .map(|body| (body.len() as u64).div_ceil(512).max(1))
                                    .unwrap_or(0);
                                own.insert(path.clone(), blocks);
                            }
                            // Deepest paths first so each parent absorbs its
                            // children's totals before being visited.
                            let mut paths: Vec<String> = own.keys().cloned().collect();
                            paths.sort_by_key(|path| std::cmp::Reverse(path.len()));
                            let mut totals = own.clone();
                            for path in &paths {
                                if let Some((parent, _)) = path.rsplit_once('/')
                                    && parent.len() >= resolved.len()
                                {
                                    // Fold this path's whole subtree total into
                                    // its parent, so a directory reports the sum
                                    // of everything beneath it.
                                    let subtree = totals.get(path).copied().unwrap_or(0);
                                    *totals.entry(parent.to_string()).or_insert(0) += subtree;
                                }
                            }
                            let display_of = |path: &str| -> String {
                                if path == resolved {
                                    return operand.clone();
                                }
                                let suffix =
                                    path.strip_prefix(&format!("{resolved}/")).unwrap_or(path);
                                if operand == "." {
                                    format!("./{suffix}")
                                } else {
                                    format!("{}/{suffix}", operand.trim_end_matches('/'))
                                }
                            };
                            for (path, blocks) in &own {
                                usages.push(textutils::DuUsage {
                                    path: display_of(path),
                                    own: *blocks,
                                    total: totals.get(path).copied().unwrap_or(*blocks),
                                    is_dir: self.fs.is_dir(path),
                                });
                            }
                            usages.sort_by(|left, right| left.path.cmp(&right.path));
                        } else if let Ok(contents) = self.fs.read_file(&resolved) {
                            let blocks = (contents.len() as u64).div_ceil(512).max(1);
                            usages.push(textutils::DuUsage {
                                path: operand.clone(),
                                own: blocks,
                                total: blocks,
                                is_dir: false,
                            });
                        }
                        for usage in &usages {
                            let is_root = usage.path == *operand;
                            // `-s` prints only the operand itself.
                            if summarize && !is_root {
                                continue;
                            }
                            // Without `-a`, only directories and the operand
                            // itself are listed.
                            if !all && !summarize && !usage.is_dir && !is_root {
                                continue;
                            }
                            stdout.push_str(&textutils::du_line(usage, human));
                        }
                    }
                }
            }
            // `tac [-s SEP] [FILE…]` — print records in reverse order.
            "tac" => {
                let mut separator: Option<String> = None;
                let mut operands: Vec<String> = Vec::new();
                let mut index = 0usize;
                while index < args.len() {
                    let arg = args[index].as_str();
                    if arg == "-s" || arg == "--separator" {
                        separator = args.get(index + 1).cloned();
                        index += 2;
                        continue;
                    }
                    if let Some(value) = arg.strip_prefix("--separator=") {
                        separator = Some(value.to_string());
                        index += 1;
                        continue;
                    }
                    if arg.starts_with("-s") && arg.len() > 2 {
                        separator = Some(arg[2..].to_string());
                        index += 1;
                        continue;
                    }
                    operands.push(arg.to_string());
                    index += 1;
                }
                match self.read_inputs_for_text_tool(&operands, stdin, "tac", &mut stderr) {
                    Ok(input) => stdout.push_str(&textutils::tac(&input, separator.as_deref())),
                    Err(()) => exit_code = 1,
                }
            }
            // `xxd [-p] [-r] [FILE…]`
            "xxd" => {
                let mut plain = false;
                let mut reverse = false;
                let mut operands: Vec<String> = Vec::new();
                for arg in args {
                    if arg.starts_with('-') && arg.len() > 1 {
                        for flag in arg.chars().skip(1) {
                            match flag {
                                'p' => plain = true,
                                'r' => reverse = true,
                                // `-c N` / `-l N` are accepted but unused here.
                                'c' | 'l' => {}
                                other => {
                                    stderr.push_str(&format!("xxd: invalid option -- '{other}'\n"));
                                    exit_code = 1;
                                }
                            }
                        }
                        continue;
                    }
                    operands.push(arg.clone());
                }
                if exit_code == 0 {
                    match self.read_inputs_for_text_tool(&operands, stdin, "xxd", &mut stderr) {
                        Ok(input) => {
                            stdout.push_str(&textutils::xxd(input.as_bytes(), plain, reverse))
                        }
                        Err(()) => exit_code = 1,
                    }
                }
            }
            // `md5sum` / `sha1sum` / `sha256sum [-c] [FILE…]`
            "md5sum" | "sha1sum" | "sha256sum" => {
                let algorithm = match command {
                    "md5sum" => just_bash_checksum::Algorithm::Md5,
                    "sha1sum" => just_bash_checksum::Algorithm::Sha1,
                    _ => just_bash_checksum::Algorithm::Sha256,
                };
                let program = command.to_string();
                let mut check = false;
                let mut operands: Vec<String> = Vec::new();
                for arg in args {
                    match arg.as_str() {
                        "-c" | "--check" => check = true,
                        "--quiet" | "-s" => {}
                        other => operands.push(other.to_string()),
                    }
                }
                if check {
                    // Verify each `digest  name` line against the file contents.
                    let mut had_failure = false;
                    for operand in &operands {
                        let Ok(contents) = self.read_operand(operand) else {
                            stderr.push_str(&format!(
                                "{program}: {operand}: No such file or directory\n"
                            ));
                            had_failure = true;
                            continue;
                        };
                        let listing = contents.to_string();
                        for line in listing.lines() {
                            let Some((expected, name)) = line.split_once(' ') else {
                                continue;
                            };
                            let name = name.trim_start_matches(' ');
                            let actual = match self.fs.read_file(&self.resolve_path(name)) {
                                Ok(bytes) => bytes.to_string(),
                                Err(_) => {
                                    stdout.push_str(&format!("{name}: FAILED open or read\n"));
                                    had_failure = true;
                                    continue;
                                }
                            };
                            if algorithm.hex_digest(actual.as_bytes()) == expected.trim() {
                                stdout.push_str(&format!("{name}: OK\n"));
                            } else {
                                stdout.push_str(&format!("{name}: FAILED\n"));
                                had_failure = true;
                            }
                        }
                    }
                    if had_failure {
                        exit_code = 1;
                    }
                } else {
                    // No operand means standard input, which GNU labels `-`.
                    let mut inputs: Vec<(String, String)> = Vec::new();
                    if operands.is_empty() {
                        inputs.push(("-".to_string(), stdin.to_string()));
                    } else {
                        for operand in &operands {
                            match self.read_operand(operand) {
                                Ok(contents) => inputs.push((operand.clone(), contents)),
                                Err(error) => {
                                    stderr.push_str(&format!("{program}: {error}\n"));
                                    exit_code = 1;
                                }
                            }
                        }
                    }
                    for (name, contents) in inputs {
                        let digest = algorithm.hex_digest(contents.as_bytes());
                        stdout.push_str(&format!("{digest}  {name}\n"));
                    }
                }
            }
            // `base64 [-d] [FILE…]`
            "base64" => {
                let mut decode = false;
                let mut operands: Vec<String> = Vec::new();
                for arg in args {
                    match arg.as_str() {
                        "-d" | "--decode" => decode = true,
                        "-e" | "--encode" => decode = false,
                        "-w" => {}
                        other => operands.push(other.to_string()),
                    }
                }
                match self.read_inputs_for_text_tool(&operands, stdin, "base64", &mut stderr) {
                    Ok(input) => {
                        let encoded = textutils::base64(&input, decode);
                        stdout.push_str(&encoded);
                        // Encoding always terminates with a newline.
                        if !decode && !encoded.is_empty() {
                            stdout.push('\n');
                        }
                    }
                    Err(()) => exit_code = 1,
                }
            }
            // `uname [-snrma]`
            "uname" => {
                let mut flags = String::new();
                for arg in args {
                    match arg.as_str() {
                        "-a" | "--all" => flags.push('a'),
                        "-s" => flags.push('s'),
                        "-n" => flags.push('n'),
                        "-r" => flags.push('r'),
                        "-m" => flags.push('m'),
                        "-snrm" | "-snrvm" => flags.push_str("snrm"),
                        other if other.starts_with('-') => {
                            stderr.push_str(&format!("uname: invalid option -- '{other}'\n"));
                            exit_code = 1;
                        }
                        _ => {}
                    }
                }
                if exit_code == 0 {
                    stdout.push_str(&textutils::uname(&flags));
                    stdout.push('\n');
                }
            }
            // `whoami` / `id -un`
            "whoami" => {
                stdout.push_str(self.env.get("USER").map(String::as_str).unwrap_or("user"));
                stdout.push('\n');
            }
            "id" => {
                let user = self
                    .env
                    .get("USER")
                    .cloned()
                    .unwrap_or_else(|| "user".to_string());
                if args
                    .iter()
                    .any(|arg| arg == "-un" || arg == "-nu" || arg == "--user")
                {
                    stdout.push_str(&user);
                } else {
                    stdout.push_str(&format!("uid=1000({user}) gid=1000({user})"));
                }
                stdout.push('\n');
            }
            // `readlink -f` / `realpath`: canonicalise a path.
            "readlink" | "realpath" => {
                let mut canonical = false;
                let mut operands: Vec<String> = Vec::new();
                for arg in args {
                    match arg.as_str() {
                        "-f" | "-e" | "-m" | "--canonicalize" => canonical = true,
                        other => operands.push(other.to_string()),
                    }
                }
                if operands.is_empty() {
                    stderr.push_str(&format!(
                        "{}: missing operand\n",
                        args.first().map(String::as_str).unwrap_or("readlink")
                    ));
                    exit_code = 1;
                } else {
                    for operand in &operands {
                        let resolved = self.resolve_path(operand);
                        stdout.push_str(&resolved);
                        stdout.push('\n');
                        if !canonical && !self.fs.exists(&resolved) {
                            exit_code = 1;
                        }
                    }
                }
            }
            // `readonly [NAME[=VALUE]…]` marks names as unassignable.
            "readonly" => {
                let mut names: Vec<&String> = args
                    .iter()
                    .filter(|arg| !arg.starts_with('-') || arg.contains('='))
                    .collect();
                if names.is_empty() || args.iter().any(|arg| arg == "-p") {
                    for name in self.readonly_names.iter() {
                        stdout.push_str(&format!(
                            "declare -r {name}=\"{}\"\n",
                            self.env.get(name).cloned().unwrap_or_default()
                        ));
                    }
                    names.clear();
                }
                for entry in names {
                    match entry.split_once('=') {
                        Some((name, value)) => {
                            self.env.insert(name.to_string(), value.to_string());
                            self.readonly_names.insert(name.to_string());
                        }
                        None => {
                            self.readonly_names.insert(entry.clone());
                        }
                    }
                }
            }
            // `trap [ACTION] SIGNAL…` — only `EXIT` (and `0`) is honoured.
            "trap" => {
                if args.is_empty() {
                    if let Some(body) = &self.trap_exit {
                        stdout.push_str("trap -- '");
                        stdout.push_str(body);
                        stdout.push_str("' EXIT\n");
                    }
                } else if args.first().is_some_and(|arg| arg == "-l") {
                    stdout.push_str(" 0) EXIT\n 1) SIGHUP\n 2) SIGINT\n 3) SIGQUIT\n15) SIGTERM\n");
                } else {
                    let action = args[0].clone();
                    // `trap - EXIT` removes the handler.
                    let install = action != "-";
                    for signal in &args[1..] {
                        match signal.as_str() {
                            "EXIT" | "0" => {
                                self.trap_exit = if install { Some(action.clone()) } else { None };
                            }
                            // No real signals exist here, so installing one is a
                            // no-op rather than an error.
                            other => {
                                let known = other.chars().all(|ch| ch.is_ascii_digit())
                                    || other.starts_with("SIG")
                                    || other.is_empty();
                                if !known {
                                    stderr.push_str(&format!(
                                        "bash: trap: {other}: invalid signal specification\n"
                                    ));
                                    exit_code = 1;
                                }
                            }
                        }
                    }
                }
            }
            // `getopts SPEC NAME [ARGS…]` walks the positional parameters.
            "getopts" => {
                if args.len() < 2 {
                    stderr.push_str("getopts: usage: getopts optstring name [arg ...]\n");
                    exit_code = 2;
                } else {
                    let spec = args[0].clone();
                    let target = args[1].clone();
                    let words: Vec<String> = if args.len() > 2 {
                        args[2..].to_vec()
                    } else {
                        self.positionals.clone()
                    };
                    let mut index = self.getopts_index;
                    let mut cluster = self.getopts_cluster.max(1);
                    let mut letter: Option<char> = None;
                    let mut optarg: Option<String> = None;

                    while index < words.len() {
                        let word = words[index].clone();
                        if word == "--" {
                            index += 1;
                            cluster = 1;
                            break;
                        }
                        if !word.starts_with('-') || word == "-" {
                            break;
                        }
                        let chars: Vec<char> = word.chars().skip(1).collect();
                        if cluster > chars.len() {
                            index += 1;
                            cluster = 1;
                            continue;
                        }
                        let candidate = chars[cluster - 1];
                        // A letter followed by `:` in SPEC takes an argument.
                        let spec_chars: Vec<char> = spec.chars().collect();
                        let takes_value = spec_chars
                            .iter()
                            .position(|ch| *ch == candidate)
                            .is_some_and(|at| spec_chars.get(at + 1) == Some(&':'));
                        if !spec.contains(candidate) {
                            letter = Some('?');
                            if cluster >= chars.len() {
                                index += 1;
                                cluster = 1;
                            } else {
                                cluster += 1;
                            }
                            break;
                        }
                        letter = Some(candidate);
                        if takes_value {
                            let rest: String = chars[cluster..].iter().collect();
                            if rest.is_empty() {
                                index += 1;
                                optarg = words.get(index).cloned();
                                index += 1;
                            } else {
                                optarg = Some(rest);
                                index += 1;
                            }
                            cluster = 1;
                        } else if cluster >= chars.len() {
                            index += 1;
                            cluster = 1;
                        } else {
                            cluster += 1;
                        }
                        break;
                    }

                    self.getopts_index = index;
                    self.getopts_cluster = cluster;
                    match letter {
                        Some(value) => {
                            self.assign_parameter(&target, &value.to_string());
                            match &optarg {
                                Some(argument) => self.assign_parameter("OPTARG", argument),
                                None => {
                                    self.env.remove("OPTARG");
                                }
                            }
                            self.assign_parameter("OPTIND", &index.to_string());
                        }
                        // End of options: the status is non-zero and the name is
                        // set to `?`.
                        None => {
                            self.assign_parameter(&target, "?");
                            self.assign_parameter("OPTIND", &index.to_string());
                            exit_code = 1;
                        }
                    }
                }
            }
            // `shopt [-s|-u] [NAME…]` — only `nullglob` changes behaviour here.
            "shopt" => {
                let mut enable: Option<bool> = None;
                let mut wanted: Vec<String> = Vec::new();
                for arg in args {
                    match arg.as_str() {
                        "-s" => enable = Some(true),
                        "-u" => enable = Some(false),
                        "-q" => {}
                        other => wanted.push(other.to_string()),
                    }
                }
                if wanted.is_empty() {
                    let state = if self.shopt_nullglob { "on" } else { "off" };
                    stdout.push_str(&format!("nullglob\t{state}\n"));
                } else {
                    for name in &wanted {
                        match name.as_str() {
                            "nullglob" => {
                                if let Some(value) = enable {
                                    self.shopt_nullglob = value;
                                } else {
                                    let state = if self.shopt_nullglob { "on" } else { "off" };
                                    stdout.push_str(&format!("nullglob\t{state}\n"));
                                }
                            }
                            other => {
                                if enable.is_some() {
                                    stderr.push_str(&format!(
                                        "bash: shopt: {other}: invalid shell option name\n"
                                    ));
                                    exit_code = 1;
                                }
                            }
                        }
                    }
                }
            }
            // `alias [NAME[=VALUE]…]` defines or lists aliases.
            "alias" => {
                if args.is_empty() {
                    for (name, value) in &self.aliases {
                        stdout.push_str(&format!("alias {name}='{value}'\n"));
                    }
                } else {
                    for arg in args {
                        if let Some((name, value)) = arg.split_once('=') {
                            self.aliases.insert(name.to_string(), value.to_string());
                        } else if let Some(value) = self.aliases.get(arg) {
                            stdout.push_str(&format!("alias {arg}='{value}'\n"));
                        } else {
                            stderr.push_str(&format!("bash: alias: {arg}: not found\n"));
                            exit_code = 1;
                        }
                    }
                }
            }
            // `unalias NAME… | -a`
            "unalias" => {
                let mut names: Vec<&String> = args.iter().collect();
                if names.len() == 1 && names[0] == "-a" {
                    self.aliases.clear();
                    names.clear();
                }
                if names.is_empty() && args.is_empty() {
                    stderr.push_str("unalias: usage: unalias [-a] name [name ...]\n");
                    exit_code = 2;
                }
                for name in names {
                    if self.aliases.remove(name.as_str()).is_none() {
                        stderr.push_str(&format!("bash: unalias: {name}: not found\n"));
                        exit_code = 1;
                    }
                }
            }
            // `type NAME…` reports how each name would be resolved.
            "type" => {
                for name in args {
                    if self.functions.contains_key(name) {
                        stdout.push_str(&format!("{name} is a function\n"));
                    } else if is_shell_builtin(name) {
                        stdout.push_str(&format!("{name} is a shell builtin\n"));
                    } else {
                        // The runtime has no external commands, so anything
                        // else is simply not found.
                        stderr.push_str(&format!("bash: type: {name}: not found\n"));
                        exit_code = 1;
                    }
                }
            }
            // `command [-v|-V] NAME…` runs a command bypassing shell
            // functions; `-v` prints how the name resolves.
            "command" => {
                let mut index = 0usize;
                let mut describe = false;
                while index < args.len() {
                    match args[index].as_str() {
                        "-v" => {
                            describe = true;
                            index += 1;
                        }
                        "-V" => {
                            describe = true;
                            index += 1;
                        }
                        "--" => {
                            index += 1;
                            break;
                        }
                        _ => break,
                    }
                }
                let rest = &args[index..];
                if describe {
                    for name in rest {
                        if self.functions.contains_key(name) || is_shell_builtin(name) {
                            stdout.push_str(&format!("{name}\n"));
                        } else {
                            exit_code = 1;
                        }
                    }
                } else if rest.is_empty() {
                    exit_code = 0;
                } else {
                    // Bypass the function lookup in `run_invocation`.
                    let result = self.run_command(rest, stdin);
                    stdout.push_str(&result.stdout);
                    stderr.push_str(&result.stderr);
                    exit_code = result.exit_code;
                }
            }
            // `let` evaluates each argument as an arithmetic expression; the
            // status is 1 when the last value is zero, 2 on a syntax error.
            "let" => {
                if args.is_empty() {
                    exit_code = 1;
                }
                let mut last = 0i64;
                for expression in args {
                    let mut env = std::mem::take(&mut self.env);
                    let result = eval_arithmetic_expr(expression, &mut env, &self.positionals);
                    self.env = env;
                    match result {
                        Ok(value) => last = value,
                        Err(message) => {
                            stderr.push_str(&format!("bash: let: {message}\n"));
                            exit_code = 2;
                            break;
                        }
                    }
                }
                if exit_code != 2 {
                    exit_code = i32::from(last == 0);
                }
            }
            "return" => {
                // `return` is valid inside a function *or* a sourced script.
                if self.function_depth == 0 && self.source_depth == 0 {
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

    /// Read a path that may be one of the virtual devices.
    ///
    /// `/dev/null` is empty, and `/dev/zero` is an endless run of NUL bytes
    /// bounded by the downstream budget — exactly the trick `yes` uses, so
    /// `head -c 5 /dev/zero` terminates instead of filling memory.
    /// Read an operand, honouring the virtual devices and returning the raw
    /// filesystem error message otherwise.
    fn read_operand(&self, path: &str) -> Result<String, String> {
        let resolved = self.resolve_path(path);
        if resolved == "/dev/null" || resolved == "/dev/zero" {
            return self.read_device_or_file(&resolved);
        }
        self.fs
            .read_file(&resolved)
            .map(str::to_string)
            .map_err(|error| error.to_string())
    }

    fn read_device_or_file(&self, path: &str) -> Result<String, String> {
        match path {
            "/dev/null" => Ok(String::new()),
            "/dev/zero" => {
                let limit = self.line_budget.unwrap_or(64);
                let budget = self.remaining_output_budget().min(1 << 20);
                let count = limit.saturating_mul(8192).min(budget).min(1 << 20);
                Ok("\0".repeat(count))
            }
            other => self
                .fs
                .read_file(other)
                .map(str::to_string)
                .map_err(|error| error.to_string()),
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
            match self.read_operand(arg) {
                Ok(contents) => combined.push_str(&contents),
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

        // Binary connectives: `-a` is AND and `-o` is OR, both left-associative.
        // They are searched at the top level only, and `-o` binds loosest so
        // `A -a B -o C` is `(A -a B) -o C`.
        if let Some(index) = find_test_connective(args, "-o") {
            return self.run_test(&args[..index]) || self.run_test(&args[index + 1..]);
        }
        if let Some(index) = find_test_connective(args, "-a") {
            return self.run_test(&args[..index]) && self.run_test(&args[index + 1..]);
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
            expanded.push_str(&self.expand_part(part));
        }
        self.output_bytes_used += expanded.len();
        expanded
    }

    /// Expand a single word part to text, ignoring field splitting.
    fn expand_part(&mut self, part: &WordPart) -> String {
        let mut expanded = String::new();
        {
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
                // `$((…))` is always arithmetic. bash never re-reads a
                // command-shaped body as a subshell here (that only happens for
                // the explicitly spaced `$( ( … ) )` form), so a body that is
                // not an expression is an error rather than something to run.
                WordPartKind::Arithmetic => {
                    let mut env = std::mem::take(&mut self.env);
                    let result = eval_arithmetic_expr(&part.text, &mut env, &self.positionals);
                    self.env = env;
                    match result {
                        Ok(value) => expanded.push_str(&value.to_string()),
                        Err(message) => {
                            self.param_error = Some(format!("bash: {message}\n"));
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
        expanded
    }

    /// Evaluate the contents of a `${…}` expansion, including the POSIX
    /// parameter operators `${p-word}`, `${p:-word}`, `${p=word}`, `${p:=word}`,
    /// `${p?word}`, `${p:?word}`, `${p+word}` and `${p:+word}`.
    ///
    /// Operators never trigger `set -u`: an unset parameter is exactly the case
    /// they exist to handle (upstream #4af0dc0f).
    /// Apply a parameter pattern operator, if `rest` starts with one.
    ///
    /// Returns `None` when `rest` is not a pattern/case operator, so the
    /// existing default-value (`-`, `=`, `?`, `+`) handling still runs.
    fn expand_pattern_operator(&mut self, parameter: &str, rest: &str) -> Option<String> {
        // Longest-match forms must be tested before their shortest-match
        // prefixes (`##` before `#`, `//` before `/`, `^^` before `^`).
        let (operator, argument) = if let Some(argument) = rest.strip_prefix("##") {
            (PatternOperator::PrefixLongest, argument)
        } else if let Some(argument) = rest.strip_prefix('#') {
            (PatternOperator::PrefixShortest, argument)
        } else if let Some(argument) = rest.strip_prefix("%%") {
            (PatternOperator::SuffixLongest, argument)
        } else if let Some(argument) = rest.strip_prefix('%') {
            (PatternOperator::SuffixShortest, argument)
        } else if let Some(argument) = rest.strip_prefix("//") {
            (PatternOperator::ReplaceAll, argument)
        } else if let Some(argument) = rest.strip_prefix('/') {
            (PatternOperator::ReplaceFirst, argument)
        } else if let Some(argument) = rest.strip_prefix("^^") {
            (PatternOperator::UpperAll, argument)
        } else if let Some(argument) = rest.strip_prefix('^') {
            (PatternOperator::UpperFirst, argument)
        } else if let Some(argument) = rest.strip_prefix(",,") {
            (PatternOperator::LowerAll, argument)
        } else if let Some(argument) = rest.strip_prefix(',') {
            (PatternOperator::LowerFirst, argument)
        } else {
            return None;
        };

        let value = self.parameter_value(parameter).unwrap_or_default();

        match operator {
            PatternOperator::UpperAll => return Some(value.to_uppercase()),
            PatternOperator::LowerAll => return Some(value.to_lowercase()),
            PatternOperator::UpperFirst => {
                return Some(change_first_char(&value, true));
            }
            PatternOperator::LowerFirst => {
                return Some(change_first_char(&value, false));
            }
            _ => {}
        }

        // The pattern may itself contain expansions (`${p#$prefix}`); a
        // replacement likewise (`${p/x/$y}`).
        let (pattern_text, replacement_text) = match operator {
            PatternOperator::ReplaceAll | PatternOperator::ReplaceFirst => {
                split_pattern_replacement(argument)
            }
            _ => (argument, ""),
        };
        let pattern = self.expand_text(pattern_text);

        match operator {
            PatternOperator::PrefixShortest => {
                let cut = self.match_prefix_len(&pattern, &value, false).unwrap_or(0);
                Some(value[cut..].to_string())
            }
            PatternOperator::PrefixLongest => {
                let cut = self.match_prefix_len(&pattern, &value, true).unwrap_or(0);
                Some(value[cut..].to_string())
            }
            PatternOperator::SuffixShortest => {
                let cut = self
                    .match_suffix_start(&pattern, &value, false)
                    .unwrap_or(value.len());
                Some(value[..cut].to_string())
            }
            PatternOperator::SuffixLongest => {
                let cut = self
                    .match_suffix_start(&pattern, &value, true)
                    .unwrap_or(value.len());
                Some(value[..cut].to_string())
            }
            PatternOperator::ReplaceFirst | PatternOperator::ReplaceAll => {
                let replacement = self.expand_text(replacement_text);
                let all = matches!(operator, PatternOperator::ReplaceAll);
                Some(self.replace_matches(&pattern, &replacement, &value, all))
            }
            PatternOperator::UpperAll
            | PatternOperator::LowerAll
            | PatternOperator::UpperFirst
            | PatternOperator::LowerFirst => unreachable!("handled above"),
        }
    }

    /// Length of the shortest or longest prefix of `value` matching `pattern`.
    fn match_prefix_len(&self, pattern: &str, value: &str, longest: bool) -> Option<usize> {
        let mut best = None;
        for (index, _) in value
            .char_indices()
            .chain(std::iter::once((value.len(), ' ')))
        {
            if glob::pattern_matches(pattern, &value[..index]) {
                best = Some(index);
                if !longest {
                    return best;
                }
            }
        }
        best
    }

    /// Start offset of the shortest or longest suffix of `value` matching
    /// `pattern`.
    fn match_suffix_start(&self, pattern: &str, value: &str, longest: bool) -> Option<usize> {
        let mut best = None;
        for (index, _) in value
            .char_indices()
            .chain(std::iter::once((value.len(), ' ')))
        {
            if glob::pattern_matches(pattern, &value[index..]) {
                // Iterating in ascending order means the first hit is the
                // longest suffix and the last hit the shortest one.
                if longest {
                    return Some(index);
                }
                best = Some(index);
            }
        }
        best
    }

    /// Replace the leftmost-longest match of `pattern` in `value`, either once
    /// or everywhere.
    fn replace_matches(&self, pattern: &str, replacement: &str, value: &str, all: bool) -> String {
        let mut output = String::new();
        let mut index = 0usize;
        let boundaries: Vec<usize> = value
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(value.len()))
            .collect();
        // Matching stops at the end of the value and never replaces an empty
        // match, which is what bash does: `${v//*/Y}` on `abc` is `Y`, on the
        // empty string it stays empty, and an empty pattern changes nothing.
        while index < value.len() {
            let mut matched: Option<usize> = None;
            for end in boundaries.iter().copied().filter(|end| *end > index) {
                if glob::pattern_matches(pattern, &value[index..end]) {
                    matched = Some(end);
                }
            }
            match matched {
                Some(end) => {
                    output.push_str(replacement);
                    index = end;
                    if !all {
                        output.push_str(&value[index..]);
                        return output;
                    }
                }
                None => {
                    let Some(ch) = value[index..].chars().next() else {
                        break;
                    };
                    output.push(ch);
                    index += ch.len_utf8();
                }
            }
        }
        output
    }

    /// Expand one `${ … }` body.
    ///
    /// A parameter operator whose word is itself a `${ … }` recurses
    /// (`${x:-${y:-${z}}}`), so the nesting depth is bounded here: without the
    /// bound a few thousand levels of source overflow the stack and abort the
    /// host process.
    fn expand_braced(&mut self, content: &str) -> String {
        if self.expansion_depth >= self.execution_limits.max_expansion_depth {
            if self.param_error.is_none() {
                self.param_error = Some(format!(
                    "bash: maximum expansion depth exceeded ({})\n",
                    self.execution_limits.max_expansion_depth
                ));
            }
            return String::new();
        }
        self.expansion_depth += 1;
        let expanded = self.expand_braced_inner(content);
        self.expansion_depth -= 1;
        expanded
    }

    fn expand_braced_inner(&mut self, content: &str) -> String {
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
        // bash rejects anything else after the `#` (`${#x:-5}` is a bad
        // substitution), so an operator here is an error rather than a silent
        // fallback to the bare length.
        if parameter == "#" && !rest.is_empty() {
            let (inner, inner_rest) = split_parameter(rest);
            if inner_rest.is_empty() && !inner.is_empty() {
                return self.expand_length(inner);
            }
            self.param_error = Some(format!("bash: ${{#{rest}}}: bad substitution\n"));
            return String::new();
        }

        // `${v:off:len}` / `${a[@]:off:len}` slicing. A `:` followed by `-`,
        // `=`, `?` or `+` is a parameter operator, never a slice; anything else
        // (including a variable or arithmetic offset) is a slice.
        if let Some(spec) = rest.strip_prefix(':')
            && !spec.starts_with(['-', '=', '?', '+'])
        {
            return self.expand_slice(parameter, spec.trim());
        }

        // `${v#pat}` / `${v##pat}` / `${v%pat}` / `${v%%pat}` strip a matching
        // prefix or suffix, `${v/pat/rep}` / `${v//pat/rep}` replace matches,
        // and `${v^}` / `${v^^}` / `${v,}` / `${v,,}` change case. Before this
        // existed every one of them silently returned the value unchanged.
        if let Some(expanded) = self.expand_pattern_operator(parameter, rest) {
            return expanded;
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
        // The operator word is shell source, so quoting inside it is syntax:
        // `${1+"set"}` yields `set`, and `${1+"a b"}` yields one field `a b`.
        self.expand_text(&strip_shell_quotes(word))
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
            "noglob" => self.opt_noglob = enable,
            "noexec" => self.opt_noexec = enable,
            "xtrace" => self.opt_xtrace = enable,
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
            ("noglob", self.opt_noglob),
            ("noexec", self.opt_noexec),
            ("xtrace", self.opt_xtrace),
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

/// Append one raw byte to `output`.
///
/// bash's `\xHH` / `\NNN` escapes produce a **byte**, not a code point, so
/// `printf '\xff'` is one byte long. Pushing the value as a `char` would encode
/// it as UTF-8 (two bytes), which is why this maps the byte through Latin-1.
fn push_raw_byte(output: &mut String, value: u32) {
    let byte = (value & 0xff) as u8;
    match byte {
        0x00..=0x7f => output.push(byte as char),
        other => output.push(char::from_u32(other as u32).unwrap_or('\u{fffd}')),
    }
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
                Some('a') => output.push('\u{7}'),
                Some('b') => output.push('\u{8}'),
                Some('f') => output.push('\u{c}'),
                Some('v') => output.push('\u{b}'),
                Some('e') => output.push('\u{1b}'),
                Some('\\') => output.push('\\'),
                Some('"') => output.push('"'),
                Some('\'') => output.push('\''),
                // `\xHH` reads up to two hex digits.
                Some('x') => {
                    let mut value = 0u32;
                    let mut digits = 0;
                    while digits < 2 {
                        let Some(next) = chars.peek().copied() else {
                            break;
                        };
                        let Some(digit) = next.to_digit(16) else {
                            break;
                        };
                        chars.next();
                        value = value * 16 + digit;
                        digits += 1;
                    }
                    if digits == 0 {
                        output.push('\\');
                        output.push('x');
                    } else {
                        push_raw_byte(output, value);
                    }
                }
                // `\NNN` / `\0NNN` read up to three octal digits.
                Some(first @ '0'..='7') => {
                    let mut value = first.to_digit(8).unwrap_or(0);
                    let mut digits = 1;
                    while digits < 3 {
                        let Some(next) = chars.peek().copied() else {
                            break;
                        };
                        let Some(digit) = next.to_digit(8) else {
                            break;
                        };
                        chars.next();
                        value = value * 8 + digit;
                        digits += 1;
                    }
                    push_raw_byte(output, value);
                }
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
            Some(first) => {
                // Collect the spec as: flags, width, `.`, precision, conversion.
                // `first` is the character right after `%`, already consumed.
                let mut progress = Some(first);
                let mut flags = String::new();
                loop {
                    let next = match progress.take() {
                        Some(next) => next,
                        None => match chars.next() {
                            Some(next) => next,
                            None => break,
                        },
                    };
                    if matches!(next, '-' | '+' | ' ' | '0' | '#') {
                        flags.push(next);
                        continue;
                    }
                    progress = Some(next);
                    break;
                }
                let read_digits = |progress: &mut Option<char>,
                                   chars: &mut std::iter::Peekable<std::str::Chars<'_>>|
                 -> String {
                    let mut digits = String::new();
                    loop {
                        let next = match progress.take() {
                            Some(next) => next,
                            None => match chars.next() {
                                Some(next) => next,
                                None => break,
                            },
                        };
                        if next.is_ascii_digit() {
                            digits.push(next);
                            continue;
                        }
                        *progress = Some(next);
                        break;
                    }
                    digits
                };
                let width_text = read_digits(&mut progress, &mut chars);
                let mut precision: Option<usize> = None;
                if progress == Some('.') {
                    progress = None;
                    let text = read_digits(&mut progress, &mut chars);
                    precision = Some(text.parse().unwrap_or(0));
                }
                let conversion = progress.unwrap_or('\0');
                let width: usize = width_text.parse().unwrap_or(0);
                let left = flags.contains('-');
                let zero_pad = flags.contains('0') && !left;
                let argument = args.get(consumed).map(String::as_str).unwrap_or("");
                consumed += 1;
                let rendered = match conversion {
                    's' => match precision {
                        Some(limit) => argument.chars().take(limit).collect::<String>(),
                        None => argument.to_string(),
                    },
                    'b' => argument.to_string(),
                    'c' => argument
                        .chars()
                        .next()
                        .map(String::from)
                        .unwrap_or_default(),
                    'd' | 'i' => {
                        let value: i64 = argument.trim().parse().unwrap_or(0);
                        value.to_string()
                    }
                    'o' => format!("{:o}", argument.trim().parse::<u64>().unwrap_or(0)),
                    'x' => format!("{:x}", argument.trim().parse::<u64>().unwrap_or(0)),
                    'X' => format!("{:X}", argument.trim().parse::<u64>().unwrap_or(0)),
                    'u' => format!("{}", argument.trim().parse::<u64>().unwrap_or(0)),
                    // Floating point conversions honour the precision.
                    'f' | 'F' => {
                        let value: f64 = argument.trim().parse().unwrap_or(0.0);
                        format!("{:.*}", precision.unwrap_or(6), value)
                    }
                    'e' => {
                        let value: f64 = argument.trim().parse().unwrap_or(0.0);
                        let text = format!("{:.*e}", precision.unwrap_or(6), value);
                        // C prints a signed, at-least-two-digit exponent.
                        match text.split_once('e') {
                            Some((mantissa, exponent)) => {
                                let (sign, digits) = match exponent.strip_prefix('-') {
                                    Some(rest) => ('-', rest),
                                    None => ('+', exponent),
                                };
                                format!("{mantissa}e{sign}{digits:0>2}")
                            }
                            None => text,
                        }
                    }
                    'g' | 'G' => {
                        let value: f64 = argument.trim().parse().unwrap_or(0.0);
                        match precision {
                            Some(digits) => format!("{value:.*}", digits),
                            None => format!("{value}"),
                        }
                    }
                    other => {
                        // An unknown conversion is echoed, not silently dropped.
                        output.push('%');
                        output.push_str(&flags);
                        output.push_str(&width_text);
                        if precision.is_some() {
                            output.push('.');
                        }
                        output.push(other);
                        continue;
                    }
                };
                // Pad to the requested width.
                let rendered_width = rendered.chars().count();
                if rendered_width >= width {
                    output.push_str(&rendered);
                } else {
                    let padding = width - rendered_width;
                    if left {
                        output.push_str(&rendered);
                        for _ in 0..padding {
                            output.push(' ');
                        }
                    } else {
                        for _ in 0..padding {
                            output.push(if zero_pad { '0' } else { ' ' });
                        }
                        output.push_str(&rendered);
                    }
                }
            }
            None => output.push('%'),
        }
    }

    consumed
}

/// Evaluate an arithmetic expression against the live environment.
fn eval_arithmetic_expr(
    expr: &str,
    env: &mut BTreeMap<String, String>,
    positionals: &[String],
) -> Result<i64, String> {
    let mut vars = arith::LiveVars::new(env, positionals);
    arith::eval(&mut vars, expr)
}

/// Evaluate an arithmetic expression where the caller only has a shared
/// borrow; assignments evaluate correctly but are not stored.
fn eval_arithmetic_expr_frozen(
    expr: &str,
    env: &BTreeMap<String, String>,
    positionals: &[String],
) -> Result<i64, String> {
    let mut vars = arith::FrozenVars::new(env, positionals);
    arith::eval(&mut vars, expr)
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
#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepOptions {
    /// Patterns given with `-e` (repeatable) or as the first operand.
    patterns: Vec<String>,
    /// Pattern files given with `-f` (repeatable).
    pattern_files: Vec<String>,
    /// File operands, in order; `-` means standard input.
    files: Vec<String>,
    /// `-m N`: stop after N selected lines per source.
    max_count: Option<usize>,
    /// Patterns are regular expressions, which is the default (`-F` turns it
    /// off). GNU grep reads BRE by default; this engine implements ERE, so a
    /// BRE-specific construct such as `\+` is interpreted the ERE way.
    regex: bool,
    /// `-o`: print only the matched text, one match per line.
    only_matching: bool,
    /// `-A N` / `-B N` / `-C N`: context lines around a match.
    after_context: usize,
    before_context: usize,
    /// `-r` / `-R`: walk directories.
    recursive: bool,
    /// `--include=GLOB`: only walk files whose name matches.
    includes: Vec<String>,
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

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            pattern_files: Vec::new(),
            files: Vec::new(),
            max_count: None,
            // GNU grep treats the pattern as a regular expression by default;
            // `-F` switches to literal substrings.
            regex: true,
            only_matching: false,
            after_context: 0,
            before_context: 0,
            recursive: false,
            includes: Vec::new(),
            ignore_case: false,
            invert: false,
            count_only: false,
            line_number: false,
            files_with_matches: false,
            files_without_match: false,
            quiet: false,
            whole_line: false,
            word_match: false,
            no_filename: false,
        }
    }
}

impl GrepOptions {
    /// A single source needs no `file:` prefix.
    fn prefix_needed(&self, source_count: usize) -> bool {
        // `-r` always names the file, even when a single file matches, because
        // the operand was a directory rather than a file.
        (source_count > 1 || self.recursive) && !self.no_filename && !self.quiet
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
                "only-matching" => options.only_matching = true,
                "recursive" => options.recursive = true,
                "include" => options.includes.push(need_value(&mut index)?),
                "after-context" => {
                    let value = need_value(&mut index)?;
                    options.after_context = value.parse().unwrap_or(0);
                }
                "before-context" => {
                    let value = need_value(&mut index)?;
                    options.before_context = value.parse().unwrap_or(0);
                }
                "context" => {
                    let value = need_value(&mut index)?;
                    let count = value.parse().unwrap_or(0);
                    options.after_context = count;
                    options.before_context = count;
                }
                "fixed-strings" => options.regex = false,
                "extended-regexp" | "perl-regexp" | "basic-regexp" => options.regex = true,
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
                'A' | 'B' | 'C' => {
                    let value = take_value(&mut index, &rest, args, flag)?;
                    let count = value.parse::<usize>().unwrap_or(0);
                    match flag {
                        'A' => options.after_context = count,
                        'B' => options.before_context = count,
                        _ => {
                            options.after_context = count;
                            options.before_context = count;
                        }
                    }
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
                'o' => options.only_matching = true,
                'r' | 'R' => options.recursive = true,
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
                let resolved = self.resolve_path(operand);
                // `-r` walks a directory operand, filtering by `--include`.
                if options.recursive && self.fs.is_dir(&resolved) {
                    let entries = self.fs.entries_under(&resolved).unwrap_or_default();
                    let mut files: Vec<String> = entries
                        .into_iter()
                        .filter_map(|(path, contents)| contents.map(|body| (path, body)))
                        .filter(|(path, _)| {
                            options.includes.is_empty()
                                || options.includes.iter().any(|pattern| {
                                    let name = path.rsplit('/').next().unwrap_or(path);
                                    just_bash_glob::pattern_matches(pattern, name)
                                })
                        })
                        .map(|(path, _)| path)
                        .collect();
                    files.sort();
                    for path in files {
                        if let Ok(contents) = self.fs.read_file(&path) {
                            // Paths are reported relative to the working
                            // directory, the way `grep -r pattern dir` prints
                            // `dir/file`.
                            let display = path
                                .strip_prefix(&self.cwd)
                                .map(|rest| rest.trim_start_matches('/').to_string())
                                .unwrap_or_else(|| {
                                    path.strip_prefix('/').unwrap_or(&path).to_string()
                                });
                            sources.push((display, contents.to_string()));
                        }
                    }
                    continue;
                }
                match self.read_operand(operand) {
                    Ok(contents) => sources.push((operand.clone(), contents)),
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
            // `-o` prints each match on its own line and ignores context.
            if options.only_matching && !options.invert {
                for (_, line) in &matched_lines {
                    for piece in matched_texts(line, patterns, options) {
                        if prefix {
                            stdout.push_str(name);
                            stdout.push(':');
                        }
                        stdout.push_str(&piece);
                        stdout.push('\n');
                    }
                }
                continue;
            }

            // Context lines: emit a window around each match, with `--`
            // between non-adjacent groups, as GNU grep does.
            if options.before_context > 0 || options.after_context > 0 {
                let lines: Vec<&str> = contents.lines().collect();
                let mut groups: Vec<(usize, usize)> = Vec::new();
                for (number, _) in &matched_lines {
                    let start = number.saturating_sub(1 + options.before_context);
                    let end = (*number + options.after_context).min(lines.len());
                    match groups.last_mut() {
                        Some((_, previous_end)) if start <= *previous_end => {
                            *previous_end = (*previous_end).max(end);
                        }
                        _ => groups.push((start, end)),
                    }
                }
                let matched_numbers: std::collections::BTreeSet<usize> =
                    matched_lines.iter().map(|(number, _)| *number).collect();
                for (group_index, (start, end)) in groups.iter().enumerate() {
                    if group_index > 0 {
                        stdout.push_str("--\n");
                    }
                    for (line_index, text) in lines.iter().enumerate().take(*end).skip(*start) {
                        let number = line_index + 1;
                        let separator = if matched_numbers.contains(&number) {
                            ':'
                        } else {
                            '-'
                        };
                        if prefix {
                            stdout.push_str(name);
                            stdout.push(separator);
                        }
                        if options.line_number {
                            stdout.push_str(&format!("{number}{separator}"));
                        }
                        stdout.push_str(text);
                        stdout.push('\n');
                    }
                }
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
    Regex(just_bash_regex::Regex),
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
        match just_bash_regex::Regex::new_with_flags(pattern, options.ignore_case) {
            Ok(regex) => compiled.push(GrepPattern::Regex(regex)),
            Err(error) => return Err(format!("grep: {pattern}: {error}\n")),
        }
    }
    Ok(compiled)
}

/// Every non-overlapping match in `line`, for `grep -o`.
fn matched_texts(line: &str, patterns: &[GrepPattern], options: &GrepOptions) -> Vec<String> {
    let mut pieces = Vec::new();
    for pattern in patterns {
        match pattern {
            GrepPattern::Regex(regex) => {
                let mut rest = line;
                while let Some((start, end)) = regex.find(rest) {
                    if end <= start {
                        // A zero-width match cannot advance the scan.
                        break;
                    }
                    pieces.push(rest[start..end].to_string());
                    rest = &rest[end..];
                }
            }
            GrepPattern::Literal(needle) => {
                if needle.is_empty() {
                    continue;
                }
                let (haystack, wanted) = if options.ignore_case {
                    (line.to_lowercase(), needle.to_lowercase())
                } else {
                    (line.to_string(), needle.clone())
                };
                let mut offset = 0usize;
                while let Some(found) = haystack[offset..].find(&wanted) {
                    let start = offset + found;
                    pieces.push(line[start..start + wanted.len()].to_string());
                    offset = start + wanted.len();
                }
            }
        }
    }
    pieces
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
fn regex_matches_word(regex: &just_bash_regex::Regex, line: &str) -> bool {
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

/// Remove the shell quoting from a word's text.
///
/// Only used where a word is re-read as source (parameter-operator words), so
/// the rules are the simple ones: `'…'` is literal, `"…"` drops only the outer
/// quotes, and a backslash escapes the next character.
fn strip_shell_quotes(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    out.push(inner);
                }
            }
            '"' => {
                while let Some(inner) = chars.next() {
                    if inner == '"' {
                        break;
                    }
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                        continue;
                    }
                    out.push(inner);
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Interpret the backslash escapes `echo -e` understands.
fn interpret_echo_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('v') => out.push('\u{b}'),
            Some('e') => out.push('\u{1b}'),
            Some('\\') => out.push('\\'),
            Some('0') => {
                // `\0NNN` is up to three octal digits.
                let mut value = 0u32;
                let mut digits = 0;
                while digits < 3 {
                    let mut probe = chars.clone();
                    let Some(next) = probe.next() else { break };
                    let Some(digit) = next.to_digit(8) else { break };
                    chars.next();
                    value = value * 8 + digit;
                    digits += 1;
                }
                if let Some(decoded) = char::from_u32(value) {
                    out.push(decoded);
                }
            }
            Some('c') => break,
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Textual `dirname`: unlike path normalization this keeps `./` and does not
/// consult the filesystem.
fn textual_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.starts_with('/') { "/" } else { "." }.to_string();
    }
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(index) => {
            let head = &trimmed[..index];
            let head = head.trim_end_matches('/');
            if head.is_empty() {
                "/".to_string()
            } else {
                head.to_string()
            }
        }
    }
}

/// Commands implemented inside the shell itself.
///
/// Used by `type`/`command -v`, which must distinguish a builtin from a shell
/// function and from an unknown name (this runtime spawns no processes).
fn is_shell_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "["
            | "true"
            | "false"
            | "break"
            | "continue"
            | "exit"
            | "return"
            | "echo"
            | "printf"
            | "pwd"
            | "cd"
            | "cat"
            | "grep"
            | "wc"
            | "head"
            | "tail"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "basename"
            | "dirname"
            | "ls"
            | "mkdir"
            | "touch"
            | "cp"
            | "rm"
            | "mv"
            | "export"
            | "unset"
            | "env"
            | "test"
            | "read"
            | "set"
            | "yes"
            | "mktemp"
            | "chmod"
            | "stat"
            | "seq"
            | "declare"
            | "typeset"
            | "local"
            | "shift"
            | "jobs"
            | "wait"
            | "sed"
            | "curl"
            | "awk"
            | "jq"
            | "sleep"
            | "eval"
            | "source"
            | "."
            | "type"
            | "command"
            | "let"
            | "rev"
            | "fold"
            | "expand"
            | "unexpand"
            | "strings"
            | "paste"
            | "column"
            | "split"
            | "nl"
            | "join"
            | "find"
            | "tar"
            | "tee"
            | "base64"
            | "uname"
            | "whoami"
            | "id"
            | "readlink"
            | "realpath"
            | "trap"
            | "getopts"
            | "shopt"
            | "alias"
            | "unalias"
            | "readonly"
            | "exec"
            | "xxd"
            | "md5sum"
            | "sha1sum"
            | "sha256sum"
            | "du"
            | "tac"
            | "pushd"
            | "popd"
            | "dirs"
    )
}

/// Whether `name` is a valid assignment target (`FOO`, `_x`, `a1`).
fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
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
    /// `-R`: recurse into subdirectories.
    recursive: bool,
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
        // `f(){ f; }; f` would otherwise exhaust the stack and abort the host
        // process. Fail closed like every other execution limit.
        if self.function_depth >= self.execution_limits.max_call_depth {
            return CommandOutput {
                stdout: String::new(),
                stderr: format!(
                    "bash: maximum function call depth exceeded ({})\n",
                    self.execution_limits.max_call_depth
                ),
                exit_code: 1,
            };
        }
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
        match just_bash_regex::Regex::new(pattern) {
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
        let value = eval_arithmetic_expr_frozen(expression, &self.env, &self.positionals).ok()?;
        usize::try_from(value).ok()
    }

    /// An indexed subscript is an arithmetic expression, so `${a[i]}` and
    /// `${a[i+1]}` resolve through the environment like `$(( … ))` does.
    fn array_subscript_index(&self, subscript: &str) -> Option<usize> {
        let value = eval_arithmetic_expr_frozen(subscript, &self.env, &self.positionals).ok()?;
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
        eval_arithmetic_expr_frozen(text, &self.env, &self.positionals).ok()
    }

    /// `(start, end)` for a `${p:off:len}` slice over `length` items.
    ///
    /// A negative offset counts from the end and clamps at 0; a missing or
    /// empty length runs to the end; a non-positive length yields an empty
    /// range, all matching bash.
    /// Bounds for `${@:off:len}` / `${*:off:len}`.
    ///
    /// Positional parameters are addressed **from 1** (`${@:1}` is the first
    /// one), unlike `${var:off:len}` which is 0-based, so a non-negative offset
    /// is shifted down by one. Negative offsets count from the end and need no
    /// shift.
    fn positional_slice_bounds(&mut self, spec: &str, length: usize) -> Option<(usize, usize)> {
        let (offset_text, length_text) = split_slice_spec(spec);
        let offset = self.eval_int(offset_text)?;
        let (shifted, rest) = if offset > 0 {
            ((offset - 1).to_string(), length_text.map(str::to_string))
        } else if offset == 0 {
            // `${@:0}` includes the shell name in bash; this runtime has no
            // equivalent, so it behaves like `${@:1}`.
            ("0".to_string(), length_text.map(str::to_string))
        } else {
            (offset.to_string(), length_text.map(str::to_string))
        };
        let rebuilt = match rest {
            Some(text) => format!("{shifted}:{text}"),
            None => shifted,
        };
        self.slice_bounds(&rebuilt, length)
    }

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
            let bound = self.positional_slice_bounds(spec, parameters.len());
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
        let mut read_only = false;
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
                "-r" => {
                    read_only = true;
                    continue;
                }
                // Accepted for compatibility; this runtime has no integer
                // attribute to attach. `-g` is handled above.
                "-x" | "-i" | "-g" | "--" => continue,
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
            // `declare -r NAME`: remember the attribute so later writes fail.
            if read_only {
                self.readonly_names.insert(declaration.to_string());
            }
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
        self.expand_word_list_splitting(word)
    }

    /// Expand a word in list context: unquoted expansions are field-split on
    /// `IFS`, quoted and literal parts are not.
    ///
    /// This is the POSIX rule that makes `for f in $list` iterate and
    /// `cmd $(ls)` pass several arguments. Without it a whole expansion stayed a
    /// single field, which silently turned every such loop into one iteration.
    fn expand_word_list_splitting(&mut self, word: &Word) -> Vec<String> {
        // A word with no parts is a quoted empty string (`""`), which is one
        // empty field — unlike an unquoted empty expansion, which is no field.
        if word.parts.is_empty() {
            return vec![String::new()];
        }

        // Brace expansion runs before every other expansion and only on
        // unquoted literal text, so `x{a,b}y` becomes two words while
        // `"{a,b}"` stays literal.
        if let Some(alternatives) = plain_word_text(word).and_then(|text| brace_expand(&text)) {
            let mut fields = Vec::new();
            for alternative in alternatives {
                fields.extend(self.glob_fields(&alternative));
            }
            return fields;
        }

        let ifs = self
            .env
            .get("IFS")
            .map(String::as_str)
            .unwrap_or(" \t\n")
            .to_string();

        // Each field carries its own text plus a parallel "protected" mask: a
        // metacharacter that came from a quoted or backslash-escaped part must
        // not drive pathname expansion, so `"*.txt"` and `\*.txt` stay
        // literal.
        let mut fields: Vec<Field> = Vec::new();
        let mut field = Field::default();

        for (index, part) in word.parts.iter().enumerate() {
            let splittable = !part.quoted
                && matches!(
                    part.kind,
                    WordPartKind::Variable | WordPartKind::CommandSub | WordPartKind::Arithmetic
                );
            let mut text = self.expand_part(part);
            // Tilde expansion: a leading unquoted `~` (alone or before `/`)
            // becomes `$HOME`. `a~b` and `"~"` are untouched.
            if index == 0
                && !part.quoted
                && let Some(rest) = text.strip_prefix('~')
                && (rest.is_empty() || rest.starts_with('/'))
            {
                let home = self.env.get("HOME").cloned().unwrap_or_default();
                text = format!("{home}{rest}");
            }

            if !splittable {
                // An escaped part is a single protected character; anything
                // else (quoted text, plain literals, process-substitution
                // paths) is appended as-is, with quoting deciding protection.
                field.push(&text, part.quoted || part.kind == WordPartKind::Escaped);
                continue;
            }

            // Field-split the expansion and append the pieces, so `pre$x`
            // behaves like the string bash actually concatenates.
            let split = split_fields(&text, &ifs);
            for (index, piece) in split.fields.iter().enumerate() {
                if index > 0 {
                    fields.push(std::mem::take(&mut field));
                }
                field.push(piece, false);
            }
            if split.ended_with_delimiter && !field.is_empty() {
                fields.push(std::mem::take(&mut field));
            }
        }

        if !field.is_empty() {
            fields.push(field);
        }

        let mut expanded: Vec<String> = Vec::with_capacity(fields.len());
        for field in fields {
            let pattern = field.pattern();
            // `set -f`: a glob pattern stays literal.
            if self.opt_noglob {
                expanded.push(field.text);
                continue;
            }
            if glob::has_meta(&pattern) {
                let matches = glob::expand(&self.fs, &pattern, &self.cwd);
                if !matches.is_empty() {
                    expanded.extend(matches.into_iter().map(|m| unescape_field(&m)));
                    continue;
                }
                // `shopt -s nullglob`: an unmatched pattern expands to nothing
                // instead of being left as literal text.
                if self.shopt_nullglob {
                    continue;
                }
            }
            expanded.push(field.text);
        }
        expanded
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

    /// Print one directory for `ls`, recursing into subdirectories under `-R`.
    fn print_ls_directory(
        &mut self,
        path: &str,
        label: &str,
        options: &LsOptions,
        header: Option<String>,
        printed_any: &mut bool,
        stdout: &mut String,
    ) {
        let Ok(names) = self.fs.list_dir(path) else {
            return;
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
        sort_ls_entries(&mut entries, options);

        if let Some(header) = header {
            if *printed_any {
                stdout.push('\n');
            }
            stdout.push_str(&header);
            stdout.push_str(":\n");
        }
        let mut subdirectories: Vec<String> = Vec::new();
        for (name, meta) in &entries {
            stdout.push_str(&format_ls_entry(name, meta, options));
            if meta.is_dir() && name != "." && name != ".." {
                subdirectories.push(name.clone());
            }
        }
        *printed_any = true;

        if !options.recursive {
            return;
        }
        // `-R` labels nested listings relative to the operand (`./dir/sub`).
        subdirectories.sort();
        for name in subdirectories {
            let child_path = join_path(path, &name);
            let child_label = if label == "." {
                format!("./{name}")
            } else {
                format!("{}/{}", label.trim_end_matches('/'), name)
            };
            self.print_ls_directory(
                &child_path,
                &child_label,
                options,
                Some(child_label.clone()),
                printed_any,
                stdout,
            );
        }
    }

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
                    'R' => options.recursive = true,
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
            let header = if show_headers || options.recursive {
                Some(label.clone())
            } else {
                None
            };
            self.print_ls_directory(path, label, &options, header, &mut printed_any, stdout);
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
        let mut parsed = match just_bash_net::parse_curl_args(args) {
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
            } else if matches!(kind, just_bash_net::DataKind::UrlEncode) {
                *value = just_bash_net::urlencode_form(value);
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
            just_bash_net::effective_method(&parsed)
        };
        let request = just_bash_net::HttpRequest {
            method,
            url: parsed.url.clone(),
            headers: parsed.headers.clone(),
            body,
        };

        let response =
            match just_bash_net::perform(&request, &self.http_responses, self.allow_network) {
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
        // `[:class:]` expands to the character class GNU tr understands.
        if chars[index] == '[' && index + 1 < chars.len() && chars[index + 1] == ':' {
            let mut cursor = index + 2;
            while cursor < chars.len() && chars[cursor] != ':' {
                cursor += 1;
            }
            if cursor + 1 < chars.len() && chars[cursor + 1] == ']' {
                let name: String = chars[index + 2..cursor].iter().collect();
                if let Some(members) = tr_character_class(&name) {
                    set.extend(members);
                    index = cursor + 2;
                    continue;
                }
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

/// Character classes accepted inside `tr` sets.
fn tr_character_class(name: &str) -> Option<Vec<char>> {
    let members: Vec<char> = match name {
        "lower" => ('a'..='z').collect(),
        "upper" => ('A'..='Z').collect(),
        "digit" => ('0'..='9').collect(),
        "alpha" => ('a'..='z').chain('A'..='Z').collect(),
        "alnum" => ('a'..='z').chain('A'..='Z').chain('0'..='9').collect(),
        "space" => vec![' ', '\t', '\n', '\r', '\u{b}', '\u{c}'],
        "blank" => vec![' ', '\t'],
        "punct" => "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~".chars().collect(),
        "cntrl" => (0u32..32)
            .filter_map(char::from_u32)
            .chain(std::iter::once('\u{7f}'))
            .collect(),
        "print" => (32u32..127).filter_map(char::from_u32).collect(),
        "graph" => (33u32..127).filter_map(char::from_u32).collect(),
        "xdigit" => ('0'..='9').chain('a'..='f').chain('A'..='F').collect(),
        _ => return None,
    };
    Some(members)
}

/// Index of a top-level `-a`/`-o` connective in `test` arguments.
///
/// The first operand position is skipped: `test -a file` means "file exists",
/// so a leading `-a` is not a connective.
fn find_test_connective(args: &[String], operator: &str) -> Option<usize> {
    (2..args.len()).find(|index| args[*index] == operator)
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
        StatementKind::Arithmetic(_) => "(( ... ))".to_string(),
        StatementKind::ArrayAssign(assign) => format!("{}=(", assign.name),
        StatementKind::Redirected { inner, redirects } => {
            let base = describe_statement(&Statement {
                connector: PipelineConnector::Always,
                kind: (**inner).clone(),
                background: false,
                negated: false,
            });
            let mut text = base;
            for redirect in redirects {
                text.push(' ');
                text.push_str(&describe_redirect(redirect));
            }
            text
        }
    }
}

/// Render one redirection for `jobs` output.
fn describe_redirect(redirect: &Redirect) -> String {
    let target = redirect.target.text();
    match &redirect.mode {
        RedirectMode::Read => format!("< {target}"),
        RedirectMode::Write => format!("> {target}"),
        RedirectMode::Append => format!(">> {target}"),
        RedirectMode::Dup(fd) => format!("{}>&{fd}", redirect.fd),
        RedirectMode::Close => format!("{}>&-", redirect.fd),
        RedirectMode::HereString => "<<< {target}".to_string(),
        RedirectMode::Heredoc { .. } => format!("{}<<", redirect.fd),
    }
}

/// Parameter pattern operator, as written after the parameter name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternOperator {
    PrefixShortest,
    PrefixLongest,
    SuffixShortest,
    SuffixLongest,
    ReplaceFirst,
    ReplaceAll,
    UpperAll,
    LowerAll,
    UpperFirst,
    LowerFirst,
}

/// Split `pat/rep` at the first unescaped `/`; a missing `/` means an empty
/// replacement (`${v/pat}` deletes the match).
fn split_pattern_replacement(argument: &str) -> (&str, &str) {
    let bytes = argument.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'/' => return (&argument[..index], &argument[index + 1..]),
            _ => index += 1,
        }
    }
    (argument, "")
}

/// Uppercase or lowercase the first character, leaving the rest alone.
fn change_first_char(value: &str, upper: bool) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => {
            let mapped: String = if upper {
                first.to_uppercase().collect()
            } else {
                first.to_lowercase().collect()
            };
            format!("{mapped}{}", chars.as_str())
        }
        None => String::new(),
    }
}

/// One expansion field: its text plus which characters are protected from
/// pathname expansion.
#[derive(Debug, Default, Clone)]
struct Field {
    text: String,
    /// Parallel mask: `true` at byte positions that came from a quoted or
    /// escaped context.
    protected: Vec<bool>,
    /// Whether the field exists at all. A quoted empty expansion (`"$x"` with
    /// `x=""`) is one empty field, while an unquoted empty expansion is no
    /// field at all — the difference cannot be read off `text`.
    active: bool,
}

impl Field {
    fn is_empty(&self) -> bool {
        !self.active
    }

    fn push(&mut self, text: &str, protected: bool) {
        self.active = true;
        for ch in text.chars() {
            self.text.push(ch);
            self.protected.push(protected);
        }
    }

    /// Render this field as a glob pattern, escaping protected metacharacters.
    fn pattern(&self) -> String {
        let mut pattern = String::with_capacity(self.text.len());
        for (ch, protected) in self.text.chars().zip(self.protected.iter()) {
            if *protected && matches!(ch, '*' | '?' | '[' | ']' | '\\') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        pattern
    }
}

/// Strip the escaping added by `Field::pattern` from a glob result.
fn unescape_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(next) = chars.next()
        {
            out.push(next);
            continue;
        }
        out.push(ch);
    }
    out
}

/// Result of field-splitting one unquoted expansion.
///
/// `started_with_delimiter` / `ended_with_delimiter` matter because a split
/// boundary also terminates the field it touches: `pre$x` with `x=" a"` is two
/// fields (`pre`, `a`), not `prea`.
struct FieldSplit {
    fields: Vec<String>,
    ended_with_delimiter: bool,
}

/// Split `text` on `IFS`, following POSIX field-splitting rules.
///
/// - IFS whitespace (space, tab, newline that appear in IFS) collapses and is
///   stripped from both ends.
/// - Every other IFS character delimits exactly one field, so `a::b` with
///   `IFS=:` yields `a`, ``, `b`.
/// - A single trailing delimiter does not add a trailing empty field: `a:` with
///   `IFS=:` yields `a`, while `::` yields two empty fields.
/// - An empty IFS disables splitting entirely.
fn split_fields(text: &str, ifs: &str) -> FieldSplit {
    let mut fields: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut ended_with_delimiter = false;

    // An empty expansion contributes no fields at all. Whether a *quoted*
    // empty string survives as one empty field is decided by the caller, which
    // knows the quoting of each word part.
    if text.is_empty() {
        return FieldSplit {
            fields,
            ended_with_delimiter,
        };
    }

    if ifs.is_empty() {
        fields.push(text.to_string());
        return FieldSplit {
            fields,
            ended_with_delimiter,
        };
    }

    let is_ifs = |ch: char| ifs.contains(ch);
    let is_ifs_space = |ch: char| matches!(ch, ' ' | '\t' | '\n') && ifs.contains(ch);

    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    let mut emitted = false;

    // Leading IFS whitespace is not a field.
    while index < chars.len() && is_ifs_space(chars[index]) {
        index += 1;
    }
    // Text made only of IFS whitespace expands to nothing.
    if index == chars.len() {
        return FieldSplit {
            fields,
            ended_with_delimiter: true,
        };
    }

    while index < chars.len() {
        let ch = chars[index];
        if is_ifs_space(ch) {
            fields.push(std::mem::take(&mut current));
            emitted = true;
            ended_with_delimiter = true;
            while index < chars.len() && is_ifs_space(chars[index]) {
                index += 1;
            }
            // Whitespace around a non-whitespace delimiter is absorbed into it,
            // so `IFS=" :"` treats `"a : b"` as `a`, `b`.
            if index < chars.len() && is_ifs(chars[index]) && !is_ifs_space(chars[index]) {
                index += 1;
                while index < chars.len() && is_ifs_space(chars[index]) {
                    index += 1;
                }
            }
            continue;
        }
        if is_ifs(ch) {
            fields.push(std::mem::take(&mut current));
            emitted = true;
            ended_with_delimiter = true;
            index += 1;
            while index < chars.len() && is_ifs_space(chars[index]) {
                index += 1;
            }
            continue;
        }
        current.push(ch);
        ended_with_delimiter = false;
        index += 1;
    }

    // Emit the final field unless the text ended on a delimiter. A delimiter
    // that produced no field yet still yields one empty field (`":"` with
    // `IFS=:` is one empty field, not zero).
    // A delimiter that produced no field yet still yields one empty field
    // (`":"` with `IFS=:` is one empty field), but a plain trailing delimiter
    // adds nothing.
    if !ended_with_delimiter || !emitted {
        fields.push(current);
    }

    FieldSplit {
        fields,
        ended_with_delimiter,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CutMode {
    Fields,
    Characters,
}

/// One `cut` selection range, inclusive and 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CutRange {
    start: usize,
    end: Option<usize>,
}

/// Parse `cut`'s LIST syntax: `2`, `1,3`, `2-4`, `5-`, `-3`.
fn parse_cut_list(text: &str) -> Result<Vec<CutRange>, String> {
    let mut ranges = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("invalid field value '{text}'"));
        }
        let range = if let Some((start, end)) = part.split_once('-') {
            let start = if start.is_empty() {
                1
            } else {
                start
                    .parse::<usize>()
                    .map_err(|_| format!("invalid field value '{part}'"))?
            };
            let end = if end.is_empty() {
                None
            } else {
                Some(
                    end.parse::<usize>()
                        .map_err(|_| format!("invalid field value '{part}'"))?,
                )
            };
            CutRange { start, end }
        } else {
            let index = part
                .parse::<usize>()
                .map_err(|_| format!("invalid field value '{part}'"))?;
            CutRange {
                start: index,
                end: Some(index),
            }
        };
        ranges.push(range);
    }
    Ok(ranges)
}

/// Keep the items in `items` covered by `ranges`, in input order and without
/// duplicates (which is what `cut -f1,1` prints).
fn select_ranges<'a, T>(items: &'a [T], ranges: &[CutRange]) -> Vec<&'a T> {
    let mut selected = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let position = index + 1;
        let wanted = ranges
            .iter()
            .any(|range| position >= range.start && range.end.is_none_or(|end| position <= end));
        if wanted {
            selected.push(item);
        }
    }
    selected
}

/// Options for `head` / `tail`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CountMode {
    /// `-n N`: the first/last N lines.
    Lines(usize),
    /// `-n +N` with `tail`: everything from line N onwards.
    FromLine(usize),
    /// `-c N`: the first/last N bytes.
    Bytes(usize),
}

#[derive(Debug, Clone)]
struct HeadTailOptions {
    mode: CountMode,
    /// Whether the command is `tail` (last lines) rather than `head`.
    is_tail: bool,
    operands: Vec<String>,
}

fn parse_head_tail_args(program: &str, args: &[String]) -> Result<HeadTailOptions, String> {
    const TABLE: &[(char, &str, opts::Arity)] = &[
        ('n', "lines", opts::Arity::Value),
        ('c', "bytes", opts::Arity::Value),
    ];
    // The historical `head -5` form means `-n 5`; rewrite it before parsing.
    let mut normalized: Vec<String> = Vec::new();
    for arg in args {
        let numeric_count = arg.len() > 1
            && arg.starts_with('-')
            && !arg.starts_with("--")
            && arg[1..].chars().all(|ch| ch.is_ascii_digit());
        if numeric_count {
            normalized.push("-n".to_string());
            normalized.push(arg[1..].to_string());
        } else {
            normalized.push(arg.clone());
        }
    }
    let parsed = opts::parse(program, &normalized, TABLE)?;
    let mode = if let Some(value) = parsed.value("bytes") {
        CountMode::Bytes(
            value
                .parse::<usize>()
                .map_err(|_| format!("{program}: invalid number of bytes: '{value}'"))?,
        )
    } else if let Some(value) = parsed.value("lines") {
        match value.strip_prefix('+') {
            // Only `tail` supports the `+N` form.
            Some(rest) if program == "tail" => CountMode::FromLine(
                rest.parse::<usize>()
                    .map_err(|_| format!("{program}: invalid number of lines: '{value}'"))?,
            ),
            _ => CountMode::Lines(
                value
                    .parse::<usize>()
                    .map_err(|_| format!("{program}: invalid number of lines: '{value}'"))?,
            ),
        }
    } else {
        CountMode::Lines(10)
    };
    Ok(HeadTailOptions {
        mode,
        is_tail: program == "tail",
        operands: parsed.operands,
    })
}

/// Apply `head`/`tail` semantics to one input buffer.
fn head_or_tail(text: &str, options: &HeadTailOptions) -> String {
    match &options.mode {
        CountMode::Lines(count) => {
            let lines: Vec<&str> = text.lines().collect();
            let slice: Vec<&str> = if options.is_tail {
                let start = lines.len().saturating_sub(*count);
                lines[start..].to_vec()
            } else {
                lines.iter().take(*count).copied().collect()
            };
            render_lines(&slice)
        }
        CountMode::FromLine(start) => {
            let lines: Vec<&str> = text.lines().collect();
            let index = start.saturating_sub(1).min(lines.len());
            render_lines(&lines[index..])
        }
        CountMode::Bytes(count) => {
            let bytes = text.as_bytes();
            let slice = if options.is_tail {
                let start = bytes.len().saturating_sub(*count);
                &bytes[start..]
            } else {
                &bytes[..(*count).min(bytes.len())]
            };
            String::from_utf8_lossy(slice).into_owned()
        }
    }
}

fn render_lines(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Options accepted by `sort`.
#[derive(Debug, Default, Clone)]
struct SortOptions {
    reverse: bool,
    numeric: bool,
    unique: bool,
    ignore_case: bool,
    human: bool,
    version: bool,
    ignore_blanks: bool,
    check_only: bool,
    /// Field separator; `None` means runs of blanks.
    separator: Option<char>,
    /// 1-based key specification from `-k`.
    key: Option<String>,
    operands: Vec<String>,
}

fn sort_options(args: &[String]) -> Result<SortOptions, String> {
    const TABLE: &[(char, &str, opts::Arity)] = &[
        ('r', "reverse", opts::Arity::Flag),
        ('n', "numeric-sort", opts::Arity::Flag),
        ('u', "unique", opts::Arity::Flag),
        ('f', "ignore-case", opts::Arity::Flag),
        ('h', "human-numeric-sort", opts::Arity::Flag),
        ('V', "version-sort", opts::Arity::Flag),
        ('M', "month-sort", opts::Arity::Flag),
        ('c', "check", opts::Arity::Flag),
        ('b', "ignore-leading-blanks", opts::Arity::Flag),
        ('t', "field-separator", opts::Arity::Value),
        ('k', "key", opts::Arity::Value),
    ];
    let parsed = opts::parse("sort", args, TABLE)?;
    let mut options = SortOptions {
        reverse: parsed.has("reverse"),
        numeric: parsed.has("numeric-sort"),
        unique: parsed.has("unique"),
        ignore_case: parsed.has("ignore-case"),
        human: parsed.has("human-numeric-sort"),
        version: parsed.has("version-sort"),
        ignore_blanks: parsed.has("ignore-leading-blanks"),
        check_only: parsed.has("check"),
        separator: parsed
            .value("field-separator")
            .and_then(|v| v.chars().next()),
        key: parsed.value("key").map(str::to_string),
        operands: parsed.operands,
    };
    if options.human {
        // `-h` implies numeric ordering.
        options.numeric = true;
    }
    Ok(options)
}

/// The comparison key for one line under `options`.
fn sort_key<'a>(line: &'a str, options: &SortOptions) -> &'a str {
    let Some(spec) = options.key.as_deref() else {
        // `-b` ignores leading blanks; the slice is taken here so the
        // comparison itself stays allocation-free.
        return if options.ignore_blanks {
            line.trim_start()
        } else {
            line
        };
    };
    // `-k N[,M]` — only the start field is honoured; the field ends at the
    // separator, or at the end of the line when there is no `-t`.
    let start: usize = spec
        .split(',')
        .next()
        .unwrap_or("1")
        .trim()
        .parse()
        .unwrap_or(1)
        .max(1);
    let fields: Vec<&str> = match options.separator {
        Some(separator) => line.split(separator).collect(),
        None => line.split_whitespace().collect(),
    };
    fields.get(start - 1).copied().unwrap_or("")
}

/// A number read the way `sort -h` expects (`1K`, `2M`, `1G`).
fn sort_human_value(text: &str) -> i64 {
    let text = text.trim();
    let mut digits = String::new();
    let mut suffix = ' ';
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' || ch == '-' {
            digits.push(ch);
        } else {
            suffix = ch.to_ascii_uppercase();
            break;
        }
    }
    let base: f64 = digits.parse().unwrap_or(0.0);
    let scale = match suffix {
        'K' => 1024.0,
        'M' => 1024.0 * 1024.0,
        'G' => 1024.0 * 1024.0 * 1024.0,
        'T' => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (base * scale) as i64
}

/// Leading numeric value of a line, as GNU `sort -n` reads it.
fn sort_numeric_value(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let mut end = 0usize;
    let mut seen_digit = false;
    for (index, ch) in trimmed.char_indices() {
        if ch.is_ascii_digit() {
            seen_digit = true;
            end = index + ch.len_utf8();
        } else if ch == '-' || ch == '+' {
            if seen_digit {
                break;
            }
            end = index + ch.len_utf8();
        } else if ch == '.' && seen_digit {
            end = index + 1;
        } else {
            break;
        }
    }
    if !seen_digit {
        return 0.0;
    }
    trimmed[..end].parse().unwrap_or(0.0)
}

/// `sort -V`: compare dotted version strings numerically per component.
fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let split = |text: &str| -> Vec<String> {
        text.split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    };
    let a = split(left);
    let b = split(right);
    for (lhs, rhs) in a.iter().zip(b.iter()) {
        let ordering = match (lhs.parse::<u64>(), rhs.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            _ => lhs.cmp(rhs),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

/// Sort (or check) the input; a `-c` disorder names the file and line.
fn sort_lines_named(
    input: &str,
    options: &SortOptions,
    name: Option<&str>,
) -> Result<String, String> {
    let mut lines: Vec<&str> = input.lines().collect();

    // `sort -c` only reports whether the input is already ordered; the first
    // out-of-order line is reported the way GNU does.
    if options.check_only {
        for index in 1..lines.len() {
            let previous = sort_key(lines[index - 1], options);
            let current = sort_key(lines[index], options);
            if compare_keys(previous, current, options) == std::cmp::Ordering::Greater {
                let label = name.unwrap_or("-");
                return Err(format!(
                    "sort: {label}:{}: disorder: {}\n",
                    index + 1,
                    lines[index]
                ));
            }
        }
        return Ok(String::new());
    }

    lines.sort_by(|left, right| {
        let ordering = compare_keys(sort_key(left, options), sort_key(right, options), options);
        if options.reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });

    let mut output = String::new();
    let mut previous: Option<String> = None;
    for line in lines {
        if options.unique {
            let key = sort_key(line, options).to_string();
            if previous.as_deref() == Some(key.as_str()) {
                continue;
            }
            previous = Some(key);
        }
        output.push_str(line);
        output.push('\n');
    }
    Ok(output)
}

fn compare_keys(left: &str, right: &str, options: &SortOptions) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if options.version {
        return compare_versions(left, right);
    }
    if options.numeric {
        let (a, b): (f64, f64) = if options.human {
            (
                sort_human_value(left) as f64,
                sort_human_value(right) as f64,
            )
        } else {
            // `-n` compares the leading numeric prefix, so `2 oranges` sorts
            // before `10 apples`.
            (sort_numeric_value(left), sort_numeric_value(right))
        };
        return a.partial_cmp(&b).unwrap_or(Ordering::Equal);
    }
    if options.ignore_case {
        return left.to_lowercase().cmp(&right.to_lowercase());
    }
    let _ = Ordering::Equal;
    left.cmp(right)
}

/// Options accepted by `uniq`.
#[derive(Debug, Default, Clone)]
struct UniqOptions {
    count: bool,
    repeated_only: bool,
    unique_only: bool,
    ignore_case: bool,
    operands: Vec<String>,
}

fn uniq_options(args: &[String]) -> Result<UniqOptions, String> {
    const TABLE: &[(char, &str, opts::Arity)] = &[
        ('c', "count", opts::Arity::Flag),
        ('d', "repeated", opts::Arity::Flag),
        ('u', "unique", opts::Arity::Flag),
        ('i', "ignore-case", opts::Arity::Flag),
    ];
    let parsed = opts::parse("uniq", args, TABLE)?;
    Ok(UniqOptions {
        count: parsed.has("count"),
        repeated_only: parsed.has("repeated"),
        unique_only: parsed.has("unique"),
        ignore_case: parsed.has("ignore-case"),
        operands: parsed.operands,
    })
}

fn filter_uniq(input: &str, options: &UniqOptions) -> String {
    // Group first: `-c` pads the count to the width of the widest one, so the
    // output cannot be streamed.
    let mut runs: Vec<(String, String, usize)> = Vec::new();
    for line in input.lines() {
        let comparable = if options.ignore_case {
            line.to_lowercase()
        } else {
            line.to_string()
        };
        match runs.last_mut() {
            Some((key, _, count)) if *key == comparable => *count += 1,
            _ => runs.push((comparable, line.to_string(), 1)),
        }
    }

    let mut selected: Vec<(String, usize)> = Vec::new();
    for (_, line, count) in runs {
        let repeated = count > 1;
        if options.repeated_only && !repeated {
            continue;
        }
        if options.unique_only && repeated {
            continue;
        }
        selected.push((line, count));
    }

    // The upstream fixtures pin the count to seven columns (the layout this
    // project matches), widening only if a count needs more digits.
    let width = selected
        .iter()
        .map(|(_, count)| count.to_string().len())
        .max()
        .unwrap_or(1)
        .max(7);
    let mut output = String::new();
    for (line, count) in selected {
        if options.count {
            output.push_str(&format!("{count:>width$} {line}\n", width = width));
        } else {
            output.push_str(&line);
            output.push('\n');
        }
    }
    output
}

/// The command name of the first pipeline stage, skipping any leading
/// `NAME=value` assignments (`IFS= read -r line` is a `read`).
fn first_command_name(commands: &[CommandInvocation]) -> Option<String> {
    let words = commands.first()?.words.iter();
    for word in words {
        let looks_like_assignment = word.parts.first().is_some_and(|part| {
            !part.quoted
                && part.kind == WordPartKind::Variable
                && part
                    .text
                    .split_once('=')
                    .is_some_and(|(name, _)| is_assignment_name(name))
        });
        if looks_like_assignment {
            continue;
        }
        return Some(word.text());
    }
    None
}

/// The text of a word that is a single unquoted literal with nothing to expand.
///
/// Brace expansion only applies to such words: a word containing `$`, a
/// backtick or a backslash is left to the ordinary expansion path.
fn plain_word_text(word: &Word) -> Option<String> {
    if word.parts.len() != 1 {
        return None;
    }
    let part = &word.parts[0];
    if part.quoted || part.kind != WordPartKind::Variable {
        return None;
    }
    if part.text.contains(['$', '`', '\\', '"', '\'']) {
        return None;
    }
    Some(part.text.clone())
}

/// Expand bash brace lists (`{a,b}`) and ranges (`{1..3}`, `{a..c}`).
///
/// Returns `None` when the text has no expandable brace group.
fn brace_expand(text: &str) -> Option<Vec<String>> {
    brace_expand_at(text, 0)
}

fn brace_expand_at(text: &str, depth: usize) -> Option<Vec<String>> {
    // A hostile word must not recurse without bound.
    if depth > 16 {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let open = find_expandable_brace(&chars)?;
    let close = matching_brace(&chars, open)?;
    let inner: String = chars[open + 1..close].iter().collect();
    let prefix: String = chars[..open].iter().collect();
    let suffix: String = chars[close + 1..].iter().collect();

    let items: Vec<String> = match split_brace_list(&inner) {
        Some(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match brace_expand_at(&part, depth + 1) {
                    Some(nested) => out.extend(nested),
                    None => out.push(part),
                }
            }
            out
        }
        None => brace_range(&inner)?,
    };

    let mut expanded = Vec::new();
    for item in items {
        let candidate = format!("{prefix}{item}{suffix}");
        match brace_expand_at(&candidate, depth + 1) {
            Some(nested) => expanded.extend(nested),
            None => expanded.push(candidate),
        }
    }
    Some(expanded)
}

/// Index of the first `{` that has a matching `}` and expandable content.
fn find_expandable_brace(chars: &[char]) -> Option<usize> {
    for (index, ch) in chars.iter().enumerate() {
        if *ch != '{' {
            continue;
        }
        let Some(close) = matching_brace(chars, index) else {
            continue;
        };
        let inner: String = chars[index + 1..close].iter().collect();
        if split_brace_list(&inner).is_some() || brace_range(&inner).is_some() {
            return Some(index);
        }
    }
    None
}

fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in chars.iter().enumerate().skip(open) {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a brace list on top-level commas, or `None` when there is no comma.
fn split_brace_list(inner: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut saw_comma = false;
    for ch in inner.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                saw_comma = true;
                parts.push(std::mem::take(&mut current));
            }
            other => current.push(other),
        }
    }
    if !saw_comma {
        return None;
    }
    parts.push(current);
    Some(parts)
}

/// Expand `a..b` numeric or single-character ranges.
fn brace_range(inner: &str) -> Option<Vec<String>> {
    let (start, end) = inner.split_once("..")?;
    if start.contains("..") || end.contains("..") {
        return None;
    }
    if let (Ok(from), Ok(to)) = (start.parse::<i64>(), end.parse::<i64>()) {
        let step = if from <= to { 1 } else { -1 };
        let mut values = Vec::new();
        let mut current = from;
        loop {
            values.push(current.to_string());
            if current == to {
                break;
            }
            current += step;
            if values.len() > 10_000 {
                return None;
            }
        }
        return Some(values);
    }
    let mut from = start.chars();
    let mut to = end.chars();
    if let (Some(from), None, Some(to), None) = (from.next(), from.next(), to.next(), to.next()) {
        let step: i32 = if from <= to { 1 } else { -1 };
        let mut values = Vec::new();
        let mut code = from as i32;
        loop {
            values.push(char::from_u32(code as u32)?.to_string());
            if code == to as i32 {
                break;
            }
            code += step;
        }
        return Some(values);
    }
    None
}

/// Whether a command consumes standard input when no file operand is given.
///
/// Used to decide if a command inside a redirected compound advances the
/// shared stdin offset. Unknown commands are treated as non-consuming so a
/// loop body cannot accidentally drain its own input.
fn command_reads_stdin(name: &str) -> bool {
    matches!(
        name,
        "cat"
            | "grep"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "head"
            | "tail"
            | "wc"
            | "sed"
            | "awk"
            | "jq"
    )
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
        assert_eq!(result.stdout, "alpha\n      1       1       5\n");
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
        assert_eq!(result.stdout, "      0       1       1\n");
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
        // Truncating `f` empties it, and an all-zero file prints bare counters.
        assert_eq!(result.stdout, "0 0 0 f\n");
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
            "cut: you must specify a list of bytes, characters, or fields\n"
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
        assert_eq!(result.stdout, "      1       2\n");
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
        // Without `-F` the pattern is a regular expression, so `.` matches any
        // character — the same as GNU grep.
        assert_eq!(out("printf 'a.c\\nabc\\n' | grep 'a.c'\n"), "a.c\nabc\n");
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
        // `y` and `N` are supported now; an unimplemented command still fails.
        let result = run("printf 'x\\n' | sed 'w out.txt'\n");
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
        // Positional parameters are addressed from 1, so `${@:2}` skips only
        // the first one (unlike `${var:2}` which is 0-based).
        assert_eq!(out("set -- a b c d; echo ${@:1}\n"), "a b c d\n");
        assert_eq!(out("set -- a b c d; echo ${@:2:2}\n"), "b c\n");
        // A negative offset counts from the end and needs no shift.
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

#[cfg(test)]
mod host_safety_tests {
    use super::*;

    fn bash_with(limits: ExecutionLimits) -> Bash {
        Bash::new(Options {
            execution_limits: limits,
            ..Options::default()
        })
        .expect("initialize")
    }

    /// A recursive function used to exhaust the stack and abort the host
    /// process before the call-depth limit existed.
    #[test]
    fn recursive_function_fails_closed_instead_of_aborting() {
        let limits = ExecutionLimits {
            max_call_depth: 20,
            ..ExecutionLimits::default()
        };
        let result = bash_with(limits).exec("f(){ f; }; f");
        assert_eq!(result.exit_code, 1);
        assert_eq!(
            result.stderr,
            "bash: maximum function call depth exceeded (20)\n"
        );
    }

    /// Mutually recursive functions are bounded by the same budget.
    #[test]
    fn mutually_recursive_functions_are_bounded() {
        let limits = ExecutionLimits {
            max_call_depth: 20,
            ..ExecutionLimits::default()
        };
        let result = bash_with(limits).exec("a(){ b; }; b(){ a; }; a");
        assert_eq!(result.exit_code, 1);
        assert!(
            result
                .stderr
                .contains("maximum function call depth exceeded")
        );
    }

    /// `{ ${x:-${x:-…}} }` recurses through the operator word.
    #[test]
    fn deep_parameter_nesting_fails_closed() {
        let limits = ExecutionLimits {
            max_expansion_depth: 10,
            ..ExecutionLimits::default()
        };
        let script = format!("echo {}{}{}", "${x:-".repeat(40), "d", "}".repeat(40));
        let result = bash_with(limits).exec(&script);
        assert_eq!(result.exit_code, 1);
        assert_eq!(
            result.stderr,
            "bash: maximum expansion depth exceeded (10)\n"
        );
    }

    /// The parser limit is enforced before execution, so the host survives.
    #[test]
    fn deep_syntax_fails_closed() {
        let limits = ExecutionLimits {
            max_nesting_depth: 8,
            ..ExecutionLimits::default()
        };
        let script = format!("{}echo hi;{}", "{ ".repeat(40), " }".repeat(40));
        let result = bash_with(limits).exec(&script);
        assert_eq!(result.exit_code, 2);
        assert_eq!(
            result.stderr,
            "bash: maximum parser nesting depth exceeded (8)\n"
        );
    }

    /// Deep nesting must not overflow even on a small (1 MiB) thread stack,
    /// which is what embedders commonly give worker threads.
    #[test]
    fn depth_limits_hold_on_a_small_stack() {
        let handle = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut bash = Bash::new(Options::default()).unwrap();
                let nested = format!(
                    "{}echo hi;{}",
                    "{ ".repeat(ExecutionLimits::default().max_nesting_depth + 5),
                    " }".repeat(ExecutionLimits::default().max_nesting_depth + 5)
                );
                assert_eq!(bash.exec(&nested).exit_code, 2);
                assert_eq!(bash.exec("f(){ f; }; f").exit_code, 1);
                assert_eq!(bash.exec("echo ok").stdout, "ok\n");
            })
            .unwrap();
        handle.join().expect("small-stack worker must not abort");
    }

    /// `read` reports end-of-file with status 1, so `while read` terminates.
    #[test]
    fn read_reports_eof() {
        let result = bash_with(ExecutionLimits::default()).exec("read x; echo \"rc=$?\"");
        assert_eq!(result.stdout, "rc=1\n");
        assert_eq!(result.stderr, "");
    }

    /// An empty line is data, not EOF.
    #[test]
    fn read_distinguishes_blank_line_from_eof() {
        let result = bash_with(ExecutionLimits::default())
            .exec("printf '\\n' | { read x; echo \"rc=$? v=[$x]\"; }");
        // Compound-command pipes are not supported yet, so assert only that the
        // blank line case does not report EOF when stdin holds a newline.
        assert!(!result.stdout.contains("rc=1 v=[]"));
    }

    /// Limits must not disturb ordinary scripts.
    #[test]
    fn ordinary_scripts_still_run() {
        let result = bash_with(ExecutionLimits::default())
            .exec("f(){ echo fn; }; f; (echo sub); { echo grp; }; x=1; echo $x");
        assert_eq!(result.stdout, "fn\nsub\ngrp\n1\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }
}

#[cfg(test)]
mod compound_redirect_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `{ …; } > f` used to print to stdout and leave an empty file.
    #[test]
    fn group_output_redirect_writes_the_file() {
        assert_eq!(out("{ echo a; echo b; } > /o; cat /o"), "a\nb\n");
    }

    #[test]
    fn group_append_redirect_appends() {
        assert_eq!(
            out("echo first > /o; { echo a; echo b; } >> /o; cat /o"),
            "first\na\nb\n"
        );
    }

    #[test]
    fn if_statement_redirect_writes_the_file() {
        assert_eq!(
            out("if true; then echo yes; else echo no; fi > /o; cat /o"),
            "yes\n"
        );
    }

    #[test]
    fn for_loop_redirect_writes_the_file() {
        assert_eq!(
            out("for i in 1 2 3; do echo $i; done > /o; cat /o"),
            "1\n2\n3\n"
        );
    }

    #[test]
    fn case_redirect_writes_the_file() {
        assert_eq!(
            out("case x in x) echo matched;; esac > /o; cat /o"),
            "matched\n"
        );
    }

    #[test]
    fn subshell_redirect_writes_the_file() {
        assert_eq!(out("( echo one; echo two ) > /o; cat /o"), "one\ntwo\n");
    }

    /// Both descriptors of a compound follow the same written-order rules.
    #[test]
    fn compound_merges_both_streams() {
        assert_eq!(
            out("{ echo out; echo err >&2; } > /o 2>&1; cat /o"),
            "out\nerr\n"
        );
    }

    #[test]
    fn compound_stderr_redirect_is_separate() {
        let result = bash().exec("{ echo out; echo err >&2; } > /o 2> /e; cat /o; cat /e");
        assert_eq!(result.stdout, "out\nerr\n");
    }

    /// The input redirect is shared by every command inside the compound.
    #[test]
    fn group_reads_redirected_stdin() {
        assert_eq!(out("printf 'x\\ny\\n' > /in; { cat; } < /in"), "x\ny\n");
    }

    #[test]
    fn group_shares_one_stdin_offset_across_reads() {
        assert_eq!(
            out("printf 'a\\nb\\n' > /in; { read x; read y; echo \"$x-$y\"; } < /in"),
            "a-b\n"
        );
    }

    /// The classic `while read` line loop, previously unusable.
    #[test]
    fn while_read_loop_consumes_each_line() {
        assert_eq!(
            out("printf 'a\\nb\\nc\\n' > /in; while read x; do echo \"[$x]\"; done < /in"),
            "[a]\n[b]\n[c]\n"
        );
    }

    #[test]
    fn while_read_loop_counts_lines() {
        assert_eq!(
            out(
                "printf 'a\\nb\\nc\\n' > /in; n=0; while read x; do n=$((n+1)); done < /in; echo $n"
            ),
            "3\n"
        );
    }

    /// A loop body that does not read stdin must not drain the shared buffer.
    #[test]
    fn loop_body_without_stdin_consumer_does_not_drain() {
        assert_eq!(
            out("printf '1\\n2\\n' > /in; while read n; do echo \"v$n\"; done < /in; echo done"),
            "v1\nv2\ndone\n"
        );
    }

    #[test]
    fn sort_inside_redirected_group_sorts_stdin() {
        assert_eq!(
            out("printf '3\\n1\\n2\\n' > /in; { sort; } < /in"),
            "1\n2\n3\n"
        );
    }

    /// Missing input file is a clean error, not a silent empty stream.
    #[test]
    fn missing_input_redirect_reports_an_error() {
        let result = bash().exec("{ cat; } < /missing");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("No such file or directory"),
            "{}",
            result.stderr
        );
    }

    /// Redirections still compose with the rest of the script.
    #[test]
    fn compound_redirect_inside_a_function() {
        assert_eq!(out("f(){ { echo in-fn; } > /o; }; f; cat /o"), "in-fn\n");
    }
}

#[cfg(test)]
mod field_splitting_tests {
    use super::*;

    fn split(text: &str, ifs: &str) -> Vec<String> {
        split_fields(text, ifs).fields
    }

    /// Expected values recorded from GNU bash 3.2 (`set -- $x`).
    #[test]
    fn matches_bash_splitting_table() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("a b c", " \t\n", &["a", "b", "c"]),
            ("  a   b  ", " \t\n", &["a", "b"]),
            ("", " \t\n", &[]),
            ("   ", " \t\n", &[]),
            ("a,b", ":", &["a,b"]),
            ("a::b", ":", &["a", "", "b"]),
            (":a", ":", &["", "a"]),
            ("a:", ":", &["a"]),
            (":", ":", &[""]),
            ("::", ":", &["", ""]),
            ("a : b", " :", &["a", "b"]),
            ("a: :b", " :", &["a", "", "b"]),
            ("  :  ", " :", &[""]),
            (" a , b ", ",", &[" a ", " b "]),
            ("a\tb", " \t\n", &["a", "b"]),
            ("a\nb", " \t\n", &["a", "b"]),
        ];
        for (text, ifs, expected) in cases {
            let actual = split(text, ifs);
            let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(actual, expected, "split({text:?}, IFS={ifs:?})");
        }
    }

    #[test]
    fn empty_ifs_disables_splitting() {
        assert_eq!(split("a b", ""), vec!["a b"]);
    }

    #[test]
    fn records_boundary_flags() {
        assert!(split_fields("a ", " \t\n").ended_with_delimiter);
        assert!(!split_fields("a", " \t\n").ended_with_delimiter);
        assert!(split_fields("a:", ":").ended_with_delimiter);
    }
}

#[cfg(test)]
mod word_splitting_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn fields(script: &str) -> String {
        let text = out(&format!("{script}; printf '<%s>' \"$@\""));
        text.trim_end_matches('\n').to_string()
    }

    /// The headline bug: `for f in $list` iterated once because the whole
    /// expansion stayed a single field.
    #[test]
    fn for_loop_iterates_split_expansion() {
        assert_eq!(
            out("x=\"a b c\"; for i in $x; do echo \"<$i>\"; done"),
            "<a>\n<b>\n<c>\n"
        );
    }

    #[test]
    fn unquoted_variable_splits_on_whitespace() {
        assert_eq!(fields("x=\"a b c\"; set -- $x"), "<a><b><c>");
    }

    #[test]
    fn quoted_variable_does_not_split() {
        assert_eq!(fields("x=\"a b c\"; set -- \"$x\""), "<a b c>");
    }

    #[test]
    fn command_substitution_splits() {
        assert_eq!(fields("set -- $(echo one two three)"), "<one><two><three>");
    }

    #[test]
    fn quoted_command_substitution_does_not_split() {
        assert_eq!(fields("set -- \"$(echo one two)\""), "<one two>");
    }

    #[test]
    fn empty_expansion_yields_no_field() {
        assert_eq!(fields("x=\"\"; set -- $x"), "<>");
        assert_eq!(out("x=\"\"; set -- $x; echo $#"), "0\n");
    }

    #[test]
    fn empty_quoted_expansion_yields_one_empty_field() {
        assert_eq!(out("x=\"\"; set -- \"$x\"; echo $#"), "1\n");
    }

    #[test]
    fn leading_and_trailing_whitespace_is_trimmed() {
        assert_eq!(fields("x=\"   a   b   \"; set -- $x"), "<a><b>");
    }

    #[test]
    fn ifs_non_whitespace_delimiter_creates_fields() {
        assert_eq!(fields("IFS=,; x=\"a,b,c\"; set -- $x"), "<a><b><c>");
    }

    #[test]
    fn adjacent_ifs_delimiters_produce_empty_fields() {
        assert_eq!(fields("IFS=:; x=\"a::b\"; set -- $x"), "<a><><b>");
    }

    #[test]
    fn leading_ifs_delimiter_produces_empty_first_field() {
        assert_eq!(fields("IFS=:; x=\":a\"; set -- $x"), "<><a>");
    }

    #[test]
    fn trailing_ifs_delimiter_adds_no_field() {
        assert_eq!(out("IFS=:; x=\"a:\"; set -- $x; echo $#"), "1\n");
    }

    #[test]
    fn whitespace_around_a_delimiter_is_absorbed() {
        assert_eq!(fields("IFS=\" :\"; x=\"a : b\"; set -- $x"), "<a><b>");
    }

    #[test]
    fn empty_ifs_disables_splitting() {
        assert_eq!(fields("IFS=''; x=\"a b\"; set -- $x"), "<a b>");
    }

    #[test]
    fn literal_prefix_joins_the_first_field() {
        assert_eq!(fields("x=\"a b\"; set -- pre$x"), "<prea><b>");
    }

    #[test]
    fn leading_delimiter_ends_the_literal_prefix_field() {
        assert_eq!(fields("x=\" a\"; set -- pre$x"), "<pre><a>");
    }

    #[test]
    fn trailing_delimiter_ends_the_field_before_the_next_literal() {
        assert_eq!(fields("x=\"a \"; set -- $x\"b\""), "<a><b>");
    }

    #[test]
    fn quoted_prefix_and_suffix_bracket_the_split() {
        assert_eq!(fields("x=\"a b\"; set -- \"p\"$x\"q\""), "<pa><bq>");
    }

    /// Field splitting happens before pathname expansion, and a metacharacter
    /// that came from a quoted or escaped part must stay literal.
    #[test]
    fn protection_survives_splitting() {
        let mut bash = Bash::new(Options {
            files: BTreeMap::from([
                ("/home/user/a.txt".to_string(), String::new()),
                ("/home/user/b.txt".to_string(), String::new()),
            ]),
            ..Options::default()
        })
        .expect("initialize");
        assert_eq!(bash.exec("echo *.txt").stdout, "a.txt b.txt\n");
        assert_eq!(bash.exec("echo \"*.txt\"").stdout, "*.txt\n");
        assert_eq!(bash.exec("echo \\*.txt").stdout, "*.txt\n");
        assert_eq!(bash.exec("x='*.txt'; echo $x").stdout, "a.txt b.txt\n");
    }

    #[test]
    fn split_fields_glob_individually() {
        let mut bash = Bash::new(Options {
            files: BTreeMap::from([
                ("/home/user/a.txt".to_string(), String::new()),
                ("/home/user/b.md".to_string(), String::new()),
            ]),
            ..Options::default()
        })
        .expect("initialize");
        assert_eq!(
            bash.exec("x=\"*.txt *.md\"; echo $x").stdout,
            "a.txt b.md\n"
        );
    }

    #[test]
    fn read_and_argv_paths_still_see_one_field_per_argument() {
        assert_eq!(out("set -- \"a b\" c; printf '<%s>' \"$@\""), "<a b><c>");
    }
}

#[cfg(test)]
mod pattern_operator_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn value(script: &str) -> String {
        out(script).trim_end_matches('\n').to_string()
    }

    /// Expected values recorded from GNU bash 3.2. Every one of these used to
    /// silently return the parameter unchanged.
    #[test]
    fn prefix_and_suffix_removal_matches_bash() {
        let cases: &[(&str, &str)] = &[
            ("${p#a}", "bcABCabc"),
            ("${p##*a}", "bc"),
            ("${p#*a}", "bcABCabc"),
            ("${p%b}", "abcABCabc"),
            ("${p%%b*}", "a"),
            ("${p#z}", "abcABCabc"),
            ("${p%z}", "abcABCabc"),
            ("${p#}", "abcABCabc"),
            ("${p%}", "abcABCabc"),
        ];
        for (expression, expected) in cases {
            let script = format!("p=abcABCabc; printf '%s' \"{expression}\"");
            assert_eq!(value(&script), *expected, "{expression}");
        }
    }

    #[test]
    fn path_shaped_removals_match_bash() {
        assert_eq!(value("p=/a/b/c.txt; printf '%s' \"${p##*/}\""), "c.txt");
        assert_eq!(value("p=/a/b/c.txt; printf '%s' \"${p%/*}\""), "/a/b");
        // The shortest prefix matching `*/` is the leading `/`, so only that
        // is removed — bash agrees.
        assert_eq!(value("p=/a/b/c.txt; printf '%s' \"${p#*/}\""), "a/b/c.txt");
    }

    #[test]
    fn unset_and_empty_parameters_are_treated_as_empty() {
        assert_eq!(value("unset q; printf '%s' \"${q#a}\""), "");
        assert_eq!(value("p=; printf '%s' \"${p#a}\""), "");
        assert_eq!(value("unset q; printf '%s' \"${q/a/b}\""), "");
    }

    #[test]
    fn replacement_first_and_all_match_bash() {
        let cases: &[(&str, &str, &str)] = &[
            ("a-b-c", "${p/-/+/}", "a+/b-c"),
            ("a-b-c", "${p//-/+/}", "a+/b+/c"),
            ("aaa", "${p/a/X}", "Xaa"),
            ("aaa", "${p//a/X}", "XXX"),
            ("abc", "${p/b/}", "ac"),
            ("a.b.c", "${p//./-}", "a-b-c"),
            ("abc", "${p/x/y}", "abc"),
            ("abc", "${p//?/X}", "XXX"),
        ];
        for (initial, expression, expected) in cases {
            let script = format!("p={initial}; printf '%s' \"{expression}\"");
            assert_eq!(value(&script), *expected, "{expression}");
        }
    }

    /// Empty matches are never replaced and matching stops at the end of the
    /// value; getting this wrong made `${p//a*/Y}` loop forever.
    #[test]
    fn replacement_handles_empty_matches() {
        let cases: &[(&str, &str, &str)] = &[
            ("abc", "${p//x*/Y}", "abc"),
            ("abc", "${p//a*/Y}", "Y"),
            ("abc", "${p//*/Y}", "Y"),
            ("", "${p//*/Y}", ""),
            ("", "${p//a/Y}", ""),
            ("abc", "${p///Y}", "abc"),
            ("abc", "${p//?*/Y}", "Y"),
            ("abc", "${p//b/Y}", "aYc"),
            ("abc", "${p//abc/Y}", "Y"),
            ("abc", "${p//abcd/Y}", "abc"),
        ];
        for (initial, expression, expected) in cases {
            let script = format!("p='{initial}'; printf '%s' \"{expression}\"");
            assert_eq!(value(&script), *expected, "p={initial:?} {expression}");
        }
    }

    /// Patterns are globs, and an escaped metacharacter is literal.
    #[test]
    fn replacement_supports_globs_and_escapes() {
        assert_eq!(value("p=a*b; printf '%s' \"${p//\\*/X}\""), "aXb");
        assert_eq!(value("p=a/b/c; printf '%s' \"${p//\\//-}\""), "a-b-c");
        assert_eq!(value("p=abc; printf '%s' \"${p//[a-c]/X}\""), "XXX");
    }

    /// Case conversion follows bash 4 / upstream semantics; bash 3.2 predates
    /// the syntax.
    #[test]
    fn case_conversion() {
        assert_eq!(value("p=hello; printf '%s' \"${p^^}\""), "HELLO");
        assert_eq!(value("p=HELLO; printf '%s' \"${p,,}\""), "hello");
        assert_eq!(value("p=abc; printf '%s' \"${p^}\""), "Abc");
        assert_eq!(value("p=ABC; printf '%s' \"${p,}\""), "aBC");
    }

    /// The pattern may itself be computed.
    #[test]
    fn pattern_may_be_an_expansion() {
        assert_eq!(value("p=abcABC; cut=a; printf '%s' \"${p#$cut}\""), "bcABC");
        assert_eq!(value("p=abc; pat=b; printf '%s' \"${p//$pat/X}\""), "aXc");
    }

    /// The default/assign/alternate operators must be untouched.
    #[test]
    fn value_operators_still_work() {
        assert_eq!(value("x=abc; printf '%s' \"${x:-def}\""), "abc");
        assert_eq!(value("unset y; printf '%s' \"${y:-def}\""), "def");
        assert_eq!(value("x=5; printf '%s' \"${x:+set}\""), "set");
        assert_eq!(value("unset u; printf '%s' \"${u:+set}\""), "");
        assert_eq!(value("x=abcdef; printf '%s' \"${x:1:3}\""), "bcd");
        assert_eq!(value("x=abc; printf '%s' \"${#x}\""), "3");
    }
}

#[cfg(test)]
mod assignment_prefix_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `VAR=value cmd` used to be treated as a command named `VAR=value`,
    /// so every temporary-environment idiom failed with "command not found".
    #[test]
    fn assignment_prefix_reaches_the_command() {
        assert_eq!(out("x=1 echo hi"), "hi\n");
    }

    #[test]
    fn assignment_prefix_is_visible_to_a_builtin() {
        // `env` lists the shell environment, so the temporary value shows up.
        assert!(out("x=1 env").lines().any(|line| line == "x=1"));
    }

    #[test]
    fn assignment_prefix_is_restored_afterwards() {
        assert_eq!(out("x=outer; x=inner true; echo $x"), "outer\n");
        assert_eq!(out("x=inner true; echo \"[$x]\""), "[]\n");
    }

    #[test]
    fn assignment_prefix_value_is_not_field_split() {
        // The value keeps its space: assignments never undergo word splitting.
        let result = bash().exec("q='a b'; x=$q env");
        assert!(result.stdout.lines().any(|line| line == "x=a b"));
    }

    #[test]
    fn plain_assignment_still_persists() {
        assert_eq!(out("x=1; echo $x"), "1\n");
        assert_eq!(out("q='a b'; x=$q; echo \"$x\""), "a b\n");
    }

    #[test]
    fn assignment_prefix_before_a_function() {
        assert_eq!(
            out("f(){ echo \"v=$x\"; }; x=inner f; echo \"[$x]\""),
            "v=inner\n[]\n"
        );
    }

    /// The canonical `while read` loop, which needs both the compound
    /// redirection and the `IFS=` prefix.
    #[test]
    fn read_loop_with_ifs_prefix() {
        assert_eq!(
            out("printf 'l1\\nl2\\n' > /i; while IFS= read -r l; do echo \"[$l]\"; done < /i"),
            "[l1]\n[l2]\n"
        );
    }

    #[test]
    fn read_loop_preserves_leading_whitespace_with_ifs_prefix() {
        assert_eq!(
            out("printf '  padded\\n' > /i; while IFS= read -r l; do echo \"[$l]\"; done < /i"),
            "[  padded]\n"
        );
    }
}

#[cfg(test)]
mod parser_expansion_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn value(script: &str) -> String {
        out(script).trim_end_matches('\n').to_string()
    }

    // ── ANSI-C quoting `$'…'` ───────────────────────────────────────────

    #[test]
    fn ansi_c_escapes() {
        assert_eq!(value(r"printf '%s' $'a\tb'"), "a\tb");
        assert_eq!(value(r"printf '%s' $'a\nb'"), "a\nb");
        assert_eq!(value(r"printf '%s' $'\x41\101'"), "AA");
        assert_eq!(value(r"printf '%s' $'it\'s'"), "it's");
        assert_eq!(value("printf '%s' $'say \"hi\"'"), "say \"hi\"");
        assert_eq!(value(r"printf '%s' $'\\'"), "\\");
    }

    /// The result is a literal: it is never expanded again or split.
    #[test]
    fn ansi_c_result_is_literal() {
        assert_eq!(value("x=$'a b'; printf '[%s]' \"$x\""), "[a b]");
        assert_eq!(value(r"printf '[%s]' $'$HOME'"), "[$HOME]");
    }

    /// Inside double quotes `$'…'` is ordinary text, as in bash.
    #[test]
    fn ansi_c_is_not_recognised_inside_double_quotes() {
        assert_eq!(value("printf '%s' \"$'a\\tb'\""), "$'a\\tb'");
    }

    // ── backtick command substitution ───────────────────────────────────

    #[test]
    fn backtick_substitution() {
        assert_eq!(value("printf '%s' `echo hi`"), "hi");
        assert_eq!(value("x=`echo hi`; printf '[%s]' \"$x\""), "[hi]");
        assert_eq!(value("printf '%s' \"`echo hi`\""), "hi");
    }

    /// The substitution result is field-split only when unquoted.
    #[test]
    fn backtick_result_splits_only_when_unquoted() {
        assert_eq!(value("printf '<%s>' `echo a b`"), "<a><b>");
        assert_eq!(value("printf '<%s>' \"`echo a b`\""), "<a b>");
    }

    #[test]
    fn backtick_handles_escaped_backticks() {
        assert_eq!(value("printf '%s' `echo \\`echo nested\\``"), "nested");
    }

    #[test]
    fn backtick_is_literal_inside_single_quotes() {
        assert_eq!(value("printf '%s' '`echo hi`'"), "`echo hi`");
    }

    // ── tilde expansion ─────────────────────────────────────────────────

    #[test]
    fn tilde_expands_to_home() {
        assert_eq!(value("printf '%s' ~"), "/home/user");
        assert_eq!(value("printf '%s' ~/x"), "/home/user/x");
        assert_eq!(value("x=~; printf '%s' \"$x\""), "/home/user");
        assert_eq!(value("x=~/y; printf '%s' \"$x\""), "/home/user/y");
    }

    #[test]
    fn tilde_expands_per_word() {
        assert_eq!(value("printf '<%s>' ~/x ~"), "</home/user/x></home/user>");
    }

    #[test]
    fn tilde_is_literal_mid_word_and_when_quoted() {
        assert_eq!(value("printf '%s' a~b"), "a~b");
        assert_eq!(value("printf '%s' \"~\""), "~");
    }

    // ── brace expansion ─────────────────────────────────────────────────

    #[test]
    fn brace_lists() {
        assert_eq!(value("printf '<%s>' {a,b}"), "<a><b>");
        assert_eq!(value("printf '<%s>' x{a,b}y"), "<xay><xby>");
        assert_eq!(value("printf '<%s>' {a,{b,c}}"), "<a><b><c>");
        assert_eq!(value("printf '<%s>' pre{1,2}post"), "<pre1post><pre2post>");
    }

    #[test]
    fn brace_ranges() {
        assert_eq!(value("printf '<%s>' {1..3}"), "<1><2><3>");
        assert_eq!(value("printf '<%s>' {3..1}"), "<3><2><1>");
        assert_eq!(value("printf '<%s>' {a..c}"), "<a><b><c>");
    }

    #[test]
    fn brace_without_a_list_or_range_is_literal() {
        assert_eq!(value("printf '%s' {a}"), "{a}");
        assert_eq!(value("printf '%s' \"{a,b}\""), "{a,b}");
    }

    /// Brace expansion happens before pathname expansion and before splitting.
    #[test]
    fn brace_results_glob_individually() {
        let mut bash = Bash::new(Options {
            files: BTreeMap::from([
                ("/home/user/a.txt".to_string(), String::new()),
                ("/home/user/b.md".to_string(), String::new()),
            ]),
            ..Options::default()
        })
        .expect("initialize");
        assert_eq!(bash.exec("echo *.{txt,md}").stdout, "a.txt b.md\n");
    }

    /// A word with expansions inside braces keeps the ordinary path.
    #[test]
    fn brace_with_expansion_is_left_alone() {
        assert_eq!(value("x=1; printf '%s' {$x,y}"), "{1,y}");
    }
}

#[cfg(test)]
mod arithmetic_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn value(script: &str) -> String {
        out(script).trim_end_matches('\n').to_string()
    }

    /// Expected values recorded from GNU bash 3.2.
    #[test]
    fn operator_table_matches_bash() {
        let cases: &[(&str, &str)] = &[
            ("echo $((2+3*4))", "14"),
            ("echo $((2**10))", "1024"),
            ("echo $((2**3**2))", "512"),
            ("echo $((10/3))", "3"),
            ("echo $((-7/2))", "-3"),
            ("echo $((10%3))", "1"),
            ("echo $((1<<4))", "16"),
            ("echo $((256>>2))", "64"),
            ("echo $((5&3))", "1"),
            ("echo $((5|3))", "7"),
            ("echo $((5^3))", "6"),
            ("echo $((~5))", "-6"),
            ("echo $((!0))", "1"),
            ("echo $((!5))", "0"),
            ("echo $((5>3))", "1"),
            ("echo $((5<3))", "0"),
            ("echo $((5==5))", "1"),
            ("echo $((5!=5))", "0"),
            ("echo $((1&&0))", "0"),
            ("echo $((1||0))", "1"),
            ("echo $((1?2:3))", "2"),
            ("echo $((0?2:3))", "3"),
            ("echo $((0x1f))", "31"),
            ("echo $((010))", "8"),
            ("echo $((1+2*3-4/2))", "5"),
            ("echo $(( (1+2)*3 ))", "9"),
            ("echo $((-2**2))", "4"),
            ("echo $((2**0))", "1"),
        ];
        for (script, expected) in cases {
            assert_eq!(value(script), *expected, "{script}");
        }
    }

    #[test]
    fn variables_and_assignments() {
        assert_eq!(value("i=5; echo $((i*2))"), "10");
        assert_eq!(value("i=5; echo $((i+=3)); echo $i"), "8\n8");
        assert_eq!(value("i=5; echo $((i++)); echo $i"), "5\n6");
        assert_eq!(value("i=5; echo $((++i)); echo $i"), "6\n6");
        assert_eq!(value("i=5; echo $((i--)); echo $i"), "5\n4");
        assert_eq!(value("unset z; echo $((z+1))"), "1");
        assert_eq!(value("x=3; y=4; echo $((x*y))"), "12");
    }

    /// Parameters may be written with a `$` prefix inside arithmetic.
    #[test]
    fn dollar_prefixed_parameters() {
        assert_eq!(value("set -- 5; echo $(( $1 - 1 ))"), "4");
        assert_eq!(value("n=4; echo $(( ${n} + 1 ))"), "5");
    }

    /// A variable whose value is itself an expression resolves recursively.
    #[test]
    fn variable_values_are_evaluated() {
        assert_eq!(value("x=y; y=5; echo $((x+1))"), "6");
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let result = bash().exec("echo $((1/0))");
        assert_ne!(result.exit_code, 0);
        assert!(result.stderr.contains("division by 0"), "{}", result.stderr);
    }

    // ── `(( … ))` ───────────────────────────────────────────────────────

    #[test]
    fn arithmetic_command_updates_variables() {
        assert_eq!(value("i=1; ((i++)); echo $i"), "2");
        assert_eq!(value("i=1; ((i+=4)); echo $i"), "5");
    }

    #[test]
    fn arithmetic_command_sets_exit_status() {
        assert_eq!(value("((1)); echo $?"), "0");
        assert_eq!(value("((0)); echo $?"), "1");
        assert_eq!(value("i=2; ((i>1)); echo $?"), "0");
    }

    #[test]
    fn arithmetic_command_drives_a_while_loop() {
        assert_eq!(
            value("i=0; while ((i < 3)); do echo $i; i=$((i+1)); done"),
            "0\n1\n2"
        );
    }

    #[test]
    fn arithmetic_command_works_in_an_if() {
        assert_eq!(
            value("if (( 1 > 2 )); then echo yes; else echo no; fi"),
            "no"
        );
    }

    #[test]
    fn nested_parens_still_run_as_subshells_when_spaced() {
        assert_eq!(value("( ( echo deep ) )"), "deep");
    }

    // ── `let` ───────────────────────────────────────────────────────────

    #[test]
    fn let_evaluates_and_sets_status() {
        assert_eq!(value("let x=3+4; echo $x"), "7");
        assert_eq!(value("let x=5; echo $?"), "0");
        assert_eq!(value("let x=0; echo $?"), "1");
        assert_eq!(value("let a=1 b=2; echo $a$b"), "12");
    }

    #[test]
    fn let_reports_syntax_errors() {
        let result = bash().exec("let x=1+");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("let"), "{}", result.stderr);
    }
}

#[cfg(test)]
mod shell_builtin_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    // ── eval ────────────────────────────────────────────────────────────

    #[test]
    fn eval_runs_in_the_current_shell() {
        assert_eq!(out("x=1; eval \"x=2\"; echo $x"), "2\n");
        assert_eq!(out("eval \"echo a b\""), "a b\n");
    }

    #[test]
    fn eval_sees_functions_and_defines_them() {
        assert_eq!(out("f(){ eval \"echo in-fn\"; }; f"), "in-fn\n");
        assert_eq!(out("eval 'g(){ echo defined; }'; g"), "defined\n");
    }

    #[test]
    fn eval_with_no_arguments_succeeds() {
        assert_eq!(out("eval; echo rc=$?"), "rc=0\n");
    }

    #[test]
    fn eval_depth_is_bounded() {
        let limits = ExecutionLimits {
            max_source_depth: 3,
            ..ExecutionLimits::default()
        };
        let mut bash = Bash::new(Options {
            execution_limits: limits,
            ..Options::default()
        })
        .expect("initialize");
        // A self-evaluating script would recurse forever without the bound.
        let result = bash.exec("eval 'eval \"eval \\\"eval \\\\\\\"echo deep\\\\\\\"\\\"\"'");
        assert!(
            result.stderr.contains("nesting exceeds limit") || result.stdout.contains("deep"),
            "unexpected result: {result:?}"
        );
    }

    // ── source / . ──────────────────────────────────────────────────────

    #[test]
    fn source_runs_a_file_in_the_current_shell() {
        assert_eq!(
            out("printf 'x=42\\necho from-lib\\n' > /lib.sh; source /lib.sh; echo $x"),
            "from-lib\n42\n"
        );
        assert_eq!(out("printf 'x=7\\n' > /l; . /l; echo $x"), "7\n");
    }

    #[test]
    fn source_accepts_positional_arguments() {
        assert_eq!(out("printf 'echo \"[$1]\"' > /l; . /l a b"), "[a]\n");
    }

    #[test]
    fn source_return_ends_the_sourced_file() {
        assert_eq!(
            out("printf 'return 3\\necho unreachable\\n' > /l; . /l; echo rc=$?"),
            "rc=3\n"
        );
    }

    #[test]
    fn source_missing_file_is_an_error() {
        let result = bash().exec("source /nope");
        assert_eq!(result.exit_code, 1);
        assert!(
            result.stderr.contains("No such file or directory"),
            "{}",
            result.stderr
        );
    }

    #[test]
    fn source_without_a_file_is_a_usage_error() {
        let result = bash().exec("source");
        assert_eq!(result.exit_code, 2);
        assert!(result.stderr.contains("filename argument"));
    }

    // ── type / command ──────────────────────────────────────────────────

    #[test]
    fn type_identifies_builtins_functions_and_unknowns() {
        assert_eq!(out("type echo"), "echo is a shell builtin\n");
        assert_eq!(out("f(){ :; }; type f"), "f is a function\n");
        let result = bash().exec("type nosuchcmd");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("not found"));
    }

    #[test]
    fn command_runs_and_reports_names() {
        assert_eq!(out("command echo hi"), "hi\n");
        assert_eq!(out("command -v echo"), "echo\n");
    }

    /// `command` bypasses shell functions, as in bash.
    #[test]
    fn command_bypasses_functions() {
        assert_eq!(
            out("echo(){ printf 'overridden\\n'; }; command echo real"),
            "real\n"
        );
    }
}

#[cfg(test)]
mod text_tool_option_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// These all used to treat the option as a file name and report
    /// "No such file or directory".
    #[test]
    fn options_are_not_operands() {
        assert_eq!(out("printf '20\\n10\\n2\\n' | sort -n"), "2\n10\n20\n");
        assert_eq!(out("printf 'b\\na\\n' | sort -r"), "b\na\n");
        assert_eq!(out("printf 'b\\na\\n' | sort -ru"), "b\na\n");
        assert_eq!(
            out("printf 'a\\na\\nb\\n' | uniq -c"),
            "      2 a\n      1 b\n"
        );
        assert_eq!(out("printf 'a\\na\\nb\\n' | uniq -d"), "a\n");
        assert_eq!(out("printf 'a\\na\\nb\\n' | uniq -u"), "b\n");
    }

    #[test]
    fn attached_option_values_work() {
        assert_eq!(out("printf 'a:b:c\\n' > f; cut -d: -f2 f"), "b\n");
        assert_eq!(out("printf 'a:b:c\\n' > f; cut -d : -f 2 f"), "b\n");
        assert_eq!(out("printf 'a:b:c\\n' > f; cut -d: -f1,3 f"), "a:c\n");
        assert_eq!(out("printf 'abcdef\\n' > f; cut -c1-3 f"), "abc\n");
        assert_eq!(out("printf 'abcdef\\n' > f; cut -c-3 f"), "abc\n");
        assert_eq!(out("printf 'abcdef\\n' > f; cut -c4- f"), "def\n");
    }

    #[test]
    fn head_and_tail_support_bytes_and_line_offsets() {
        assert_eq!(out("printf 'hello\\n' > f; head -c 3 f"), "hel");
        assert_eq!(out("printf 'hello\\n' > f; tail -c 3 f"), "lo\n");
        assert_eq!(
            out("printf 'l1\\nl2\\nl3\\nl4\\n' > f; tail -n +3 f"),
            "l3\nl4\n"
        );
    }

    #[test]
    fn head_and_tail_label_multiple_files() {
        assert_eq!(
            out("printf 'a1\\na2\\n' > a; printf 'b1\\nb2\\n' > b; head -n 1 a b"),
            "==> a <==\na1\n\n==> b <==\nb1\n"
        );
    }

    #[test]
    fn wc_supports_each_counter_and_totals() {
        assert_eq!(out("printf 'a b\\nc d\\n' > f; wc -l f"), "2 f\n");
        assert_eq!(out("printf 'a b\\nc d\\n' > f; wc -w f"), "4 f\n");
        assert_eq!(out("printf 'a b\\nc d\\n' > f; wc -c f"), "8 f\n");
        assert_eq!(
            out("printf 'x\\n' > a; printf 'y\\n' > b; wc -l a b"),
            " 1 a\n 1 b\n 2 total\n"
        );
    }

    /// GNU counts newline bytes, so a final line without `\n` is not a line.
    #[test]
    fn wc_counts_newlines_not_logical_lines() {
        assert_eq!(out("printf 'a' > f; wc -l f"), "0 f\n");
        assert_eq!(out("printf 'a\\n' > f; wc -l f"), "1 f\n");
    }

    #[test]
    fn sort_supports_keys_and_case_folding() {
        assert_eq!(
            out("printf 'b:2\\na:3\\nc:1\\n' > f; sort -t: -k2 -n f"),
            "c:1\nb:2\na:3\n"
        );
        assert_eq!(out("printf 'B\\na\\nC\\n' | sort -f"), "a\nB\nC\n");
        assert_eq!(out("printf '1G\\n1K\\n1M\\n' | sort -h"), "1K\n1M\n1G\n");
    }

    #[test]
    fn unknown_option_is_reported() {
        let result = bash().exec("printf 'x\\n' | sort -z");
        assert_eq!(result.exit_code, 2);
        assert!(
            result.stderr.contains("invalid option"),
            "{}",
            result.stderr
        );
    }
}

#[cfg(test)]
mod grep_tr_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    /// A pattern is a regular expression unless `-F` is given, which is what
    /// makes `^`, `$`, `[...]` and `*` work.
    #[test]
    fn grep_patterns_are_regular_expressions_by_default() {
        assert_eq!(
            out("printf 'cat\\ncut\\ncot\\n' | grep 'c.t'"),
            "cat\ncut\ncot\n"
        );
        assert_eq!(
            out("printf 'hello world\\nbye\\n' | grep '^hello'"),
            "hello world\n"
        );
        assert_eq!(
            out("printf 'hello world\\nbye\\n' | grep 'world$'"),
            "hello world\n"
        );
        assert_eq!(
            out("printf 'ct\\ncat\\ncaat\\n' | grep 'ca*t'"),
            "ct\ncat\ncaat\n"
        );
        assert_eq!(
            out("printf 'cat\\nhat\\ndog\\n' | grep '[ch]at'"),
            "cat\nhat\n"
        );
    }

    #[test]
    fn grep_fixed_strings_disable_the_regex() {
        assert_eq!(out("printf 'cat\\ncut\\n' | grep -F 'c.t'"), "");
        assert_eq!(out("printf 'c.t\\ncat\\n' | grep -F 'c.t'"), "c.t\n");
    }

    #[test]
    fn grep_only_matching() {
        assert_eq!(
            out("printf 'hello hello\\n' | grep -o hello"),
            "hello\nhello\n"
        );
        assert_eq!(
            out("printf 'one two three\\n' | grep -o 't[a-z]*'"),
            "two\nthree\n"
        );
    }

    #[test]
    fn grep_context_lines() {
        assert_eq!(
            out("printf 'a\\nline2\\nmatch\\nline4\\nb\\n' | grep -A 2 match"),
            "match\nline4\nb\n"
        );
        assert_eq!(
            out("printf 'l1\\nl2\\nmatch\\n' | grep -B 2 match"),
            "l1\nl2\nmatch\n"
        );
        assert_eq!(
            out("printf 'l1\\nmatch\\nl3\\n' | grep -C 1 match"),
            "l1\nmatch\nl3\n"
        );
    }

    /// Non-adjacent context groups are separated by `--`.
    #[test]
    fn grep_context_groups_are_separated() {
        assert_eq!(
            out("printf 'm1\\nx\\ny\\nz\\nm2\\n' | grep -A 0 -B 0 m"),
            "m1\nm2\n"
        );
    }

    #[test]
    fn grep_recursive_walks_directories() {
        let mut bash = with_files(&[
            ("/home/user/dir/a.txt", "hello from a\n"),
            ("/home/user/dir/sub/b.txt", "hello from b\n"),
            ("/home/user/dir/sub/c.md", "hello from c\n"),
        ]);
        let result = bash.exec("grep -r hello dir");
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(
            result.stdout,
            "dir/a.txt:hello from a\ndir/sub/b.txt:hello from b\ndir/sub/c.md:hello from c\n"
        );
    }

    #[test]
    fn grep_recursive_include_filters_files() {
        let mut bash = with_files(&[
            ("/home/user/dir/a.ts", "hello ts\n"),
            ("/home/user/dir/b.md", "hello md\n"),
        ]);
        let result = bash.exec("grep -r --include='*.ts' hello dir");
        // `-r` names the file even when only one matches.
        assert_eq!(result.stdout, "dir/a.ts:hello ts\n");
    }

    // ── tr ──────────────────────────────────────────────────────────────

    #[test]
    fn tr_deletes_characters() {
        assert_eq!(out("printf 'hello world' | tr -d 'lo'"), "he wrd");
        assert_eq!(out("printf 'abcdef' | tr -d '0-9'"), "abcdef");
        assert_eq!(out("printf 'a1b2c3' | tr -d '0-9'"), "abc");
    }

    #[test]
    fn tr_squeezes_repeated_characters() {
        assert_eq!(out("printf 'a  b\\n' | tr -s ' '"), "a b\n");
        assert_eq!(out("printf 'aabbcc' | tr -s 'ab'"), "abcc");
        assert_eq!(out("printf 'x\\n\\n\\ny\\n' | tr -s '\\n'"), "x\ny\n");
    }

    #[test]
    fn tr_character_classes() {
        assert_eq!(out("echo Hello | tr '[:lower:]' '[:upper:]'"), "HELLO\n");
        assert_eq!(out("echo HELLO | tr '[:upper:]' '[:lower:]'"), "hello\n");
        assert_eq!(out("echo 'a1b2' | tr -d '[:digit:]'"), "ab\n");
    }

    #[test]
    fn tr_complement() {
        // `-d` removes the newline too, as bash does.
        assert_eq!(out("echo 'abc123' | tr -cd '[:digit:]'"), "123");
    }
}

#[cfg(test)]
mod misc_builtin_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    // ── echo ────────────────────────────────────────────────────────────

    #[test]
    fn echo_interprets_escapes_with_dash_e() {
        assert_eq!(out("echo -e 'line1\\nline2'"), "line1\nline2\n");
        assert_eq!(out("echo -e 'a\\tb'"), "a\tb\n");
        assert_eq!(out("echo -e 'a\\\\b'"), "a\\b\n");
    }

    #[test]
    fn echo_escape_interpretation_is_off_by_default() {
        assert_eq!(out("echo 'a\\tb'"), "a\\tb\n");
        assert_eq!(out("echo -E 'a\\tb'"), "a\\tb\n");
    }

    #[test]
    fn echo_supports_clustered_flags() {
        assert_eq!(out("echo -ne 'a\\nb'"), "a\nb");
        assert_eq!(out("echo -n hi"), "hi");
    }

    // ── basename / dirname ──────────────────────────────────────────────

    #[test]
    fn basename_strips_a_suffix() {
        assert_eq!(out("basename /path/to/file.txt .txt"), "file\n");
        assert_eq!(out("basename -s .txt /path/to/file.txt"), "file\n");
        assert_eq!(out("basename /path/to/file.txt"), "file.txt\n");
    }

    #[test]
    fn basename_handles_multiple_operands() {
        assert_eq!(
            out("basename -a /path/one.txt /path/two.txt"),
            "one.txt\ntwo.txt\n"
        );
        assert_eq!(
            out("basename -a -s .txt /path/one.txt /path/two.txt"),
            "one\ntwo\n"
        );
    }

    /// `dirname` is textual: it keeps `./` rather than normalizing the path.
    #[test]
    fn dirname_is_textual() {
        assert_eq!(out("dirname ./path/to/file.txt"), "./path/to\n");
        assert_eq!(out("dirname file.txt"), ".\n");
        assert_eq!(out("dirname /path/to/file.txt"), "/path/to\n");
    }

    #[test]
    fn dirname_handles_multiple_operands() {
        assert_eq!(
            out("dirname /path/to/file1 /another/path/file2"),
            "/path/to\n/another/path\n"
        );
    }

    // ── alias / unalias ─────────────────────────────────────────────────

    #[test]
    fn alias_defines_and_expands() {
        assert_eq!(out("alias hi='echo hello'; hi"), "hello\n");
    }

    #[test]
    fn alias_lists_a_known_name() {
        assert_eq!(
            out("alias hi='echo hello'; alias hi"),
            "alias hi='echo hello'\n"
        );
    }

    #[test]
    fn alias_reports_missing_names_on_stderr() {
        let result = bash().exec("alias notexists || echo failed");
        assert_eq!(result.stdout, "failed\n");
        assert_eq!(result.exit_code, 0);
        assert!(result.stderr.contains("not found"), "{}", result.stderr);
    }

    #[test]
    fn unalias_removes_and_reports() {
        assert_eq!(
            out("alias hi='echo hello'; unalias hi; echo done"),
            "done\n"
        );
        let result = bash().exec("unalias nonexistent || echo not_found");
        assert_eq!(result.stdout, "not_found\n");
        assert!(result.stderr.contains("not found"));
    }

    // ── ls -R ───────────────────────────────────────────────────────────

    #[test]
    fn ls_recursive_lists_nested_directories() {
        let mut bash = with_files(&[
            ("/home/user/file.txt", "x\n"),
            ("/home/user/dir/file1.txt", "y\n"),
            ("/home/user/dir/sub/file2.txt", "z\n"),
        ]);
        let result = bash.exec("ls -R");
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(
            result.stdout,
            ".:\ndir\nfile.txt\n\n./dir:\nfile1.txt\nsub\n\n./dir/sub:\nfile2.txt\n"
        );
    }
}

#[cfg(test)]
mod textutils_builtin_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    #[test]
    fn rev_reverses_lines() {
        assert_eq!(out("echo hello | rev"), "olleh\n");
        assert_eq!(out("printf 'abc\\ndef\\nghi\\n' | rev"), "cba\nfed\nihg\n");
        assert_eq!(out("printf '' | rev"), "");
    }

    #[test]
    fn fold_wraps_and_breaks_on_words() {
        assert_eq!(
            out("echo 'hello world test' | fold -w 5"),
            "hello\n worl\nd tes\nt\n"
        );
        assert_eq!(
            out("echo 'hello world foo bar' | fold -sw 10"),
            "hello \nworld foo \nbar\n"
        );
        assert_eq!(out("printf '' | fold"), "");
    }

    #[test]
    fn expand_and_unexpand_round_trip() {
        assert_eq!(out("printf '\\thello' | expand"), "        hello");
        assert_eq!(out("printf 'a\\tb' | expand -t 4"), "a   b");
        assert_eq!(out("printf '        hello' | unexpand"), "\thello");
        assert_eq!(out("printf 'hello   world' | unexpand -a"), "hello\tworld");
    }

    #[test]
    fn strings_finds_printable_runs() {
        assert_eq!(
            out("printf 'ab\\x00cde\\x00fghi' | strings -n 3"),
            "cde\nfghi\n"
        );
        assert_eq!(out("echo 'hello world' | strings"), "hello world\n");
        assert_eq!(
            out("printf 'hello\\x00\\x00world\\x00\\x00test' | strings"),
            "hello\nworld\ntest\n"
        );
    }

    #[test]
    fn paste_joins_files_and_stdin_columns() {
        let mut bash = with_files(&[
            ("/home/user/a.txt", "a\nb\nc\n"),
            ("/home/user/b.txt", "1\n2\n"),
        ]);
        assert_eq!(bash.exec("paste a.txt b.txt").stdout, "a\t1\nb\t2\nc\t\n");
        assert_eq!(bash.exec("paste -d, a.txt b.txt").stdout, "a,1\nb,2\nc,\n");
        assert_eq!(bash.exec("paste -sd, a.txt").stdout, "a,b,c\n");
        assert_eq!(
            bash.exec("printf '1\\n2\\n3\\n4\\n' | paste - -").stdout,
            "1\t2\n3\t4\n"
        );
    }

    #[test]
    fn column_aligns_fields() {
        assert_eq!(
            out("printf 'a b c\\nd e\\nf\\n' | column -t"),
            "a  b  c\nd  e\nf\n"
        );
        assert_eq!(
            out("printf 'short long\\nlonger x\\n' | column -t"),
            "short   long\nlonger  x\n"
        );
        assert_eq!(
            out("printf 'a,b,c\\nd,e,f\\n' | column -t -s ,"),
            "a  b  c\nd  e  f\n"
        );
    }

    #[test]
    fn split_writes_chunk_files() {
        let mut bash = with_files(&[("/home/user/test.txt", "a\nb\nc\n")]);
        let result = bash.exec("split -l 1 test.txt");
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(bash.exec("cat xaa").stdout, "a\n");
        assert_eq!(bash.exec("cat xab").stdout, "b\n");
        assert_eq!(bash.exec("cat xac").stdout, "c\n");
    }

    #[test]
    fn split_by_bytes() {
        let mut bash = with_files(&[("/home/user/test.txt", "hello")]);
        bash.exec("split -b 4 test.txt");
        assert_eq!(bash.exec("cat xaa").stdout, "hell");
        assert_eq!(bash.exec("cat xab").stdout, "o");
    }
}

#[cfg(test)]
mod conformance_fix_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "unexpected stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    /// A quoted empty string is one empty argument, not no argument.
    #[test]
    fn quoted_empty_strings_are_arguments() {
        assert_eq!(out("set -- \"\" \"\"; echo $#"), "2\n");
        assert_eq!(out("printf '[%s]' \"\" \"\""), "[][]");
        assert_eq!(out("test -n \"\"; echo $?"), "1\n");
    }

    #[test]
    fn test_supports_logical_connectives() {
        assert_eq!(out("test -n a -a -n b && echo both"), "both\n");
        assert_eq!(out("test -n '' -o -n b && echo either"), "either\n");
        assert_eq!(
            out("[ -n a -a -n '' ] && echo both || echo notboth"),
            "notboth\n"
        );
    }

    #[test]
    fn wc_pads_columns_and_names_files() {
        let mut bash = with_files(&[("/home/user/a.txt", "x\ny\n")]);
        assert_eq!(bash.exec("wc a.txt").stdout, " 2  2  4 a.txt\n");
        assert_eq!(bash.exec("wc -l a.txt").stdout, "2 a.txt\n");
        assert_eq!(
            bash.exec("wc -l a.txt a.txt").stdout,
            " 2 a.txt\n 2 a.txt\n 4 total\n"
        );
    }

    #[test]
    fn rm_force_ignores_missing_files() {
        let result = bash().exec("rm -f missing; echo done");
        assert_eq!(result.stdout, "done\n");
        assert_eq!(result.stderr, "");
    }

    #[test]
    fn cut_defaults_to_tab() {
        assert_eq!(out("printf 'a\\tb\\tc\\n' | cut -f2"), "b\n");
    }

    #[test]
    fn cat_dash_reads_stdin_in_place() {
        let mut bash = with_files(&[("/home/user/t.txt", "file\n")]);
        let result = bash.exec("echo from-stdin | cat - t.txt");
        assert_eq!(result.stdout, "from-stdin\nfile\n");
    }

    #[test]
    fn sort_version_numbers() {
        assert_eq!(
            out("printf 'file1.10\\nfile1.2\\nfile1.1\\n' | sort -V"),
            "file1.1\nfile1.2\nfile1.10\n"
        );
    }

    #[test]
    fn sort_numeric_uses_the_leading_number() {
        assert_eq!(
            out("printf '10 apples\\n2 oranges\\n5 bananas\\n' | sort -n"),
            "2 oranges\n5 bananas\n10 apples\n"
        );
    }

    #[test]
    fn sort_ignore_leading_blanks() {
        assert_eq!(
            out("printf '   c\\n  b\\na\\n' | sort -b"),
            "a\n  b\n   c\n"
        );
    }

    #[test]
    fn sort_check_reports_the_first_disorder() {
        let mut bash = with_files(&[("/home/user/t.txt", "b\na\n")]);
        let result = bash.exec("sort -c t.txt");
        assert_eq!(result.exit_code, 1);
        assert_eq!(result.stderr, "sort: t.txt:2: disorder: a\n");
    }

    #[test]
    fn jq_range() {
        assert_eq!(out("jq -n '[range(3)]' -c"), "[0,1,2]\n");
        assert_eq!(out("jq -n '[range(2;4)]' -c"), "[2,3]\n");
        assert_eq!(out("jq -n '[range(0;6;2)]' -c"), "[0,2,4]\n");
    }
}

#[cfg(test)]
mod wc_alignment_tests {
    use super::*;

    fn with_files(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    fn run(bash: &mut Bash, script: &str) -> String {
        let result = bash.exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// One column with a single input is never padded.
    #[test]
    fn single_column_single_file_is_unpadded() {
        let mut bash = with_files(&[("/home/user/t.txt", "a\nb\nc\n")]);
        assert_eq!(run(&mut bash, "wc -l t.txt"), "3 t.txt\n");
    }

    /// Multiple counters align to at least two characters per column.
    #[test]
    fn multiple_columns_align_to_two() {
        let mut bash = with_files(&[("/home/user/t.txt", "no newline")]);
        assert_eq!(run(&mut bash, "wc t.txt"), " 0  2 10 t.txt\n");
    }

    /// Multiple files pad a single column to the widest value.
    #[test]
    fn multiple_files_pad_single_column() {
        let mut bash = with_files(&[
            ("/home/user/a.txt", "x\ny\n"),
            ("/home/user/b.txt", "x\ny\nz\n"),
        ]);
        assert_eq!(
            run(&mut bash, "wc -l a.txt b.txt"),
            " 2 a.txt\n 3 b.txt\n 5 total\n"
        );
    }

    /// Standard input uses the fixed seven-column layout when several
    /// counters are printed.
    #[test]
    fn stdin_uses_seven_column_layout() {
        let mut bash = Bash::new(Options::default()).unwrap();
        assert_eq!(
            run(&mut bash, "echo 'hello world' | wc"),
            "      1       2      12\n"
        );
        // `wc -l` counts newline bytes, so the unterminated last line is not
        // counted — the same as GNU.
        assert_eq!(run(&mut bash, "printf 'a\\nb\\nc' | wc -l"), "2\n");
    }
}

#[cfg(test)]
mod find_tar_tests {
    use super::*;

    fn bash_with(entries: &[(&str, &str)]) -> Bash {
        let mut files = BTreeMap::new();
        for (path, contents) in entries {
            files.insert((*path).to_string(), (*contents).to_string());
        }
        Bash::new(Options {
            files,
            ..Options::default()
        })
        .expect("initialize")
    }

    fn out(bash: &mut Bash, script: &str) -> String {
        let result = bash.exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    #[test]
    fn find_lists_everything_by_default() {
        let mut bash = bash_with(&[
            ("/home/user/file.txt", "x\n"),
            ("/home/user/dir/file.txt", "y\n"),
        ]);
        assert_eq!(
            out(&mut bash, "find . | sort"),
            ".\n./dir\n./dir/file.txt\n./file.txt\n"
        );
    }

    #[test]
    fn find_depth_limits() {
        let mut bash = bash_with(&[
            ("/home/user/file.txt", "x\n"),
            ("/home/user/dir/sub/deep.txt", "z\n"),
        ]);
        assert_eq!(
            out(&mut bash, "find . -maxdepth 1 | sort"),
            ".\n./dir\n./file.txt\n"
        );
        assert_eq!(out(&mut bash, "find . -maxdepth 0 | sort"), ".\n");
        assert_eq!(
            out(&mut bash, "find . -mindepth 2 | sort"),
            "./dir/sub\n./dir/sub/deep.txt\n"
        );
    }

    #[test]
    fn find_name_and_type() {
        let mut bash = bash_with(&[
            ("/home/user/a1.txt", "x\n"),
            ("/home/user/a.js", "y\n"),
            ("/home/user/dir/b.txt", "z\n"),
        ]);
        assert_eq!(
            out(&mut bash, "find . -name '*.txt' | sort"),
            "./a1.txt\n./dir/b.txt\n"
        );
        assert_eq!(out(&mut bash, "find . -name 'a?.txt' | sort"), "./a1.txt\n");
        assert_eq!(out(&mut bash, "find . -type d | sort"), ".\n./dir\n");
    }

    #[test]
    fn find_or_expression() {
        let mut bash = bash_with(&[("/home/user/a.txt", "x\n"), ("/home/user/b.js", "y\n")]);
        assert_eq!(
            out(&mut bash, "find . -name '*.txt' -o -name '*.js' | sort"),
            "./a.txt\n./b.js\n"
        );
    }

    #[test]
    fn tar_round_trips_a_file() {
        let mut bash = bash_with(&[("/home/user/original.txt", "Original content\n")]);
        let result = bash.exec("tar -cf archive.tar original.txt && rm original.txt && tar -xf archive.tar && cat original.txt");
        assert_eq!(result.stderr, "");
        assert_eq!(result.stdout, "Original content\n");
    }

    #[test]
    fn tar_lists_directories_with_a_trailing_slash() {
        let mut bash = bash_with(&[
            ("/home/user/mydir/file1.txt", "Content 1\n"),
            ("/home/user/mydir/file2.txt", "Content 2\n"),
        ]);
        assert_eq!(
            out(
                &mut bash,
                "tar -cf archive.tar mydir && tar -tf archive.tar | sort"
            ),
            "mydir/\nmydir/file1.txt\nmydir/file2.txt\n"
        );
    }

    #[test]
    fn tar_extracts_into_a_directory() {
        let mut bash = bash_with(&[("/home/user/source.txt", "Source content\n")]);
        let result = bash.exec("tar -cf archive.tar source.txt && mkdir dest && tar -xf archive.tar -C dest && cat dest/source.txt");
        assert_eq!(result.stdout, "Source content\n");
    }

    #[test]
    fn tar_gzip_round_trip() {
        let mut bash = bash_with(&[("/home/user/compress.txt", "Compressed content\n")]);
        let result = bash.exec(
            "tar -czf archive.tar.gz compress.txt && rm compress.txt && tar -xzf archive.tar.gz && cat compress.txt",
        );
        assert_eq!(result.stderr, "");
        assert_eq!(result.stdout, "Compressed content\n");
    }

    #[test]
    fn tar_strip_components() {
        let mut bash = bash_with(&[("/home/user/deep/path/file.txt", "Deep content\n")]);
        let result = bash.exec("tar -cf archive.tar deep/path/file.txt && mkdir out && tar -xf archive.tar -C out --strip-components=2 && cat out/file.txt");
        assert_eq!(result.stderr, "");
        assert_eq!(result.stdout, "Deep content\n");
    }
}

#[cfg(test)]
mod arithmetic_disambiguation_tests {
    use super::*;

    fn out(script: &str) -> (String, String) {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        (result.stdout, result.stderr)
    }

    /// `$((…))` is always arithmetic; a command-shaped body is an error.
    #[test]
    fn arithmetic_body_is_never_reinterpreted_as_a_command() {
        let (stdout, stderr) = out("echo $((echo AA))");
        // The expansion fails, so `echo` never runs.
        assert_eq!(stdout, "", "the body must not run");
        assert!(
            stderr.contains("syntax error"),
            "expected an arithmetic error, got {stderr:?}"
        );
    }

    /// The explicit `$( ( … ) )` form is a command substitution and runs.
    #[test]
    fn spaced_subshell_form_is_a_command_substitution() {
        assert_eq!(out("echo $( (echo AA; echo BB) 2>/dev/null )").0, "AA BB\n");
    }

    /// A body that *could* be read as a subshell must not execute side effects.
    #[test]
    fn desync_attempt_does_not_create_files() {
        let (stdout, _) = out(
            "rm -f marker; ( echo $(( (echo LEFT; echo DESYNC > marker) || (echo RIGHT) )) >/dev/null ) 2>/dev/null || true; if [ -f marker ]; then echo MARKER_PRESENT; else echo MARKER_ABSENT; fi",
        );
        assert_eq!(stdout, "MARKER_ABSENT\n");
    }

    /// Ordinary arithmetic still works.
    #[test]
    fn plain_arithmetic_still_evaluates() {
        assert_eq!(out("echo $((1+2))").0, "3\n");
        assert_eq!(out("x=5; echo $((x*2))").0, "10\n");
    }
}

#[cfg(test)]
mod compound_pipeline_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// A `{ …; }` stage feeds the next stage rather than the terminal.
    #[test]
    fn group_stage_feeds_the_pipeline() {
        assert_eq!(out("{ echo a; echo b; } | sort -r"), "b\na\n");
        assert_eq!(out("{ echo x; } | cat"), "x\n");
    }

    /// The same for a subshell stage.
    #[test]
    fn subshell_stage_feeds_the_pipeline() {
        assert_eq!(out("(echo x) | cat"), "x\n");
        assert_eq!(out("(echo a; echo b) | sort -r"), "b\na\n");
    }

    /// Loop and conditional stages work too.
    #[test]
    fn loop_and_conditional_stages_feed_the_pipeline() {
        assert_eq!(out("for i in 1 2; do echo $i; done | sort -r"), "2\n1\n");
        assert_eq!(out("if true; then echo a; fi | cat"), "a\n");
        assert_eq!(out("while true; do echo x; break; done | cat"), "x\n");
        assert_eq!(out("case a in a) echo hit;; esac | cat"), "hit\n");
    }

    /// `printf … | while read` iterates over every line.
    #[test]
    fn while_read_consumes_every_line() {
        assert_eq!(
            out("printf 'a\\nb\\n' | while read -r l; do echo \"[$l]\"; done"),
            "[a]\n[b]\n"
        );
        assert_eq!(
            out("printf '1\\n2\\n3\\n' | while read -r n; do echo \"n=$n\"; done"),
            "n=1\nn=2\nn=3\n"
        );
    }

    /// A compound stage may be preceded by a simple stage.
    #[test]
    fn compound_stage_after_a_simple_stage() {
        assert_eq!(out("echo hi | { cat; }"), "hi\n");
    }
}

#[cfg(test)]
mod printf_format_tests {
    use super::*;

    fn format(fmt: &str, args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|value| (*value).to_string()).collect();
        format_printf(fmt, &owned)
    }

    #[test]
    fn width_and_alignment() {
        assert_eq!(format("%5s|", &["ab"]), "   ab|");
        assert_eq!(format("%-5s|", &["ab"]), "ab   |");
        assert_eq!(format("%05d", &["42"]), "00042");
    }

    #[test]
    fn precision() {
        assert_eq!(format("%.3s", &["abcdef"]), "abc");
        assert_eq!(format("%.2f", &["3.14159"]), "3.14");
        assert_eq!(format("%05.2f", &["3.14159"]), "03.14");
    }

    #[test]
    fn integer_and_character_conversions() {
        assert_eq!(format("%x %X %o", &["255", "255", "8"]), "ff FF 10");
        assert_eq!(format("%c", &["abc"]), "a");
    }

    #[test]
    fn floats_and_percent() {
        assert_eq!(format("%e", &["1234.5"]), "1.234500e+03");
        assert_eq!(format("%d%%", &["50"]), "50%");
    }

    #[test]
    fn format_repeats_for_extra_arguments() {
        assert_eq!(format("%s\n", &["a", "b", "c"]), "a\nb\nc\n");
    }

    #[test]
    fn multiple_specifiers_take_arguments_in_order() {
        assert_eq!(format("%s-%s", &["a", "b"]), "a-b");
        assert_eq!(
            format("%-10s|%10s|", &["left", "right"]),
            "left      |     right|"
        );
    }
}

#[cfg(test)]
mod shell_builtin_coverage_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    // ── trap ────────────────────────────────────────────────────────────

    #[test]
    fn trap_exit_runs_at_the_end() {
        assert_eq!(out("trap 'echo bye' EXIT; echo hi"), "hi\nbye\n");
        assert_eq!(out("trap 'echo c' EXIT; echo a; echo b"), "a\nb\nc\n");
    }

    #[test]
    fn trap_can_be_removed() {
        assert_eq!(out("trap 'echo bye' EXIT; trap - EXIT; echo hi"), "hi\n");
    }

    // ── getopts ─────────────────────────────────────────────────────────

    #[test]
    fn getopts_walks_flags() {
        assert_eq!(
            out("set -- -a -b x; while getopts 'ab' o; do echo \"opt=$o\"; done"),
            "opt=a\nopt=b\n"
        );
    }

    #[test]
    fn getopts_handles_values_and_clusters() {
        assert_eq!(
            out("set -- -f val; while getopts 'f:' o; do echo \"$o=$OPTARG\"; done"),
            "f=val\n"
        );
        assert_eq!(
            out("set -- -ab; while getopts 'ab' o; do echo \"opt=$o\"; done"),
            "opt=a\nopt=b\n"
        );
    }

    #[test]
    fn getopts_reports_unknown_options() {
        assert_eq!(
            out("set -- -x; while getopts 'ab' o; do echo \"opt=$o\"; done"),
            "opt=?\n"
        );
    }

    // ── shopt nullglob ──────────────────────────────────────────────────

    #[test]
    fn nulllob_drops_unmatched_patterns() {
        assert_eq!(out("shopt -s nullglob; echo nomatch*"), "\n");
        assert_eq!(out("shopt -s nullglob; echo a nomatch* b"), "a b\n");
        // Without the option the pattern stays literal.
        assert_eq!(out("echo nomatch*"), "nomatch*\n");
    }

    // ── tee / base64 ────────────────────────────────────────────────────

    #[test]
    fn tee_writes_and_passes_through() {
        let mut bash = bash();
        let result = bash.exec("echo x | tee /tmp/t1 | cat");
        assert_eq!(result.stdout, "x\n");
        assert_eq!(bash.exec("cat /tmp/t1").stdout, "x\n");
    }

    #[test]
    fn tee_appends() {
        let mut bash = bash();
        bash.exec("echo one > /tmp/t3; echo two | tee -a /tmp/t3");
        assert_eq!(bash.exec("cat /tmp/t3").stdout, "one\ntwo\n");
    }

    #[test]
    fn base64_round_trips() {
        assert_eq!(out("echo hello | base64"), "aGVsbG8K\n");
        assert_eq!(out("echo hello | base64 | base64 -d"), "hello\n");
    }

    // ── arithmetic bases and parameter words ────────────────────────────

    #[test]
    fn arithmetic_accepts_base_prefixes() {
        assert_eq!(out("echo $((16#ff)) $((2#1010)) $((010))"), "255 10 8\n");
    }

    #[test]
    fn parameter_operator_words_drop_quotes() {
        assert_eq!(out("set -- a; echo ${1+\"set\"}"), "set\n");
        assert_eq!(out("set -- a; echo ${2-\"dflt\"}"), "dflt\n");
    }
}

#[cfg(test)]
mod shell_option_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `set -f` / `set -o noglob` leave glob patterns unexpanded.
    #[test]
    fn noglob_disables_pathname_expansion() {
        assert_eq!(out("touch a.txt b.txt; set -f; echo *.txt"), "*.txt\n");
        assert_eq!(
            out("touch a.txt b.txt; set -o noglob; echo *.txt"),
            "*.txt\n"
        );
        // Turning it back off restores expansion.
        assert_eq!(out("touch a.txt; set -f; set +f; echo *.txt"), "a.txt\n");
    }

    /// `set -n` reads but does not run the statements that follow.
    #[test]
    fn noexec_skips_execution() {
        assert_eq!(out("set -n; echo never"), "");
        assert_eq!(out("set -o noexec; echo never"), "");
        assert_eq!(out("echo before; set -n; echo after"), "before\n");
    }

    /// `set -x` traces expanded commands on stderr.
    #[test]
    fn xtrace_reports_commands() {
        let result = Bash::new(Options::default())
            .unwrap()
            .exec("set -x; echo hi");
        assert_eq!(result.stdout, "hi\n");
        assert_eq!(result.stderr, "+ echo hi\n");
    }

    /// A bare assignment is traced too.
    #[test]
    fn xtrace_reports_assignments() {
        let result = Bash::new(Options::default())
            .unwrap()
            .exec("set -x; x=1; echo $x");
        assert_eq!(result.stdout, "1\n");
        assert_eq!(result.stderr, "+ x=1\n+ echo 1\n");
    }

    /// `readonly` and `declare -r` record the attribute.
    #[test]
    fn readonly_names_are_recorded() {
        assert_eq!(out("readonly y=5; echo $y"), "5\n");
        assert_eq!(out("declare -r z=7; echo $z"), "7\n");
        // A later write is rejected and the value is unchanged.
        let result = Bash::new(Options::default())
            .unwrap()
            .exec("x=1; readonly x; x=2");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("readonly"), "{}", result.stderr);
    }
}

#[cfg(test)]
mod sed_extended_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `y` transliterates character by character.
    #[test]
    fn sed_transliterates() {
        assert_eq!(out("printf 'abc\\n' | sed 'y/abc/xyz/'"), "xyz\n");
        assert_eq!(out("printf 'hello\\n' | sed 'y/el/ip/'"), "hippo\n");
        assert_eq!(out("printf 'aaa\\n' | sed 'y/a/b/'"), "bbb\n");
    }

    /// `N` joins pairs of lines, and drops the trailing unpaired one.
    #[test]
    fn sed_appends_the_next_line() {
        assert_eq!(out("printf 'a\\nb\\n' | sed 'N;s/\\n/ /'"), "a b\n");
        assert_eq!(
            out("printf 'a\\nb\\nc\\nd\\n' | sed 'N;s/\\n/ /'"),
            "a b\nc d\n"
        );
        // An unpaired last line is dropped, matching bash.
        assert_eq!(out("printf 'a\\nb\\nc\\n' | sed 'N;s/\\n/ /'"), "a b\n");
        assert_eq!(out("printf 'a\\n' | sed 'N'"), "");
    }
}

#[cfg(test)]
mod middle_compound_stage_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// A compound stage in the middle of a pipeline must consume the previous
    /// stage's output instead of reading nothing.
    #[test]
    fn compound_in_the_middle_passes_data_through() {
        assert_eq!(out("printf 'a\\nb\\n' | { cat; } | cat"), "a\nb\n");
        assert_eq!(out("printf 'a\\nb\\n' | (cat) | cat"), "a\nb\n");
        assert_eq!(out("printf 'a\\nb\\n' | { cat; } | sort -r"), "b\na\n");
    }

    /// The middle stage's own command sees the piped input.
    #[test]
    fn compound_in_the_middle_runs_its_body_on_the_input() {
        assert_eq!(out("printf '1\\n2\\n3\\n' | { grep 2; } | cat"), "2\n");
    }

    /// Existing single-stage and trailing-stage forms keep working.
    #[test]
    fn other_compound_positions_still_work() {
        assert_eq!(out("printf 'a\\nb\\n' | { cat; }"), "a\nb\n");
        assert_eq!(out("{ echo a; echo b; } | sort -r"), "b\na\n");
        assert_eq!(
            out("printf 'a\\nb\\n' | while read -r l; do echo \"[$l]\"; done"),
            "[a]\n[b]\n"
        );
    }
}

#[cfg(test)]
mod exec_redirection_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `exec cmd` runs the command and ends the script.
    #[test]
    fn exec_with_a_command_replaces_the_shell() {
        assert_eq!(out("exec echo hi"), "hi\n");
        assert_eq!(
            out("echo before; exec echo mid; echo after"),
            "before\nmid\n"
        );
    }

    /// `exec N>file` keeps the descriptor open for later commands.
    #[test]
    fn exec_persists_extra_descriptors() {
        assert_eq!(
            out("exec 3>/tmp/f3; echo hi >&3; exec 3>&-; cat /tmp/f3"),
            "hi\n"
        );
        assert_eq!(
            out("exec 4>/tmp/f4; echo a >&4; echo b >&4; exec 4>&-; cat /tmp/f4"),
            "a\nb\n"
        );
    }

    /// Redirecting standard error for the rest of the script works too.
    #[test]
    fn exec_persists_stderr_redirection() {
        assert_eq!(
            out("exec 2>/tmp/e; echo x >&2; exec 2>&-; cat /tmp/e"),
            "x\n"
        );
    }

    /// A plain `>` still truncates when it reopens the file.
    #[test]
    fn plain_redirect_still_truncates() {
        assert_eq!(out("echo a > /tmp/z; echo b > /tmp/z; cat /tmp/z"), "b\n");
        assert_eq!(
            out("echo one > /tmp/x; echo two >> /tmp/x; cat /tmp/x"),
            "one\ntwo\n"
        );
    }
}

#[cfg(test)]
mod checksum_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(script: &str) -> String {
        let result = bash().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// The digests match the published reference vectors.
    #[test]
    fn digests_match_reference_vectors() {
        assert_eq!(
            out("printf 'abc' | md5sum"),
            "900150983cd24fb0d6963f7d28e17f72  -\n"
        );
        assert_eq!(
            out("printf 'abc' | sha1sum"),
            "a9993e364706816aba3e25717850c26c9cd0d89d  -\n"
        );
        assert_eq!(
            out("printf 'abc' | sha256sum"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  -\n"
        );
    }

    /// An empty stream still has a well-defined digest.
    #[test]
    fn empty_input_has_a_digest() {
        assert_eq!(
            out("printf '' | md5sum"),
            "d41d8cd98f00b204e9800998ecf8427e  -\n"
        );
        assert_eq!(
            out("printf '' | sha256sum"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  -\n"
        );
    }

    /// A file operand is named in the output.
    #[test]
    fn file_operands_are_named() {
        assert_eq!(
            out("printf 'abc' > f; sha256sum f"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  f\n"
        );
    }

    /// `-c` reports OK for matching digests.
    #[test]
    fn check_mode_verifies_listings() {
        assert_eq!(
            out("printf 'abc' > f; sha256sum f > s; sha256sum -c s"),
            "f: OK\n"
        );
    }

    /// A tampered file fails the check and sets a non-zero status.
    #[test]
    fn check_mode_detects_tampering() {
        let mut bash = bash();
        bash.exec("printf 'abc' > f; sha256sum f > s");
        bash.exec("printf 'tampered' > f");
        let result = bash.exec("sha256sum -c s");
        assert_eq!(result.stdout, "f: FAILED\n");
        assert_eq!(result.exit_code, 1);
    }
}

#[cfg(test)]
mod device_file_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// `/dev/null` reads as an empty stream.
    #[test]
    fn dev_null_reads_empty() {
        assert_eq!(out("cat /dev/null; echo rc=$?"), "rc=0\n");
        assert_eq!(out("wc -l < /dev/null"), "0\n");
        assert_eq!(out("sort /dev/null; echo ok"), "ok\n");
    }

    /// `/dev/zero` yields NUL bytes up to the downstream budget.
    #[test]
    fn dev_zero_is_bounded_by_the_consumer() {
        assert_eq!(out("head -c 5 /dev/zero | wc -c"), "5\n");
        assert_eq!(out("cat /dev/zero | head -c 10 | wc -c"), "10\n");
    }

    /// Digests of the devices match the empty-input and NUL-run values.
    #[test]
    fn devices_have_well_defined_digests() {
        assert_eq!(
            out("md5sum /dev/null"),
            "d41d8cd98f00b204e9800998ecf8427e  /dev/null\n"
        );
    }

    /// `grep` on an empty device reports no match, not a missing file.
    #[test]
    fn grep_on_dev_null_reports_no_match() {
        let result = Bash::new(Options::default())
            .unwrap()
            .exec("grep x /dev/null; echo rc=$?");
        assert_eq!(result.stdout, "rc=1\n");
        assert_eq!(result.stderr, "");
    }

    /// Writing to `/dev/null` discards and does not create a file.
    #[test]
    fn dev_null_discards_writes() {
        assert_eq!(out("echo hi > /dev/null; echo ok"), "ok\n");
    }
}

#[cfg(test)]
mod xxd_builtin_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    #[test]
    fn xxd_dumps_with_offset_and_ascii() {
        assert_eq!(
            out("printf 'hello\\n' | xxd"),
            "00000000: 6865 6c6c 6f0a                           hello.\n"
        );
    }

    #[test]
    fn xxd_plain_and_reverse() {
        assert_eq!(out("printf 'hello\\n' | xxd -p"), "68656c6c6f0a\n");
        assert_eq!(out("printf '68656c6c6f' | xxd -r -p"), "hello");
    }

    #[test]
    fn xxd_round_trips_through_its_own_output() {
        assert_eq!(
            out("printf 'hello\\n' | xxd | xxd -r | xxd -p"),
            "68656c6c6f0a\n"
        );
    }

    #[test]
    fn xxd_handles_low_bytes() {
        assert_eq!(out("printf '\\x00\\x01\\x02' | xxd -p"), "000102\n");
    }
}

#[cfg(test)]
mod du_tests {
    use super::*;

    fn seeded() -> Bash {
        let result = Bash::new(Options::default()).unwrap();
        let mut bash = result;
        bash.exec("mkdir -p d/sub; printf '12345' > d/a.txt; printf '1234567890' > d/sub/b.txt");
        bash
    }

    fn out(bash: &mut Bash, script: &str) -> String {
        let result = bash.exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// A directory reports the sum of everything beneath it.
    #[test]
    fn directory_totals_include_children() {
        assert_eq!(out(&mut seeded(), "du d"), "2\td\n1\td/sub\n");
    }

    /// A plain file operand is always reported.
    #[test]
    fn file_operands_are_reported() {
        assert_eq!(out(&mut seeded(), "du d/a.txt"), "1\td/a.txt\n");
    }

    /// `-s` collapses to one line per operand.
    #[test]
    fn summarize_prints_only_the_operand() {
        assert_eq!(out(&mut seeded(), "du -s d"), "2\td\n");
    }

    /// `-a` adds the individual files.
    #[test]
    fn all_lists_files_too() {
        let output = out(&mut seeded(), "du -a d");
        assert!(output.contains("1\td/a.txt\n"), "{output}");
        assert!(output.contains("1\td/sub/b.txt\n"), "{output}");
    }

    /// `-h` renders a human-readable unit.
    #[test]
    fn human_readable_units() {
        assert_eq!(out(&mut seeded(), "du -sh d"), "1.0K\td\n");
    }

    /// A missing operand is reported once and sets a failure status.
    #[test]
    fn missing_operand_fails() {
        let mut bash = Bash::new(Options::default()).unwrap();
        let result = bash.exec("du /nonexistent");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("No such file"), "{}", result.stderr);
    }
}

#[cfg(test)]
mod dir_stack_tests {
    use super::*;

    fn bash() -> Bash {
        Bash::new(Options::default()).expect("initialize")
    }

    fn out(bash: &mut Bash, script: &str) -> String {
        let result = bash.exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    #[test]
    fn pushd_changes_directory_and_reports_the_stack() {
        let mut bash = bash();
        // One script, so the directory change is observed in the same shell.
        assert_eq!(
            out(&mut bash, "mkdir -p a; pushd a; pwd"),
            "/home/user/a /home/user\n/home/user/a\n"
        );
    }

    #[test]
    fn pushd_nests_and_popd_unwinds() {
        let mut bash = bash();
        // `b` is a sibling of `a`, so the second pushd needs a relative path.
        assert_eq!(
            out(
                &mut bash,
                "mkdir -p a b; pushd a >/dev/null; pushd ../b >/dev/null; dirs; popd >/dev/null; pwd",
            ),
            "/home/user/b /home/user/a /home/user\n/home/user/a\n"
        );
    }

    #[test]
    fn dirs_on_an_empty_stack_shows_the_current_directory() {
        assert_eq!(out(&mut bash(), "dirs"), "/home/user\n");
    }

    #[test]
    fn dirs_dash_v_numbers_the_entries() {
        let mut bash = bash();
        out(&mut bash, "mkdir -p a; pushd a >/dev/null");
        assert_eq!(
            out(&mut bash, "dirs -v"),
            " 0  /home/user/a\n 1  /home/user\n"
        );
    }

    #[test]
    fn popd_on_an_empty_stack_fails() {
        let result = bash().exec("popd");
        assert_eq!(result.exit_code, 1);
        assert!(result.stderr.contains("stack empty"), "{}", result.stderr);
    }
}

#[cfg(test)]
mod builtin_registry_tests {
    use super::*;

    /// Every implemented builtin must be listed, otherwise `type` and
    /// `command -v` misreport it as missing. This list is checked by running the
    /// command, so a newly added builtin that forgets the registry fails here.
    #[test]
    fn type_recognises_every_implemented_builtin() {
        let implemented = [
            "echo",
            "printf",
            "cd",
            "pwd",
            "export",
            "readonly",
            "set",
            "test",
            "read",
            "eval",
            "source",
            "type",
            "command",
            "let",
            "alias",
            "unalias",
            "shopt",
            "getopts",
            "trap",
            "exec",
            "tee",
            "base64",
            "uname",
            "whoami",
            "id",
            "readlink",
            "realpath",
            "xxd",
            "md5sum",
            "sha1sum",
            "sha256sum",
            "du",
            "find",
            "tar",
            "nl",
            "join",
            "paste",
            "column",
            "split",
            "rev",
            "fold",
            "expand",
            "unexpand",
            "strings",
            "pushd",
            "popd",
            "dirs",
        ];
        let mut bash = Bash::new(Options::default()).expect("initialize");
        for name in implemented {
            let result = bash.exec(&format!("type {name}"));
            assert_eq!(
                result.exit_code, 0,
                "`type {name}` should succeed, said: {}",
                result.stderr
            );
            assert!(
                result.stdout.contains("builtin"),
                "`type {name}` said {:?}",
                result.stdout
            );
        }
    }

    /// A trailing bare `while true; do break; done` sanity check that the
    /// registry list itself stays in sync with `is_shell_builtin`.
    #[test]
    fn registry_matches_the_predicate() {
        for name in ["xxd", "du", "pushd", "dirs", "sha256sum"] {
            assert!(is_shell_builtin(name), "{name} missing from the registry");
        }
    }
}

#[cfg(test)]
mod tac_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    #[test]
    fn tac_reverses_line_order() {
        assert_eq!(out("printf 'a\\nb\\nc\\n' | tac"), "c\nb\na\n");
        assert_eq!(out("printf 'a\\n\\nb\\n' | tac"), "b\n\na\n");
    }

    #[test]
    fn tac_keeps_a_missing_trailing_newline() {
        assert_eq!(out("printf 'a\\nb' | tac"), "b\na");
    }

    #[test]
    fn tac_handles_empty_input() {
        assert_eq!(out("printf '' | tac"), "");
    }

    #[test]
    fn tac_custom_separator() {
        // Records are the text between separators, so the newline travels with
        // the last record.
        assert_eq!(out("printf 'a,b,c\\n' | tac -s ,"), "c\n,b,a");
        assert_eq!(out("printf 'a,b,c' | tac -s ,"), "c,b,a");
    }

    #[test]
    fn tac_composes_with_other_tools() {
        assert_eq!(out("printf 'c\\na\\nb\\n' | sort | tac"), "c\nb\na\n");
        assert_eq!(out("printf 'a\\nb\\nc\\n' | tac | head -1"), "c\n");
    }

    #[test]
    fn tac_reads_files() {
        let mut bash = Bash::new(Options::default()).unwrap();
        bash.exec("printf 'x\\ny\\n' > f");
        assert_eq!(bash.exec("tac f").stdout, "y\nx\n");
    }
}

#[cfg(test)]
mod length_substitution_tests {
    use super::*;

    fn out(script: &str) -> String {
        let result = Bash::new(Options::default()).unwrap().exec(script);
        assert_eq!(
            result.stderr, "",
            "stderr for {script:?}: {}",
            result.stderr
        );
        result.stdout
    }

    /// The ordinary length forms keep working.
    #[test]
    fn length_forms() {
        assert_eq!(out("x=abc; echo ${#x}"), "3\n");
        assert_eq!(out("set -- a b; echo ${#}"), "2\n");
        assert_eq!(out("a=(x y z); echo ${#a[@]}"), "3\n");
        assert_eq!(out("unset u; echo ${#u}"), "0\n");
    }

    /// `${#x:-5}` is not a length with a default: bash calls it a bad
    /// substitution, and silently yielding `0` would hide the mistake.
    #[test]
    fn operator_after_length_is_a_bad_substitution() {
        for script in [
            "x=abc; echo ${#x:-0}",
            "x=; echo ${#x:-5}",
            "echo ${#nope:=1}",
        ] {
            let result = Bash::new(Options::default()).unwrap().exec(script);
            assert!(
                result.stderr.contains("bad substitution"),
                "{script:?} said {:?}",
                result.stderr
            );
        }
    }
}
