# WSL Terminal — Rust rewrite (in progress)

A native Rust reimplementation of WSL Terminal, on the `rust-rewrite` branch.
The existing C#/WPF app under `src/` is the shipped product and stays untouched
until this reaches parity.

## Why

The C#/WPF app idles ~150 MB — almost entirely the bundled .NET + WPF runtime,
a floor that can't be tuned away (see the v1.0.3 memory work). A native build
removes that floor; comparable Rust terminals idle ~30–60 MB. Performance is
expected to *match*, not beat, the current app: the throughput bottleneck is the
shared WSL relay transport, and the VT parser is already at Windows-Terminal
parity. The win is RAM, GC-free latency, and a smaller binary — not raw speed.

## What is reused unchanged

- `native/wslptyd.c`, `native/wslpty.c` — the Linux PTY server + single-session
  helper, and the length-prefixed multiplex protocol. Language-agnostic; the
  Rust host speaks the same frames the C# host does.

## Stack (planned)

| concern            | C#/WPF today                 | Rust plan |
|--------------------|------------------------------|-----------|
| window / input     | WPF Window                   | `winit` |
| GPU surface        | WPF visual tree              | `wgpu` |
| text + atlas       | WPF GlyphRun                 | `swash` (shaping) + glyph atlas on wgpu |
| color emoji        | Direct2D/DirectWrite         | COLR/CBDT via `swash`, or DirectWrite over `windows` crate |
| translucency       | AllowsTransparency (layered) | layered window via `windows` crate |
| launch wslg.exe    | CreateProcess + pipes        | `std::process` + `windows` pipes |
| file editor/syntax | AvalonEdit + .xshd           | `syntect` (TextMate grammars) in a custom view |

## Crates (workspace members)

- `wslterm-core` — **portable, no-GUI, fully unit-tested**: VT parser, screen
  grid + scrollback, input encoding, SGR/color. Ports directly from
  `src/WslTerminal/Vt/*` and `Ui/InputEncoder.cs`. This is where we start —
  it's the bulk of the correctness surface and needs no renderer to test.
- `wslterm-pty` — Windows-side launch of `wslg.exe` + the wslptyd multiplex
  client (ports `WslProcess.cs` / `WslMux.cs` / `WslBootstrap.cs`).
- `wslterm` — the GUI app (winit + wgpu), tabs/panes/sidebar. Last, once the
  core is proven.

## Status

- [x] workspace scaffold
- [x] `wslterm-core`: VT parser + screen + scrollback + SGR/color + charwidth
      (27 tests, incl. all 21 ported `--vttest` cases and the v1.0.2 SGR/`>4m`
      regression tests)
- [ ] `wslterm-core`: input encoder
- [ ] `wslterm-pty`: wslg launch + mux client
- [ ] `wslterm`: renderer, window, tabs/panes
- [ ] sidebar / editor / highlighting

## Build

```
cd rust
cargo test -p wslterm-core      # headless, runs anywhere
cargo run  -p wslterm           # the GUI (Windows)
```
