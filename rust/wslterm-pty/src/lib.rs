//! Windows-side WSL transport for WSL Terminal: launch `wslg.exe` and speak the
//! wslptyd multiplex protocol. Ports `WslProcess.cs`, `WslMux.cs`,
//! `WslBootstrap.cs` from the C# app.
//!
//! - `bootstrap`: locate helpers + build the staging shell commands (pure).
//! - `protocol`: the length-prefixed frame format + demux (transport-free, tested).
//! - `process`:  launch wslg.exe headlessly (Windows; std::process).
//! - `mux`:      session map + reader thread, delivering `MuxEvent`s.

pub mod bootstrap;
pub mod mux;
pub mod process;
pub mod protocol;
#[cfg(windows)]
pub mod vsock;

pub use mux::{MuxEvent, WslMux};
pub use process::WslProcess;
