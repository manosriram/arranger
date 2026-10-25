//! Shared by the `arranger` (CLI + daemon) and `arranger-gui` binaries.
//! Only `arranger-gui` links winit/OpenGL, so the daemon stays lean.

pub mod adapter;
pub mod chat;
pub mod client;
pub mod daemon;
pub mod headless;
pub mod machine;
pub mod proto;
pub mod pty;
pub mod review;
pub mod screen;
pub mod security;
pub mod shell;
