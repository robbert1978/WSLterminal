# Running `wslptyd` under systemd (optional, advanced)

By default you **don't need this**. WSL Terminal auto-starts `wslptyd` inside the
WSL VM on first use (staged to `/tmp/wslptyd` and launched detached), shares one
daemon across all windows, and shuts it down when the last window closes.

This directory is for people who'd rather have **systemd** own the daemon, which
gives you:

- **No cold start** — the daemon is already listening, so opening a window is an
  instant vsock connect instead of the ~4–5 s first-run bootstrap.
- **journald logging** and `systemctl` lifecycle (`status`, `restart`, …).
- A binary installed at a **stable path** instead of re-staged to `/tmp`.

It's intentionally **not part of the GitHub release** — it lives in the source
tree for those who want it.

## How it fits together

`wslptyd --vsock 5523` binds `AF_VSOCK` on port **5523**, accepts one connection
per window, and forks a PTY session per connection. On its own it **auto-exits
when the last connection closes** (for the app's ephemeral bootstrap), so the
unit passes **`--persist`** to keep it resident instead — otherwise `Restart=always`
would cycle the service every time you closed a window, and the cgroup kill on
restart would tear down sessions you were still using. The unit also sets
**`KillMode=process`** so a stop/restart only signals the listener, never your
shells. The app probes the port first and only falls back to its own bootstrap if
nothing is listening — so once this service is up, the app just connects to it.

Run it as a **user service** so the shells it spawns are *yours* (your uid,
`$HOME`, and login config), not root's.

## Prerequisites

systemd must be enabled in WSL2 (WSL 0.67.6+). In the distro:

```ini
# /etc/wsl.conf
[boot]
systemd=true
```

Then from Windows: `wsl --shutdown` and reopen the distro. Verify with
`systemctl is-system-running` (it should print `running` or `degraded`).

> If your distro/setup doesn't use systemd, skip all of this — the app's
> built-in auto-start already works without it.

## Install

1. **Build the daemon** (from the repo root, on Windows):

   ```powershell
   ./build.ps1
   ```

   or inside WSL: `cd native && sh build.sh`. Either way the ELF lands in
   `artifacts/wslptyd`.

2. **Install the binary** to a stable path on your `PATH` — **not** `/tmp`
   (the daemon unlinks `/tmp/wslptyd` on exit):

   ```sh
   mkdir -p ~/.local/bin
   cp /mnt/c/path/to/WSLterminal/artifacts/wslptyd ~/.local/bin/wslptyd
   chmod +x ~/.local/bin/wslptyd
   ```

3. **Install the unit** and enable it:

   ```sh
   mkdir -p ~/.config/systemd/user
   cp /mnt/c/path/to/WSLterminal/systemd/wslptyd.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now wslptyd.service
   ```

4. **Keep it running without an open shell / across boots** — by default a user
   manager stops when you log out. Enable lingering so the service starts at WSL
   boot and stays up:

   ```sh
   sudo loginctl enable-linger "$USER"
   ```

## Verify

```sh
systemctl --user status wslptyd.service      # should be active (running)
journalctl --user -u wslptyd.service -e      # logs
ss -lx 2>/dev/null; ss --vsock -l 2>/dev/null # port 5523 listening (if ss supports vsock)
```

Open a WSL Terminal window — it should appear instantly (no first-run delay).

## System-wide alternative

To run it as a **system** service instead (starts with the VM, no lingering
needed), set the user explicitly and install under `/etc`:

```ini
# /etc/systemd/system/wslptyd.service  — add these to the [Service] section:
User=YOUR_USERNAME
Group=YOUR_USERNAME
ExecStart=/usr/local/bin/wslptyd --vsock 5523
```

```sh
sudo cp artifacts/wslptyd /usr/local/bin/wslptyd
sudo cp systemd/wslptyd.service /etc/systemd/system/   # then edit User=/ExecStart as above
sudo systemctl daemon-reload
sudo systemctl enable --now wslptyd.service
```

(Use `systemctl` / `journalctl -u wslptyd` without `--user` for a system unit.)

## Notes & troubleshooting

- **Port:** 5523, matched to the app's `wslterm-pty` client. If you change it,
  you'd also have to change the app — don't.
- **It should stay resident** (`--persist`), not restart on every window close.
  If `systemctl --user status wslptyd` shows a climbing restart counter while you
  use it, you're on an old unit/binary without `--persist` — rebuild + reinstall
  the daemon and unit (below). The repeated restarts there can drop active
  sessions and make the app exit.
- **`EADDRINUSE` flapping:** if you opened a window *before* the service started,
  the app may have bootstrapped its own `/tmp/wslptyd` on 5523; the service then
  can't bind and exits. Fix by `pkill -f 'wslptyd --vsock'` (or `wsl --shutdown`)
  and let systemd own the port. Letting the service start at boot (lingering)
  avoids this.
- **Updating:** rebuild, copy the new `wslptyd` over `~/.local/bin/wslptyd`, then
  `systemctl --user restart wslptyd.service`.

## Uninstall

```sh
systemctl --user disable --now wslptyd.service
rm ~/.config/systemd/user/wslptyd.service ~/.local/bin/wslptyd
systemctl --user daemon-reload
# sudo loginctl disable-linger "$USER"   # if you enabled it
```
