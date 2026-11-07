//! Authenticated per-window identity (`crucible-window-identity-v1`, crucible-term plan 02):
//! grant minting for `cterm` terminal spawns, the five pinned `bind_surface` checks as a pure
//! function over an injectable [`ProcView`] and [`Grants`], and the surface-keyed identity table.
//!
//! The wire side (global, dispatch) is `crate::identity_protocol`; this module decides.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crucible_config::TerminalKind;
use crucible_persist::strip_deleted;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::SpawnKind;
use crate::state::{Backend, CrucibleState};

/// The env var a granted spawn carries its grant in.
pub const GRANT_ENV: &str = "CRUCIBLE_WINDOW_GRANT";

/// The env var a granted `cterm` spawn carries its session key in (CDE -> cterm client; the
/// client strips it before the child).
pub const SESSION_ENV: &str = "CRUCIBLE_TERM_SESSION";

/// A bound window's identity: `spawn_pid` is the CDE-spawned thin client (in the spawn
/// registry), `root_pid` the shell or wrapper the daemon forked for this window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowIdentity {
    /// Compositor-assigned id (from 1; kept within `u32`, the wire type of `bound`).
    pub id: u64,
    /// The CDE spawn the grant was minted for.
    pub spawn_pid: u32,
    /// The process the binding client forked for this window.
    pub root_pid: u32,
    /// The session key (`CRUCIBLE_TERM_SESSION`) minted for the spawn, if it carried one.
    pub session: Option<String>,
    /// The binding client's disconnect flag (shared with its `ClientState`).
    pub client: ClientLink,
}

/// A Wayland client's disconnect flag, shared between its `ClientState` and the identities it
/// binds. Two links are equal when they are the same flag.
#[derive(Debug, Clone, Default)]
pub struct ClientLink(Arc<AtomicBool>);

impl ClientLink {
    /// Records that the client disconnected.
    pub fn mark_disconnected(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether the client has disconnected.
    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl PartialEq for ClientLink {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ClientLink {}

/// How a window's destruction is handled (crucible-term plan 03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destruction {
    /// A bound window whose client disconnected: kept in its slot by a loss placeholder.
    Lost,
    /// Anything else (unbound, or the client is still connected): today's close.
    Close,
}

/// Classifies a window's destruction from its identity: lost only when bound and its client
/// has disconnected.
#[must_use]
pub fn classify_destruction(identity: Option<&WindowIdentity>) -> Destruction {
    match identity {
        Some(i) if i.client.is_disconnected() => Destruction::Lost,
        _ => Destruction::Close,
    }
}

/// The `/proc` facts the checks need, injectable so they unit-test without a real process.
pub trait ProcView {
    /// The executable of `pid` (`/proc/<pid>/exe`, trailing ` (deleted)` stripped).
    fn exe(&self, pid: u32) -> Option<PathBuf>;
    /// The parent pid of `pid`.
    fn ppid(&self, pid: u32) -> Option<u32>;
}

/// The real `/proc`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealProc;

impl ProcView for RealProc {
    fn exe(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| strip_deleted(&p))
    }

    fn ppid(&self, pid: u32) -> Option<u32> {
        let stat = crucible_persist::capture::read_stat(Path::new("/proc"), pid)?;
        u32::try_from(stat.ppid).ok()
    }
}

/// The live, unused grants: lookup by value and single-use consumption.
pub trait Grants {
    /// The spawn pid and canonical exe of the live spawn holding unused `grant`.
    fn lookup(&self, grant: &str) -> Option<(u32, PathBuf)>;
    /// Marks `grant` used.
    fn consume(&mut self, grant: &str);
}

/// The binding peer's credentials (from the Wayland connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    /// Peer process id.
    pub pid: u32,
    /// Peer user id.
    pub uid: u32,
}

/// Runs the five pinned checks for a `bind_surface(grant, root_pid)` from `peer` on a surface
/// that `has_role` or not; on success consumes the grant and returns its spawn pid.
///
/// # Errors
/// The rejection reason sent to the client.
pub fn check_bind(
    has_role: bool,
    grant: &str,
    root_pid: u32,
    peer: Peer,
    own_uid: u32,
    grants: &mut impl Grants,
    proc: &impl ProcView,
) -> Result<u32, &'static str> {
    if has_role {
        return Err("surface already has a role");
    }
    let Some((spawn_pid, exe)) = grants.lookup(grant) else {
        return Err("grant unknown, used, or its spawn reaped");
    };
    if peer.uid != own_uid {
        return Err("peer uid differs from the compositor uid");
    }
    if proc.exe(peer.pid).as_deref() != Some(exe.as_path()) {
        return Err("peer executable differs from the granted executable");
    }
    if proc.ppid(root_pid) != Some(peer.pid) {
        return Err("root_pid is not a child of the peer");
    }
    grants.consume(grant);
    Ok(spawn_pid)
}

