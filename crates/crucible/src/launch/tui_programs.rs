//! TUI program resolution for the launcher: the built-in list plus configured patterns, resolved
//! against `PATH` into absolute executables. Pure over the explicit directory list.

use std::collections::HashSet;
use std::path::PathBuf;

use crucible_persist::is_executable_file;

use super::desktop::{DesktopEntry, sq};
use super::{Item, LaunchClass};

/// TUI programs offered when found on `PATH`, beside any `launcher { tui }` config entries.
pub const BUILTIN_TUI: &[&str] = &[
    "htop",
    "btop",
    "nvtop",
    "top",
    "nano",
    "vim",
    "nvim",
    "mc",
    "ranger",
    "yazi",
    "lazygit",
    "tig",
    "ncdu",
    "claude",
    "codex",
    "glances",
    "bottom",
    "lazydocker",
    "k9s",
    "ncmpcpp",
    "weechat",
    "irssi",
    "cmus",
    "newsboat",
];

/// Whether `*` globbing `pattern` (`*` = any run of characters, possibly empty) matches `name`.
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// The built-in names plus the `:`-separated `configured` env value, deduped, built-ins first.
#[must_use]
pub fn tui_patterns(configured: Option<&std::ffi::OsStr>) -> Vec<String> {
    let mut out: Vec<String> = BUILTIN_TUI.iter().map(|s| (*s).to_string()).collect();
    if let Some(v) = configured.and_then(|v| v.to_str()) {
        for p in v.split(':').filter(|p| !p.is_empty()) {
            if !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Resolves exact names and `*` globs against `dirs` (pure over the explicit list). Exact names
/// take the first directory holding an executable regular file (symlinks followed); globs match
/// every executable basename, the earlier directory winning a duplicate. Relative hits are made
/// absolute (canonicalized against the cwd) or dropped. Returns `(name, absolute path)` deduped by
/// name and sorted by name. Unreadable directories are skipped.
#[must_use]
pub fn resolve_tui(patterns: &[String], dirs: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for pat in patterns {
        if pat.contains('*') {
            for dir in dirs {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    continue;
                };
                for e in rd.flatten() {
                    let Some(name) = e.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    if glob_match(pat, &name) && is_executable_file(&e.path()) {
                        found.entry(name).or_insert_with(|| e.path());
                    }
                }
            }
        } else if let Some(p) = dirs
            .iter()
            .map(|d| d.join(pat))
            .find(|p| is_executable_file(p))
        {
            found.entry(pat.clone()).or_insert(p);
        }
    }
    found
        .into_iter()
        .filter_map(|(name, p)| {
            let abs = if p.is_absolute() {
                p
            } else {
                std::fs::canonicalize(&p).ok()?
            };
            Some((name, abs))
        })
        .collect()
}

/// TUI rows for the resolved programs: skipped for any program whose name equals the basename of
/// a desktop entry's `Exec` first word (wrapper commands such as `env … vim` are not recognised).
/// The command is the quoted absolute path (the compositor wraps it in the terminal via
/// `run-tui`). Terminal gating is applied once, over the combined list, in `build_items`.
#[must_use]
pub fn tui_items(programs: &[(String, PathBuf)], entries: &[DesktopEntry]) -> Vec<Item> {
    let desktop: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.exec.split_whitespace().next())
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .collect();
    programs
        .iter()
        .filter(|(name, _)| !desktop.contains(name.as_str()))
        .map(|(name, path)| Item {
            filter: name.clone(),
            label: name.clone(),
            class: LaunchClass::Tui,
            cmd: sq(&path.to_string_lossy()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-launch-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put(dir: &Path, name: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn glob_matching_is_star_only() {
        assert!(glob_match("codex-*", "codex-cli"));
        assert!(glob_match("codex-*", "codex-"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(!glob_match("codex-*", "codex"));
        assert!(glob_match("htop", "htop") && !glob_match("htop", "htop2"));
    }

    #[test]
    fn resolution_skips_non_executables_and_dirs_follows_symlinks_and_prefers_earlier() {
        let root = tmp("resolve");
        let (a, b, missing) = (root.join("a"), root.join("b"), root.join("nope"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        put(&a, "plain", 0o644);
        put(&b, "plain", 0o755);
        put(&a, "both", 0o755);
        put(&b, "both", 0o755);
        std::fs::create_dir(a.join("adir")).unwrap();
        put(&b, "real", 0o755);
        std::os::unix::fs::symlink(b.join("real"), a.join("link")).unwrap();
        put(&a, "tool-1", 0o755);
        put(&b, "tool-2", 0o755);
        put(&b, "tool-1", 0o755);
        put(&b, "tool-x", 0o600);
        let dirs = [missing, a.clone(), b.clone()];
        let got = resolve_tui(
            &pats(&["plain", "both", "adir", "link", "absent", "tool-*", "zzz-*"]),
            &dirs,
        );
        let want = vec![
            ("both".to_string(), a.join("both")),
            ("link".to_string(), a.join("link")),
            ("plain".to_string(), b.join("plain")),
            ("tool-1".to_string(), a.join("tool-1")),
            ("tool-2".to_string(), b.join("tool-2")),
        ];
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tui_patterns_merge_builtin_and_env() {
        let p = tui_patterns(Some(std::ffi::OsStr::new("k9s:foo-*::htop")));
        assert!(p.contains(&"htop".to_string()) && p.contains(&"foo-*".to_string()));
        assert_eq!(p.iter().filter(|x| *x == "htop").count(), 1);
        assert_eq!(tui_patterns(None).len(), BUILTIN_TUI.len());
    }

    #[test]
    fn tui_rows_dedupe_against_desktop_and_use_the_absolute_path() {
        let programs = vec![
            ("htop".to_string(), PathBuf::from("/usr/bin/htop")),
            ("k9s".to_string(), PathBuf::from("/opt/it's/k9s")),
        ];
        let entries = [DesktopEntry {
            name: "Htop".into(),
            exec: "/usr/bin/htop".into(),
            terminal: true,
            terminal_emulator: false,
        }];
        let items = tui_items(&programs, &entries);
        assert_eq!(items.len(), 1);
        assert!(items[0].class == LaunchClass::Tui && items[0].label == "k9s");
        assert_eq!(items[0].cmd, "'/opt/it'\\''s/k9s'");
        assert_eq!(tui_items(&programs, &[]).len(), 2);
    }
}
