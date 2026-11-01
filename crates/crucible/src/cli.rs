//! Argument-parsing plumbing shared by every subcommand: the parsed-or-help outcome, the
//! `--help` detector and the one `run()` opening (`parse_or_exit`).

/// What a subcommand's parser found: a request for its usage, or the parsed arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed<T> {
    /// `--help`/`-h` in a non-payload position: print the usage, exit 0, no side effect.
    Help,
    /// The parsed arguments to run with.
    Run(T),
}

/// True when `--help`/`-h` appears in a non-payload position of a subcommand's arguments. Payload
/// positions never count: the value of any of `value_flags` (for example `--rename`), and, when
/// `stop_at_dashdash`, everything from `--` on.
pub fn help_requested(args: &[String], value_flags: &[&str], stop_at_dashdash: bool) -> bool {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" if stop_at_dashdash => return false,
            f if value_flags.contains(&f) => i += 1,
            "--help" | "-h" => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// The one `run()` opening of every subcommand. `Run(value)` yields `Ok(value)`; `Help` prints
/// `usage` to stdout and yields `Err(0)`; a parse error prints `crucible <name>: <error>` and the
/// `usage` to stderr and yields `Err(2)`. The `Err` code is what the subcommand's `run` returns.
pub fn parse_or_exit<T>(
    name: &str,
    usage: &str,
    parsed: Result<Parsed<T>, String>,
) -> Result<T, i32> {
    match parsed {
        Ok(Parsed::Run(value)) => Ok(value),
        Ok(Parsed::Help) => {
            print!("{usage}");
            Err(0)
        }
        Err(e) => {
            eprintln!("crucible {name}: {e}\n\n{usage}");
            Err(2)
        }
    }
}

/// Like [`parse_or_exit`] but a parse error prints only `crucible <name>: <error>` (no usage), for
/// `msg`, whose errors are about the request rather than the flags.
pub fn parse_or_exit_terse<T>(
    name: &str,
    usage: &str,
    parsed: Result<Parsed<T>, String>,
) -> Result<T, i32> {
    match parsed {
        Err(e) => {
            eprintln!("crucible {name}: {e}");
            Err(2)
        }
        other => parse_or_exit(name, usage, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn help_requested_skips_the_value_flag_and_stops_at_dashdash() {
        assert!(help_requested(&args(&["--help"]), &[], false));
        assert!(help_requested(&args(&["x", "-h"]), &[], false));
        assert!(!help_requested(&args(&[]), &[], false));
        assert!(!help_requested(
            &args(&["--rename", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(help_requested(
            &args(&["--rename", "x", "-h"]),
            &["--rename", "--root"],
            false
        ));
        assert!(!help_requested(
            &args(&["--root", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(help_requested(
            &args(&["--root", "", "--help"]),
            &["--rename", "--root"],
            false
        ));
        assert!(!help_requested(&args(&["--", "-h"]), &[], true));
        assert!(help_requested(&args(&["--", "-h"]), &[], false));
    }

    #[test]
    fn parse_or_exit_yields_the_value_on_run() {
        assert_eq!(parse_or_exit("x", "u", Ok(Parsed::Run(7))), Ok(7));
        assert_eq!(parse_or_exit_terse("x", "u", Ok(Parsed::Run(7))), Ok(7));
    }

    #[test]
    fn parse_or_exit_exits_zero_on_help() {
        assert_eq!(parse_or_exit::<()>("x", "u", Ok(Parsed::Help)), Err(0));
        assert_eq!(
            parse_or_exit_terse::<()>("x", "u", Ok(Parsed::Help)),
            Err(0)
        );
    }

    #[test]
    fn parse_or_exit_exits_two_on_error() {
        assert_eq!(parse_or_exit::<()>("x", "u", Err("bad".into())), Err(2));
        assert_eq!(
            parse_or_exit_terse::<()>("x", "u", Err("bad".into())),
            Err(2)
        );
    }
}
