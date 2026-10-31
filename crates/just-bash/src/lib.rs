//! Facade crate for the `just-bash` virtual shell runtime.
//!
//! The implementation is split across focused crates under `crates/`. This
//! crate re-exports each of them as a module (so existing `just_bash::shell`
//! paths keep working) and lifts the primary types into the crate root.
//!
//! | Module | Implementation crate |
//! | --- | --- |
//! | [`awk`] | `just-bash-awk` |
//! | [`fs`] | `just-bash-fs` |
//! | [`glob`] | `just-bash-glob` |
//! | [`jq`] | `just-bash-jq` |
//! | [`net`] | `just-bash-net` |
//! | [`parser`] | `just-bash-parser` |
//! | [`regex`] | `just-bash-regex` |
//! | [`sed`] | `just-bash-sed` |
//! | [`shell`] | `just-bash-shell` |

pub use just_bash_awk as awk;
pub use just_bash_checksum as checksum;
pub use just_bash_fs as fs;
pub use just_bash_glob as glob;
pub use just_bash_jq as jq;
pub use just_bash_net as net;
pub use just_bash_parser as parser;
pub use just_bash_regex as regex;
pub use just_bash_sed as sed;
pub use just_bash_shell as shell;
pub use just_bash_tar as tar;

pub use just_bash_awk::{AwkRun, MAX_RECORDS as AWK_MAX_RECORDS};
pub use just_bash_fs::{
    DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, EntryKind, Error, InMemoryFs, Metadata,
};
pub use just_bash_glob::{component_matches, expand as glob_expand, pattern_matches};
pub use just_bash_jq::{JqRun, Json};
pub use just_bash_net::{CurlError, HttpRequest, HttpResponse};
pub use just_bash_parser::{
    ArrayAssign, CaseClause, CaseStatement, CommandInvocation, CondExpr, CondTerm, ForStatement,
    FunctionDef, IfStatement, ParseLimits, PipelineConnector, Redirect, RedirectMode, Script,
    Statement, StatementKind, WhileStatement, Word, WordPart, WordPartKind, parse_condition,
    parse_script, parse_script_with_limits,
};
pub use just_bash_regex::Regex;
pub use just_bash_shell::{Bash, ExecResult, ExecutionLimits, Options};
