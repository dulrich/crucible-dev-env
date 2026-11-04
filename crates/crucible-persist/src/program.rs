//! Program resolution shared by the compositor's spawn path and the launcher: the "is this path an
//! executable regular file" check, the reinstall-safe `crucible` executable, a child's `PATH`
//! ([`child_path`]), the `run` shell-line pre-flight ([`preflight_command`]) and the strict TERM
//! program resolver ([`resolve_program`]). Both resolvers walk `PATH` through one private
//! candidate iterator, so their empty/relative-entry rules cannot drift. Nothing here spawns.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Whether `path` (symlinks followed) is a regular file with any execute bit set.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The suffix Linux appends to `/proc/self/exe` when the running binary was replaced or removed.
const DELETED_SUFFIX: &str = " (deleted)";

/// The running binary's path is gone: it was removed (not replaced) while the process ran.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the crucible binary was removed while running: {}", .0.display())]
pub struct ExeRemoved(pub PathBuf);

/// Resolves a raw `current_exe()` path across a reinstall. A path without the ` (deleted)` suffix
/// is returned unchanged; with it, the stripped path is returned only when `is_exe` accepts it
/// (the binary was replaced in place), else [`ExeRemoved`] carries the stripped path.
///
/// # Errors
/// [`ExeRemoved`] when the suffix is present and the stripped path is not an executable.
pub fn reinstalled_exe(
    raw: PathBuf,
    is_exe: impl Fn(&Path) -> bool,
) -> Result<PathBuf, ExeRemoved> {
    let Some(stripped) = raw.to_str().and_then(|s| s.strip_suffix(DELETED_SUFFIX)) else {
        return Ok(raw);
    };
    let stripped = PathBuf::from(stripped);
    if is_exe(&stripped) {
        Ok(stripped)
    } else {
        Err(ExeRemoved(stripped))
    }
}

/// The running `crucible` binary's path, resolved on every call (never cached) so a reinstall
/// under a running process yields the fresh binary rather than `… (deleted)`.
///
/// # Errors
/// [`CrucibleExeError`] when `current_exe()` fails or the binary was removed.
pub fn crucible_exe() -> Result<PathBuf, CrucibleExeError> {
    let raw = std::env::current_exe().map_err(CrucibleExeError::Io)?;
    Ok(reinstalled_exe(raw, is_executable_file)?)
}

/// The running binary's basename (e.g. `crucible`, or `crucible-trixie` when installed under a
/// distro-suffixed name), taken from [`crucible_exe`] on every call (never cached) so it follows
/// a reinstall. Any resolution failure yields `crucible`.
#[must_use]
pub fn own_exe_name() -> String {
    crucible_exe()
        .ok()
        .and_then(|p| exe_basename(&p))
        .unwrap_or_else(|| "crucible".to_string())
}

/// The final path component of `exe` as a string, if it has a UTF-8 one.
fn exe_basename(exe: &Path) -> Option<String> {
    exe.file_name().and_then(|n| n.to_str()).map(str::to_string)
}

/// Why [`crucible_exe`] failed.
#[derive(Debug, thiserror::Error)]
pub enum CrucibleExeError {
    /// `current_exe()` itself failed.
    #[error("cannot resolve the crucible binary: {0}")]
    Io(#[source] std::io::Error),
    /// The binary was removed rather than replaced.
    #[error(transparent)]
    Removed(#[from] ExeRemoved),
}

/// A spawned child's `PATH`: `exe_dir` prepended to the inherited `PATH`, unless it is already an
/// entry. An unset inherited `PATH` yields just `exe_dir`.
#[must_use]
pub fn child_path(exe_dir: &Path, inherited: Option<&OsStr>) -> OsString {
    let Some(inherited) = inherited else {
        return exe_dir.as_os_str().to_os_string();
    };
    if std::env::split_paths(inherited).any(|p| p == exe_dir) {
        return inherited.to_os_string();
    }
    let mut entries = vec![exe_dir.to_path_buf()];
    entries.extend(std::env::split_paths(inherited));
    std::env::join_paths(entries).unwrap_or_else(|_| inherited.to_os_string())
}

/// POSIX sh special/regular builtins and reserved words the pre-flight lets through.
pub const SH_BUILTINS: &[&str] = &[
    ".", ":", "break", "cd", "command", "continue", "eval", "exec", "exit", "export", "getopts",
    "read", "readonly", "return", "set", "shift", "times", "trap", "type", "ulimit", "umask",
    "unset", "wait", "alias", "bg", "fg", "jobs", "kill", "printf", "echo", "test", "true",
    "false", "if", "case", "for", "while", "until",
];

/// `p` itself when absolute, else `p` relative to `cwd`.
fn against(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// The one `PATH` walk: `dir.join(word)` for each `path` entry in order, where an empty entry is
/// `cwd` and a relative entry resolves against `cwd`. An unset `path` yields nothing.
fn path_candidates<'a>(
    word: &'a str,
    cwd: &'a Path,
    path: Option<&'a OsStr>,
) -> impl Iterator<Item = PathBuf> + 'a {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(move |dir| {
            if dir.as_os_str().is_empty() {
                cwd.join(word)
            } else {
                against(cwd, &dir).join(word)
            }
        })
}

