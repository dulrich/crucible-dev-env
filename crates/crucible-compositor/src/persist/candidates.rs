//! The picker candidates: live workspaces (in order), then saved ones not live, then folders under
//! the configured roots, with slot pins filled in. The pure builders are unit-tested here; the
//! `impl CrucibleState` block only gathers their inputs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crucible_persist::store;

use crate::ipc::{Candidate, CandidateKind};
use crate::state::{Backend, CrucibleState};

/// Immediate, non-hidden subdirectories of each root as `(basename, path)`, each root's entries
/// sorted by name, roots in order. Unreadable roots and non-UTF-8 names are skipped.
#[must_use]
pub fn folder_entries(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let path = e.path();
                (!name.starts_with('.') && path.is_dir()).then_some((name, path))
            })
            .collect();
        dirs.sort();
        out.extend(dirs);
    }
    out
}

/// The `candidates` list: live (in the given order), then saved not live, then folders.
#[must_use]
pub fn build_candidates(
    live: Vec<(String, Option<PathBuf>)>,
    saved: Vec<(String, Option<PathBuf>)>,
    folders: Vec<(String, PathBuf)>,
) -> Vec<Candidate> {
    let live_names: HashSet<String> = live.iter().map(|(n, _)| n.clone()).collect();
    let mut out: Vec<Candidate> = live
        .into_iter()
        .map(|(name, path)| Candidate {
            kind: CandidateKind::Live,
            name,
            path,
            slot: None,
        })
        .collect();
    out.extend(
        saved
            .into_iter()
            .filter(|(n, _)| !live_names.contains(n))
            .map(|(name, root)| Candidate {
                kind: CandidateKind::Saved,
                name,
                path: root,
                slot: None,
            }),
    );
    out.extend(folders.into_iter().map(|(name, path)| Candidate {
        kind: CandidateKind::Folder,
        name,
        path: Some(path),
        slot: None,
    }));
    out
}

/// The root of each saved file, cached by file path and modification time: a file is parsed only
/// when it is new or has changed since it was last read.
#[derive(Debug, Default)]
pub struct SavedRootCache {
    entries: HashMap<PathBuf, (SystemTime, Option<PathBuf>)>,
}

impl SavedRootCache {
    /// The root of `file` as of `mtime`: the cached one when `mtime` matches the cached read, else
    /// `load()` (cached under `mtime`). With no `mtime` the file is always loaded and never cached.
    pub fn root(
        &mut self,
        file: &Path,
        mtime: Option<SystemTime>,
        load: impl FnOnce() -> Option<PathBuf>,
    ) -> Option<PathBuf> {
        let Some(mtime) = mtime else { return load() };
        if let Some((seen, root)) = self.entries.get(file) {
            if *seen == mtime {
                return root.clone();
            }
        }
        let root = load();
        self.entries
            .insert(file.to_path_buf(), (mtime, root.clone()));
        root
    }

    /// Forgets every file not in `listed` (deleted saves).
    pub fn retain_listed(&mut self, listed: &[(String, PathBuf)]) {
        self.entries
            .retain(|file, _| listed.iter().any(|(_, f)| f == file));
    }
}

/// Saved workspaces with their roots: the `cached` record's root when `cached` knows the name
/// (`Some(root)`), else `file_root` of the saved file itself (an unreadable file just has no root).
#[must_use]
pub fn saved_with_roots(
    listed: Vec<(String, PathBuf)>,
    cached: impl Fn(&str) -> Option<Option<PathBuf>>,
    mut file_root: impl FnMut(&Path) -> Option<PathBuf>,
) -> Vec<(String, Option<PathBuf>)> {
    listed
        .into_iter()
        .map(|(name, file)| {
            let root = cached(&name).unwrap_or_else(|| file_root(&file));
            (name, root)
        })
        .collect()
}

/// Fills `slot` on live and saved candidates from `slot_of` (folders never carry one).
pub fn apply_slots(candidates: &mut [Candidate], slot_of: impl Fn(&str) -> Option<u8>) {
    for c in candidates
        .iter_mut()
        .filter(|c| c.kind != CandidateKind::Folder)
    {
        c.slot = slot_of(&c.name);
    }
}

