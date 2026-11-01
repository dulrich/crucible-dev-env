//! Shell text: quoting and joining argv into a command string, and recognising shell programs.

use std::path::Path;

/// Quotes one argument for a POSIX shell: left bare when it only holds `[A-Za-z0-9_./=:,+@%-]`,
/// otherwise single-quoted with `'` written as `'\''`. The empty argument becomes `''`.
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./=:,+@%-".contains(c);
    if !arg.is_empty() && arg.chars().all(safe) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Joins arguments into one shell-quoted command string.
#[must_use]
pub fn join_command(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `argv0` names an interactive shell (basename, a login shell's leading `-` ignored).
#[must_use]
pub fn is_shell_program(argv0: &str) -> bool {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(argv0);
    matches!(
        name.trim_start_matches('-'),
        "sh" | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "ksh"
            | "mksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "elvish"
            | "xonsh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("cargo"), "cargo");
        assert_eq!(
            shell_quote("--flag=a,b:c+d@e%f-g_h./i"),
            "--flag=a,b:c+d@e%f-g_h./i"
        );
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a;b$c\"d"), "'a;b$c\"d'");
        assert_eq!(shell_quote("*"), "'*'");
        let args = ["echo".to_string(), "a b".to_string(), "c'd".to_string()];
        assert_eq!(join_command(&args), "echo 'a b' 'c'\\''d'");
    }

    #[test]
    fn shell_program_names() {
        for s in [
            "/bin/bash",
            "bash",
            "-bash",
            "/usr/bin/zsh",
            "fish",
            "/bin/sh",
        ] {
            assert!(is_shell_program(s), "{s}");
        }
        for s in ["htop", "/usr/bin/vim", "bashtop", "cargo"] {
            assert!(!is_shell_program(s), "{s}");
        }
    }
}
