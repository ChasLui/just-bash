pub mod fs;
pub mod parser;
pub mod shell;

pub use fs::{BashError, InMemoryFs};
pub use parser::{
    CommandInvocation, ForStatement, IfStatement, ParseLimits, PipelineConnector, Redirect,
    RedirectMode, Script, Statement, StatementKind, WhileStatement, Word, WordPart, WordPartKind,
    parse_script, parse_script_with_limits,
};
pub use shell::{Bash, BashExecResult, BashExecutionLimits, BashOptions};
