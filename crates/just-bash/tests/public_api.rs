//! Public API contract for the `just-bash` facade crate.
//!
//! After the split into sibling crates under `crates/`, the facade re-exports
//! each implementation crate as a module and lifts the primary types into the
//! crate root. Both spellings existed before the split, so both must keep
//! resolving, and each pair must name the *same* type.
//!
//! These tests exercise the public surface only. They are the regression guard
//! for embedders who depend on `just_bash::shell::Bash`, `just_bash::Bash`, or
//! the `just-bash-rs` binary.

use std::collections::BTreeMap;

/// Each module path and its crate-root re-export must denote one type.
///
/// A function that takes one spelling and returns the other only compiles
/// while they are the same type. If a re-export were swapped for a distinct
/// wrapper type, values could no longer cross the boundary — which is exactly
/// the breakage this guards against.
#[test]
fn module_paths_and_root_reexports_name_the_same_type() {
    fn same_bash(value: just_bash::shell::Bash) -> just_bash::Bash {
        value
    }
    fn same_options(value: just_bash::shell::Options) -> just_bash::Options {
        value
    }
    fn same_exec_result(value: just_bash::shell::ExecResult) -> just_bash::ExecResult {
        value
    }
    fn same_limits(value: just_bash::shell::ExecutionLimits) -> just_bash::ExecutionLimits {
        value
    }
    fn same_fs(value: just_bash::fs::InMemoryFs) -> just_bash::InMemoryFs {
        value
    }
    fn same_error(value: just_bash::fs::Error) -> just_bash::Error {
        value
    }
    fn same_metadata(value: just_bash::fs::Metadata) -> just_bash::Metadata {
        value
    }
    fn same_entry_kind(value: just_bash::fs::EntryKind) -> just_bash::EntryKind {
        value
    }
    fn same_regex(value: just_bash::regex::Regex) -> just_bash::Regex {
        value
    }
    fn same_json(value: just_bash::jq::Json) -> just_bash::Json {
        value
    }
    fn same_jq_run(value: just_bash::jq::JqRun) -> just_bash::JqRun {
        value
    }
    fn same_awk_run(value: just_bash::awk::AwkRun) -> just_bash::AwkRun {
        value
    }
    fn same_http_request(value: just_bash::net::HttpRequest) -> just_bash::HttpRequest {
        value
    }
    fn same_http_response(value: just_bash::net::HttpResponse) -> just_bash::HttpResponse {
        value
    }
    fn same_curl_error(value: just_bash::net::CurlError) -> just_bash::CurlError {
        value
    }
    fn same_script(value: just_bash::parser::Script) -> just_bash::Script {
        value
    }
    fn same_parse_limits(value: just_bash::parser::ParseLimits) -> just_bash::ParseLimits {
        value
    }

    // shell
    let mut bash = same_bash(just_bash::Bash::new(just_bash::Options::default()).unwrap());
    assert!(bash.fs().is_dir("/"));
    assert!(same_options(just_bash::Options::default()).isolate_exec);
    assert_eq!(
        same_limits(just_bash::ExecutionLimits::default()).max_script_size_bytes,
        1_048_576
    );
    let result = same_exec_result(bash.exec("true"));
    assert_eq!(result.exit_code, 0);

    // fs
    let mut fs = same_fs(just_bash::InMemoryFs::new());
    fs.write_file("/a.txt", "hi").expect("write");
    assert_eq!(
        same_metadata(fs.metadata("/a.txt").expect("metadata")).size,
        2
    );
    assert!(matches!(
        same_entry_kind(just_bash::EntryKind::File),
        just_bash::EntryKind::File
    ));
    assert_eq!(
        same_error(just_bash::Error::Parse("x".to_string())).to_string(),
        "x"
    );

    // regex + jq
    assert!(same_regex(just_bash::Regex::new("a+").unwrap()).is_match("aaa"));
    assert_eq!(same_json(just_bash::Json::Null), just_bash::Json::Null);

    // awk + jq result structs
    let jq_run = same_jq_run(just_bash::JqRun {
        stdout: "1\n".to_string(),
        stderr: String::new(),
        exit_code: 0,
    });
    assert_eq!(jq_run.stdout, "1\n");
    let awk_run = same_awk_run(just_bash::AwkRun {
        stdout: "2\n".to_string(),
        stderr: String::new(),
        exit_code: 0,
    });
    assert_eq!(awk_run.exit_code, 0);

    // net
    let request = same_http_request(just_bash::HttpRequest {
        method: "GET".to_string(),
        url: "http://example.test/".to_string(),
        headers: Vec::new(),
        body: None,
    });
    let response = same_http_response(just_bash::HttpResponse {
        status: 200,
        reason: "OK".to_string(),
        headers: Vec::new(),
        body: "body".to_string(),
    });
    assert_eq!(response.body, "body");
    assert!(matches!(
        same_curl_error(just_bash::CurlError::NetworkDisabled),
        just_bash::CurlError::NetworkDisabled
    ));

    // parser
    let script = same_script(just_bash::parse_script("echo hi").unwrap());
    assert_eq!(script.statements.len(), 1);
    assert_eq!(
        same_parse_limits(just_bash::ParseLimits::default()).max_command_substitution_depth,
        just_bash::parser::ParseLimits::default().max_command_substitution_depth
    );
    // The `HttpRequest` built above must reach `perform` unchanged.
    assert!(just_bash::net::perform(&request, &BTreeMap::new(), false).is_err());
}

