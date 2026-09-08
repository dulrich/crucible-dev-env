//! Output lifecycle: creation, removal (with workspace migration), stored geometry, and focus.

use super::*;
use crate::{OutputId, geometry::Rect};

impl World {
    /// Adds a new output with `rect`, giving it its own independent initial workspace + empty pane.
    ///
    /// Mirrors what [`World::new`] does for the first output. The new output starts with no windows
    /// and is **not** made the focused output, so nothing currently visible changes. Returns the new
    /// [`OutputId`].
    pub fn add_output(&mut self, rect: Rect) -> OutputId {
        let output_id = OutputId::from_raw(self.output_ids.alloc());
        let workspace_id = self.build_workspace("1");
        self.outputs.insert(
            output_id,
            Output {
                id: output_id,
                workspaces: vec![workspace_id],
                active: 0,
                rect,
            },
        );
        // New output is empty and not focused: nothing visible changes.
        output_id
    }

    /// Removes an output **without destroying any windows**.
    ///
    /// The removed output's entire workspace stack (and thus its panes and windows, which live in
    /// the arena maps and are never touched here) is **appended onto a surviving output's**
    /// workspace stack. The survivor is the focused output if it survives, else the lowest remaining
    /// [`OutputId`]. The migrated workspaces become inactive on the survivor, so their windows are
    /// hidden, not closed.
    ///
    /// If `id` was the focused output, focus repoints to the survivor. Returns
    /// [`LayoutError::LastOutput`] when asked to remove the only output, or
    /// [`LayoutError::UnknownOutput`] for an unknown id.
    pub fn remove_output(&mut self, id: OutputId) -> Result<(), LayoutError> {
        if !self.outputs.contains_key(&id) {
            return Err(LayoutError::UnknownOutput(id));
        }
        if self.outputs.len() == 1 {
            return Err(LayoutError::LastOutput);
        }

        // Pick the survivor: the focused output if it isn't the one being removed, else the lowest
        // remaining output id.
        let survivor = if self.focused_output != id {
            self.focused_output
        } else {
            self.outputs
                .keys()
                .copied()
                .find(|oid| *oid != id)
                .expect("≥1 other output remains")
        };

        let removed = self.outputs.remove(&id).expect("output present");
        // Migrate the removed output's workspaces (with their panes/windows) onto the survivor.
        // They become inactive (the survivor keeps its own active index), so windows are hidden.
        let survivor_output = self.outputs.get_mut(&survivor).expect("survivor present");
        survivor_output.workspaces.extend(removed.workspaces);

        // Repoint focus if we just removed the focused output.
        if self.focused_output == id {
            self.focused_output = survivor;
        }

        // Windows from the migrated (now inactive) workspaces are hidden, not removed.
        Ok(())
    }

    /// Sets the stored pixel geometry of `id`. Errors if the output is unknown.
    pub fn set_output_rect(&mut self, id: OutputId, rect: Rect) -> Result<(), LayoutError> {
        let output = self
            .outputs
            .get_mut(&id)
            .ok_or(LayoutError::UnknownOutput(id))?;
        output.rect = rect;
        Ok(())
    }

    /// Routes focus to `id` so subsequent window/workspace ops act on that output.
    ///
    /// The next derived [`super::Projection`] reports the newly focused output's focus. Errors if
    /// `id` is unknown.
    pub fn set_focused_output(&mut self, id: OutputId) -> Result<(), LayoutError> {
        if !self.outputs.contains_key(&id) {
            return Err(LayoutError::UnknownOutput(id));
        }
        self.focused_output = id;
        Ok(())
    }
}
