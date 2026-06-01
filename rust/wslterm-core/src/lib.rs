//! Portable VT-emulator core for WSL Terminal — no GUI, no platform deps.
//!
//! Ports the logic in `src/WslTerminal/Vt/*` and `Ui/InputEncoder.cs` from the
//! C# app. Everything here is unit-testable without a renderer or a real PTY,
//! which is why the rewrite starts here: it's the bulk of the correctness
//! surface (the SGR/underline and `\e[>4m` bugs fixed in v1.0.2 live here).

#[macro_use]
mod flags;

pub mod cell;
pub mod color;

pub use cell::{Cell, CellFlags};
