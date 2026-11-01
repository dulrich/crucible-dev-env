//! The one text-table layout every list overlay and `crucible mem` share: [`layout`] turns column
//! specs and per-row cells (already sanitised) into a header line and one string per row.

/// How a column's cells line up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Pad after the cell.
    Left,
    /// Pad before the cell.
    Right,
}

/// One column: its header text, alignment, and whether it is dropped when every cell is empty.
#[derive(Debug, Clone, Copy)]
pub struct Col<'a> {
    pub header: &'a str,
    pub align: Align,
    /// Drop the column (header included) when every row's cell is empty.
    pub hide_if_empty: bool,
}

impl<'a> Col<'a> {
    /// A left-aligned column that is always shown.
    #[must_use]
    pub fn left(header: &'a str) -> Col<'a> {
        Col {
            header,
            align: Align::Left,
            hide_if_empty: false,
        }
    }

    /// A right-aligned column that is always shown.
    #[must_use]
    pub fn right(header: &'a str) -> Col<'a> {
        Col {
            header,
            align: Align::Right,
            hide_if_empty: false,
        }
    }

    /// The same column, dropped when every cell is empty.
    #[must_use]
    pub fn hidden_if_empty(self) -> Col<'a> {
        Col {
            hide_if_empty: true,
            ..self
        }
    }
}

/// Lays out a table: each column is as wide as its widest cell (header included, measured in
/// chars like [`crate::overlay::term::fit`]) with two spaces between columns. `Left` pads after,
/// `Right` pads before; the last column is never padded after and trailing blanks are trimmed. A
/// `hide_if_empty` column whose cells are all empty is dropped; a row with fewer cells than
/// columns has empty cells.
#[must_use]
pub fn layout(cols: &[Col<'_>], rows: &[Vec<String>]) -> (String, Vec<String>) {
    let shown: Vec<(usize, &Col<'_>)> = cols
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !(c.hide_if_empty && rows.iter().all(|r| r.get(*i).is_none_or(String::is_empty)))
        })
        .collect();
    let widths: Vec<usize> = shown
        .iter()
        .map(|(i, c)| {
            rows.iter()
                .filter_map(|r| r.get(*i))
                .map(|cell| cell.chars().count())
                .fold(c.header.chars().count(), usize::max)
        })
        .collect();
    let line = |cells: &[&str]| -> String {
        let mut out = String::new();
        for (n, ((i, c), w)) in shown.iter().zip(&widths).enumerate() {
            let cell = cells.get(*i).copied().unwrap_or("");
            let pad = " ".repeat(w - cell.chars().count());
            match c.align {
                Align::Left => {
                    out.push_str(cell);
                    if n + 1 < shown.len() {
                        out.push_str(&pad);
                    }
                }
                Align::Right => {
                    out.push_str(&pad);
                    out.push_str(cell);
                }
            }
            if n + 1 < shown.len() {
                out.push_str("  ");
            }
        }
        out.trim_end().to_string()
    };
    let header = line(&cols.iter().map(|c| c.header).collect::<Vec<_>>());
    let body = rows
        .iter()
        .map(|r| line(&r.iter().map(String::as_str).collect::<Vec<_>>()))
        .collect();
    (header, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.iter().map(|c| (*c).to_string()).collect())
            .collect()
    }

    #[test]
    fn columns_pad_to_the_widest_cell_and_leave_the_last_unpadded() {
        let rows = cells(&[&["live", "a", "/x"], &["saved", "long name"]]);
        let cols = [Col::left("kind"), Col::left("name"), Col::left("root")];
        let (h, b) = layout(&cols, &rows);
        assert_eq!(h, "kind   name       root");
        assert_eq!(b, ["live   a          /x", "saved  long name"]);
    }

    #[test]
    fn right_aligned_columns_pad_before_and_the_header_counts_toward_width() {
        let rows = cells(&[&["1", "x"], &["1234", "yy"]]);
        let cols = [Col::right("N"), Col::left("what")];
        let (h, b) = layout(&cols, &rows);
        assert_eq!(h, "   N  what");
        assert_eq!(b, ["   1  x", "1234  yy"]);
    }

    #[test]
    fn a_last_right_column_is_padded_before_only() {
        let rows = cells(&[&["a", "9"], &["b", "10"]]);
        let (h, b) = layout(&[Col::left("k"), Col::right("n")], &rows);
        assert_eq!(h, "k   n");
        assert_eq!(b, ["a   9", "b  10"]);
    }

    #[test]
    fn an_all_empty_hide_if_empty_column_is_dropped_with_its_header() {
        let cols = [
            Col::left("kind"),
            Col::left("slot").hidden_if_empty(),
            Col::left("name"),
        ];
        let (h, b) = layout(&cols, &cells(&[&["live", "", "a"]]));
        assert_eq!(h, "kind  name");
        assert_eq!(b, ["live  a"]);
        let (h, b) = layout(&cols, &cells(&[&["live", "", "a"], &["saved", "3", "b"]]));
        assert_eq!(h, "kind   slot  name");
        assert_eq!(b, ["live         a", "saved  3     b"]);
    }

    #[test]
    fn no_rows_gives_a_header_of_the_header_widths() {
        let (h, b) = layout(&[Col::left("a"), Col::left("b").hidden_if_empty()], &[]);
        assert_eq!(h, "a");
        assert!(b.is_empty());
    }
}
