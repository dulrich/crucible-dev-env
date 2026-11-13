//! Writing the `theme` block's per-terminal fragments to disk (the I/O edge of
//! [`crucible_config::render_theme`]).
//!
//! With a `theme`, every config apply (startup and each reload) writes
//! `<dir>/{foot.ini,cterm.kdl,urxvt.Xresources,ghostty.conf}`; a file whose content is unchanged
//! is not rewritten, and a changed one is replaced atomically (temp file in the same directory,
//! then rename). Without a `theme` nothing is written and nothing is deleted.

use std::io;
use std::path::{Path, PathBuf};

use crucible_config::{ThemeConfig, ThemeTarget, render_theme};

/// Writes every fragment of `theme` into `dir` (created as needed) and returns the targets whose
/// file changed.
///
/// # Errors
/// The first I/O error (creating the directory, writing or renaming a file).
pub fn write_fragments(theme: &ThemeConfig, dir: &Path) -> io::Result<Vec<ThemeTarget>> {
    std::fs::create_dir_all(dir)?;
    let mut changed = Vec::new();
    for target in ThemeTarget::ALL {
        let path = dir.join(target.file_name());
        if write_if_changed(&path, &render_theme(theme, target))? {
            changed.push(target);
        }
    }
    Ok(changed)
}

/// Replaces `path` with `content` atomically unless it already holds exactly `content`.
/// Returns whether it wrote.
fn write_if_changed(path: &Path, content: &str) -> io::Result<bool> {
    if std::fs::read(path).is_ok_and(|old| old == content.as_bytes()) {
        return Ok(false);
    }
    let tmp = temp_path(path);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        std::fs::remove_file(&tmp).ok();
    })?;
    Ok(true)
}

/// `<dir>/.<name>.tmp-<pid>`: hidden, beside `path`, so the rename stays on one filesystem.
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

/// The urxvt fragment's path when a `theme` is set and a fragment directory resolves.
#[must_use]
pub fn urxvt_fragment(theme: Option<&ThemeConfig>, dir: Option<&Path>) -> Option<PathBuf> {
    theme?;
    Some(dir?.join(ThemeTarget::Urxvt.file_name()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_config::Rgb;

    #[test]
    fn writes_all_fragments_then_only_what_changed() {
        let dir = std::env::temp_dir().join(format!("cde-terminal-theme-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let nested = dir.join("cde").join("terminal-theme");
        let mut theme = ThemeConfig {
            foreground: Some(Rgb(1, 2, 3)),
            ..ThemeConfig::default()
        };
        assert_eq!(write_fragments(&theme, &nested).unwrap(), ThemeTarget::ALL);
        let foot = std::fs::read_to_string(nested.join("foot.ini")).unwrap();
        assert!(foot.contains("foreground=010203"), "{foot}");
        // Unchanged: nothing rewritten.
        assert!(write_fragments(&theme, &nested).unwrap().is_empty());
        // A cursor change touches every terminal's file; no temp file is left behind.
        theme.cursor = Some(Rgb(4, 5, 6));
        assert_eq!(write_fragments(&theme, &nested).unwrap().len(), 4);
        let names: Vec<_> = std::fs::read_dir(&nested)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn urxvt_fragment_needs_a_theme_and_a_dir() {
        let t = ThemeConfig::default();
        let dir = Path::new("/c/cde/terminal-theme");
        assert_eq!(
            urxvt_fragment(Some(&t), Some(dir)),
            Some(PathBuf::from("/c/cde/terminal-theme/urxvt.Xresources"))
        );
        assert_eq!(urxvt_fragment(None, Some(dir)), None);
        assert_eq!(urxvt_fragment(Some(&t), None), None);
    }
}
