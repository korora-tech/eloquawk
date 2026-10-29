//! Command-line parsing as a pure function (SPEC 2.1). Flags grow per milestone; at M4 they
//! are `--demo`, `--theme`, `--demo-actions`, `-h` / `--help` and `-V` / `--version`.
//! `ELOQUAWK_DEMO=1` is read by `main` and passed in, so parsing stays testable.

use crate::config::ThemePref;

pub const USAGE: &str =
    "usage: eloquawk [--demo] [--theme light|dark|system] [--demo-actions <steps>]

  --demo                  run with a generated mailbox (no Google account, nothing saved)
  --theme <theme>         light, dark or system for this session (not saved)
  --demo-actions <steps>  demo only: run comma-separated scripted steps (see docs/SPEC.md)
  -h, --help              show this help
  -V, --version           show the version

Environment: ELOQUAWK_DEMO=1 is the same as --demo.";

/// Options for a normal run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub demo: bool,
    /// Overrides the saved theme for this session.
    pub theme: Option<ThemePref>,
    /// A `--demo-actions` script, unparsed.
    pub demo_actions: Option<String>,
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
    let mut out = Args {
        demo: env_demo,
        ..Args::default()
    };
    let mut it = args.iter().map(AsRef::as_ref);
    while let Some(arg) = it.next() {
        // `--flag=value` and `--flag value` are both accepted.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v)),
            _ => (arg, None),
        };
        let mut value = |name: &str| {
            inline
                .map(str::to_string)
                .or_else(|| it.next().map(str::to_string))
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag {
            "--demo" if inline.is_none() => out.demo = true,
            "--theme" => {
                out.theme = Some(match value("--theme")?.as_str() {
                    "light" => ThemePref::Light,
                    "dark" => ThemePref::Dark,
                    "system" => ThemePref::System,
                    other => return Err(format!("unknown theme {other}")),
                })
            }
            "--demo-actions" => out.demo_actions = Some(value("--demo-actions")?),
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    if out.demo_actions.is_some() && !out.demo {
        return Err("--demo-actions needs --demo".into());
    }
    Ok(Parsed::Run(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Args {
        Args {
            demo: true,
            ..Args::default()
        }
    }

    #[test]
    fn no_arguments_runs_against_google() {
        assert_eq!(parse::<&str>(&[], false), Ok(Parsed::Run(Args::default())));
    }

    #[test]
    fn demo_from_flag_or_environment() {
        assert_eq!(parse(&["--demo"], false), Ok(Parsed::Run(demo())));
        assert_eq!(parse::<&str>(&[], true), Ok(Parsed::Run(demo())));
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

    #[test]
    fn theme_flag() {
        let run = |a: &[&str]| match parse(a, false) {
            Ok(Parsed::Run(args)) => Ok(args.theme),
            Ok(other) => Err(format!("{other:?}")),
            Err(e) => Err(e),
        };
        assert_eq!(run(&["--theme", "light"]), Ok(Some(ThemePref::Light)));
        assert_eq!(run(&["--theme=dark"]), Ok(Some(ThemePref::Dark)));
        assert_eq!(run(&["--theme", "system"]), Ok(Some(ThemePref::System)));
        assert_eq!(run(&[]), Ok(None));
        assert_eq!(
            run(&["--theme", "sepia"]),
            Err("unknown theme sepia".into())
        );
        assert_eq!(run(&["--theme"]), Err("--theme needs a value".into()));
    }

    #[test]
    fn demo_actions_need_demo() {
        assert_eq!(
            parse(&["--demo-actions", "j,shot"], false),
            Err("--demo-actions needs --demo".into())
        );
        let expected = Args {
            demo_actions: Some("j,j,o".into()),
            ..demo()
        };
        assert_eq!(
            parse(&["--demo", "--demo-actions", "j,j,o"], false),
            Ok(Parsed::Run(expected.clone()))
        );
        assert_eq!(
            parse(&["--demo-actions=j,j,o"], true),
            Ok(Parsed::Run(expected))
        );
        assert_eq!(
            parse(&["--demo", "--demo-actions"], false),
            Err("--demo-actions needs a value".into())
        );
    }
}
