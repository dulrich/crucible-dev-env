//! `Stack<T>`: the one ordered-stack-with-a-focused-item primitive behind both a [`super::Pane`]'s
//! window stack and an [`super::Output`]'s workspace stack (decision #3 — "stack" is one concept).
//!
//! Its fields are private so every index fix-up lives here; the focus index is never out of range
//! (an empty stack's focus is 0).

/// An ordered stack of items with exactly one focused index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack<T> {
    items: Vec<T>,
    focused: usize,
}

/// [`Stack::set_focus`] was given an index not below the stack length (an empty stack accepts
/// only 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutOfRange {
    pub(crate) len: usize,
}

impl<T> Default for Stack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Stack<T> {
    /// An empty stack (focus 0).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            focused: 0,
        }
    }

    /// A one-item stack focused on `item`.
    #[must_use]
    pub(crate) fn of(item: T) -> Self {
        Self {
            items: vec![item],
            focused: 0,
        }
    }

    /// Pushes `item` on the end and focuses it.
    pub(crate) fn push(&mut self, item: T) {
        self.items.push(item);
        self.focused = self.items.len() - 1;
    }

    /// Inserts `item` at `min(index, len)`, keeping the focused *item* focused (inserting at or
    /// before the focused index increments it). Inserting into an empty stack focuses index 0.
    pub(crate) fn insert(&mut self, index: usize, item: T) {
        let at = index.min(self.items.len());
        if !self.items.is_empty() && at <= self.focused {
            self.focused += 1;
        }
        self.items.insert(at, item);
    }

    /// Appends `items` without changing which item is focused.
    pub(crate) fn extend(&mut self, items: impl IntoIterator<Item = T>) {
        self.items.extend(items);
    }

    /// Removes and returns the item at `index` (`None` if out of range). A removal below focus
    /// decrements focus so the same item stays focused; removing the focused item keeps the index,
    /// clamped to the new end; an emptied stack has focus 0.
    pub(crate) fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let item = self.items.remove(index);
        if index < self.focused {
            self.focused -= 1;
        } else if self.focused >= self.items.len() {
            self.focused = self.items.len().saturating_sub(1);
        }
        Some(item)
    }

    /// Focuses `index`. Errors when `index` is not below the length (an empty stack accepts 0).
    pub(crate) fn set_focus(&mut self, index: usize) -> Result<(), OutOfRange> {
        let len = self.items.len();
        if index >= len.max(1) {
            return Err(OutOfRange { len });
        }
        self.focused = index;
        Ok(())
    }

    /// Rotates focus by `delta` steps with wraparound (no-op on an empty stack).
    pub(crate) fn cycle(&mut self, delta: i64) {
        let len = self.items.len();
        if len == 0 {
            return;
        }
        let len_i = len as i64;
        self.focused = (self.focused as i64 + delta).rem_euclid(len_i) as usize;
    }

    /// Moves the item at `from` to index `to` (clamped to the last index), keeping the same item
    /// focused. Returns the final index, or `None` when `from` is out of range.
    pub(crate) fn move_to(&mut self, from: usize, to: usize) -> Option<usize> {
        let len = self.items.len();
        if from >= len {
            return None;
        }
        let to = to.min(len - 1);
        let item = self.items.remove(from);
        self.items.insert(to, item);
        if self.focused == from {
            self.focused = to;
        } else if from < self.focused && self.focused <= to {
            self.focused -= 1;
        } else if to <= self.focused && self.focused < from {
            self.focused += 1;
        }
        Some(to)
    }

    /// The focused item, or `None` for an empty stack.
    #[must_use]
    pub fn focused(&self) -> Option<&T> {
        self.items.get(self.focused)
    }

    /// The focused index (0 for an empty stack).
    #[must_use]
    pub fn focused_index(&self) -> usize {
        self.focused
    }

    /// Whether the stack holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items in stack order.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.items
    }

    /// Consumes the stack, returning its items in stack order.
    #[must_use]
    pub(crate) fn into_vec(self) -> Vec<T> {
        self.items
    }
}

