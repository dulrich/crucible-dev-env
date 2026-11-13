//! `crucible terminal-theme <foot|cterm|urxvt|ghostty>` — prints the config's `theme` block
//! rendered for one terminal, for inspection. Reads the config file only; writes nothing.

use crucible_config::{Config, Named, ThemeTarget, render_theme};

use crate::cli::{Parsed, help_requested};

/// Usage text for `crucible terminal-theme --help`.
pub const USAGE: &str = "usage: crucible terminal-theme <foot|cterm|urxvt|ghostty>\n\nPrints the `theme` block of the CDE config ($XDG_CONFIG_HOME/cde/config.kdl, else\n~/.config/cde/config.kdl) rendered for one terminal: the same text CDE writes to\n$XDG_CONFIG_HOME/cde/terminal-theme/{foot.ini,cterm.kdl,urxvt.Xresources,ghostty.conf}.\nWrites no files.\n\n  -h, --help   print this usage and exit 0\n\nExit 1 when the config has no `theme` block or cannot be loaded; exit 2 on a bad argument.\n";

/// Parses the arguments: exactly one terminal name.
///
/// # Errors
/// A message for a missing, unknown or extra argument.
pub fn parse_args(args: &[String]) -> Result<Parsed<ThemeTarget>, String> {
    if help_requested(args, &[], false) {
        return Ok(Parsed::Help);
    }
    match args {
        [one] => ThemeTarget::from_name(one)
            .map(Parsed::Run)
            .ok_or_else(|| format!("unknown terminal {one:?}")),
        [] => Err("missing terminal name".into()),
        _ => Err("expected exactly one terminal name".into()),
    }
}

/// The fragment for `target` from `config`, or the stderr message when it has no `theme`.
///
/// # Errors
/// The message when `config` has no `theme` block.
pub fn fragment(config: &Config, target: ThemeTarget) -> Result<String, &'static str> {
    config
        .theme
        .as_ref()
        .map(|t| render_theme(t, target))
        .ok_or("the CDE config has no `theme` block")
}

/// Entry point.
pub fn run(args: &[String]) -> i32 {
    let target = match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Ok(Parsed::Run(t)) => t,
        Err(msg) => {
            eprintln!("crucible terminal-theme: {msg}\n{USAGE}");
            return 2;
        }
    };
    let config = match crucible_config::config_path() {
        None => Config::default(),
        Some(path) => match crucible_config::load(&path) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("crucible terminal-theme: {err}");
                return 1;
            }
        },
    };
    match fragment(&config, target) {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(msg) => {
            eprintln!("crucible terminal-theme: {msg}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_one_terminal_name_or_help() {
        assert_eq!(
            parse_args(&s(&["foot"])),
            Ok(Parsed::Run(ThemeTarget::Foot))
        );
        assert_eq!(
            parse_args(&s(&["ghostty"])),
            Ok(Parsed::Run(ThemeTarget::Ghostty))
        );
        assert_eq!(parse_args(&s(&["--help"])), Ok(Parsed::Help));
        assert!(parse_args(&s(&[])).is_err());
        assert!(parse_args(&s(&["kitty"])).is_err());
        assert!(parse_args(&s(&["foot", "cterm"])).is_err());
    }

    #[test]
    fn fragment_needs_a_theme() {
        assert!(fragment(&Config::default(), ThemeTarget::Foot).is_err());
        let c = crucible_config::parse("theme { foreground \"#010203\"; }\n").unwrap();
        assert!(
            fragment(&c, ThemeTarget::Foot)
                .unwrap()
                .contains("foreground=010203")
        );
    }
}
