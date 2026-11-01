//! `/proc` access: text parsers, per-pid readers, the process snapshot and the ancestor walk.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

/// The `/proc/<pid>/stat` fields capture needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Parent pid (4th field).
    pub ppid: i64,
    /// Foreground process group of the controlling terminal (8th field); `-1` when none.
    pub tpgid: i64,
}

/// Parses the text of `/proc/<pid>/children` (whitespace-separated pids).
#[must_use]
pub fn parse_children(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// Parses `/proc/<pid>/stat`. The `comm` field is parenthesised and may contain spaces and `)`,
/// so fields are read after the **last** `)`.
#[must_use]
pub fn parse_stat(text: &str) -> Option<Stat> {
    let rest = &text[text.rfind(')')? + 1..];
    // After comm: state(3) ppid(4) pgrp(5) session(6) tty_nr(7) tpgid(8)
    let mut f = rest.split_whitespace();
    let _state = f.next()?;
    let ppid = f.next()?.parse().ok()?;
    let _pgrp = f.next()?;
    let _session = f.next()?;
    let _tty = f.next()?;
    let tpgid = f.next()?.parse().ok()?;
    Some(Stat { ppid, tpgid })
}

/// Splits the bytes of `/proc/<pid>/cmdline` into arguments (NUL-separated). Non-UTF-8 bytes are
/// replaced lossily: the result is for display and relaunch, not identity.
#[must_use]
pub fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

fn pid_dir(proc_root: &Path, pid: u32) -> PathBuf {
    proc_root.join(pid.to_string())
}

/// Direct children of `pid`, from `/proc/<pid>/task/*/children` (tasks in numeric order). A
/// missing pid yields an empty list.
#[must_use]
pub fn children_of(proc_root: &Path, pid: u32) -> Vec<u32> {
    let task_dir = pid_dir(proc_root, pid).join("task");
    let Ok(entries) = fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut tasks: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
        .collect();
    tasks.sort();
    tasks
        .into_iter()
        .filter_map(|(_, p)| fs::read_to_string(p.join("children")).ok())
        .flat_map(|t| parse_children(&t))
        .collect()
}

pub(super) fn read_stat(proc_root: &Path, pid: u32) -> Option<Stat> {
    parse_stat(&fs::read_to_string(pid_dir(proc_root, pid).join("stat")).ok()?)
}

pub(super) fn read_cwd(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    fs::read_link(pid_dir(proc_root, pid).join("cwd")).ok()
}

pub(super) fn read_cmdline(proc_root: &Path, pid: u32) -> Vec<String> {
    fs::read(pid_dir(proc_root, pid).join("cmdline"))
        .map(|b| parse_cmdline(&b))
        .unwrap_or_default()
}

fn read_comm(proc_root: &Path, pid: u32) -> String {
    fs::read_to_string(pid_dir(proc_root, pid).join("comm"))
        .map(|c| c.trim().to_string())
        .unwrap_or_default()
}

/// One process of a [`ProcSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    /// Parent pid.
    pub ppid: u32,
    /// The kernel's short process name (`comm`); empty when unreadable.
    pub comm: String,
    /// The command line; empty for kernel threads and unreadable processes.
    pub argv: Vec<String>,
}

/// Every process under a `proc_root` with a parseable `stat`, read once.
#[derive(Debug, Clone, Default)]
pub struct ProcSnapshot(HashMap<u32, ProcInfo>);

impl ProcSnapshot {
    /// Reads every numeric `<proc_root>/<pid>` directory with a parseable `stat`.
    #[must_use]
    pub fn read(proc_root: &Path) -> Self {
        let Ok(entries) = fs::read_dir(proc_root) else {
            return Self::default();
        };
        let procs = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let ppid = u32::try_from(read_stat(proc_root, pid)?.ppid).ok()?;
                let info = ProcInfo {
                    ppid,
                    comm: read_comm(proc_root, pid),
                    argv: read_cmdline(proc_root, pid),
                };
                Some((pid, info))
            })
            .collect();
        Self(procs)
    }

    /// The process `pid`, if it was in the snapshot.
    #[must_use]
    pub fn get(&self, pid: u32) -> Option<&ProcInfo> {
        self.0.get(&pid)
    }

    /// Every `(pid, process)` pair, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &ProcInfo)> {
        self.0.iter().map(|(p, i)| (*p, i))
    }

    /// Parent pid to its child pids (each list in no particular order).
    #[must_use]
    pub fn children(&self) -> HashMap<u32, Vec<u32>> {
        let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, info) in self.iter() {
            kids.entry(info.ppid).or_default().push(pid);
        }
        kids
    }
}

