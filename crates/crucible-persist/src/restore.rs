//! Restore planning, the inverse of capture: [`prepare_restore`] resolves every fallible restore
//! precondition and turns a [`WorkspaceRecord`] into one [`SpawnJob`] per restorable window
//! (directory, terminal argv built through [`Invocation`], target pane, origin) before the
//! compositor touches its model. Pure: nothing here spawns; the compositor applies the jobs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::invocation::{Invocation, RelaunchWrapper, ResumeMode, ResumeWrapper};
use crate::record::{WindowRecord, WorkspaceRecord};

/// A restored terminal's working directory: the saved cwd, else the workspace root, else `$HOME`,
/// else `/`.
#[must_use]
pub fn resolve_cwd(
    saved: &Path,
    root: Option<&Path>,
    home: Option<&Path>,
    is_dir: impl Fn(&Path) -> bool,
) -> PathBuf {
    [Some(saved), root, home]
        .into_iter()
        .flatten()
        .find(|p| is_dir(p))
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// One restore spawn, planned before the model exists: `pane_index` indexes `record.panes()`
/// (the order `World::insert_workspace` returns pane ids in) and is bound to a `PaneId` after
/// the insert.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnJob {
    /// The spawn directory.
    pub dir: PathBuf,
    /// The child's argv (before `with_tiling_args`).
    pub args: Vec<OsString>,
    /// Index of the target pane in tree order.
    pub pane_index: usize,
    /// The window's index in the saved stack.
    pub saved_index: usize,
    /// Whether it was the pane's focused window.
    pub focused: bool,
    /// The saved record the child is spawned from (its provenance).
    pub origin: WindowRecord,
}

/// The inverse of capture: one [`SpawnJob`] per restorable window of `record`, in pane order.
/// Terminals resolve their directory by saved cwd, root, `home`, `/`; apps by root, `home`, `/`
/// (each candidate checked with `is_dir`). Apps without a saved command line are skipped.
#[must_use]
pub fn plan_spawns(
    record: &WorkspaceRecord,
    home: Option<&Path>,
    crucible: &Path,
    exec_flag: &str,
    is_dir: impl Fn(&Path) -> bool,
) -> Vec<SpawnJob> {
    let root = record.root.as_deref();
    let mut jobs = Vec::new();
    for (pane_index, pane) in record.panes().into_iter().enumerate() {
        for (saved_index, window) in pane.windows.iter().enumerate() {
            let (dir, args) = match window {
                WindowRecord::Terminal { cwd, command } => {
                    let dir = resolve_cwd(cwd, root, home, &is_dir);
                    // `<exec-flag> <crucible> resume --cwd <dir> -- <command>` (the command is ONE
                    // element), or no arguments at all for an idle shell.
                    let args = match command {
                        Some(command) => Invocation::Resume(ResumeWrapper {
                            cwd: dir.clone(),
                            command: Some(command.clone()),
                            mode: ResumeMode::Banner,
                        })
                        .terminal_argv(exec_flag, crucible),
                        None => Vec::new(),
                    };
                    (dir, args)
                }
                WindowRecord::App {
                    app_id,
                    cmdline: Some(cmdline),
                } => {
                    let dir = app_cwd(root, home, &is_dir);
                    // `<exec-flag> <crucible> relaunch [--app-id <id>] -- <cmdline>`.
                    let args = Invocation::Relaunch(RelaunchWrapper {
                        app_id: app_id.clone(),
                        cmdline: cmdline.clone(),
                    })
                    .terminal_argv(exec_flag, crucible);
                    (dir, args)
                }
                WindowRecord::App { cmdline: None, .. } => continue,
            };
            jobs.push(SpawnJob {
                dir,
                args,
                pane_index,
                saved_index,
                focused: pane.focused == saved_index,
                origin: window.clone(),
            });
        }
    }
    jobs
}

/// A restored app's working directory: the workspace root, else `home`, else `/`.
fn app_cwd(root: Option<&Path>, home: Option<&Path>, is_dir: impl Fn(&Path) -> bool) -> PathBuf {
    [root, home]
        .into_iter()
        .flatten()
        .find(|p| is_dir(p))
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf)
}

