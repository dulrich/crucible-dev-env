//! `crucible mem [--pid <n>]` — real memory (PSS and swap) per logical app of a running CDE.
//!
//! Every direct child of the compositor process is one logical app (a terminal, a browser,
//! Xwayland, ...); its whole process subtree is summed from `/proc/<pid>/smaps_rollup`. Read-only
//! on `/proc`: no IPC, no compositor involvement. Every `/proc` read goes through a `proc_root`
//! parameter so the core is tested on a fixture tree.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crucible_persist::capture::{ProcInfo, ProcSnapshot, ancestors, is_shell_program};
use crucible_persist::{crucible_subcommand, is_cde_process};

use crate::cli::{Parsed, help_requested};
use crate::table::{self, Col};

/// Usage text for `crucible mem --help`.
pub const USAGE: &str = "usage: crucible mem [--pid <n>]\n\nReal memory per logical app of a running CDE: one row per direct child process of the compositor\n(PSS, swap, process count, label), largest PSS first, then the compositor itself and a TOTAL.\nPSS and swap are summed over the child's whole process subtree from /proc/<pid>/smaps_rollup. The\nlabel is the child's name plus the deepest process under it that is not a shell and\nnot one of CDE's own `crucible` processes (ties: largest PSS), e.g. `ghostty › htop`. A child whose\nsubtree holds the overlay host gets ` (overlay)` appended. A cterm daemon serving this compositor (same\nWAYLAND_DISPLAY, owned by you) gets one row, `cterm daemon (+N windows)`: the daemon plus its thin clients\n(compositor children named cterm with no child process). What runs in its windows is not in that row:\neach window counts toward its app (the deepest process that is not a shell or one of CDE's own, else\nthe shell), and windows running the same app share one row, e.g. `claude`, `htop`.\n\n  --pid <n>    the compositor's pid. Without it, the compositor is the process owned by you whose\n               command is `crucible` with `--winit` or `--tty-udev`; with several, the one that is an\n               ancestor of this command, else the lowest pid. None found: message on stderr, exit 1.\n  -h, --help   print this usage and exit 0\n\nOnly processes CDE spawned itself are counted: D-Bus-activated apps parented to `systemd --user`\nare not. A process that is unreadable or vanished counts as zero and its row is marked `*`.\n";

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

/// Whether `argv` is a resident cterm daemon: argv[0] basename `cterm`, argv[1] `daemon`.
fn is_cterm_daemon(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "cterm"))
        && argv.get(1).is_some_and(|a| a == "daemon")
}

/// The raw value of `key` in `/proc/<pid>/environ` (compared byte-for-byte by the caller).
fn environ_var(proc_root: &Path, pid: u32, key: &[u8]) -> Option<Vec<u8>> {
    let env = fs::read(proc_root.join(pid.to_string()).join("environ")).ok()?;
    env.split(|&b| b == 0).find_map(|kv| {
        kv.strip_prefix(key)
            .and_then(|r| r.strip_prefix(b"="))
            .map(<[u8]>::to_vec)
    })
}

/// The subtree of `root` with depths; a visited set guards against cycles.
fn subtree(kids: &HashMap<u32, Vec<u32>>, root: u32) -> Vec<(u32, usize)> {
    let mut tree = vec![(root, 0usize)];
    let mut seen = HashSet::from([root]);
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
    tree
}

