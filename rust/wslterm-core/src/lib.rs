//! Portable VT-emulator core for WSL Terminal — no GUI, no platform deps.
//!
//! Ports the logic in `src/WslTerminal/Vt/*` from the C# app: the VT/ANSI
//! parser, the screen grid + scrollback, SGR/color handling, and char width.
//! Everything here is unit-testable without a renderer or a real PTY — it's the
//! bulk of the correctness surface (the SGR-colon and `\e[>4m` underline bugs
//! fixed in v1.0.2 live here, and their regression tests are ported verbatim).

#[macro_use]
mod flags;

pub mod cell;
pub mod charwidth;
pub mod color;
pub mod parser;
pub mod screen;
pub mod terminal;

pub use cell::{Cell, CellFlags};
pub use parser::{ParserSinks, VtParser};
pub use screen::{MouseTracking, Screen};
pub use terminal::Terminal;
