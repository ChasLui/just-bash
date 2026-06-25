use std::io::{self, Read};

use just_bash::{Bash, Options};

fn main() {
    let mut script = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if script.is_empty() {
        io::stdin().read_to_string(&mut script).expect("read stdin");
    }

    let mut bash = Bash::new(Options::default()).expect("initialize just-bash");
    let result = bash.exec(&script);
    print!("{}", result.stdout);
    eprint!("{}", result.stderr);
    std::process::exit(result.exit_code);
}
