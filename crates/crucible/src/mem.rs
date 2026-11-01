//! `crucible mem [--pid <n>]` — real memory (PSS and swap) per logical app of a running CDE.
//!
//! Every direct child of the compositor process is one logical app (a terminal, a browser,
//! Xwayland, ...); its whole process subtree is summed from `/proc/<pid>/smaps_rollup`. Read-only
//! on `/proc`: no IPC, no compositor involvement. Every `/proc` read goes through a `proc_root`
//! parameter so the core is tested on a fixture tree.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crucible_persist::capture::{ProcInfo, ProcSnapshot, ancestors, is_shell_program};
use crucible_persist::{crucible_subcommand, is_cde_process};

use crate::{Parsed, help_requested};

/// Usage text for `crucible mem --help`.
pub const USAGE: &str = "usage: crucible mem [--pid <n>]\n\nReal memory per logical app of a running CDE: one row per direct child process of the compositor\n(PSS, swap, process count, label), largest PSS first, then the compositor itself and a TOTAL.\nPSS and swap are summed over the child's whole process subtree from /proc/<pid>/smaps_rollup. The\nlabel is the child's name plus the deepest process under it that is not a shell and\nnot one of CDE's own `crucible` processes (ties: largest PSS), e.g. `ghostty › htop`. A child whose\nsubtree holds the overlay host gets ` (overlay)` appended.\n\n  --pid <n>    the compositor's pid. Without it, the compositor is the process owned by you whose\n               command is `crucible` with `--winit` or `--tty-udev`; with several, the one that is an\n               ancestor of this command, else the lowest pid. None found: message on stderr, exit 1.\n  -h, --help   print this usage and exit 0\n\nOnly processes CDE spawned itself are counted: D-Bus-activated apps parented to `systemd --user`\nare not. A process that is unreadable or vanished counts as zero and its row is marked `*`.\n";

/// Parsed arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    /// `--pid`, if given.
    pub pid: Option<u32>,
}

/// Parses the arguments; `--help`/`-h` (not as the `--pid` value) is [`Parsed::Help`].
///
/// # Errors
/// A message for an unknown argument or a missing/invalid `--pid` value.
pub fn parse_args(args: &[String]) -> Result<Parsed<Args>, String> {
    if help_requested(args, &["--pid"], false) {
        return Ok(Parsed::Help);
    }
    let mut pid = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pid" => {
                if pid.is_some() {
                    return Err("--pid given twice".into());
                }
                let v = it.next().ok_or("--pid needs a value")?;
                let n: u32 = v
                    .parse()
                    .map_err(|_| format!("--pid: {v:?} is not a pid"))?;
                if n == 0 {
                    return Err("--pid: 0 is not a pid".into());
                }
                pid = Some(n);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Parsed::Run(Args { pid }))
}

/// `(pss_kb, swap_pss_kb)` from the text of an `smaps_rollup` file; a missing field is 0.
fn parse_rollup(text: &str) -> (u64, u64) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| {
                l.strip_prefix(name)?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
            .unwrap_or(0)
    };
    (field("Pss:"), field("SwapPss:"))
}

fn read_rollup(proc_root: &Path, pid: u32) -> Option<(u64, u64)> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("smaps_rollup")).ok()?;
    Some(parse_rollup(&text))
}

/// The compositor pid: the process owned by `uid` whose argv[0] is `crucible` (basename) with a
/// `--winit` or `--tty-udev` argument. Several: an ancestor of `self_pid` wins, else the lowest.
#[must_use]
pub fn find_compositor(proc_root: &Path, uid: u32, self_pid: u32) -> Option<u32> {
    let mut found: Vec<u32> = ProcSnapshot::read(proc_root)
        .iter()
        .filter(|(pid, p)| {
            crucible_subcommand(&p.argv).is_some()
                && p.argv.iter().any(|a| a == "--winit" || a == "--tty-udev")
                && fs::metadata(proc_root.join(pid.to_string())).is_ok_and(|m| m.uid() == uid)
        })
        .map(|(pid, _)| pid)
        .collect();
    found.sort_unstable();
    let up = ancestors(proc_root, self_pid, 64);
    found
        .iter()
        .copied()
        .find(|p| up.contains(p))
        .or_else(|| found.first().copied())
}

