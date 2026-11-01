//! The one shared "is this path an executable regular file" check (used by the compositor's
//! spawn pre-flight and the launcher's PATH resolution).

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
}
