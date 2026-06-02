//! Headless WSL launch. Ports `src/WslTerminal/WslProcess.cs`.
//!
//! Prefers `%ProgramFiles%\WSL\wslg.exe` (GUI-subsystem launcher — no console
//! window) over `System32\wsl.exe`, overridable via `$WSL_LAUNCHER`. The child's
//! stdin/stdout are the wslptyd transport.
//!
//! Uses std::process::Command. On Windows we add CREATE_NO_WINDOW so the wsl.exe
//! *fallback* stays windowless too (it's a no-op for the GUI-subsystem wslg.exe).

use std::io;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Resolve the launcher: `$WSL_LAUNCHER`, then `%ProgramFiles%\WSL\wslg.exe`,
/// then `System32\wsl.exe`.
pub fn resolve_launcher() -> PathBuf {
    if let Ok(p) = std::env::var("WSL_LAUNCHER") {
        let pb = PathBuf::from(&p);
        if !p.trim().is_empty() && pb.is_file() {
            return pb;
        }
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        let wslg = PathBuf::from(pf).join("WSL").join("wslg.exe");
        if wslg.is_file() {
            return wslg;
        }
    }
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    PathBuf::from(sysroot).join("System32").join("wsl.exe")
}

/// A launched WSL process with its raw stdio. `stdin`/`stdout` are the transport.
pub struct WslProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl WslProcess {
    /// Launch `<launcher> ~ --distribution <distro> <command>`. The distro name
    /// is passed unquoted (wsl parses its command line raw). `command` is one
    /// already-built shell snippet (see `bootstrap::build_server_command`).
    pub fn launch(distribution: &str, command: &str) -> io::Result<WslProcess> {
        let launcher = resolve_launcher();
        let mut cmd = Command::new(&launcher);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        build_tail(&mut cmd, distribution, command);
        no_window(&mut cmd);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
        // Drain the launcher's stderr to ours so WSL-side errors are visible
        // (the C# app surfaced these in a console; here we just forward them).
        if let Some(mut err) = child.stderr.take() {
            std::thread::Builder::new()
                .name("wsl-stderr".into())
                .spawn(move || {
                    let mut sink = io::stderr();
                    let _ = io::copy(&mut err, &mut sink);
                })
                .ok();
        }
        Ok(WslProcess { child, stdin: Some(stdin), stdout: Some(stdout) })
    }

    /// Take the transport handles (exactly once, by the mux).
    pub fn take_stdio(&mut self) -> (ChildStdin, ChildStdout) {
        (
            self.stdin.take().expect("stdin already taken"),
            self.stdout.take().expect("stdout already taken"),
        )
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

/// `System32\wsl.exe` — used for the detached vsock bootstrap. Unlike `wslg.exe`
/// (a GUI-session launcher that tears down its child tree on exit), `wsl.exe`
/// lets a `setsid`-detached daemon keep running after it returns.
fn resolve_wsl_exe() -> PathBuf {
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    PathBuf::from(sysroot).join("System32").join("wsl.exe")
}

/// Standard base64 encode (no deps) — used to ship the bootstrap snippet through
/// wsl.exe without any quotes/metacharacters on the Windows command line.
fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        s.push(A[((n >> 18) & 63) as usize] as char);
        s.push(A[((n >> 12) & 63) as usize] as char);
        s.push(if c.len() > 1 { A[((n >> 6) & 63) as usize] as char } else { '=' });
        s.push(if c.len() > 2 { A[(n & 63) as usize] as char } else { '=' });
    }
    s
}

/// Fire-and-forget bootstrap: start the vsock daemon and return immediately —
/// do NOT wait. With the `exec setsid` snippet the daemon becomes the foreground
/// of this `wsl.exe`, which stays alive hosting it until the daemon auto-exits
/// (when the last window closes); waiting here would block forever. We
/// base64-encode the snippet and run
/// `wsl.exe -d <distro> -- /bin/sh -c "echo <b64> | base64 -d | /bin/sh"` so the
/// only thing on the Windows command line is the base64 alphabet — wsl.exe can't
/// mangle the quotes inside (it does, otherwise). `wsl.exe` (not `wslg.exe`) so
/// no GUI session tears the daemon down.
pub fn spawn_bootstrap(distribution: &str, command: &str) -> io::Result<()> {
    let inner = format!("echo {} | base64 -d | /bin/sh", b64(command.as_bytes()));
    let mut cmd = Command::new(resolve_wsl_exe());
    cmd.arg("-d").arg(distribution).arg("--").arg("/bin/sh").arg("-c").arg(&inner);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    no_window(&mut cmd);
    cmd.spawn()?; // detached: drop the Child without waiting
    Ok(())
}

/// Build `~ --distribution <name> <command>` as the launcher's argument tail.
///
/// wsl/wslg parse their tail raw and run it through the login shell, so the
/// shell snippet must reach them VERBATIM. On Windows we use `raw_arg` to append
/// it without std's usual quoting — otherwise the whole `d=...; rm -f...` snippet
/// would be wrapped in one quoted argv element and the login shell (zsh) would
/// try to `exec` it as a single program name. This mirrors the C# `CreateProcess`
/// path, which appends the command unquoted. The distro name is also unquoted
/// (quotes would yield WSL_E_DISTRO_NOT_FOUND).
#[cfg(windows)]
fn build_tail(cmd: &mut Command, distribution: &str, command: &str) {
    use std::os::windows::process::CommandExt;
    cmd.raw_arg(format!("~ --distribution {distribution} {command}"));
}

#[cfg(not(windows))]
fn build_tail(cmd: &mut Command, distribution: &str, command: &str) {
    cmd.arg("~").arg("--distribution").arg(distribution).arg(command);
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}
