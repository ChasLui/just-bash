pub mod fs;
pub mod glob;
pub mod net;
pub mod parser;
pub mod regex;
pub mod sed;
pub mod shell;

pub use fs::{DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, EntryKind, Error, InMemoryFs, Metadata};
pub use glob::{component_matches, expand as glob_expand, pattern_matches};
pub use net::{CurlError, HttpRequest, HttpResponse};
pub use parser::{
    ArrayAssign, CaseClause, CaseStatement, CommandInvocation, CondExpr, CondTerm, ForStatement,
    FunctionDef, IfStatement, ParseLimits, PipelineConnector, Redirect, RedirectMode, Script,
    Statement, StatementKind, WhileStatement, Word, WordPart, WordPartKind, parse_condition,
    parse_script, parse_script_with_limits,
};
pub use regex::Regex;
pub use shell::{Bash, ExecResult, ExecutionLimits, Options};
