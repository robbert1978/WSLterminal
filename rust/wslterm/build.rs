//! Embed the WSL (Tux) icon into the exe so Explorer/taskbar show it. The same
//! `wsl.ico` is also loaded at runtime for the window icon (see main.rs).

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("wsl.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon embed failed: {e}");
        }
    }
}
// 2026-06-01T20:37:35.8455516+07:00