/// The parser entry points must stay callable through both spellings, and the
/// re-exported AST types stay nameable via the root.
#[test]
fn parser_entry_points_resolve_through_both_paths() {
    let via_module = just_bash::parser::parse_script("a; b").unwrap();
    let via_root = just_bash::parse_script("a; b").unwrap();
    assert_eq!(via_module.statements.len(), via_root.statements.len());
    assert_eq!(via_root.statements.len(), 2);

    let limited = just_bash::parse_script_with_limits("c", just_bash::ParseLimits::default());
    assert_eq!(limited.unwrap().statements.len(), 1);

    // `parse_condition` takes the re-exported `Word` type; a unary operator
    // needs its operand to form a complete `[[ … ]]` expression.
    let words = vec![
        just_bash::Word::literal("-n"),
        just_bash::Word::literal("value"),
    ];
    let condition = just_bash::parse_condition(&words).unwrap();
    assert!(matches!(condition, just_bash::CondExpr::Term(_)));

    // Every parser item lifted into the root remains nameable.
    let _: Option<just_bash::ArrayAssign> = None;
    let _: Option<just_bash::CaseClause> = None;
    let _: Option<just_bash::CaseStatement> = None;
    let _: Option<just_bash::CommandInvocation> = None;
    let _: Option<just_bash::CondExpr> = None;
    let _: Option<just_bash::CondTerm> = None;
    let _: Option<just_bash::ForStatement> = None;
    let _: Option<just_bash::FunctionDef> = None;
    let _: Option<just_bash::IfStatement> = None;
    let _: Option<just_bash::PipelineConnector> = None;
    let _: Option<just_bash::Redirect> = None;
    let _: Option<just_bash::RedirectMode> = None;
    let _: Option<just_bash::Statement> = None;
    let _: Option<just_bash::StatementKind> = None;
    let _: Option<just_bash::WhileStatement> = None;
    let _: Option<just_bash::Word> = None;
    let _: Option<just_bash::WordPart> = None;
    let _: Option<just_bash::WordPartKind> = None;
}

/// The glob helpers keep both their module path and their root alias.
#[test]
fn glob_helpers_resolve_through_both_paths() {
    // `glob_expand` must keep the signature of `glob::expand`: binding both to
    // the same function-pointer type only compiles while that holds. Comparing
    // the pointers by address is unreliable, so assert the behavior instead.
    let alias: fn(&just_bash::InMemoryFs, &str, &str) -> Vec<String> = just_bash::glob_expand;
    let original: fn(&just_bash::InMemoryFs, &str, &str) -> Vec<String> = just_bash::glob::expand;

    let fs = just_bash::InMemoryFs::new();
    assert_eq!(
        alias(&fs, "*.txt", "/"),
        original(&fs, "*.txt", "/"),
        "`glob_expand` must behave exactly like `glob::expand`"
    );
    assert_eq!(
        just_bash::glob_expand(&fs, "*.txt", "/"),
        Vec::<String>::new()
    );
    assert!(just_bash::glob::pattern_matches("a*", "abc"));
    assert!(just_bash::glob::component_matches("a*", "abc"));
    assert!(just_bash::glob::has_meta("a*"));
    assert!(!just_bash::glob::has_meta("abc"));
}

/// The `awk` record cap is exported under its historical alias.
#[test]
fn awk_record_cap_is_exported_under_both_names() {
    assert_eq!(just_bash::AWK_MAX_RECORDS, just_bash::awk::MAX_RECORDS);
    assert_eq!(just_bash::AWK_MAX_RECORDS, 100_000);
}

