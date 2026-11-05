//! The `crucible-window-identity-v1` protocol server (`protocols/crucible-window-identity-v1.xml`):
//! the generated server code, the manager global, and the `bind_surface` dispatch. The decision
//! (the five pinned checks, the identity table) is `crate::persist::identity`.

use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::get_role;

use crate::state::{Backend, CrucibleState};

/// The generated protocol code.
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
pub mod generated {
    use wayland_server;
    use wayland_server::protocol::*;

    /// Interface descriptors.
    pub mod __interfaces {
        use wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocols/crucible-window-identity-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("../../protocols/crucible-window-identity-v1.xml");
}

use generated::crucible_window_identity_manager_v1::{
    self as manager, CrucibleWindowIdentityManagerV1,
};
use generated::crucible_window_identity_v1::{self as identity, CrucibleWindowIdentityV1};

/// The sha256 of the canonical protocol XML; crucible-term's copy asserts the same constant.
pub const IDENTITY_XML_SHA256: &str =
    "4c28f2619fa151b526a53d44637ca6c2999803b961d7f9fc0c908f8e047cae9a";

/// Advertises `crucible_window_identity_manager_v1` (v1) to every client.
pub fn create_global<B: Backend + 'static>(dh: &DisplayHandle) {
    let _ = dh.create_global::<CrucibleState<B>, CrucibleWindowIdentityManagerV1, ()>(1, ());
}

impl<B: Backend + 'static> GlobalDispatch<CrucibleWindowIdentityManagerV1, ()>
    for CrucibleState<B>
{
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<CrucibleWindowIdentityManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl<B: Backend + 'static> Dispatch<CrucibleWindowIdentityManagerV1, ()> for CrucibleState<B> {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &CrucibleWindowIdentityManagerV1,
        request: manager::Request,
        _data: &(),
        dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            manager::Request::BindSurface {
                id,
                surface,
                grant,
                root_pid,
            } => {
                let obj = data_init.init(id, ());
                let has_role = get_role(&surface).is_some();
                let peer = client.get_credentials(dh).ok().and_then(|c| {
                    Some(crate::persist::identity::Peer {
                        pid: u32::try_from(c.pid).ok()?,
                        uid: c.uid,
                    })
                });
                match state.bind_surface_identity(&surface, has_role, &grant, root_pid, peer) {
                    Ok(window_id) => obj.bound(window_id),
                    Err(reason) => {
                        tracing::warn!(reason, surface = ?surface.id(), "identity: bind rejected");
                        obj.rejected(reason.to_string());
                    }
                }
            }
            manager::Request::Destroy => {}
        }
    }
}

impl<B: Backend + 'static> Dispatch<CrucibleWindowIdentityV1, ()> for CrucibleState<B> {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &CrucibleWindowIdentityV1,
        request: identity::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let identity::Request::Destroy = request;
    }
}

#[cfg(test)]
mod tests {
    use super::IDENTITY_XML_SHA256;
    use sha2::{Digest, Sha256};

    #[test]
    fn protocol_xml_sha256_is_pinned() {
        let xml = include_bytes!("../../../protocols/crucible-window-identity-v1.xml");
        let hex: String = Sha256::digest(xml)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hex, IDENTITY_XML_SHA256);
    }
}
