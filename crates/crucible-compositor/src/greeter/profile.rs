//! The greeter profile's one I/O edge: [`load_profile`] reads `<dir>/profile.kdl` (missing,
//! unreadable or invalid → [`GreeterProfile::fallback`] with one `warn!`). The profile type, its
//! writer and its parser are pure and live in `crucible-config` (`greeter_profile`).

use std::path::{Path, PathBuf};

use crucible_config::{GreeterProfile, PROFILE_FILE, parse_profile};

/// The profile file for `--profile <dir>`; `None` without `--profile` (never `$HOME` or
/// `$XDG_CONFIG_HOME`).
#[must_use]
pub fn profile_path(dir: Option<&Path>) -> Option<PathBuf> {
    dir.map(|d| d.join(PROFILE_FILE))
}

/// Reads the profile under `dir`; missing, unreadable or invalid → the fallback, with one `warn!`.
#[must_use]
pub fn load_profile(dir: Option<&Path>) -> GreeterProfile {
    let Some(path) = profile_path(dir) else {
        tracing::warn!("no --profile given; using the fallback greeter look");
        return GreeterProfile::fallback();
    };
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| parse_profile(&t).map_err(|e| e.to_string()));
    match parsed {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(path = %path.display(), %err, "greeter profile unusable; using the fallback look");
            GreeterProfile::fallback()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_profile_falls_back() {
        let dir = std::env::temp_dir().join(format!("cde-greeter-profile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Missing file, and no --profile at all.
        assert_eq!(load_profile(Some(&dir)), GreeterProfile::fallback());
        assert_eq!(load_profile(None), GreeterProfile::fallback());
        assert_eq!(profile_path(None), None);
        assert_eq!(
            profile_path(Some(&dir)),
            Some(dir.join("profile.kdl")),
            "only --profile decides the path"
        );
        // Invalid values.
        std::fs::write(dir.join(PROFILE_FILE), "style \"nope\"\n").unwrap();
        assert_eq!(load_profile(Some(&dir)), GreeterProfile::fallback());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
