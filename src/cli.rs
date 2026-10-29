//! Command-line parsing as a pure function (SPEC 2.1). Flags grow per milestone; at M3 they
//! are `--demo`, `-h` / `--help` and `-V` / `--version`. `ELOQUAWK_DEMO=1` is read by `main`
//! and passed in, so parsing stays testable.

pub const USAGE: &str = "usage: eloquawk [--demo]

  --demo         run with a generated mailbox (no Google account, nothing saved)
  -h, --help     show this help
  -V, --version  show the version

Environment: ELOQUAWK_DEMO=1 is the same as --demo.";

/// Options for a normal run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub demo: bool,
}

/// What `main` should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
    Version,
}

/// True when the `ELOQUAWK_DEMO` value turns demo mode on.
pub fn env_demo(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// Parses the arguments after the program name. `env_demo` is the `ELOQUAWK_DEMO=1` switch.
/// An unknown argument is an error carrying the message to print.
pub fn parse<S: AsRef<str>>(args: &[S], env_demo: bool) -> Result<Parsed, String> {
    let mut out = Args { demo: env_demo };
    for arg in args {
        match arg.as_ref() {
            "--demo" => out.demo = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Parsed::Run(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_runs_against_google() {
        assert_eq!(
            parse::<&str>(&[], false),
            Ok(Parsed::Run(Args { demo: false }))
        );
    }

    #[test]
    fn demo_from_flag_or_environment() {
        assert_eq!(
            parse(&["--demo"], false),
            Ok(Parsed::Run(Args { demo: true }))
        );
        assert_eq!(
            parse::<&str>(&[], true),
            Ok(Parsed::Run(Args { demo: true }))
        );
        assert!(env_demo(Some(std::ffi::OsStr::new("1"))));
        assert!(!env_demo(Some(std::ffi::OsStr::new("0"))));
        assert!(!env_demo(None));
    }

    #[test]
    fn help_and_version_win() {
        assert_eq!(parse(&["--demo", "-h"], false), Ok(Parsed::Help));
        assert_eq!(parse(&["--help"], false), Ok(Parsed::Help));
        assert_eq!(parse(&["-V"], false), Ok(Parsed::Version));
        assert_eq!(parse(&["--version"], true), Ok(Parsed::Version));
    }

    #[test]
    fn unknown_arguments_are_errors() {
        assert_eq!(
            parse(&["--nope"], false),
            Err("unknown argument --nope".into())
        );
    }
}
