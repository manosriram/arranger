//! Shared by the `arranger` (CLI + daemon) and `arranger-gui` binaries.
//! Only `arranger-gui` links winit/OpenGL, so the daemon stays lean.

pub mod adapter;
pub mod client;
pub mod daemon;
pub mod proto;
pub mod pty;
pub mod screen;
