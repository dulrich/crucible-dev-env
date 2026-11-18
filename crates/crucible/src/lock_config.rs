//! `crucible lock-config` — prints the swaylock config CDE generates for the config's `lock`
//! block, for inspection. Reads the config file (and, for a `user` background, the wallpaper
//! state file); writes nothing.

use crucible_compositor::lock_config::render_for_env;

use crate::cli::{Parsed, help_requested};

/// Usage text for `crucible lock-config --help`.
pub const USAGE: &str = "usage: crucible lock-config\n\nPrints the swaylock config CDE generates for the `lock` block of the CDE config\n($XDG_CONFIG_HOME/cde/config.kdl, else ~/.config/cde/config.kdl): the text written to\n$XDG_CONFIG_HOME/cde/swaylock/config and passed to swaylock as `-C <file>`.\nWrites no files. Without a `lock` block it prints nothing to stdout and a note to stderr.\n\n  -h, --help   print this usage and exit 0\n\nExit 1 when the config cannot be loaded; exit 2 on a bad argument.\n";

/// Parses the arguments: none besides `--help`.
///
/// # Errors
/// A message for any other argument.
pub fn parse_args(args: &[String]) -> Result<Parsed<()>, String> {
    if help_requested(args, &[], false) {
        return Ok(Parsed::Help);
    }
    match args.first() {
        None => Ok(Parsed::Run(())),
        Some(extra) => Err(format!("unexpected argument {extra:?}")),
    }
}

/// Entry point.
pub fn run(args: &[String]) -> i32 {
    match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Ok(Parsed::Run(())) => {}
        Err(msg) => {
            eprintln!("crucible lock-config: {msg}\n{USAGE}");
            return 2;
        }
    }
    let config = match crucible_config::load_from_env() {
        Ok((_, c)) => c,
        Err((_, err)) => {
            eprintln!("crucible lock-config: {err}");
            return 1;
        }
    };
    match render_for_env(&config) {
        Some(text) => print!("{text}"),
        None => eprintln!("crucible lock-config: the CDE config has no `lock` block"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_no_arguments_but_help() {
        assert_eq!(parse_args(&[]), Ok(Parsed::Run(())));
        assert_eq!(parse_args(&["--help".to_string()]), Ok(Parsed::Help));
        assert!(parse_args(&["x".to_string()]).is_err());
    }
}
