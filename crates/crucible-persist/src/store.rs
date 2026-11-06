//! The on-disk workspace store: one `<encoded-name>.kdl` file per workspace.
//!
//! File names are the workspace name with `/`, `%`, NUL and a leading `.` percent-encoded (so a
//! name can never be a path, escape the directory, or hide as a dotfile) plus `.kdl`; the mapping
//! is reversible. Saves are atomic: a temp file in the same directory, then `rename`.

use crate::{PersistError, WorkspaceRecord};
use std::{
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// File extension of workspace files.
const EXT: &str = ".kdl";
/// Longest allowed encoded name (bytes), leaving room for the extension within NAME_MAX (255).
const MAX_ENCODED: usize = 250;

/// The default store directory: `$XDG_STATE_HOME/cde/workspaces/`, else
/// `$HOME/.local/state/cde/workspaces/` (empty variables count as unset). With neither set it
/// falls back to the relative `.local/state/cde/workspaces`.
#[must_use]
pub fn store_dir() -> PathBuf {
    store_dir_from(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure form of [`store_dir`] over explicit environment values.
#[must_use]
pub fn store_dir_from(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> PathBuf {
    let non_empty = |v: Option<&OsStr>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    let base = non_empty(xdg_state_home)
        .or_else(|| non_empty(home).map(|h| h.join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from(".local/state"));
    base.join("cde").join("workspaces")
}

/// Encodes a workspace name as a file name (including the `.kdl` extension).
///
/// Errors with [`PersistError::InvalidName`] for an empty name or one whose encoding is too long.
pub fn encode_file_name(name: &str) -> Result<String, PersistError> {
    if name.is_empty() {
        return Err(PersistError::InvalidName(name.to_string()));
    }
    let mut out = String::with_capacity(name.len() + EXT.len());
    for (i, c) in name.chars().enumerate() {
        match c {
            '/' | '%' | '\0' => out.push_str(&format!("%{:02X}", c as u8)),
            '.' if i == 0 => out.push_str("%2E"),
            _ => out.push(c),
        }
    }
    if out.len() > MAX_ENCODED {
        return Err(PersistError::InvalidName(name.to_string()));
    }
    out.push_str(EXT);
    Ok(out)
}

/// Decodes a file name produced by [`encode_file_name`]; `None` if it lacks the `.kdl` extension
/// or holds a malformed escape.
#[must_use]
pub fn decode_file_name(file: &str) -> Option<String> {
    let stem = file.strip_suffix(EXT)?;
    let bytes = stem.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = stem.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let name = String::from_utf8(out).ok()?;
    (!name.is_empty()).then_some(name)
}

fn io(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> PersistError {
    let path = path.to_path_buf();
    move |source| PersistError::Io { op, path, source }
}

/// Atomically saves `record` into `dir` (created if missing) and returns the file path.
///
/// The text is written to a temp file in `dir`, synced, then renamed over the target, so readers
/// never see a partial file.
pub fn save(dir: &Path, record: &WorkspaceRecord) -> Result<PathBuf, PersistError> {
    let text = record.to_kdl()?;
    let target = dir.join(encode_file_name(&record.name)?);
    write_atomic(dir, &target, &text)?;
    Ok(target)
}

/// Writes `text` to `target` (inside `dir`, created if missing) via a dot-prefixed temp file in
/// `dir`, synced, then renamed over the target; the temp file is removed on failure.
pub(crate) fn write_atomic(dir: &Path, target: &Path, text: &str) -> Result<(), PersistError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(dir).map_err(io("create directory", dir))?;
    // Starts with '.', which encoded names never do, so listings skip leftovers.
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        return Err(io("write", &tmp)(e));
    }
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(io("rename", target)(e));
    }
    Ok(())
}

/// The trash directory for a store: `<store parent>/deleted/` (a sibling of the store, never
/// inside it, so [`list_saved`] cannot see it). A store with no parent uses a relative `deleted`.
#[must_use]
pub fn deleted_dir(store_dir: &Path) -> PathBuf {
    store_dir
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join("deleted")
}

/// Deletes the saved workspace `name` by moving its file to [`deleted_dir`] (created if missing,
/// replacing an earlier trashed file of the same name). `Ok(false)` if there is no such record.
///
/// Uses `rename`, falling back to copy + remove when the trash is on another filesystem (`EXDEV`).
pub fn delete(dir: &Path, name: &str) -> Result<bool, PersistError> {
    let file = encode_file_name(name)?;
    let src = dir.join(&file);
    if !src.is_file() {
        return Ok(false);
    }
    let trash = deleted_dir(dir);
    fs::create_dir_all(&trash).map_err(io("create directory", &trash))?;
    let dst = trash.join(&file);
    if let Err(e) = fs::rename(&src, &dst) {
        if e.raw_os_error() != Some(18) {
            return Err(io("rename", &src)(e));
        }
        fs::copy(&src, &dst).map_err(io("copy", &dst))?;
        fs::remove_file(&src).map_err(io("remove", &src))?;
    }
    Ok(true)
}

/// Loads and parses the workspace file at `path`.
pub fn load(path: &Path) -> Result<WorkspaceRecord, PersistError> {
    let text = fs::read_to_string(path).map_err(io("read", path))?;
    WorkspaceRecord::from_kdl(&text)
}

/// Lists saved workspaces in `dir` as `(name, path)`, sorted by name. A missing directory yields
/// an empty list; files that are not `<encoded-name>.kdl` are ignored.
#[must_use]
pub fn list_saved(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file = e.file_name().into_string().ok()?;
            let name = decode_file_name(&file)?;
            e.file_type().ok()?.is_file().then(|| (name, e.path()))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{PaneRecord, RecordNode};

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cde-persist-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn rec(name: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            name: name.into(),
            root: None,
            focused_pane: 0,
            output: None,
            tree: RecordNode::Pane(PaneRecord {
                focused: 0,
                windows: vec![],
            }),
        }
    }

    #[test]
    fn store_dir_prefers_xdg_then_home() {
        let s = |v: &str| Some(OsStr::new(v).to_owned());
        let (x, h) = (s("/x"), s("/h"));
        assert_eq!(
            store_dir_from(x.as_deref(), h.as_deref()),
            PathBuf::from("/x/cde/workspaces")
        );
        assert_eq!(
            store_dir_from(None, h.as_deref()),
            PathBuf::from("/h/.local/state/cde/workspaces")
        );
        let empty = s("");
        assert_eq!(
            store_dir_from(empty.as_deref(), h.as_deref()),
            PathBuf::from("/h/.local/state/cde/workspaces")
        );
    }

    #[test]
    fn file_names_round_trip_and_are_path_safe() {
        for name in [
            "plain", "a/b", "100%", "x:y", ".hidden", "..", "a.b", "é ü", "%2F", "a\0b",
        ] {
            let f = encode_file_name(name).unwrap();
            assert!(!f.contains('/') && !f.contains('\0'), "{f}");
            assert!(!f.starts_with('.'), "{f}");
            assert!(f.ends_with(".kdl"));
            assert_eq!(decode_file_name(&f).as_deref(), Some(name), "{name}");
        }
        assert_eq!(encode_file_name("a/b").unwrap(), "a%2Fb.kdl");
        assert_eq!(encode_file_name(".x").unwrap(), "%2Ex.kdl");
        assert_eq!(encode_file_name("x:y").unwrap(), "x:y.kdl");
    }

    #[test]
    fn bad_names_and_files_are_rejected() {
        assert!(encode_file_name("").is_err());
        assert!(encode_file_name(&"a".repeat(300)).is_err());
        assert_eq!(decode_file_name("noext"), None);
        assert_eq!(decode_file_name("bad%zz.kdl"), None);
        assert_eq!(decode_file_name("trunc%2.kdl"), None);
        assert_eq!(decode_file_name(".kdl"), None);
    }

    #[test]
    fn save_load_list_round_trip_with_awkward_names() {
        let dir = temp_dir("rt").join("nested"); // save creates the directory
        let names = ["plain", "a/b", "100%", "x:y", ".dot"];
        for n in names {
            let p = save(&dir, &rec(n)).unwrap();
            assert_eq!(p.parent().unwrap(), dir);
            assert_eq!(load(&p).unwrap(), rec(n));
        }
        let listed = list_saved(&dir);
        let mut want: Vec<&str> = names.to_vec();
        want.sort_unstable();
        assert_eq!(
            listed.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            want
        );
        // Overwrite is atomic-in-place and leaves no temp files.
        save(&dir, &rec("plain")).unwrap();
        let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), names.len());
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn delete_moves_the_record_into_deleted_and_unlists_it() {
        let root = temp_dir("del");
        let dir = root.join("workspaces");
        save(&dir, &rec("a/b")).unwrap();
        save(&dir, &rec("keep")).unwrap();
        assert!(delete(&dir, "a/b").unwrap());
        let moved = root.join("deleted").join("a%2Fb.kdl");
        assert_eq!(load(&moved).unwrap(), rec("a/b"));
        assert!(!dir.join("a%2Fb.kdl").exists());
        let names: Vec<_> = list_saved(&dir).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["keep"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_missing_is_false_and_creates_nothing() {
        let root = temp_dir("delmiss");
        let dir = root.join("workspaces");
        fs::create_dir_all(&dir).unwrap();
        assert!(!delete(&dir, "nope").unwrap());
        assert!(!root.join("deleted").exists());
        assert!(delete(&dir, "").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn second_delete_replaces_the_earlier_trash_file() {
        let root = temp_dir("deltwice");
        let dir = root.join("workspaces");
        let mut first = rec("w");
        first.focused_pane = 1;
        save(&dir, &first).unwrap();
        assert!(delete(&dir, "w").unwrap());
        save(&dir, &rec("w")).unwrap();
        assert!(delete(&dir, "w").unwrap());
        let trashed = load(&root.join("deleted").join("w.kdl")).unwrap();
        assert_eq!(trashed, rec("w"));
        assert_eq!(fs::read_dir(root.join("deleted")).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_saved_ignores_junk_and_missing_dir() {
        assert!(list_saved(Path::new("/nonexistent/cde-test-dir")).is_empty());
        let dir = temp_dir("junk");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join(".tmp-1-1"), "x").unwrap();
        fs::write(dir.join("bad%zz.kdl"), "x").unwrap();
        fs::create_dir(dir.join("subdir.kdl")).unwrap();
        fs::write(dir.join("ok.kdl"), "x").unwrap();
        let listed = list_saved(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "ok");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reports_missing_and_malformed() {
        let dir = temp_dir("load");
        fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            load(&dir.join("nope.kdl")),
            Err(PersistError::Io { .. })
        ));
        fs::write(dir.join("bad.kdl"), "version {{{").unwrap();
        assert!(matches!(
            load(&dir.join("bad.kdl")),
            Err(PersistError::Parse { .. })
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