/// The surface-keyed identity table with its id counter.
#[derive(Debug)]
pub struct IdentityTable<K> {
    by_surface: HashMap<K, WindowIdentity>,
    next_id: u32,
}

impl<K> Default for IdentityTable<K> {
    fn default() -> Self {
        Self {
            by_surface: HashMap::new(),
            next_id: 1,
        }
    }
}

impl<K: Eq + Hash> IdentityTable<K> {
    /// Records a new identity for `key` and returns it (ids from 1, wrapping within `u32`, never 0).
    pub fn insert(
        &mut self,
        key: K,
        spawn_pid: u32,
        root_pid: u32,
        session: Option<String>,
        client: ClientLink,
    ) -> WindowIdentity {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        let identity = WindowIdentity {
            id: u64::from(id),
            spawn_pid,
            root_pid,
            session,
            client,
        };
        self.by_surface.insert(key, identity.clone());
        identity
    }

    /// The identity bound to `key`.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<WindowIdentity> {
        self.by_surface.get(key).cloned()
    }

    /// Drops `key`'s identity (surface destroyed).
    pub fn remove(&mut self, key: &K) {
        self.by_surface.remove(key);
    }
}

/// Whether a spawn of `kind` running `program` gets a grant: only `cterm` terminal spawns.
#[must_use]
pub fn wants_grant(kind: SpawnKind, program: &str) -> bool {
    kind == SpawnKind::Terminal && TerminalKind::of(program) == TerminalKind::Cterm
}

/// 32 lowercase hex chars from 16 bytes of `/dev/urandom`; `None` when it cannot be read.
#[must_use]
pub fn mint_grant() -> Option<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl<B: Backend + 'static> CrucibleState<B> {
    /// Handles one `bind_surface`: the checks, then the identity recorded for `surface`.
    /// Returns the window id to send in `bound`, or the rejection reason.
    pub(crate) fn bind_surface_identity(
        &mut self,
        surface: &WlSurface,
        has_role: bool,
        grant: &str,
        root_pid: u32,
        peer: Option<Peer>,
    ) -> Result<u32, &'static str> {
        let Some(peer) = peer else {
            return Err("peer credentials unavailable");
        };
        let own_uid = rustix::process::getuid().as_raw();
        let spawn_pid = check_bind(
            has_role,
            grant,
            root_pid,
            peer,
            own_uid,
            &mut self.persist.spawn,
            &RealProc,
        )?;
        let session = self
            .persist
            .spawn
            .get(spawn_pid)
            .and_then(|r| r.session.clone());
        // The binding client's disconnect flag (XWayland clients never bind, so a missing
        // `ClientState` only yields an unshared flag that is never set).
        let client = surface
            .client()
            .and_then(|c| {
                c.get_data::<crate::state::ClientState>()
                    .map(|d| d.link.clone())
            })
            .unwrap_or_default();
        let identity =
            self.persist
                .identities
                .insert(surface.id(), spawn_pid, root_pid, session, client);
        tracing::info!(
            id = identity.id,
            spawn_pid,
            root_pid,
            "identity: surface bound"
        );
        Ok(u32::try_from(identity.id).unwrap_or(u32::MAX))
    }

    /// The identity bound to `surface`, if any.
    #[must_use]
    pub fn surface_identity(&self, surface: &WlSurface) -> Option<WindowIdentity> {
        self.persist.identities.get(&surface.id())
    }

    /// Drops `surface`'s identity (called when the surface is destroyed).
    pub fn forget_surface_identity(&mut self, surface: &WlSurface) {
        self.persist.identities.remove(&surface.id());
    }
}

