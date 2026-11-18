//! Terminal and app tiling: gaps, rings, the per-terminal tiling args and the ring clamps.

use super::*;

/// Tiling of CDE-spawned terminals, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalTiling {
    /// Space between the tile edge and the terminal content; the terminal draws it as its own
    /// padding.
    pub gap: u32,
    /// Focus-ring thickness; never more than `gap` (clamped at parse time).
    pub ring: u32,
    /// Distance the focus ring and `frame` are drawn inside the tile edge; with `ring` never more
    /// than `gap` (clamped at parse time).
    pub ring_inset: u32,
    /// Colour of the frame drawn at the ring position on unfocused tiles. `None` (default) draws
    /// no frame.
    pub frame: Option<Rgb>,
}

/// Tiling of every other managed window, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppTiling {
    /// Space between the tile edge and the window; CDE insets the window and paints the inset.
    pub gap: u32,
    /// Focus-ring thickness; never more than `gap` (clamped at parse time).
    pub ring: u32,
    /// Colour painted over the part of the inset the ring does not cover.
    pub fill: Rgb,
}

/// Per-class tiling: terminals fill their tile, apps are inset by their gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilingConfig {
    /// CDE-spawned terminals.
    pub terminal: TerminalTiling,
    /// Every other managed window.
    pub app: AppTiling,
    /// The program-less `terminal-args` node (`{gap}` = the terminal gap, see [`substitute_gap`]),
    /// inserted before the exec flag on spawns of the configured terminal. `None`: no node (the
    /// built-in default applies); `Some(vec![])`: an explicit empty node (no arguments).
    pub terminal_args: Option<Vec<String>>,
    /// `terminal-args program="<basename>"` nodes: `(basename, args)`, one per program (a repeated
    /// node keeps the last). An empty list disables that program's built-in default.
    pub program_terminal_args: Vec<(String, Vec<String>)>,
}

/// The built-in `tiling.terminal-args` for a spawned terminal of `kind` (`{gap}` unsubstituted):
/// foot-style padding for `cterm`/`foot`, urxvt's internal border, ghostty's window padding;
/// nothing for [`TerminalKind::Other`] (`footclient` and `urxvtc` included). Pure.
#[must_use]
pub fn builtin_tiling_args(kind: TerminalKind) -> &'static [&'static str] {
    match kind {
        TerminalKind::Cterm | TerminalKind::Foot => &["-o", "pad={gap}x{gap} center"],
        TerminalKind::Urxvt => &["-b", "{gap}"],
        TerminalKind::Ghostty => &[
            "--window-padding-x={gap}",
            "--window-padding-y={gap}",
            "--window-padding-color=extend-always",
            "--window-padding-balance=true",
        ],
        TerminalKind::Other => &[],
    }
}

impl Default for TilingConfig {
    /// Terminal gap 6 / ring 3; app gap 0 / ring 0 with the muted chrome grey (`#737373`, the
    /// inactive stack-pip colour) as fill.
    fn default() -> Self {
        Self {
            terminal: TerminalTiling {
                gap: 6,
                ring: 3,
                ring_inset: 0,
                frame: None,
            },
            app: AppTiling {
                gap: 0,
                ring: 0,
                fill: Rgb(0x73, 0x73, 0x73),
            },
            terminal_args: None,
            program_terminal_args: Vec::new(),
        }
    }
}

impl TilingConfig {
    /// The terminal args for a spawn of the program whose basename is `program_basename`, with
    /// `{gap}` replaced by the terminal gap. Order: a `program=` node equal to the basename; else
    /// the program-less node, only when `is_configured_terminal`; else the built-in default
    /// ([`builtin_tiling_args`]). An explicit node, even an empty one, replaces the default for the
    /// program(s) it applies to. Pure.
    #[must_use]
    pub fn effective_terminal_args(
        &self,
        program_basename: &str,
        is_configured_terminal: bool,
    ) -> Vec<String> {
        let gap = self.terminal.gap;
        if let Some((_, args)) = self
            .program_terminal_args
            .iter()
            .find(|(p, _)| p == program_basename)
        {
            return substitute_gap(args, gap);
        }
        if is_configured_terminal && let Some(args) = &self.terminal_args {
            return substitute_gap(args, gap);
        }
        builtin_tiling_args(TerminalKind::of(program_basename))
            .iter()
            .map(|a| a.replace("{gap}", &gap.to_string()))
            .collect()
    }

    /// The arguments a tiling terminal spawn of `program` (`configured` when it is the configured
    /// terminal) puts before the exec flag: [`Self::effective_terminal_args`] for its basename,
    /// then the `theme`'s spawn arguments (ghostty's `--config-file=` into `theme_dir`; nothing
    /// without a `theme`). **Pure.**
    #[must_use]
    pub fn spawn_args(
        &self,
        program: &str,
        configured: bool,
        theme: Option<&crate::theme::ThemeConfig>,
        theme_dir: Option<&Path>,
    ) -> Vec<String> {
        let basename = Path::new(program)
            .file_name()
            .and_then(|b| b.to_str())
            .unwrap_or(program);
        let mut args = self.effective_terminal_args(basename, configured);
        args.extend(crate::theme::theme_spawn_args(
            theme,
            TerminalKind::of(program),
            theme_dir,
        ));
        args
    }
}

/// Replaces every `{gap}` in `args` with `gap`. Pure.
#[must_use]
pub fn substitute_gap(args: &[String], gap: u32) -> Vec<String> {
    let gap = gap.to_string();
    args.iter().map(|a| a.replace("{gap}", &gap)).collect()
}

/// `(ring, clamped)`: `ring` limited to `gap`; `clamped` is whether it was reduced. Pure.
#[must_use]
pub fn clamp_ring(gap: u32, ring: u32) -> (u32, bool) {
    if ring > gap {
        (gap, true)
    } else {
        (ring, false)
    }
}

/// `(ring_inset, clamped)`: `ring_inset` limited to `gap - ring` (callers clamp `ring` to `gap`
/// first); `clamped` is whether it was reduced. Pure.
#[must_use]
pub fn clamp_ring_inset(gap: u32, ring: u32, ring_inset: u32) -> (u32, bool) {
    let max = gap.saturating_sub(ring);
    if ring_inset > max {
        (max, true)
    } else {
        (ring_inset, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_clamps_to_gap_and_gap_substitutes() {
        assert_eq!(clamp_ring(6, 3), (3, false));
        assert_eq!(clamp_ring(2, 9), (2, true));
        assert_eq!(clamp_ring_inset(9, 1, 4), (4, false));
        assert_eq!(clamp_ring_inset(6, 3, 5), (3, true));
        assert_eq!(clamp_ring_inset(2, 2, 1), (0, true));
        assert_eq!(
            substitute_gap(&["a={gap}-{gap}".to_string(), "b".to_string()], 7),
            vec!["a=7-7".to_string(), "b".to_string()]
        );
    }
}
