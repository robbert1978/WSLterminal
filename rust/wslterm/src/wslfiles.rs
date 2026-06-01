//! Bridge WSL (Linux) paths to Windows via the `\\wsl.localhost\<distro>\…`
//! share, so the sidebar/editor can list, read and write files without spawning
//! a process. Ports `src/WslTerminal/Ui/WslFiles.cs`.

use std::path::PathBuf;

/// Map a Linux path to its Windows UNC form under the distro share.
pub fn to_unc(distro: &str, linux_path: &str) -> PathBuf {
    let rel = linux_path.replace('/', "\\");
    let rel = rel.trim_start_matches('\\');
    PathBuf::from(format!(r"\\wsl.localhost\{distro}\{rel}"))
}

/// One directory entry.
pub struct Entry {
    pub name: String,
    pub linux_path: String,
    pub is_dir: bool,
}

/// List a Linux directory's immediate children (dirs first, then files, each
/// case-insensitively sorted). Empty on any I/O error. `show_hidden` includes
/// dotfiles.
pub fn list(distro: &str, linux_dir: &str, show_hidden: bool) -> Vec<Entry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let base = linux_dir.trim_end_matches('/');
    let rd = match std::fs::read_dir(to_unc(distro, linux_dir)) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let linux_path = format!("{base}/{name}");
        let e = Entry { name, linux_path, is_dir };
        if is_dir {
            dirs.push(e);
        } else {
            files.push(e);
        }
    }
    let key = |e: &Entry| e.name.to_lowercase();
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.append(&mut files);
    dirs
}

/// Read a file's bytes, capped at `max` (default 2 MiB). `None` on error.
pub fn read_bytes(distro: &str, linux_path: &str, max: usize) -> Option<Vec<u8>> {
    let bytes = std::fs::read(to_unc(distro, linux_path)).ok()?;
    if bytes.len() > max {
        Some(bytes[..max].to_vec())
    } else {
        Some(bytes)
    }
}

/// Overwrite a WSL file with UTF-8 text (in place; keeps inode/permissions).
pub fn write_text(distro: &str, linux_path: &str, text: &str) -> bool {
    std::fs::write(to_unc(distro, linux_path), text.as_bytes()).is_ok()
}

/// True if the first chunk looks binary (contains a NUL).
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}