/// The table type `PersistState` holds.
pub(super) type SurfaceIdentities = IdentityTable<ObjectId>;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeProc {
        exe: HashMap<u32, PathBuf>,
        ppid: HashMap<u32, u32>,
    }

    impl ProcView for FakeProc {
        fn exe(&self, pid: u32) -> Option<PathBuf> {
            self.exe.get(&pid).map(|p| strip_deleted(p))
        }
        fn ppid(&self, pid: u32) -> Option<u32> {
            self.ppid.get(&pid).copied()
        }
    }

    /// A fake registry: grant → (spawn pid, exe, used); reaping removes the entry.
    #[derive(Default)]
    struct FakeGrants(HashMap<String, (u32, PathBuf, bool)>);

    impl Grants for FakeGrants {
        fn lookup(&self, grant: &str) -> Option<(u32, PathBuf)> {
            self.0
                .get(grant)
                .filter(|(_, _, used)| !used)
                .map(|(pid, exe, _)| (*pid, exe.clone()))
        }
        fn consume(&mut self, grant: &str) {
            if let Some(e) = self.0.get_mut(grant) {
                e.2 = true;
            }
        }
    }

    const UID: u32 = 1000;
    const SPAWN: u32 = 100;
    const DAEMON: u32 = 200;
    const SHELL: u32 = 300;
    const PEER: Peer = Peer {
        pid: DAEMON,
        uid: UID,
    };

    fn fixture() -> (FakeGrants, FakeProc) {
        let mut g = FakeGrants::default();
        g.0.insert("g1".into(), (SPAWN, PathBuf::from("/usr/bin/cterm"), false));
        let mut p = FakeProc::default();
        p.exe.insert(DAEMON, PathBuf::from("/usr/bin/cterm"));
        p.ppid.insert(SHELL, DAEMON);
        (g, p)
    }

    fn bind(
        g: &mut FakeGrants,
        p: &FakeProc,
        role: bool,
        grant: &str,
        peer: Peer,
    ) -> Result<u32, &'static str> {
        check_bind(role, grant, SHELL, peer, UID, g, p)
    }

    #[test]
    fn success_yields_spawn_pid_and_records_identity() {
        let (mut g, p) = fixture();
        assert_eq!(bind(&mut g, &p, false, "g1", PEER), Ok(SPAWN));
        let mut table = IdentityTable::<u8>::default();
        let id = table.insert(7, SPAWN, SHELL, None, ClientLink::default());
        assert_eq!(id.id, 1);
        assert_eq!(table.get(&7), Some(id));
        assert_eq!(
            table
                .insert(8, SPAWN, SHELL, None, ClientLink::default())
                .id,
            2
        );
        table.remove(&7);
        assert_eq!(table.get(&7), None);
    }

    #[test]
    fn unknown_grant_rejected() {
        let (mut g, p) = fixture();
        assert!(bind(&mut g, &p, false, "nope", PEER).is_err());
    }

    #[test]
    fn grant_is_single_use() {
        let (mut g, p) = fixture();
        assert!(bind(&mut g, &p, false, "g1", PEER).is_ok());
        assert!(bind(&mut g, &p, false, "g1", PEER).is_err());
    }

    #[test]
    fn grant_of_reaped_spawn_rejected() {
        let (mut g, p) = fixture();
        g.0.remove("g1");
        assert!(bind(&mut g, &p, false, "g1", PEER).is_err());
    }

    #[test]
    fn foreign_uid_rejected() {
        let (mut g, p) = fixture();
        let peer = Peer {
            pid: DAEMON,
            uid: 0,
        };
        assert_eq!(
            bind(&mut g, &p, false, "g1", peer),
            Err("peer uid differs from the compositor uid")
        );
        // A rejection does not consume the grant.
        assert!(bind(&mut g, &p, false, "g1", PEER).is_ok());
    }

    #[test]
    fn wrong_exe_rejected_and_deleted_suffix_tolerated() {
        let (mut g, mut p) = fixture();
        p.exe.insert(DAEMON, PathBuf::from("/usr/bin/evil"));
        assert!(bind(&mut g, &p, false, "g1", PEER).is_err());
        p.exe
            .insert(DAEMON, PathBuf::from("/usr/bin/cterm (deleted)"));
        assert_eq!(bind(&mut g, &p, false, "g1", PEER), Ok(SPAWN));
    }

    #[test]
    fn root_pid_not_child_of_peer_rejected() {
        let (mut g, mut p) = fixture();
        p.ppid.insert(SHELL, 1);
        assert_eq!(
            bind(&mut g, &p, false, "g1", PEER),
            Err("root_pid is not a child of the peer")
        );
    }

    #[test]
    fn surface_with_role_rejected() {
        let (mut g, p) = fixture();
        assert_eq!(
            bind(&mut g, &p, true, "g1", PEER),
            Err("surface already has a role")
        );
    }

    #[test]
    fn grant_minted_only_for_cterm_terminal_spawns() {
        assert_eq!(TerminalKind::of("cterm"), TerminalKind::Cterm);
        assert_eq!(TerminalKind::of("/usr/bin/cterm"), TerminalKind::Cterm);
        assert!(wants_grant(SpawnKind::Terminal, "/usr/bin/cterm"));
        assert!(!wants_grant(SpawnKind::Terminal, "foot"));
        assert!(!wants_grant(SpawnKind::App, "cterm"));
        assert!(!wants_grant(SpawnKind::Overlay, "cterm"));
        let g = mint_grant().expect("urandom");
        assert_eq!(g.len(), 32);
        assert!(
            g.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }
}