/// Every fallible restore precondition plus the spawn plan, resolved before the model is
/// touched: `crucible` is the reinstall-safe executable path, and a failure leaves nothing to undo.
pub fn prepare_restore(
    record: &WorkspaceRecord,
    crucible: std::io::Result<PathBuf>,
    home: Option<&Path>,
    exec_flag: &str,
    is_dir: impl Fn(&Path) -> bool,
) -> Result<Vec<SpawnJob>, String> {
    let crucible = crucible.map_err(|e| e.to_string())?;
    Ok(plan_spawns(record, home, &crucible, exec_flag, is_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{PaneRecord, RecordNode};

    fn rec(root: Option<&str>, tree: RecordNode) -> WorkspaceRecord {
        WorkspaceRecord {
            name: "w".into(),
            root: root.map(PathBuf::from),
            focused_pane: 0,
            output: None,
            tree,
        }
    }

    fn term(cwd: &str, command: Option<&str>) -> WindowRecord {
        WindowRecord::Terminal {
            cwd: cwd.into(),
            command: command.map(str::to_string),
        }
    }

    fn app(cmdline: Option<&str>) -> WindowRecord {
        WindowRecord::App {
            app_id: Some("org.x".into()),
            cmdline: cmdline.map(str::to_string),
        }
    }

    fn pane_node(focused: usize, windows: Vec<WindowRecord>) -> RecordNode {
        RecordNode::Pane(PaneRecord { focused, windows })
    }

    fn dirs(list: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| list.iter().any(|d| Path::new(d) == p)
    }

    const CRUCIBLE: &str = "/bin/crucible";

    fn argv(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn plan(r: &WorkspaceRecord, home: Option<&str>, ok: &'static [&'static str]) -> Vec<SpawnJob> {
        plan_spawns(r, home.map(Path::new), Path::new(CRUCIBLE), "-e", dirs(ok))
    }

    #[test]
    fn terminal_job_uses_restore_args_and_the_resolved_cwd() {
        let w = term("/gone", Some("vim x"));
        let r = rec(Some("/r"), pane_node(0, vec![w.clone()]));
        let jobs = plan(&r, Some("/h"), &["/r", "/h"]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].dir, PathBuf::from("/r"));
        assert_eq!(
            jobs[0].args,
            argv(&["-e", CRUCIBLE, "resume", "--cwd", "/r", "--", "vim x"])
        );
        assert_eq!(jobs[0].origin, w);
        // An idle shell (no saved command) gets no arguments at all.
        let idle = rec(None, pane_node(0, vec![term("/r", None)]));
        assert!(plan(&idle, None, &["/r"])[0].args.is_empty());
    }

    #[test]
    fn app_job_uses_relaunch_args_and_the_root_home_slash_chain() {
        let w = app(Some("firefox"));
        let expect = argv(&[
            "-e", CRUCIBLE, "relaunch", "--app-id", "org.x", "--", "firefox",
        ]);
        let r = rec(Some("/r"), pane_node(0, vec![w.clone()]));
        let job = |home, ok| plan(&r, home, ok).remove(0);
        // Root valid.
        let j = job(Some("/h"), &["/r", "/h"]);
        assert_eq!((j.dir, j.args, j.origin), ("/r".into(), expect.clone(), w));
        // Root invalid: home.
        assert_eq!(job(Some("/h"), &["/h"]).dir, PathBuf::from("/h"));
        // Neither: `/`.
        assert_eq!(job(Some("/h"), &[]).dir, PathBuf::from("/"));
        // No root: home.
        let no_root = rec(None, pane_node(0, vec![app(Some("firefox"))]));
        assert_eq!(
            plan(&no_root, Some("/h"), &["/h"])[0].dir,
            PathBuf::from("/h")
        );
        // No root, no home: `/`.
        assert_eq!(plan(&no_root, None, &[])[0].dir, PathBuf::from("/"));
    }

    #[test]
    fn app_without_a_cmdline_is_not_planned() {
        let r = rec(None, pane_node(0, vec![app(None)]));
        assert!(plan(&r, Some("/h"), &["/h"]).is_empty());
    }

    #[test]
    fn jobs_carry_pane_index_saved_index_and_focus() {
        let tree = RecordNode::Split {
            axis: crucible_layout::Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(pane_node(1, vec![term("/a", None), term("/b", None)])),
            second: Box::new(pane_node(0, vec![app(None), term("/c", None)])),
        };
        let r = rec(None, tree);
        let got: Vec<(usize, usize, bool)> = plan(&r, None, &["/a", "/b", "/c"])
            .iter()
            .map(|j| (j.pane_index, j.saved_index, j.focused))
            .collect();
        // The skipped app keeps its saved index: the terminal after it is index 1, unfocused.
        assert_eq!(got, [(0, 0, false), (0, 1, true), (1, 1, false)]);
    }

    #[test]
    fn a_failed_restore_precondition_plans_nothing() {
        let r = rec(None, pane_node(0, vec![term("/a", None)]));
        let err = std::io::Error::other("no exe");
        let got = prepare_restore(&r, Err(err), None, "-e", dirs(&[]));
        assert_eq!(got.unwrap_err(), "no exe");
        let ok = prepare_restore(&r, Ok(CRUCIBLE.into()), None, "-e", dirs(&["/a"]));
        assert_eq!(ok.unwrap().len(), 1);
    }

    #[test]
    fn cwd_falls_back_to_root_then_home() {
        let ok =
            |dirs: &'static [&'static str]| move |p: &Path| dirs.iter().any(|d| Path::new(d) == p);
        let (saved, root, home) = (
            Path::new("/s"),
            Some(Path::new("/r")),
            Some(Path::new("/h")),
        );
        assert_eq!(
            resolve_cwd(saved, root, home, ok(&["/s", "/r"])),
            PathBuf::from("/s")
        );
        assert_eq!(
            resolve_cwd(saved, root, home, ok(&["/r", "/h"])),
            PathBuf::from("/r")
        );
        assert_eq!(
            resolve_cwd(saved, None, home, ok(&["/h"])),
            PathBuf::from("/h")
        );
        assert_eq!(resolve_cwd(saved, root, home, ok(&[])), PathBuf::from("/"));
    }
}
