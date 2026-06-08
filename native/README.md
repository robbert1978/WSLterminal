# `native/` — the in-VM Linux PTY helpers

These two small C programs run **inside** the WSL2 distro and give the Windows
app a *genuine Linux pseudo-terminal* — `forkpty()` → `/dev/pts/N` with the login
shell as session leader, full job control, line discipline, `SIGWINCH`, `ioctl`
resize — instead of the cooked stdio you get from invoking `wsl.exe` through
conhost/ConPTY.

| file | role |
|---|---|
| **`wslptyd.c`** | The **multiplexed PTY server** the GUI uses: one daemon serves **many** sessions (split panes, tabs, windows) over **one** connection. |
| **`wslpty.c`** | A **single-session** helper (one PTY over its own stdin/stdout). Used for diagnostics / as a minimal reference. |
| **`build.sh`** | Builds both as **static** ELFs into `../artifacts/` (so they run in any distro). |

> Building/running: `../build.ps1` compiles these inside WSL, or run
> `cd native && sh build.sh` directly. The Windows side is the `wslterm-pty`
> crate; running the daemon under systemd is documented in `../systemd/`.

## Topology (how `wslptyd` fits in)

```
        Windows host                       │              WSL2 VM (Linux)
                                           │
  ┌─────────────────────────┐             │     ┌───────────────────────────────┐
  │ wslterm.exe (GUI)        │             │     │ wslptyd --vsock 5523          │
  │                          │             │     │ (listener)                    │
  │  wslterm-pty crate:      │  AF_HYPERV  │     │      │ accept()                │
  │   • vsock client         │   vsock     │     │      ▼ fork()                 │
  │   • mux (framed) ●───────┼─────────────┼────▶│   connection child  (1/window)│
  │                          │  port 5523  │     │    run_session_loop + poll()  │
  └─────────────────────────┘             │     │      ├ forkpty ▶ shell 1 (pts) │
                                           │     │      ├ forkpty ▶ shell 2 (pts) │
   one connection per window,             │     │      └ forkpty ▶ shell 3 (pts) │
   N panes multiplexed on it              │     └───────────────────────────────┘
```

The listener binds `AF_VSOCK` (`CID_ANY`) on port **5523** and `accept()`-forks a
child per connection. Each child runs one `run_session_loop` that `poll()`s the
connection plus every session's PTY master, and `forkpty()`s a login shell per
`OPEN`. One daemon is shared by all windows; if vsock is unavailable the GUI
instead launches `wslptyd` with a single connection over **stdin/stdout** (the
`wslg.exe` pipe fallback — same protocol, no `--vsock`).

## `wslptyd` wire protocol

Length-prefixed frames, **little-endian**, both directions:

```
  ┌──────────┬──────┬──────────┬─────────────────┐
  │ session  │ type │   len    │  payload (len B) │
  │  u32     │  u8  │   u32    │       …          │
  └──────────┴──────┴──────────┴─────────────────┘
       4        1        4
```

| dir | type | name | payload |
|---|---|---|---|
| host → daemon | 1 | `OPEN` | `u16 cols, u16 rows, u32 cwdLen, cwd, u32 shellLen, shell` → `forkpty()` a shell |
| host → daemon | 2 | `DATA` | raw bytes → write to that session's PTY master |
| host → daemon | 3 | `RESIZE` | `u16 cols, u16 rows` → `TIOCSWINSZ` |
| host → daemon | 4 | `SIGNAL` | `u8 signo` → `kill(child, signo)` |
| host → daemon | 5 | `CLOSE` | *(empty)* → `SIGHUP` + reap the session |
| daemon → host | 2 | `DATA` | raw PTY output for that session |
| daemon → host | 6 | `EXIT` | `u32 exitcode` → session ended |
| daemon → host | 7 | `INFO` | this distro's registration name (sent once on connect; session id 0) |

`OPEN` honors the requested `cwd` if it exists, else falls back to `$HOME`. The
daemon exports `WSLTERM=1`, `TERM_PROGRAM=WSLTerminal`, and `WSL_DISTRO_NAME`
(self-detected via `wslpath -m /` when WSL didn't already set it — the daemon is
detached from the session that normally would) so shells/apps (and the
shell-integration script) can detect this terminal.

## Data path: zero-copy PTY → host (`splice`)

PTY output is the hot path, so it's forwarded **without copying the payload
through userspace** — only the 9-byte header is. Each readable master is moved
master → pipe → connection entirely in the kernel:

```
  shell ─▶ pts (slave) ═▶ master ──splice()──▶ [ pipe ] ──splice()──▶ vsock ─▶ host
                                   └──────────── kernel, zero-copy ───────────┘
  per chunk: write the 9-byte DATA header (userspace), then splice the payload.

  fallback if splice is unsupported (EINVAL/ENOSYS):
       master ──read()──▶ buf (64 KB) ──writev(hdr+payload)──▶ vsock      (one copy)
```

A single `O_CLOEXEC` pipe is reused per connection (drained fully each chunk, so
shells never inherit it). The fallback flips on permanently if `splice` is ever
rejected, so it's safe on older kernels.

## Lifecycle

- **Auto-exit:** when the last connection closes the listener exits, so nothing
  lingers (the app's ephemeral bootstrap relies on this). It also `unlink`s its
  staged copy at `/tmp/wslptyd`.
- **`--persist`:** disables that auto-exit, for running under systemd where
  `Restart=always` would otherwise cycle the service and tear down live sessions.
  See `../systemd/`.
- **`EADDRINUSE`:** if another daemon already owns 5523, `wslptyd` exits 0 and
  leaves the staged binary alone (it yields to the running owner).

## `wslpty` (single-session helper)

A minimal bridge: it `forkpty()`s **one** login shell and relays bytes between
the master and its own **stdin/stdout**. The Windows side launches it via
`wslapi.dll`'s `WslLaunch()`, handing three Windows pipe handles as fd 0/1/2.

Its protocol is simpler than the daemon's (no session id; **raw** output):

- **stdout (fd 1):** raw PTY output, verbatim.
- **stdin (fd 0):** framed control/data:

  | type | name | payload |
  |---|---|---|
  | `0x00` | `DATA` | `u32 len`, then `len` bytes → PTY master |
  | `0x01` | `RESIZE` | `u16 cols, u16 rows` → `TIOCSWINSZ` |
  | `0x02` | `SIGNAL` | `u8 signo` → `kill(child, signo)` |

Standalone test aid: `wslpty --exec <cmd> [args…]` runs `<cmd>` on the PTY
instead of the login shell, e.g. `wslpty --exec sh -c 'tty'`. Set `WSLPTY_DEBUG=1`
for stderr tracing.

## Build

```sh
sh build.sh            # CC=cc by default; static, -lutil (forkpty)
```

Both link `-static` so a binary built in one distro runs in any distro; outputs
land in `../artifacts/{wslptyd,wslpty}`, from where the Windows host stages them
into WSL at runtime.