/// Pre-flight for `run`/`run-in-terminal`: does the command's first shell word resolve? Passes
/// when the word holds shell syntax the check cannot judge, is a sh builtin/keyword, is a path
/// (relative to `cwd`) to an existing file, or is an executable file on `path` (relative entries
/// and empty entries resolve against `cwd`). Never spawns or waits.
///
/// # Errors
/// The unresolved first word.
pub fn preflight_command(cmd: &str, cwd: &Path, path: Option<&OsStr>) -> Result<(), String> {
    let Some(word) = cmd.split_whitespace().next() else {
        return Ok(());
    };
    let syntax = |c: char| "='\"$`\\~*?[()<>|;&".contains(c);
    if word.contains(syntax) || word.starts_with(['{', '!']) || SH_BUILTINS.contains(&word) {
        return Ok(());
    }
    if word.contains('/') {
        return if against(cwd, Path::new(word)).is_file() {
            Ok(())
        } else {
            Err(word.to_string())
        };
    }
    if path_candidates(word, cwd, path).any(|c| is_executable_file(&c)) {
        Ok(())
    } else {
        Err(word.to_string())
    }
}

/// Strictly resolves a TERM `program` to an executable regular file, unlike [`preflight_command`]
/// (a shell-line check that lets builtins and shell syntax through). A word containing `/` must
/// itself be an executable file (relative to `cwd` if not absolute); a bare word must be one in a
/// `path` directory (empty or relative entries resolve against `cwd`).
///
/// # Errors
/// The unresolved program (empty for an empty program).
pub fn resolve_program(program: &str, cwd: &Path, path: Option<&OsStr>) -> Result<PathBuf, String> {
    if program.is_empty() {
        return Err(String::new());
    }
    if program.contains('/') {
        let full = against(cwd, Path::new(program));
        return if is_executable_file(&full) {
            Ok(full)
        } else {
            Err(program.to_string())
        };
    }
    path_candidates(program, cwd, path)
        .find(|candidate| is_executable_file(candidate))
        .ok_or_else(|| program.to_string())
}

