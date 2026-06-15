//! Configuration for Crucible Dev Env (CDE).
//!
//! CDE is configured by a declarative KDL file (config-file-only for the MVP, settings UI on the
//! roadmap). This crate handles parsing, validation, and hot-reload (watching the file and emitting
//! a new config on change). Implementation lands in Phase 3 alongside persistence; see `plans/`.