/// One output row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// PSS in kB.
    pub pss_kb: u64,
    /// Swap (SwapPss) in kB.
    pub swap_kb: u64,
    /// Processes counted.
    pub procs: usize,
    /// Display label.
    pub label: String,
    /// Some process in the row was unreadable or vanished (counted as zero).
    pub unreadable: bool,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// One row per direct child of the compositor, PSS descending.
    pub children: Vec<Row>,
    /// The compositor's own process.
    pub compositor: Row,
    /// Sum of everything above.
    pub total: Row,
}

/// Whether a process is skipped when labelling: a shell or one of CDE's own `crucible` processes
/// (resume/relaunch wrappers, the overlay host, `crucible mem` itself).
fn skipped_in_label(p: &ProcInfo) -> bool {
    is_cde_process(&p.argv, &p.comm)
        || p.argv.first().is_some_and(|a| is_shell_program(a))
        || (p.argv.is_empty() && is_shell_program(&p.comm))
}

/// Builds the report for the compositor `pid` from a `/proc`-shaped tree.
#[must_use]
pub fn build_report(proc_root: &Path, pid: u32) -> Report {
    let procs = ProcSnapshot::read(proc_root);
    let mut kids = procs.children();
    // The compositor is never anybody's child here (also guards a pid-1 style self-parent).
    for list in kids.values_mut() {
        list.retain(|&p| p != pid);
    }
    let name = |p: u32| {
        procs
            .get(p)
            .map_or_else(|| format!("[{p}]"), |i| i.comm.clone())
    };

    let mut children: Vec<Row> = Vec::new();
    for &child in kids.get(&pid).map_or(&[][..], Vec::as_slice) {
        // Subtree with depths; a visited set guards against cycles.
        let mut tree = vec![(child, 0usize)];
        let mut seen = HashSet::from([child]);
        let mut i = 0;
        while i < tree.len() {
            let (p, d) = tree[i];
            for &k in kids.get(&p).map_or(&[][..], Vec::as_slice) {
                if seen.insert(k) {
                    tree.push((k, d + 1));
                }
            }
            i += 1;
        }
        let (mut pss, mut swap, mut unreadable) = (0, 0, false);
        let mut best: Option<(usize, u64, Reverse<u32>)> = None;
        let mut overlay = false;
        for &(p, d) in &tree {
            let mem = read_rollup(proc_root, p);
            unreadable |= mem.is_none();
            let (a, b) = mem.unwrap_or((0, 0));
            pss += a;
            swap += b;
            let Some(info) = procs.get(p) else {
                continue;
            };
            overlay |= crucible_subcommand(&info.argv) == Some("overlay-host");
            if d > 0 && !skipped_in_label(info) {
                // Deepest first, then largest PSS, then lowest pid (deterministic).
                best = best.max(Some((d, a, Reverse(p))));
            }
        }
        let mut label = name(child);
        if let Some((_, _, Reverse(p))) = best {
            let _ = write!(label, " › {}", name(p));
        }
        if overlay {
            label.push_str(" (overlay)");
        }
        children.push(Row {
            pss_kb: pss,
            swap_kb: swap,
            procs: tree.len(),
            label,
            unreadable,
        });
    }
    children.sort_by(|a, b| b.pss_kb.cmp(&a.pss_kb).then_with(|| a.label.cmp(&b.label)));

    let mem = read_rollup(proc_root, pid);
    let (cp, cs) = mem.unwrap_or((0, 0));
    let compositor = Row {
        pss_kb: cp,
        swap_kb: cs,
        procs: 1,
        label: format!("{} (compositor)", name(pid)),
        unreadable: mem.is_none(),
    };
    let total = Row {
        pss_kb: cp + children.iter().map(|r| r.pss_kb).sum::<u64>(),
        swap_kb: cs + children.iter().map(|r| r.swap_kb).sum::<u64>(),
        procs: 1 + children.iter().map(|r| r.procs).sum::<usize>(),
        label: "TOTAL".into(),
        unreadable: compositor.unreadable || children.iter().any(|r| r.unreadable),
    };
    Report {
        children,
        compositor,
        total,
    }
}

