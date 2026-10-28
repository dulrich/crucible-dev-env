//! The one shared "is this path an executable regular file" check (used by the compositor's
//! spawn pre-flight and the launcher's PATH resolution).

use std::path::Path;

/// Whether `path` (symlinks followed) is a regular file with any execute bit set.
#[must_use]
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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
}
