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
| surface            | WPF visual tree              | `softbuffer` CPU buffer (chosen over `wgpu` — see below) |
| text               | WPF GlyphRun                 | `ab_glyph` rasterization now; `swash` shaping later |
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
- [x] `wslterm-core`: input encoder (port InputEncoder.cs) — 8 tests
- [x] `wslterm-pty`: wslptyd protocol + bootstrap + wslg launch + mux reader
      (7 tests over in-memory transports; live wslg spawn is a thin std::process
      wrapper, not covered by unit tests)
- [x] `wslterm`: **GUI milestone 1** — winit + softbuffer + ab_glyph window that
      renders the core grid (SGR color, wide glyphs, block cursor) fed by a live
      `wslterm-pty` WSL session; keystrokes encoded via `core::input`. Verified on
      Windows: live zsh prompt renders, ~19 MB RSS (vs ~150 MB on WPF).
- [x] `wslterm`: milestone 2 — DPI scaling (font scales with monitor
      scale_factor) + mouse-wheel scrollback (cursor hidden in history, snaps to
      bottom on input/output)
- [x] `wslterm`: flood-survival + perf — bounded mux channel, off-thread feed,
      glyph cache, batched feed, ASCII width fast-path (termbench TermMarkV2
      662s -> 56s, ~40 MB, no OOM)
- [x] `wslterm`: text selection + clipboard — drag to select, Ctrl+Shift+C copy,
      Ctrl+Shift+V / Shift+Insert / middle-click paste (bracketed-paste aware)
- [x] `wslterm`: window translucency — uniform layered-window opacity
      (`$WSLTERM_OPACITY`, default 0.92; Ctrl+=/Ctrl+- to adjust live)
- [x] `wslterm`: settings.json config (colors/font/opacity) + Ctrl+/-/0 font zoom
- [x] `wslterm`: tabs (Ctrl+Shift+T/W, Ctrl+Tab, click / middle-click close)
- [x] `wslterm`: new window (Ctrl+Shift+N — spawns a child process; `--cd`)
- [x] `wslterm`: split panes (Alt+Shift +/-, click-to-focus, per-pane PTY)
- [x] `wslterm`: file sidebar (Ctrl+Shift+E, follows shell cwd, Ctrl+Shift+H
      hidden toggle; click dir = cd, click file = open)
- [x] `wslterm`: file viewer/editor (line numbers, edit, Ctrl+S save via the
      \\wsl.localhost share, Esc close)
- [ ] `wslterm`: syntax highlighting in the editor (syntect) — not yet
- [ ] `wslterm`: faster partial repaint, word/line select, per-pixel translucency, color emoji

### CPU vs GPU rendering

Milestone 1 renders on the CPU (`softbuffer` pixel buffer + `ab_glyph`
rasterization) rather than `wgpu`. This keeps RAM at terminal-appropriate levels
(the wgpu/D3D device + swapchain alone cost tens of MB — defeating the rewrite's
purpose) and removes a large class of init/driver failure. A full-grid repaint of
an 80×25 cell window is well under a millisecond; the throughput bottleneck is the
shared WSL transport, as predicted. We revisit GPU only if a real workload needs
it.

## Build

```
cd rust
cargo test -p wslterm-core      # headless, runs anywhere
cargo run  -p wslterm           # the GUI (Windows)
```
