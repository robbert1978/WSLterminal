# WSL Terminal

A Windows terminal app that runs a WSL shell on a **genuine Linux PTY**, launched
**headlessly** — no `conhost`/ConPTY, no Windows Terminal window, and not the
cooked stdio you get from a plain piped `wsl.exe`.

```
Windows (C# / .NET 9, WPF, one process)            WSL2 distro (Linux)
┌─────────────────────────────┐                    ┌──────────────────────────────┐
│ Window 1  TerminalView+VT ──┐│                    │ wslptyd  (one server)        │
│ Window 2  TerminalView+VT ──┤│  multiplexed       │   session 1: forkpty→/dev/pts │
│ Window N  TerminalView+VT ──┘│  frames over ONE   │   session 2: forkpty→/dev/pts │
│ WslMux ─ CreateProcess ──────┼─ wslg.exe pipe ──► │   …each a login shell on a pty│
│   wslg.exe (GUI subsystem)   │                    └──────────────────────────────┘
└─────────────────────────────┘
   N windows = 1 wslg.exe + 1 server (+ N shells); GUI subsystem → no console window
```

## Why this design

* **Headless `wslg.exe` (GUI subsystem).** Talking to a WSL2 distro goes through
  the `wsl.exe` machinery either way — even `wslapi.dll`'s `WslLaunch` just shells
  out to `wsl.exe` (verified by inspecting the child's command line), and it does
  so *without* `CREATE_NO_WINDOW`, so on Win11 (where Windows Terminal is the
  default terminal) it pops a WT window. We instead launch
  **`C:\Program Files\WSL\wslg.exe`** — the GUI-subsystem launcher Microsoft ships
  next to `wsl.exe` — with redirected pipe handles. Because it's a GUI-subsystem
  binary, Windows never allocates a console for it, so no console/terminal window
  ever appears (a console-subsystem `wsl.exe` flashes one even with
  `CREATE_NO_WINDOW`). It takes the same arguments and relays stdio identically;
  we fall back to `System32\wsl.exe` (with `CREATE_NO_WINDOW`) if `wslg.exe` is
  absent, and `$WSL_LAUNCHER` overrides the choice. The app itself is also a
  **GUI-subsystem** binary, so launching it doesn't allocate a console either.
* **One PTY server, many sessions.** `native/wslptyd.c` runs once per distro and
  multiplexes every terminal over a single `wslg.exe` + pipe connection. Opening N
  windows costs 1 `wslg.exe` + 1 server (+ the N shells), instead of
  N×(launcher + helper). Each `OPEN` does a real `forkpty()` → its own `/dev/pts/N`
  login shell, so every session is a genuine TTY (job control, line discipline,
  `SIGWINCH`, echo). A plain piped `wsl.exe <cmd>` reports `tty: not a tty`; through
  this path the shell reports `/dev/pts/N`. (A single-session helper, `wslpty.c`,
  is still used by the console diagnostic modes.)
* **Single instance.** The app is single-instance (per-user named pipe): the first
  launch is the host that owns the server; later launches forward their
  (distro, cwd) to the host and exit. So *every* window — whether from Ctrl+Shift+N
  or relaunching the exe — lives in one process and shares one `wslptyd` per distro.
* **Multiplex protocol.** Length-prefixed frames `[u32 session][u8 type][u32 len][payload]`:
  `OPEN`(cols,rows,cwd,shell), `DATA`, `RESIZE`, `SIGNAL`, `CLOSE` (host→server)
  and `DATA`, `EXIT` (server→host). The single-session helper uses a simpler
  unframed-session variant:

  | type | payload | action |
  |------|---------|--------|
  | `0x00` DATA   | `u32le len`, bytes | write to the PTY master |
  | `0x01` RESIZE | `u16le cols`, `u16le rows` | `TIOCSWINSZ` (+ `SIGWINCH`) |
  | `0x02` SIGNAL | `u8 signo` | `kill(child, signo)` |

  The PTY's output streams back on stdout as raw VT bytes, which the Windows
  side parses and renders.

## Layout

```
native/wslptyd.c       multiplexed PTY server (one server, many /dev/pts sessions)
native/wslpty.c        single-session helper (used by the console diagnostic modes)
native/build.sh        builds both inside WSL -> artifacts/
src/WslTerminal/
  Native.cs            P/Invoke: CreatePipe / CreateProcess (headless) / console attach
  WslProcess.cs        headless wsl.exe launch (CREATE_NO_WINDOW) -> raw stdio streams
  WslMux.cs            multiplexes N PTY sessions over one wslptyd; per-distro manager
  WslSession.cs        single-session framing over WslProcess (console modes)
  ConsoleHelper.cs     attaches a parent console for the diagnostic modes (GUI-subsystem build)
  WslBootstrap.cs      stages wslptyd/wslpty into /tmp and builds the launch commands
  Vt/                  VT emulator: Theme, Cell, Screen, VtParser, Terminal (+ text extraction)
  Ui/TerminalView.cs   WPF GlyphRun renderer + keyboard/mouse/selection/resize/zoom
  Ui/EmojiRenderer.cs  Direct2D/DirectWrite color-emoji rasterizer (cached bitmaps)
  Ui/InputEncoder.cs   keys -> xterm/VT byte sequences
  Ui/MainWindow.cs     window: tab strip + chrome; manages tabs, panes, settings
  Ui/TerminalTab.cs    one tab = a pane tree (Root/Active) + its strip chip
  Ui/Pane.cs           pane tree: Pane (Terminal+TerminalView+MuxSession) | SplitNode
  Ui/SettingsWindow.cs appearance dialog (font / size / scheme / colors)
  Settings.cs          persisted appearance (JSON in %APPDATA%\WslTerminal)
  Schemes.cs           built-in color schemes
  wsl.ico              app/window icon (official WSL icon)
  Program.cs           STA entry point and modes
```

## Build

```powershell
./build.ps1            # builds wslpty in WSL, then the .NET app
```

Requires: WSL2 with a C toolchain (`gcc`/`cc`, `pty.h`, `libutil`) and the
.NET 9 SDK with the Windows Desktop runtime. The first build restores one NuGet
package (`Vortice.Direct2D1`, for color-emoji rasterization), so it needs network
access once.

### Standalone single-file build

```powershell
dotnet publish src\WslTerminal\WslTerminal.csproj -c Release -r win-x64 --self-contained
```

This emits a single, self-contained **`WslTerminal.exe`** (.NET runtime + WPF +
all dependency DLLs bundled and compressed inside the exe — no .NET install
needed to run it) under `…\net9.0-windows\win-x64\publish\`. Drop the two Linux
helpers next to it so the app can stage them into the distro:

```
publish\WslTerminal.exe
publish\artifacts\wslpty
publish\artifacts\wslptyd
```

(WPF can't be trimmed, so the exe is ~130 MB on disk; that's the runtime, not the
app.) The helpers come from `artifacts\` after `build.ps1` / `native/build.sh`.

## Run

```powershell
# the terminal app
src\WslTerminal\bin\Release\net9.0-windows\WslTerminal.exe [--distro Ubuntu] [--cd /linux/path]
```

The app is a GUI-subsystem binary (so launching it never pops a console window).
The diagnostic modes below print to a console; since PowerShell's `&` doesn't
block on a GUI exe, capture them with
`Start-Process WslTerminal.exe -ArgumentList '--selftest' -Wait -RedirectStandardOutput out.txt`:

```
--selftest     # drives `tty; exit` through the path -> /dev/pts/N
--probe        # contrast case (no helper -> "not a tty")
--vttest       # headless VT-emulator unit checks
--rendertest   # headless GlyphRun render check (offscreen bitmap)
--settingstest # verify the appearance dialog opens
--tabtest      # open a window, add/close tabs, assert the tab count tracks
--splittest    # split the active pane right/down, close one; assert pane count
--muxtest      # open two PTY sessions over one server; assert distinct /dev/pts
--emojitest    # render emoji/kaomoji/CJK and assert fallback + combining work
--benchtest    # headless VT parse+grid throughput (termbench-style workloads)
--pipetest     # end-to-end input throughput through the real WSL pipe (no render)
--interactive  # minimal console relay (no GUI)
```

## Features

* **Tabs and split panes** — multiple terminals per window; each tab is a tree of
  panes you can split right/down (draggable splitters), and each pane is its own
  session on the shared server. Plus one PTY **server** per distro
  multiplexing all windows over a single wsl.exe; the app is single-instance, so
  relaunching it reuses the same host + server.
* Real `/dev/pts/N` shell via `forkpty` (full job control, resize, echo).
* Mouse text **selection** (drag, double-click word, triple-click line) with
  copy (Ctrl+Shift+C or right-click) and paste.
* VT/ANSI emulator: SGR (16/256/truecolor), cursor/erase/scroll-region ops,
  insert/delete, alternate screen, scrollback, DEC line-drawing, OSC titles,
  bracketed paste, device-status replies, UTF-8 incl. wide chars.
* Unicode rendering with **font fallback** (CJK, Thai, symbols) and **combining
  marks** for characters the primary font lacks (via `FormattedText`), plus
  **color emoji** rasterized with **Direct2D + DirectWrite** (WPF can't render
  color fonts) and cached per grapheme.
* GPU-friendly `GlyphRun` rendering with fixed monospace advances (no ligature
  drift), bold/italic, underline/strike, reverse, block cursor.
* Keyboard: arrows (normal/application), Home/End/PgUp/PgDn/Insert/Delete,
  F1–F12, Ctrl/Alt/Shift modifiers, meta-prefix, Ctrl-letter control codes.
* Mouse-wheel scrollback, paste (Ctrl+Shift+V / Shift+Insert), live window
  resize → `SIGWINCH`.
* Configurable font (family + size), color scheme, and bg/fg/cursor colors
  (Ctrl+, dialog; Ctrl +/-/0 and Ctrl+wheel to zoom), persisted to JSON.
* Ctrl+Shift+N opens a new window in the shell's current directory (OSC 7).

## Performance

Input draining (VT parse + grid update) runs on the reader thread, decoupled
from rendering (a 60 Hz `DispatcherTimer` renders the latest grid), so render
speed never gates throughput. The scrollback is a ring buffer (O(1) push, with
row-array recycling) and the parser bulk-processes printable runs. On
termbench-style workloads the parse+grid stage matches or beats Windows
Terminal; the end-to-end rate (~0.036 GB/s on ManyLine) is on par with WT and
bounded by the shared WSL relay transport, not the terminal. Measure with
`--benchtest` (parse+grid) and `--pipetest` (full input chain).

## Keys

| key / action | does |
|-----|--------|
| Ctrl+Shift+T / the + button | new tab (in the active tab's directory) |
| Alt+Shift+= / Alt+Shift+− | split the active pane right / down (same directory) |
| Ctrl+Shift+W | close the active pane (last pane closes the tab) |
| tab ✕ / middle-click tab | close the whole tab (all its panes) |
| Ctrl+Tab / Ctrl+Shift+Tab / click a tab | next / previous / select tab |
| click a pane / drag a splitter | focus that pane / resize the split |
| drag / double-click / triple-click | select chars / word / line |
| Ctrl+Shift+C, or right-click (with selection) | copy selection |
| Ctrl+Shift+V / Shift+Insert / right-click (no selection) | paste |
| Ctrl+Shift+N | new window in the shell's current directory |
| Ctrl+, | open the appearance settings dialog |
| Ctrl+= / Ctrl+- / Ctrl+0 | increase / decrease / reset font size |
| Ctrl+mouse wheel | zoom font size |
| Shift+PageUp / PageDown, mouse wheel | scroll the scrollback |

## Appearance / settings

Press **Ctrl+,** for a conhost-style dialog to change the font family, size, a
named color scheme (Campbell, One Half Dark, Solarized Dark, Tango Dark), and
the background/foreground/cursor colors. Changes apply live and persist to
`%APPDATA%\WslTerminal\settings.json`.

Settings keys (the JSON is also editable directly):

| key | meaning |
|-----|---------|
| `FontFamily`, `FontSize` | font face and size **in points** (like Windows Terminal/conhost) |
| `Background`, `Foreground`, `Cursor`, `Selection` | `#RRGGBB` colors |
| `Ansi` | the 16 ANSI colors (`black…white`, then `bright*`) |
| `Opacity` | window opacity %, 10–100 (100 = opaque) |

`Opacity` < 100 makes the window translucent (the desktop shows through the
background). WPF transparency requires a borderless window, so in that mode the
app draws a small custom title bar (drag + minimize/maximize/close) with resize
borders; at `100` it uses the normal native frame. (WT's `useAcrylic`/blur and
`backgroundImage` aren't supported — this is plain opacity.)

**New window in the same directory** (Ctrl+Shift+N) works because the shell
reports its working directory via OSC 7; the new window is launched with
`--cd <that path>`.
