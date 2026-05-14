pub mod fs;
pub mod parser;
pub mod shell;

pub use fs::{BashError, InMemoryFs};
pub use parser::{
    CommandInvocation, Pipeline, PipelineConnector, Redirect, RedirectMode, Script, Word, WordPart,
    parse_script,
};
pub use shell::{Bash, BashExecResult, BashOptions};