fn mib(kb: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = kb as f64 / 1024.0;
    format!("{v:.1} MiB")
}

/// Renders the report as text.
#[must_use]
pub fn format_report(pid: u32, r: &Report) -> String {
    let mut out = format!(
        "{:>11}  {:>11}  {:>5}  APP (compositor pid {pid})\n",
        "PSS", "SWAP", "PROCS"
    );
    let mut any_flag = false;
    for row in r.children.iter().chain([&r.compositor, &r.total]) {
        any_flag |= row.unreadable;
        let _ = writeln!(
            out,
            "{:>11}  {:>11}  {:>5}  {}{}",
            mib(row.pss_kb),
            mib(row.swap_kb),
            row.procs,
            row.label,
            if row.unreadable { " *" } else { "" }
        );
    }
    if any_flag {
        out.push_str("* some processes were unreadable or vanished and count as zero\n");
    }
    out
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(Parsed::Run(p)) => p,
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("crucible mem: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let proc_root = Path::new("/proc");
    let pid = parsed.pid.or_else(|| {
        let uid = rustix::process::getuid().as_raw();
        find_compositor(proc_root, uid, std::process::id())
    });
    let Some(pid) = pid else {
        eprintln!("crucible mem: no running CDE compositor found (pass --pid <n>)");
        return 1;
    };
    if !proc_root.join(pid.to_string()).is_dir() {
        eprintln!("crucible mem: no process {pid}");
        return 1;
    }
    print!("{}", format_report(pid, &build_report(proc_root, pid)));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            let d = std::env::temp_dir().join(format!("cde-mem-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Self(d)
        }
        /// `rollup`: `Some((pss, swap))` writes smaps_rollup; `None` leaves it unreadable.
        fn proc(&self, pid: u32, ppid: u32, comm: &str, argv: &[&str], rollup: Option<(u64, u64)>) {
            let d = self.0.join(pid.to_string());
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join("stat"),
                format!("{pid} ({comm}) S {ppid} 1 1 0 -1 0"),
            )
            .unwrap();
            fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
            let mut cmd = argv.join("\0");
            cmd.push('\0');
            fs::write(d.join("cmdline"), cmd).unwrap();
            if let Some((p, s)) = rollup {
                fs::write(
                    d.join("smaps_rollup"),
                    format!("Rss: 1 kB\nPss:   {p} kB\nSwapPss:  {s} kB\n"),
                )
                .unwrap();
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn s(a: &[&str]) -> Vec<String> {
        a.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn groups_subtrees_labels_skip_wrappers_and_shells_and_sorts() {
        let f = Fixture::new("tree");
        f.proc(
            10,
            1,
            "crucible",
            &["/x/crucible", "--winit"],
            Some((100, 1)),
        );
        // ghostty -> crucible resume -> bash -> htop
        f.proc(20, 10, "ghostty", &["ghostty"], Some((1000, 10)));
        f.proc(
            21,
            20,
            "crucible",
            &["/x/crucible", "resume", "--cwd", "/"],
            Some((50, 0)),
        );
        f.proc(22, 21, "bash", &["-bash"], Some((20, 0)));
        f.proc(23, 22, "htop", &["htop"], Some((30, 5)));
        // idle terminal: only a wrapper-less shell
        f.proc(30, 10, "ghostty", &["ghostty"], Some((400, 0)));
        f.proc(31, 30, "bash", &["bash"], Some((10, 0)));
        // browser with two same-depth helpers: the larger PSS labels it
        f.proc(40, 10, "firefox-bin", &["firefox-bin"], Some((3000, 100)));
        f.proc(41, 40, "Isolated Web Co", &["x"], Some((10, 0)));
        f.proc(42, 40, "WebExtensions", &["x"], Some((90, 0)));
        // not a child of the compositor
        f.proc(50, 1, "other", &["other"], Some((9999, 0)));

        let r = build_report(&f.0, 10);
        let labels: Vec<_> = r.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["firefox-bin › WebExtensions", "ghostty › htop", "ghostty"]
        );
        assert_eq!(r.children[1].pss_kb, 1100);
        assert_eq!(r.children[1].swap_kb, 15);
        assert_eq!(r.children[1].procs, 4);
        assert_eq!(r.total.pss_kb, 100 + 3100 + 1100 + 410);
        assert_eq!(r.total.procs, 1 + 3 + 4 + 2);
        assert!(!r.total.unreadable);
    }

    #[test]
    fn overlay_host_is_suffixed_and_cde_processes_never_label() {
        let f = Fixture::new("overlay");
        f.proc(10, 1, "crucible", &["/x/crucible", "--winit"], None);
        // terminal -> crucible overlay-host
        f.proc(20, 10, "ghostty", &["ghostty"], Some((100, 0)));
        f.proc(
            21,
            20,
            "crucible",
            &["/x/crucible", "overlay-host", "--socket", "/s"],
            Some((10, 0)),
        );
        // terminal -> shell -> crucible mem
        f.proc(30, 10, "ghostty", &["ghostty"], Some((200, 0)));
        f.proc(31, 30, "bash", &["bash"], Some((10, 0)));
        f.proc(32, 31, "crucible", &["crucible", "mem"], Some((10, 0)));
        let r = build_report(&f.0, 10);
        let labels: Vec<_> = r.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["ghostty", "ghostty (overlay)"]);
    }

    #[test]
    fn unreadable_process_counts_zero_and_is_flagged() {
        let f = Fixture::new("unread");
        f.proc(10, 1, "crucible", &["crucible", "--winit"], Some((100, 0)));
        f.proc(20, 10, "foot", &["foot"], Some((500, 0)));
        f.proc(21, 20, "gone", &["gone"], None);
        let r = build_report(&f.0, 10);
        assert_eq!(r.children[0].pss_kb, 500);
        assert!(r.children[0].unreadable);
        assert!(r.total.unreadable);
        let text = format_report(10, &r);
        assert!(text.contains("foot › gone *"));
        assert!(text.contains("* some processes"));
    }

    #[test]
    fn compositor_discovery_prefers_ancestor_then_lowest() {
        let f = Fixture::new("find");
        let uid = rustix::process::getuid().as_raw();
        f.proc(5, 1, "crucible", &["/a/crucible", "--tty-udev"], None);
        f.proc(8, 1, "crucible", &["/a/crucible", "--winit"], None);
        f.proc(9, 1, "crucible", &["/a/crucible", "pick"], None);
        f.proc(12, 8, "sh", &["sh"], None);
        f.proc(13, 12, "crucible", &["crucible", "mem"], None);
        assert_eq!(find_compositor(&f.0, uid, 13), Some(8));
        assert_eq!(find_compositor(&f.0, uid, 99), Some(5));
        assert_eq!(find_compositor(&f.0, uid.wrapping_add(1), 13), None);
    }

    #[test]
    fn args_parse_strictly() {
        assert_eq!(parse_args(&s(&["--help"])), Ok(Parsed::Help));
        assert_eq!(
            parse_args(&s(&["--pid", "42"])),
            Ok(Parsed::Run(Args { pid: Some(42) }))
        );
        assert_eq!(parse_args(&s(&[])), Ok(Parsed::Run(Args { pid: None })));
        assert!(parse_args(&s(&["--pid", "x"])).is_err());
        assert!(parse_args(&s(&["--pid"])).is_err());
        assert!(parse_args(&s(&["--pid", "0"])).is_err());
        assert!(parse_args(&s(&["--bogus"])).is_err());
    }
}
