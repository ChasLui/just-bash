use std::io::{self, Read, Write};

use just_bash::{Bash, Options};

/// Write `text` to `handle`, treating a closed reader as success.
///
/// `just-bash-rs … | head` is an ordinary thing to do: the downstream process
/// exits after a few lines and the pipe closes, which makes `print!` panic. A
/// shell must simply stop writing instead.
fn write_ignoring_closed<R: Write>(mut handle: R, text: &str) {
    let _ = handle.write_all(text.as_bytes());
    let _ = handle.flush();
}

fn main() {
    let mut script = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if script.is_empty() && io::stdin().read_to_string(&mut script).is_err() {
        eprintln!("just-bash: cannot read the script from standard input");
        std::process::exit(2);
    }

    let mut bash = match Bash::new(Options::default()) {
        Ok(bash) => bash,
        Err(error) => {
            eprintln!("just-bash: cannot initialize: {error}");
            std::process::exit(2);
        }
    };
    let result = bash.exec(&script);
    write_ignoring_closed(io::stdout(), &result.stdout);
    write_ignoring_closed(io::stderr(), &result.stderr);
    std::process::exit(result.exit_code);
}