/// The ancestors of `pid` (parent first), walking `ppid` up to `max_depth` steps; stops at pid 0
/// or an unreadable `stat`. `pid` itself is not included.
#[must_use]
pub fn ancestors(proc_root: &Path, pid: u32, max_depth: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cur = pid;
    while out.len() < max_depth {
        let Some(stat) = read_stat(proc_root, cur) else {
            break;
        };
        let Ok(parent) = u32::try_from(stat.ppid) else {
            break;
        };
        if parent == 0 {
            break;
        }
        out.push(parent);
        cur = parent;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::fake_proc::FakeProc;

    #[test]
    fn proc_snapshot_reads_numeric_dirs_with_stat() {
        let p = FakeProc::new("snapshot");
        p.add((10, "crucible", 1), 10, "/", &["/x/crucible", "mem"], &[11]);
        p.add((11, "bash", 10), 10, "/", &[], &[]);
        fs::write(p.proc_path("11/comm"), "bash\n").unwrap();
        fs::create_dir_all(p.proc_path("self")).unwrap(); // not numeric
        fs::create_dir_all(p.proc_path("12")).unwrap(); // no stat
        let snap = ProcSnapshot::read(&p.proc_root());
        assert_eq!(snap.iter().count(), 2);
        let ten = snap.get(10).unwrap();
        assert_eq!((ten.ppid, ten.comm.as_str()), (1, ""));
        assert_eq!(ten.argv, ["/x/crucible", "mem"]);
        let eleven = snap.get(11).unwrap();
        assert_eq!((eleven.ppid, eleven.comm.as_str()), (10, "bash"));
        assert!(eleven.argv.is_empty());
        assert!(snap.get(12).is_none());
        assert_eq!(snap.children()[&10], [11]);
        assert!(
            ProcSnapshot::read(&p.proc_root().join("missing"))
                .get(1)
                .is_none()
        );
    }

    #[test]
    fn parse_stat_handles_awkward_comm() {
        let s = parse_stat("42 (bash) S 7 42 42 34816 99 4194560 1 2 3").unwrap();
        assert_eq!(s, Stat { ppid: 7, tpgid: 99 });
        let s = parse_stat("42 (we ird) x) S 7 42 42 34816 -1 0").unwrap();
        assert_eq!(s, Stat { ppid: 7, tpgid: -1 });
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) S 1 2"), None);
        assert_eq!(parse_stat("1 (x) S notnum 2 3 4 5"), None);
    }

    #[test]
    fn parse_children_and_cmdline() {
        assert_eq!(parse_children("12 34 \n"), vec![12, 34]);
        assert!(parse_children("").is_empty());
        assert_eq!(parse_cmdline(b"a\0b c\0"), vec!["a", "b c"]);
        assert_eq!(parse_cmdline(b"a\0\0"), vec!["a", ""]);
        assert!(parse_cmdline(b"").is_empty());
    }

    #[test]
    fn ancestors_walks_ppid_with_depth_limit() {
        let p = FakeProc::new("anc");
        p.add((10, "a", 0), 0, "/", &["a"], &[]);
        p.add((20, "b", 10), 0, "/", &["b"], &[]);
        p.add((30, "c", 20), 0, "/", &["c"], &[]);
        let root = p.proc_root();
        assert_eq!(ancestors(&root, 30, 10), vec![20, 10]);
        assert_eq!(ancestors(&root, 30, 1), vec![20]);
        assert_eq!(ancestors(&root, 30, 0), Vec::<u32>::new());
        assert!(ancestors(&root, 999, 5).is_empty());
    }
}
