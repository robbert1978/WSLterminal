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
        // wsl/wslg take: `~ --distribution <name> <argv...>`. We pass the shell
        // snippet as a single trailing arg; wslg runs it via the login shell.
        let mut cmd = Command::new(&launcher);
        cmd.arg("~")
            .arg("--distribution")
            .arg(distribution)
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        no_window(&mut cmd);

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no stdout"))?;
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

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}