/// Every implementation module stays reachable by path, including the `sed`
/// types that are deliberately module-only.
#[test]
fn implementation_modules_stay_reachable() {
    // fs constants, via module and root.
    assert_eq!(
        just_bash::DEFAULT_FILE_MODE,
        just_bash::fs::DEFAULT_FILE_MODE
    );
    assert_eq!(just_bash::DEFAULT_DIR_MODE, just_bash::fs::DEFAULT_DIR_MODE);
    assert_eq!(just_bash::fs::EPOCH_SECONDS, 1_700_000_000);

    let metadata = just_bash::fs::Metadata {
        kind: just_bash::fs::EntryKind::Directory,
        size: 0,
        mode: just_bash::DEFAULT_DIR_MODE,
        mtime: just_bash::fs::EPOCH_SECONDS,
    };
    assert!(metadata.is_dir());
    assert_eq!(metadata.type_char(), 'd');

    // `sed` is module-only by design; the paths must still resolve.
    let _: Option<just_bash::sed::SedRun> = None;
    let _: Option<just_bash::sed::SedAddress> = None;
    let _: Option<just_bash::sed::SedCommand> = None;
    let _: Option<just_bash::sed::SedOutcome> = None;
    assert!(just_bash::sed::parse_script("s/a/b/").is_ok());

    // `regex` exposes its budget constant through the module path.
    assert_eq!(just_bash::regex::DEFAULT_STEP_BUDGET, 100_000);
}

/// `curl` stays fail-closed by default, and `net` resolves through the facade.
#[test]
fn net_module_resolves_through_the_facade() {
    let args = just_bash::net::parse_curl_args(&["http://example.test/".to_string()]).unwrap();
    assert_eq!(args.url, "http://example.test/");
    assert_eq!(just_bash::net::effective_method(&args), "GET");
    assert_eq!(just_bash::net::urlencode_form("a b"), "a%20b");

    let request = just_bash::HttpRequest {
        method: "GET".to_string(),
        url: "http://example.test/".to_string(),
        headers: Vec::new(),
        body: None,
    };
    // No mock entry and network disabled: the default path must fail closed.
    let error = just_bash::net::perform(&request, &BTreeMap::new(), false).unwrap_err();
    assert!(matches!(error, just_bash::CurlError::NetworkDisabled));

    // An injected response needs no I/O.
    let mut mock = BTreeMap::new();
    mock.insert("http://example.test/".to_string(), "mocked".to_string());
    let response = just_bash::net::perform(&request, &mock, false).unwrap();
    assert_eq!(response.body, "mocked");
}

/// Running a snippet through either spelling yields identical results, and
/// `Bash::fs` hands back the facade's filesystem type.
#[test]
fn shell_execution_is_identical_through_both_paths() {
    let script = "echo hello; printf '%s\\n' a b | wc -l";

    let mut via_root = just_bash::Bash::new(just_bash::Options::default()).unwrap();
    let root_result = via_root.exec(script);

    let mut via_module = just_bash::shell::Bash::new(just_bash::shell::Options::default()).unwrap();
    let module_result = via_module.exec(script);

    assert_eq!(root_result.stdout, module_result.stdout);
    assert_eq!(root_result.stderr, module_result.stderr);
    assert_eq!(root_result.exit_code, module_result.exit_code);
    assert_eq!(root_result.stdout, "hello\n2\n");

    // Filesystem changes are observable through the facade type. The default
    // working directory is `/home/user`.
    let mut bash = just_bash::Bash::new(just_bash::Options::default()).unwrap();
    bash.exec("mkdir -p work && echo x > work/f.txt");
    assert!(bash.fs().is_dir("/home/user/work"));
    assert!(bash.fs().is_file("/home/user/work/f.txt"));
    assert_eq!(bash.fs().read_file("/home/user/work/f.txt").unwrap(), "x\n");
}

/// The `just-bash-rs` binary target keeps its name and both input modes.
#[test]
fn cli_binary_accepts_argv_and_stdin() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let from_argv = Command::new(env!("CARGO_BIN_EXE_just-bash-rs"))
        .arg("echo from-argv")
        .output()
        .expect("run just-bash-rs");
    assert!(from_argv.status.success());
    assert_eq!(String::from_utf8_lossy(&from_argv.stdout), "from-argv\n");

    let mut child = Command::new(env!("CARGO_BIN_EXE_just-bash-rs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn just-bash-rs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"echo from-stdin")
        .expect("write stdin");
    let from_stdin = child.wait_with_output().expect("wait for just-bash-rs");
    assert!(from_stdin.status.success());
    assert_eq!(String::from_utf8_lossy(&from_stdin.stdout), "from-stdin\n");
}