impl<T: PartialEq> Stack<T> {
    /// The index of `item`, if present.
    #[must_use]
    pub fn position(&self, item: &T) -> Option<usize> {
        self.items.iter().position(|i| i == item)
    }

    /// Removes `item` if present (same focus fix-up as [`Stack::remove`]); returns whether it was.
    pub(crate) fn remove_item(&mut self, item: &T) -> bool {
        match self.position(item) {
            Some(idx) => self.remove(idx).is_some(),
            None => false,
        }
    }

    /// Focuses `item` if present; returns whether it was.
    pub(crate) fn focus_item(&mut self, item: &T) -> bool {
        match self.position(item) {
            Some(idx) => {
                self.focused = idx;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abc(focus: usize) -> Stack<char> {
        let mut s = Stack::new();
        for c in ['a', 'b', 'c'] {
            s.push(c);
        }
        s.set_focus(focus).unwrap();
        s
    }

    /// Ported from the removed `cycle_index` unit test (same cases).
    #[test]
    fn cycle_wraps_forward_and_back() {
        let at = |focus: usize, delta: i64| {
            let mut s = abc(focus);
            s.cycle(delta);
            s.focused_index()
        };
        assert_eq!(at(0, 1), 1);
        assert_eq!(at(2, 1), 0);
        assert_eq!(at(0, -1), 2);
        assert_eq!(at(0, 4), 1);
        let mut e: Stack<u8> = Stack::new();
        e.cycle(1);
        assert_eq!(e.focused_index(), 0);
        let mut one = Stack::of(1u8);
        one.cycle(1);
        assert_eq!(one.focused_index(), 0);
    }

    #[test]
    fn push_focuses_new_item() {
        let mut s = Stack::new();
        assert_eq!(s.focused(), None);
        s.push(1);
        s.push(2);
        assert_eq!(s.focused(), Some(&2));
        assert_eq!(s.focused_index(), 1);
    }

    #[test]
    fn insert_at_or_before_focus_keeps_focused_item() {
        let mut s = abc(1);
        s.insert(1, 'x');
        assert_eq!(s.as_slice(), &['a', 'x', 'b', 'c']);
        assert_eq!(s.focused(), Some(&'b'));
        s.insert(99, 'y');
        assert_eq!(s.focused(), Some(&'b'));
        assert_eq!(s.as_slice().last(), Some(&'y'));
        let mut e = Stack::new();
        e.insert(5, 'z');
        assert_eq!(e.focused(), Some(&'z'));
    }

    #[test]
    fn remove_below_focus_decrements() {
        let mut s = abc(1);
        assert_eq!(s.remove(0), Some('a'));
        assert_eq!(s.focused(), Some(&'b'));
        assert_eq!(s.focused_index(), 0);
    }

    #[test]
    fn remove_focused_at_end_clamps_and_down_to_empty() {
        let mut s = abc(2);
        assert!(s.remove_item(&'c'));
        assert_eq!(s.focused(), Some(&'b'));
        let mut s = abc(1);
        s.remove(1);
        assert_eq!(s.focused(), Some(&'c'), "same index shows next item");
        s.remove(0);
        s.remove(0);
        assert!(s.is_empty());
        assert_eq!(s.focused_index(), 0);
        assert_eq!(s.remove(0), None);
    }

    #[test]
    fn set_focus_range_and_cycle_wraps() {
        let mut s = abc(0);
        assert_eq!(s.set_focus(3), Err(OutOfRange { len: 3 }));
        s.cycle(-1);
        assert_eq!(s.focused_index(), 2);
        s.cycle(4);
        assert_eq!(s.focused_index(), 0);
        let mut e: Stack<u8> = Stack::new();
        assert!(e.set_focus(0).is_ok());
        assert!(e.set_focus(1).is_err());
        e.cycle(1);
        assert_eq!(e.focused_index(), 0);
        assert_eq!(abc(0).position(&'c'), Some(2));
    }
}