/// Whether two program names are the same executable by basename (`/usr/bin/ghostty` and
/// `ghostty`).
#[must_use]
pub fn same_program(a: &str, b: &str) -> bool {
    let base = |s: &str| Path::new(s).file_name().map(OsStr::to_os_string);
    matches!((base(a), base(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_executable_regular_files_count() {
        let dir =
            std::env::temp_dir().join(format!("crucible-persist-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let put = |name: &str, mode: u32| {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p
        };
        assert!(is_executable_file(&put("run", 0o755)));
        assert!(!is_executable_file(&put("plain", 0o644)));
        assert!(!is_executable_file(&dir));
        assert!(!is_executable_file(&dir.join("missing")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reinstalled_exe_passes_plain_paths_through() {
        let p = PathBuf::from("/home/u/.local/bin/crucible");
        assert_eq!(reinstalled_exe(p.clone(), |_| false), Ok(p));
    }

    #[test]
    fn reinstalled_exe_strips_suffix_when_replacement_is_executable() {
        let raw = PathBuf::from("/home/u/.local/bin/crucible (deleted)");
        let got = reinstalled_exe(raw, |p| p == Path::new("/home/u/.local/bin/crucible"));
        assert_eq!(got, Ok(PathBuf::from("/home/u/.local/bin/crucible")));
    }

    #[test]
    fn reinstalled_exe_fails_when_stripped_path_is_missing() {
        let raw = PathBuf::from("/home/u/.local/bin/crucible (deleted)");
        assert_eq!(
            reinstalled_exe(raw, |_| false),
            Err(ExeRemoved(PathBuf::from("/home/u/.local/bin/crucible")))
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cde-pre-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(path: &Path, mode: u32) {
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn resolve_program_is_strict() {
        let d = scratch("resolve");
        touch(&d.join("goodterm"), 0o755);
        touch(&d.join("plain"), 0o644);
        let path = std::ffi::OsString::from(d.as_os_str());
        let p = Some(path.as_os_str());
        let root = Path::new("/");
        assert_eq!(resolve_program("goodterm", root, p), Ok(d.join("goodterm")));
        assert_eq!(resolve_program("plain", root, p), Err("plain".into()));
        assert_eq!(resolve_program("missing", root, p), Err("missing".into()));
        assert_eq!(resolve_program("cd", root, p), Err("cd".into()));
        assert_eq!(
            resolve_program("goodterm", root, None),
            Err("goodterm".into())
        );
        assert_eq!(resolve_program("", root, p), Err(String::new()));
        let abs = d.join("goodterm");
        assert_eq!(resolve_program(abs.to_str().unwrap(), root, None), Ok(abs));
        assert_eq!(
            resolve_program("./goodterm", &d, None),
            Ok(d.join("./goodterm"))
        );
        assert!(resolve_program("./plain", &d, None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn same_program_compares_basenames() {
        assert!(same_program("ghostty", "/usr/bin/ghostty"));
        assert!(same_program("/a/kitty", "/b/kitty"));
        assert!(!same_program("ghostty", "kitty"));
        assert!(!same_program("", ""));
    }

    #[test]
    fn child_path_prepends_once() {
        let dir = Path::new("/opt/cde");
        assert_eq!(child_path(dir, None), OsString::from("/opt/cde"));
        assert_eq!(
            child_path(dir, Some("/usr/bin:/bin".as_ref())),
            OsString::from("/opt/cde:/usr/bin:/bin")
        );
        assert_eq!(
            child_path(dir, Some("/usr/bin:/opt/cde".as_ref())),
            OsString::from("/usr/bin:/opt/cde")
        );
    }

    #[test]
    fn preflight_finds_path_commands_and_names_misses() {
        let bin = scratch("bin");
        touch(&bin.join("tool"), 0o755);
        touch(&bin.join("data"), 0o644);
        let path = bin.clone().into_os_string();
        let cwd = Path::new("/");
        assert_eq!(preflight_command("tool --x", cwd, Some(&path)), Ok(()));
        assert_eq!(
            preflight_command("nosuch --x", cwd, Some(&path)),
            Err("nosuch".to_string())
        );
        assert_eq!(
            preflight_command("data", cwd, Some(&path)),
            Err("data".to_string())
        );
        assert_eq!(
            preflight_command("tool", cwd, None),
            Err("tool".to_string())
        );
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[test]
    fn preflight_resolves_paths_against_the_cwd() {
        let cwd = scratch("cwd");
        touch(&cwd.join("x"), 0o755);
        let abs = cwd.join("x");
        assert_eq!(
            preflight_command(abs.to_str().unwrap(), Path::new("/"), None),
            Ok(())
        );
        assert_eq!(preflight_command("./x", &cwd, None), Ok(()));
        assert_eq!(preflight_command("./y", &cwd, None), Err("./y".to_string()));
        // A relative PATH entry and an empty one both resolve against the cwd.
        assert_eq!(preflight_command("x", &cwd, Some(".".as_ref())), Ok(()));
        assert_eq!(preflight_command("x", &cwd, Some("".as_ref())), Ok(()));
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn preflight_passes_what_it_cannot_judge() {
        let cwd = Path::new("/");
        for cmd in [
            "'quoted prog' a",
            "\"q\" a",
            "FOO=1 nosuch",
            "$EDITOR f",
            "~/bin/x",
            "(cd /; nosuch)",
            "cd /tmp && nosuch",
            "{ nosuch; }",
            "! nosuch",
            "echo hi",
            "exec nosuch",
            "if true; then :; fi",
            "",
        ] {
            assert_eq!(preflight_command(cmd, cwd, None), Ok(()), "{cmd:?}");
        }
    }

    #[test]
    fn own_name_follows_a_reinstalled_exe() {
        let raw = PathBuf::from("/home/u/.local/bin/crucible-trixie (deleted)");
        let exe = reinstalled_exe(raw, |_| true).unwrap();
        assert_eq!(exe_basename(&exe).as_deref(), Some("crucible-trixie"));
        assert_eq!(exe_basename(Path::new("/")), None);
        // Whatever the test binary is called, the resolver yields its basename.
        assert!(!own_exe_name().is_empty());
    }
}
