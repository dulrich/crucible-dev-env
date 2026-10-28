//! XDG desktop-entry discovery for the launcher: parsing, field-code stripping and the
//! `applications` directory scan. Pure over explicit paths except [`data_dirs_from_env`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A launchable desktop entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// The unlocalised `Name`.
    pub name: String,
    /// `Exec` with its field codes stripped.
    pub exec: String,
    /// `Terminal=true`.
    pub terminal: bool,
}

/// Removes the desktop-entry field codes (`%f %F %u %U %d %D %n %N %i %c %k %v %m`) wherever they
/// appear, turns `%%` into `%`, collapses the resulting runs of spaces and trims.
#[must_use]
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('%') => {
                chars.next();
                out.push('%');
            }
            Some(n) if "fFuUdDnNickvm".contains(*n) => {
                chars.next();
            }
            _ => out.push('%'),
        }
    }
    out.split(' ')
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a `.desktop` file. `None` unless it is a visible `Type=Application` entry with a name
/// and a non-empty command after field-code stripping.
#[must_use]
pub fn parse_desktop_entry(text: &str) -> Option<DesktopEntry> {
    let mut in_group = false;
    let (mut name, mut exec, mut kind) = (None, None, None);
    let (mut no_display, mut hidden, mut terminal) = (false, false, false);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Type" => kind = Some(value.to_string()),
            "NoDisplay" => no_display = value == "true",
            "Hidden" => hidden = value == "true",
            "Terminal" => terminal = value == "true",
            _ => {}
        }
    }
    if kind.as_deref() != Some("Application") || no_display || hidden {
        return None;
    }
    let name = name.filter(|n| !n.is_empty())?;
    let exec = strip_field_codes(&exec?);
    if exec.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        name,
        exec,
        terminal,
    })
}

/// The `applications` directories to scan, highest precedence first: `$XDG_DATA_HOME` (default
/// `$HOME/.local/share`), then each `$XDG_DATA_DIRS` entry (default `/usr/local/share:/usr/share`).
#[must_use]
pub fn data_dirs_from_env() -> Vec<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let home = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let dirs = var("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(std::env::split_paths(&dirs))
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("applications"))
        .collect()
}

const MAX_DEPTH: usize = 8;

/// Recursively collects `*.desktop` files under `root` as `(desktop-file id, path)`, in sorted
/// order. Unreadable directories are skipped.
fn desktop_files(root: &Path, rel: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let rel_child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let path = root.join(&rel_child);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            desktop_files(root, &rel_child, depth + 1, out);
        } else if meta.is_file() && name.ends_with(".desktop") {
            out.push((rel_child.replace('/', "-"), path));
        }
    }
}

/// Launchable entries from `dirs` (highest precedence first). The first directory to provide a
/// desktop-file id wins even when its file is filtered out, so a hidden user file shadows the
/// system one. Sorted by name, case-insensitively.
#[must_use]
pub fn desktop_entries(dirs: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for dir in dirs {
        let mut files = Vec::new();
        desktop_files(dir, "", 0, &mut files);
        for (id, path) in files {
            if !seen.insert(id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse_desktop_entry(&text) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries
}

/// Single-quotes `s` for a POSIX shell.
#[must_use]
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-launch-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, exec: &str, terminal: bool) -> DesktopEntry {
        DesktopEntry {
            name: name.into(),
            exec: exec.into(),
            terminal,
        }
    }

    #[test]
    fn field_codes_are_stripped() {
        assert_eq!(strip_field_codes("firefox %u"), "firefox");
        assert_eq!(strip_field_codes("app --f=%f  %U x"), "app --f= x");
        assert_eq!(strip_field_codes("echo 100%% %k"), "echo 100%");
        assert_eq!(strip_field_codes("%F"), "");
    }

    #[test]
    fn desktop_entry_parsing_honours_groups_and_filters() {
        let text = "# c\n[Desktop Entry]\nType=Application\nName=Foo\nName[de]=Fuu\nExec = foo %U\nTerminal=true\n\n[Desktop Action x]\nName=Other\nExec=other\n";
        assert_eq!(parse_desktop_entry(text), Some(entry("Foo", "foo", true)));
        let hidden = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nNoDisplay=true\n";
        assert_eq!(parse_desktop_entry(hidden), None);
        let hidden = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nHidden=true\n";
        assert_eq!(parse_desktop_entry(hidden), None);
        let link = "[Desktop Entry]\nType=Link\nName=A\nExec=a\n";
        assert_eq!(parse_desktop_entry(link), None);
        let only_code = "[Desktop Entry]\nType=Application\nName=A\nExec=%U\n";
        assert_eq!(parse_desktop_entry(only_code), None);
        let wrong_group = "[Other]\nType=Application\nName=A\nExec=a\n";
        assert_eq!(parse_desktop_entry(wrong_group), None);
    }

    #[test]
    fn earlier_dir_shadows_even_when_hidden() {
        let root = tmp("shadow");
        let (user, sys) = (root.join("user"), root.join("sys"));
        std::fs::create_dir_all(sys.join("sub")).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let app = |n: &str| format!("[Desktop Entry]\nType=Application\nName={n}\nExec={n}\n");
        std::fs::write(sys.join("a.desktop"), app("Alpha")).unwrap();
        std::fs::write(sys.join("sub").join("b.desktop"), app("Bravo")).unwrap();
        std::fs::write(sys.join("c.desktop"), app("charlie")).unwrap();
        std::fs::write(
            user.join("a.desktop"),
            "[Desktop Entry]\nType=Application\nName=Alpha\nExec=a\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(user.join("sub-b.desktop"), app("Bravo-user")).unwrap();
        std::fs::write(user.join("notes.txt"), "x").unwrap();
        let got = desktop_entries(&[user, sys, root.join("missing")]);
        let names: Vec<&str> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Bravo-user", "charlie"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
