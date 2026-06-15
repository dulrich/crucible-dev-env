//! `udev`/DRM/KMS backend for running on a real TTY.
//!
//! Future home of the production backend: session management via `libseat`, GPU enumeration and
//! hotplug via `udev`, mode-setting and page-flip scheduling via DRM/KMS, buffer allocation via
//! GBM, and multi-GPU rendering via the multi-renderer. Drives real outputs and `libinput`
//! devices. Selected at runtime via `--tty-udev`.
//!
//! Implemented in a later workstream (WS-2); this is a WS-1 stub.
//!
//! WS-1 references [`smithay_drm_extras`] below only to *link* the dependency under `-D warnings`;
//! real EDID/connector discovery lands in WS-2.

use smithay_drm_extras as _;