impl<B: Backend + 'static> CrucibleState<B> {
    pub(super) fn candidates(&mut self) -> Vec<Candidate> {
        let world = &self.layout.world;
        let live = self
            .live_workspaces()
            .into_iter()
            .filter_map(|(_, ws)| {
                let name = world.workspace(ws)?.name.clone();
                Some((name, self.persist.workspace.root(ws).cloned()))
            })
            .collect();
        let listed = store::list_saved(&self.persist.store_dir);
        self.persist.picker_roots.retain_listed(&listed);
        let (saved_cache, picker_roots) = (&self.persist.saved, &mut self.persist.picker_roots);
        let saved = saved_with_roots(
            listed,
            |n| saved_cache.get(n).map(|(r, _)| r.root.clone()),
            |file| {
                let mtime = std::fs::metadata(file).and_then(|m| m.modified()).ok();
                picker_roots.root(file, mtime, || store::load(file).ok().and_then(|r| r.root))
            },
        );
        let mut out = build_candidates(live, saved, folder_entries(&self.config.picker.roots));
        if self.config.picker.slots {
            apply_slots(&mut out, |n| self.persist.slots.slot_of(n));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::tests::{pane, record};

    #[test]
    fn candidates_order_live_saved_folder_and_dedupe_live() {
        let c = build_candidates(
            vec![("b".into(), None), ("a".into(), Some("/r".into()))],
            vec![("a".into(), None), ("z".into(), Some("/zroot".into()))],
            vec![("proj".into(), "/code/proj".into())],
        );
        let got: Vec<(CandidateKind, &str)> = c.iter().map(|c| (c.kind, c.name.as_str())).collect();
        assert_eq!(
            got,
            [
                (CandidateKind::Live, "b"),
                (CandidateKind::Live, "a"),
                (CandidateKind::Saved, "z"),
                (CandidateKind::Folder, "proj"),
            ]
        );
        assert_eq!(c[2].path, Some(PathBuf::from("/zroot")));
    }

    #[test]
    fn saved_roots_prefer_the_cache_and_fall_back_to_the_file() {
        let dir = std::env::temp_dir().join(format!("cde-savedroots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = record(Some("/on/disk"), pane(vec![]));
        rec.name = "disk".into();
        let file = store::save(&dir, &rec).unwrap();
        let listed = vec![
            ("disk".to_string(), file),
            ("cached".to_string(), dir.join("missing.kdl")),
            ("broken".to_string(), dir.join("missing2.kdl")),
        ];
        let loads = std::cell::Cell::new(0);
        let mut cache = SavedRootCache::default();
        let mtime = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        let mut read = |listed: Vec<(String, PathBuf)>, mtime| {
            saved_with_roots(
                listed,
                |n| match n {
                    "cached" => Some(Some(PathBuf::from("/cached"))),
                    _ => None,
                },
                |file| {
                    cache.root(file, Some(mtime), || {
                        loads.set(loads.get() + 1);
                        store::load(file).ok().and_then(|r| r.root)
                    })
                },
            )
        };
        let got = read(listed.clone(), mtime);
        assert_eq!(got[0].1, Some(PathBuf::from("/on/disk")));
        assert_eq!(got[1].1, Some(PathBuf::from("/cached")));
        assert_eq!(got[2].1, None);
        assert_eq!(loads.get(), 2); // "cached" is answered without reading its file
        // Unchanged mtime: no file is parsed again.
        assert_eq!(read(listed.clone(), mtime), got);
        assert_eq!(loads.get(), 2);
        // A changed mtime re-reads exactly the changed file.
        let mut rec = record(Some("/moved"), pane(vec![]));
        rec.name = "disk".into();
        store::save(&dir, &rec).unwrap();
        let later = mtime + std::time::Duration::from_secs(1);
        let got = read(listed, later);
        assert_eq!(got[0].1, Some(PathBuf::from("/moved")));
        assert_eq!(loads.get(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_slots_fills_live_and_saved_but_never_folders() {
        let mut c = build_candidates(
            vec![("a".into(), None)],
            vec![("z".into(), None)],
            vec![("a".into(), "/code/a".into())],
        );
        assert!(c.iter().all(|c| c.slot.is_none()));
        apply_slots(&mut c, |n| match n {
            "a" => Some(2),
            "z" => Some(9),
            _ => None,
        });
        let got: Vec<(CandidateKind, Option<u8>)> = c.iter().map(|c| (c.kind, c.slot)).collect();
        assert_eq!(
            got,
            [
                (CandidateKind::Live, Some(2)),
                (CandidateKind::Saved, Some(9)),
                (CandidateKind::Folder, None),
            ]
        );
    }

    #[test]
    fn folder_entries_lists_sorted_visible_subdirectories() {
        let root = std::env::temp_dir().join(format!("cde-folders-{}", std::process::id()));
        for d in ["b", "a", ".hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("file"), b"").unwrap();
        let got = folder_entries(&[root.clone(), root.join("missing")]);
        let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(got[0].1, root.join("a"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