/// Sums a subtree into a row labelled `label`, plus ` › <deepest non-skipped process>` and the
/// ` (overlay)` suffix when the subtree holds the overlay host.
fn subtree_row(
    proc_root: &Path,
    procs: &ProcSnapshot,
    tree: &[(u32, usize)],
    mut label: String,
) -> Row {
    let (mut pss, mut swap, mut unreadable) = (0, 0, false);
    let mut best: Option<(usize, u64, Reverse<u32>)> = None;
    let mut overlay = false;
    for &(p, d) in tree {
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
    if let Some((_, _, Reverse(p))) = best {
        let n = procs
            .get(p)
            .map_or_else(|| format!("[{p}]"), |i| i.comm.clone());
        let _ = write!(label, " › {n}");
    }
    if overlay {
        label.push_str(" (overlay)");
    }
    Row {
        pss_kb: pss,
        swap_kb: swap,
        procs: tree.len(),
        label,
        unreadable,
    }
}

/// The app a daemon window runs: the deepest process that is neither a shell nor one of CDE's
/// own (ties: lowest pid), else the deepest shell, else the window's root (`crucible resume` for a restore banner not yet confirmed).
fn window_app(procs: &ProcSnapshot, tree: &[(u32, usize)]) -> String {
    let pick = |skip_shells: bool| {
        tree.iter()
            .filter_map(|&(p, d)| procs.get(p).map(|i| (p, d, i)))
            .filter(|(_, _, i)| {
                if skip_shells {
                    !skipped_in_label(i)
                } else {
                    !is_cde_process(&i.argv, &i.comm)
                }
            })
            .max_by_key(|&(p, d, _)| (d, Reverse(p)))
            .map(|(_, _, i)| i.comm.clone())
    };
    pick(true)
        .or_else(|| pick(false))
        .or_else(|| {
            tree.first().and_then(|&(p, _)| procs.get(p)).map(|i| {
                crucible_subcommand(&i.argv)
                    .map_or_else(|| i.comm.clone(), |s| format!("crucible {s}"))
            })
        })
        .unwrap_or_else(|| "?".into())
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
    let mut top_rows: Vec<(u32, Row)> = Vec::new();
    let mut counted: HashSet<u32> = HashSet::new();
    let top: &[u32] = kids.get(&pid).map_or(&[][..], Vec::as_slice);
    for &child in top {
        let tree = subtree(&kids, child);
        counted.extend(tree.iter().map(|&(p, _)| p));
        top_rows.push((child, subtree_row(proc_root, &procs, &tree, name(child))));
    }
    // cterm daemons serving this compositor: detached into their own session, so they are not
    // descendants; matched by the WAYLAND_DISPLAY of the first readable compositor child.
    let display = top
        .iter()
        .find_map(|&c| environ_var(proc_root, c, b"WAYLAND_DISPLAY"));
    if let Some(display) = display {
        let uid = rustix::process::getuid().as_raw();
        let mut daemons: Vec<u32> = procs
            .iter()
            .filter(|&(p, info)| {
                p != pid
                    && !counted.contains(&p)
                    && is_cterm_daemon(&info.argv)
                    && fs::metadata(proc_root.join(p.to_string())).is_ok_and(|m| m.uid() == uid)
                    && environ_var(proc_root, p, b"WAYLAND_DISPLAY").as_deref()
                        == Some(&display[..])
            })
            .map(|(p, _)| p)
            .collect();
        daemons.sort_unstable();
        // Thin clients (a `cterm` child of the compositor with no child of its own) belong to
        // the daemon row, not their own rows; with several daemons, to the first.
        let thin: Vec<u32> = if daemons.is_empty() {
            Vec::new()
        } else {
            top.iter()
                .copied()
                .filter(|c| {
                    kids.get(c).is_none_or(Vec::is_empty)
                        && procs.get(*c).is_some_and(|i| {
                            !is_cterm_daemon(&i.argv)
                                && i.argv.first().is_some_and(|a| {
                                    Path::new(a).file_name().is_some_and(|n| n == "cterm")
                                })
                        })
                })
                .collect()
        };
        top_rows.retain(|(p, _)| !thin.contains(p));
        let mut apps: Vec<Row> = Vec::new();
        for (i, d) in daemons.into_iter().enumerate() {
            let windows: &[u32] = kids.get(&d).map_or(&[][..], Vec::as_slice);
            let mut own: Vec<(u32, usize)> = vec![(d, 0)];
            if i == 0 {
                own.extend(thin.iter().map(|&c| (c, 0)));
            }
            let mut row = subtree_row(proc_root, &procs, &own, String::new());
            row.label = format!(
                "cterm daemon (+{} window{})",
                windows.len(),
                if windows.len() == 1 { "" } else { "s" }
            );
            children.push(row);
            // Each window's subtree counts toward its app, summed by app name.
            for &w in windows {
                let tree = subtree(&kids, w);
                let label = window_app(&procs, &tree);
                let r = subtree_row(proc_root, &procs, &tree, String::new());
                match apps.iter_mut().find(|a| a.label == label) {
                    Some(a) => {
                        a.pss_kb += r.pss_kb;
                        a.swap_kb += r.swap_kb;
                        a.procs += r.procs;
                        a.unreadable |= r.unreadable;
                    }
                    None => apps.push(Row { label, ..r }),
                }
            }
        }
        children.extend(apps);
    }
    children.extend(top_rows.into_iter().map(|(_, r)| r));
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

/// Renders the report as text: a table (PSS, SWAP and PROCS right-aligned, widths from the
/// content) plus the unreadable-process footnote.
#[must_use]
pub fn format_report(pid: u32, r: &Report) -> String {
    let app = format!("APP (compositor pid {pid})");
    let cols = [
        Col::right("PSS"),
        Col::right("SWAP"),
        Col::right("PROCS"),
        Col::left(&app),
    ];
    let rows: Vec<&Row> = r.children.iter().chain([&r.compositor, &r.total]).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            vec![
                mib(row.pss_kb),
                mib(row.swap_kb),
                row.procs.to_string(),
                format!("{}{}", row.label, if row.unreadable { " *" } else { "" }),
            ]
        })
        .collect();
    let (header, lines) = table::layout(&cols, &cells);
    let mut out = format!("{header}\n");
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    if rows.iter().any(|row| row.unreadable) {
        out.push_str("* some processes were unreadable or vanished and count as zero\n");
    }
    out
}

