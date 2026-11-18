//! `cde-login`: the CDE login daemon. It serves the greetd protocol to `crucible greeter` on
//! VT 7, starts the user session on VT 8 in graphics mode while the greeter stays in front, and
//! switches VTs only once the session reports its first frame ready.

pub mod daemon;
pub mod pam;
pub mod seam;
pub mod server;
pub mod sys;
pub mod vt;
pub mod worker;
