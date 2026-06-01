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
