//! The one shared test fixture: a fake `/proc` tree plus a claude home under a unique temp dir.

use std::{fs, os::unix::fs::symlink, path::PathBuf};

use super::CaptureEnv;

/// A session id used by the claude fixtures.
pub(super) const SESSION_ID: &str = "d6e856ed-0000-0000-0000-000000000000";

/// A claude session record body for `pid`, holding [`SESSION_ID`].
pub(super) fn session_json(pid: u32) -> String {
    format!(r#"{{"pid":{pid},"sessionId":"{SESSION_ID}","cwd":"/w","name":"Lush Egret"}}"#)
}

/// `<base>/proc` (the fake `/proc`) and `<base>/claude/sessions` (the claude home's session
/// records), removed on drop. `tag` must be unique among tests (tests run in parallel); the
/// process id is added to the directory name.
pub(super) struct FakeProc {
    base: PathBuf,
}

impl FakeProc {
    pub(super) fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("cde-proc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("proc")).unwrap();
        fs::create_dir_all(base.join("claude").join("sessions")).unwrap();
        FakeProc { base }
    }

    /// The fake `/proc` root.
    pub(super) fn proc_root(&self) -> PathBuf {
        self.base.join("proc")
    }

    /// The claude home (holds `sessions/`).
    pub(super) fn claude_home(&self) -> PathBuf {
        self.base.join("claude")
    }

    /// The claude sessions directory.
    pub(super) fn sessions(&self) -> PathBuf {
        self.claude_home().join("sessions")
    }

    /// The capture environment over this fixture.
    pub(super) fn env(&self) -> CaptureEnv {
        CaptureEnv {
            proc_root: self.proc_root(),
            claude_home: self.claude_home(),
            own_exe: "crucible".to_string(),
        }
    }

    /// Adds a process `(pid, comm, ppid)`; `kids` are read from `task/<pid>/children`.
    pub(super) fn add(
        &self,
        id: (u32, &str, u32),
        tpgid: i64,
        cwd: &str,
        argv: &[&str],
        kids: &[u32],
    ) {
        let (pid, comm, ppid) = id;
        let d = self.proc_root().join(pid.to_string());
        let task = d.join("task").join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(
            d.join("stat"),
            format!("{pid} ({comm}) S {ppid} {pid} {pid} 34816 {tpgid} 4194304 100 0 0 0\n"),
        )
        .unwrap();
        symlink(cwd, d.join("cwd")).unwrap();
        let cmd: Vec<u8> = argv
            .iter()
            .flat_map(|a| [a.as_bytes(), &[0]].concat())
            .collect();
        fs::write(d.join("cmdline"), cmd).unwrap();
        let list: Vec<String> = kids.iter().map(|k| format!("{k} ")).collect();
        fs::write(task.join("children"), list.concat()).unwrap();
    }

    /// [`Self::add`] for a process whose `comm` does not matter (`x`).
    pub(super) fn add_anon(
        &self,
        pid: u32,
        ppid: u32,
        tpgid: i64,
        cwd: &str,
        argv: &[&str],
        kids: &[u32],
    ) {
        self.add((pid, "x", ppid), tpgid, cwd, argv, kids);
    }

    /// A process with no readable stat, argv or cwd and no children.
    pub(super) fn add_bare(&self, pid: u32) {
        let task = self
            .proc_root()
            .join(pid.to_string())
            .join("task")
            .join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("children"), "").unwrap();
    }

    /// Writes the claude session record `sessions/<pid>.json`.
    pub(super) fn record(&self, pid: u32, body: &str) {
        fs::write(self.sessions().join(format!("{pid}.json")), body).unwrap();
    }

    /// The path of an entry directly under the fake `/proc` (for hand-made files).
    pub(super) fn proc_path(&self, rel: &str) -> PathBuf {
        self.proc_root().join(rel)
    }
}

impl Drop for FakeProc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}