/// Runs the subcommand; returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let parsed = match crate::cli::parse_or_exit("mem", USAGE, parse_args(args)) {
        Ok(v) => v,
        Err(code) => return code,
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
        /// Writes `/proc/<pid>/environ` as NUL-separated `K=V`.
        fn environ(&self, pid: u32, vars: &[(&str, &str)]) {
            let env: String = vars.iter().map(|(k, v)| format!("{k}={v}\0")).collect();
            fs::write(self.0.join(pid.to_string()).join("environ"), env).unwrap();
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
    fn report_columns_take_their_widths_from_the_content() {
        let row = |pss_kb, procs, label: &str| Row {
            pss_kb,
            swap_kb: 0,
            procs,
            label: label.into(),
            unreadable: false,
        };
        let r = Report {
            children: vec![row(10240, 3, "ghostty")],
            compositor: row(2048, 1, "crucible"),
            total: row(12288, 4, "TOTAL"),
        };
        // Before (fixed widths 11/11/5), the header row was
        // "        PSS         SWAP  PROCS  APP (compositor pid 10)" and the child row
        // "   10.0 MiB      0.0 MiB      3  ghostty".
        let lines = [
            "     PSS     SWAP  PROCS  APP (compositor pid 10)",
            "10.0 MiB  0.0 MiB      3  ghostty",
            " 2.0 MiB  0.0 MiB      1  crucible",
            "12.0 MiB  0.0 MiB      4  TOTAL",
        ];
        assert_eq!(format_report(10, &r), lines.join("\n") + "\n");
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

    /// Compositor 10 (child cterm 20 on wayland-1) plus the given daemon-side processes.
    fn daemon_fixture(tag: &str) -> Fixture {
        let f = Fixture::new(tag);
        f.proc(
            10,
            1,
            "crucible",
            &["/x/crucible", "--winit"],
            Some((100, 0)),
        );
        f.proc(20, 10, "cterm", &["/x/cterm"], Some((300, 0)));
        f.environ(20, &[("HOME", "/h"), ("WAYLAND_DISPLAY", "wayland-1")]);
        // resident daemon on wayland-1: two windows, one running htop
        f.proc(60, 1, "cterm", &["/x/cterm", "daemon"], Some((500, 7)));
        f.environ(60, &[("WAYLAND_DISPLAY", "wayland-1")]);
        f.proc(61, 60, "bash", &["-bash"], Some((20, 0)));
        f.proc(62, 61, "htop", &["htop"], Some((30, 1)));
        f.proc(63, 60, "zsh", &["zsh"], Some((10, 0)));
        f
    }

    #[test]
    fn matching_cterm_daemon_gets_one_row_with_its_window_count() {
        let f = daemon_fixture("daemon");
        let r = build_report(&f.0, 10);
        let d: Vec<_> = r
            .children
            .iter()
            .filter(|c| c.label.starts_with("cterm daemon"))
            .collect();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].label, "cterm daemon (+2 windows)");
        // the daemon itself plus thin client 20; the windows' apps are not counted here
        assert_eq!((d[0].pss_kb, d[0].swap_kb, d[0].procs), (800, 7, 2));
        assert!(format_report(10, &r).contains("  cterm daemon (+2 windows)\n"));
        let row = |l: &str| {
            r.children
                .iter()
                .find(|c| c.label == l)
                .map(|c| (c.pss_kb, c.swap_kb, c.procs))
        };
        assert_eq!(
            row("htop"),
            Some((50, 1, 2)),
            "bash + htop, labelled by the app"
        );
        assert_eq!(
            row("zsh"),
            Some((10, 0, 1)),
            "a bare shell window is the shell"
        );
        assert_eq!(row("cterm"), None, "the thin client has no row of its own");
    }

    #[test]
    fn total_includes_the_daemon_row() {
        let f = daemon_fixture("daemontotal");
        let r = build_report(&f.0, 10);
        assert_eq!(r.children.len(), 3);
        assert_eq!(r.total.pss_kb, 100 + 300 + 560);
        assert_eq!(r.total.procs, 1 + 1 + 4);
    }

    #[test]
    fn daemon_on_another_display_is_excluded() {
        let f = daemon_fixture("daemonother");
        f.proc(70, 1, "cterm", &["/y/cterm", "daemon"], Some((900, 0)));
        f.environ(70, &[("WAYLAND_DISPLAY", "wayland-10")]);
        f.proc(71, 70, "bash", &["bash"], Some((10, 0)));
        let r = build_report(&f.0, 10);
        assert_eq!(r.children.len(), 3);
        assert!(r.children.iter().all(|c| c.pss_kb != 910));
    }

    #[test]
    fn cterm_without_daemon_argument_outside_the_tree_is_excluded() {
        let f = daemon_fixture("daemonplain");
        f.proc(
            80,
            1,
            "cterm",
            &["/x/cterm", "-e", "daemon"],
            Some((900, 0)),
        );
        f.environ(80, &[("WAYLAND_DISPLAY", "wayland-1")]);
        f.proc(81, 1, "cterm", &["/x/cterm"], Some((900, 0)));
        f.environ(81, &[("WAYLAND_DISPLAY", "wayland-1")]);
        let r = build_report(&f.0, 10);
        let labels: Vec<_> = r.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["cterm daemon (+2 windows)", "htop", "zsh"]);
    }

    #[test]
    fn windows_of_one_app_share_a_row_and_a_standalone_cterm_keeps_its_own() {
        let f = daemon_fixture("daemonapps");
        f.proc(
            64,
            60,
            "crucible",
            &["/x/crucible", "resume", "--cwd", "/", "--", "htop"],
            Some((5, 0)),
        );
        f.proc(65, 64, "htop", &["htop"], Some((40, 0)));
        f.proc(30, 10, "cterm", &["/x/cterm"], Some((900, 0)));
        f.proc(31, 30, "bash", &["bash"], Some((10, 0)));
        // a restore banner not yet confirmed: only the wrapper runs
        f.proc(
            66,
            60,
            "crucible",
            &["/x/crucible", "resume", "--cwd", "/", "--", "vim"],
            Some((3, 0)),
        );
        let r = build_report(&f.0, 10);
        let labels: Vec<_> = r
            .children
            .iter()
            .map(|c| (c.label.as_str(), c.pss_kb, c.procs))
            .collect();
        assert_eq!(
            labels,
            [
                ("cterm", 910, 2),
                ("cterm daemon (+4 windows)", 800, 2),
                ("htop", 95, 4),
                ("zsh", 10, 1),
                ("crucible resume", 3, 1),
            ]
        );
    }
}
