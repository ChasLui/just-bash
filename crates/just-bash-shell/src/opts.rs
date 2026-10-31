//! A small `getopt`-style option parser shared by the text builtins.
//!
//! GNU utilities accept three spellings for an option that takes a value —
//! `-d X`, `-dX` and `--delimiter=X` — plus clustered flags such as `-rn`.
//! Hand-written loops got this wrong in a way that was actively misleading:
//! `sort -n` tried to open a file called `-n`, and `cut -d: -f2` printed a
//! usage error. This module handles the spellings once so every caller agrees.

/// One parsed option: either a flag or a flag with a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    /// Short letter, or the full long name for `--options`.
    pub name: String,
    /// `None` for a flag, `Some(value)` for an option that takes one.
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    pub opts: Vec<Opt>,
    pub operands: Vec<String>,
}

impl Parsed {
    /// Whether a flag was given, in any spelling.
    pub fn has(&self, name: &str) -> bool {
        self.opts.iter().any(|opt| opt.name == name)
    }

    /// The last value given for `name`, if any.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.opts
            .iter()
            .rev()
            .find(|opt| opt.name == name)
            .and_then(|opt| opt.value.as_deref())
    }
}

/// How an option consumes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    /// No value.
    Flag,
    /// Takes a value: `-d X`, `-dX`, or `--delimiter=X`.
    Value,
}

/// Parse `args` given a table of `(short, long, arity)`.
///
/// An unknown option is an error, reported with `program` so the message reads
/// like GNU's (`sort: invalid option -- 'z'`). A `-` operand is kept as an
/// operand (it means standard input for most of these tools).
pub fn parse(
    program: &str,
    args: &[String],
    table: &[(char, &str, Arity)],
) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut index = 0usize;
    let mut options_done = false;

    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;

        if options_done {
            parsed.operands.push(arg.to_string());
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        if arg == "-" || !arg.starts_with('-') {
            parsed.operands.push(arg.to_string());
            continue;
        }

        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let Some((_, _, arity)) = table.iter().find(|(_, long_name, _)| *long_name == name)
            else {
                return Err(format!("{program}: unrecognized option '--{name}'"));
            };
            match (arity, inline) {
                (Arity::Flag, None) => parsed.opts.push(Opt {
                    name: name.to_string(),
                    value: None,
                }),
                (Arity::Flag, Some(_)) => {
                    return Err(format!(
                        "{program}: option '--{name}' doesn't allow a value"
                    ));
                }
                (Arity::Value, Some(value)) => parsed.opts.push(Opt {
                    name: name.to_string(),
                    value: Some(value),
                }),
                (Arity::Value, None) => {
                    let Some(value) = args.get(index) else {
                        return Err(format!("{program}: option '--{name}' requires a value"));
                    };
                    index += 1;
                    parsed.opts.push(Opt {
                        name: name.to_string(),
                        value: Some(value.clone()),
                    });
                }
            }
            continue;
        }

        // A short cluster: `-rn`, `-d:`, `-f2`.
        let cluster: Vec<char> = arg[1..].chars().collect();
        let mut position = 0usize;
        while position < cluster.len() {
            let letter = cluster[position];
            position += 1;
            let Some((_, long_name, arity)) = table.iter().find(|(short, _, _)| *short == letter)
            else {
                return Err(format!("{program}: invalid option -- '{letter}'"));
            };
            match arity {
                Arity::Flag => parsed.opts.push(Opt {
                    name: long_name.to_string(),
                    value: None,
                }),
                Arity::Value => {
                    // The rest of the cluster is the value (`-f2`), otherwise
                    // the next argument is (`-f 2`).
                    let attached: String = cluster[position..].iter().collect();
                    let value = if attached.is_empty() {
                        let Some(value) = args.get(index) else {
                            return Err(format!(
                                "{program}: option requires a value -- '{letter}'"
                            ));
                        };
                        index += 1;
                        value.clone()
                    } else {
                        attached
                    };
                    position = cluster.len();
                    parsed.opts.push(Opt {
                        name: long_name.to_string(),
                        value: Some(value),
                    });
                }
            }
        }
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<(char, &'static str, Arity)> {
        vec![
            ('n', "numeric-sort", Arity::Flag),
            ('r', "reverse", Arity::Flag),
            ('d', "delimiter", Arity::Value),
            ('f', "fields", Arity::Value),
        ]
    }

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn clustered_flags() {
        let parsed = parse("sort", &args(&["-rn", "file"]), &table()).unwrap();
        assert!(parsed.has("numeric-sort"));
        assert!(parsed.has("reverse"));
        assert_eq!(parsed.operands, vec!["file"]);
    }

    #[test]
    fn attached_and_separate_values() {
        let attached = parse("cut", &args(&["-d:", "-f2", "file"]), &table()).unwrap();
        assert_eq!(attached.value("delimiter"), Some(":"));
        assert_eq!(attached.value("fields"), Some("2"));

        let separate = parse("cut", &args(&["-d", ":", "-f", "2"]), &table()).unwrap();
        assert_eq!(separate.value("delimiter"), Some(":"));
        assert_eq!(separate.value("fields"), Some("2"));
    }

    #[test]
    fn long_options_accept_both_spellings() {
        let parsed = parse("cut", &args(&["--delimiter=:", "x"]), &table()).unwrap();
        assert_eq!(parsed.value("delimiter"), Some(":"));
        let parsed = parse("cut", &args(&["--delimiter", ":", "x"]), &table()).unwrap();
        assert_eq!(parsed.value("delimiter"), Some(":"));
    }

    #[test]
    fn double_dash_ends_options() {
        let parsed = parse("sort", &args(&["--", "-n"]), &table()).unwrap();
        assert!(!parsed.has("numeric-sort"));
        assert_eq!(parsed.operands, vec!["-n"]);
    }

    #[test]
    fn lone_dash_is_an_operand() {
        let parsed = parse("cat", &args(&["-"]), &table()).unwrap();
        assert_eq!(parsed.operands, vec!["-"]);
    }

    #[test]
    fn unknown_option_is_an_error() {
        let error = parse("sort", &args(&["-z"]), &table()).unwrap_err();
        assert!(error.contains("invalid option"), "{error}");
    }
}
