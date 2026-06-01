//! Locate the Linux helpers and build the staging shell commands.
//! Ports `src/WslTerminal/WslBootstrap.cs`. Pure + unit-tested.

use std::path::{Path, PathBuf};

/// Translate a Windows path to its `/mnt/<drive>/...` WSL form.
pub fn windows_to_wsl_path(win_path: &str) -> String {
    // Mirror Path.GetFullPath loosely: we only need the drive-letter rewrite.
    let full = win_path;
    let bytes = full.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let mut rest: String = full[2..].replace('\\', "/");
        if !rest.starts_with('/') {
            rest.insert(0, '/');
        }
        return format!("/mnt/{drive}{rest}");
    }
    full.replace('\\', "/")
}

/// Find `artifacts/<name>` by walking up from the executable's directory (up to
/// 8 levels), honoring an env override. Ports `ResolveArtifact`.
pub fn resolve_artifact(name: &str, env_var: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        let pb = PathBuf::from(&p);
        if !p.trim().is_empty() && pb.is_file() {
            return Some(pb);
        }
    }
    let mut dir: Option<PathBuf> = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf()));
    for _ in 0..8 {
        let d = match &dir {
            Some(d) => d.clone(),
            None => break,
        };
        let candidate = d.join("artifacts").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    None
}

pub fn resolve_server() -> Option<PathBuf> {
    resolve_artifact("wslptyd", "WSLPTYD_BIN")
}
pub fn resolve_helper() -> Option<PathBuf> {
    resolve_artifact("wslpty", "WSLPTY_BIN")
}

/// Shell snippet that stages the multiplexed server into a fixed `/tmp/wslptyd`
/// and execs it. `rm -f` before `cp` avoids ETXTBSY from a stale daemon. Ports
/// `BuildServerCommand`.
pub fn build_server_command(server_win_path: &Path) -> String {
    let src = windows_to_wsl_path(&server_win_path.to_string_lossy());
    format!("d=/tmp/wslptyd; rm -f \"$d\" 2>/dev/null; cp '{src}' \"$d\" 2>/dev/null; chmod +x \"$d\"; exec \"$d\"")
}

/// Single-session helper launch command (used by diagnostics). Ports
/// `BuildLaunchCommand`.
pub fn build_launch_command(helper_win_path: &Path, start_dir: Option<&str>) -> String {
    let src = windows_to_wsl_path(&helper_win_path.to_string_lossy());
    let mut s = format!("d=/tmp/wslpty.$$; cp '{src}' \"$d\" 2>/dev/null; chmod +x \"$d\"; ");
    if let Some(dir) = start_dir {
        if !dir.is_empty() {
            let escaped = dir.replace('\'', "'\\''");
            s.push_str(&format!("cd -- '{escaped}' 2>/dev/null; "));
        }
    }
    s.push_str("exec \"$d\"");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn win_to_wsl_drive() {
        assert_eq!(windows_to_wsl_path(r"C:\Users\rob\x"), "/mnt/c/Users/rob/x");
        assert_eq!(windows_to_wsl_path(r"D:\a\b"), "/mnt/d/a/b");
    }

    #[test]
    fn server_command_fixed_name_and_rmf() {
        let cmd = build_server_command(&PathBuf::from(r"C:\proj\artifacts\wslptyd"));
        assert!(cmd.contains("d=/tmp/wslptyd;"));
        assert!(cmd.contains("rm -f \"$d\""));
        assert!(cmd.contains("/mnt/c/proj/artifacts/wslptyd"));
        assert!(cmd.trim_end().ends_with("exec \"$d\""));
        assert!(!cmd.contains("wslptyd.$$")); // singleton: no PID suffix
    }

    #[test]
    fn launch_command_with_cwd_is_escaped() {
        let cmd = build_launch_command(&PathBuf::from(r"C:\p\artifacts\wslpty"), Some("/home/o'brien"));
        assert!(cmd.contains("d=/tmp/wslpty.$$;")); // helper keeps PID suffix
        assert!(cmd.contains(r"cd -- '/home/o'\''brien'"));
        assert!(cmd.contains("exec \"$d\""));
    }
}
