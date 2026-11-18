//! The rename path: `ipc_rename` and its save/finish steps.

use super::*;

impl<B: Backend + 'static> CrucibleState<B> {
    pub(in crate::persist) fn ipc_rename(&mut self, new: &str) -> Result<(), String> {
        let ws = self.layout.world.active_workspace_id();
        let old = self
            .layout
            .world
            .workspace(ws)
            .map(|w| w.name.clone())
            .ok_or_else(|| "no active workspace".to_string())?;
        if old == new {
            return Ok(());
        }
        if self.name_taken(new, &self.persist.saved_names()) {
            return Err(format!("a workspace named {new} already exists"));
        }
        self.persist.file_of(new)?;
        let old_file = self.persist.file_of(&old).ok();
        // Step 1, the only fallible write: the record under the new name. Nothing else has
        // changed yet, so a failure here leaves the model, the pins, `saved` and the old file.
        let renamed = match old_file.as_ref().is_some_and(|f| f.exists()) {
            true => {
                let record = self
                    .capture_record(ws)
                    .record()
                    .ok_or_else(|| "workspace vanished".to_string())?;
                Some(save_renamed(&self.persist.store_dir, record, new)?)
            }
            false => None,
        };
        if let Err(e) = self.with_model(|w| w.rename_workspace(ws, new)) {
            if renamed.is_some() {
                if let Ok(f) = self.persist.file_of(new) {
                    let _ = std::fs::remove_file(f);
                }
            }
            return Err(e.to_string());
        }
        // Pins follow a user-driven rename (and only that one).
        if finish_rename(
            &mut self.persist.slots,
            &mut self.persist.saved,
            &old,
            new,
            renamed,
        ) {
            self.save_slots();
        }
        if let Some(f) = old_file {
            let _ = std::fs::remove_file(f);
        }
        info!(from = old, to = new, "workspace renamed");
        Ok(())
    }
}

/// Writes `record` under the name `new` and returns it renamed; the one fallible step of a rename.
fn save_renamed(
    store_dir: &Path,
    mut record: WorkspaceRecord,
    new: &str,
) -> Result<WorkspaceRecord, String> {
    record.name = new.to_string();
    store::save(store_dir, &record).map_err(|e| e.to_string())?;
    Ok(record)
}

/// The infallible tail of a rename, run once the new record is on disk and the model is renamed:
/// moves the slot pins and the `saved` entry from `old` to `new`. `renamed` is the record that
/// was written under `new` (`None` when the workspace had no file). Returns whether any pin moved.
fn finish_rename(
    slots: &mut Slots,
    saved: &mut HashMap<String, (WorkspaceRecord, Instant)>,
    old: &str,
    new: &str,
    renamed: Option<WorkspaceRecord>,
) -> bool {
    saved.remove(old);
    if let Some(record) = renamed {
        saved.insert(new.to_string(), (record, Instant::now()));
    }
    slots.rename(old, new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_persist::{PaneRecord, RecordNode};

    fn record(name: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            name: name.into(),
            root: Some("/r".into()),
            focused_pane: 0,
            output: None,
            tree: RecordNode::Pane(PaneRecord {
                focused: 0,
                windows: vec![],
            }),
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cde-rename-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_failed_save_under_the_new_name_changes_nothing() {
        // A regular file where the store dir should be: every save fails.
        let dir = scratch("fail");
        let blocked = dir.join("store");
        std::fs::write(&blocked, b"x").unwrap();
        let old_file = dir.join("old-file");
        std::fs::write(&old_file, b"x").unwrap();
        let mut slots = Slots::new();
        slots.pin(1, "old");
        let saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        let err = save_renamed(&blocked, record("old"), "new");
        // `ipc_rename` stops at this `Err` before the model, pins, `saved` or old file are
        // touched; the Smithay-bound method itself is not constructible in a test.
        assert!(err.is_err());
        assert_eq!(slots.get(1), Some("old"));
        assert!(saved.contains_key("old") && old_file.exists());
        assert!(blocked.is_file(), "the store path is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_rename_writes_the_new_file_and_moves_pins_and_saved() {
        let dir = scratch("ok");
        let old_path = store::save(&dir, &record("old")).unwrap();
        let mut slots = Slots::new();
        slots.pin(2, "old");
        let mut saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        let renamed = save_renamed(&dir, record("old"), "new").unwrap();
        let new_path = dir.join(store::encode_file_name("new").unwrap());
        assert!(finish_rename(
            &mut slots,
            &mut saved,
            "old",
            "new",
            Some(renamed)
        ));
        std::fs::remove_file(&old_path).unwrap();
        assert!(new_path.exists() && !old_path.exists());
        assert_eq!(slots.get(2), Some("new"));
        assert!(!saved.contains_key("old") && saved["new"].0.name == "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_without_a_file_moves_pins_but_saves_nothing() {
        let mut slots = Slots::new();
        slots.pin(3, "old");
        let mut saved = HashMap::from([("old".to_string(), (record("old"), Instant::now()))]);
        assert!(finish_rename(&mut slots, &mut saved, "old", "new", None));
        assert_eq!(slots.get(3), Some("new"));
        assert!(saved.is_empty());
    }
}
