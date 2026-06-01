//! Embed the WSL (Tux) icon into the exe so Explorer/taskbar show it. The same
//! `wsl.ico` is also loaded at runtime for the window icon (see main.rs).

fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("wsl.ico");
        let _ = res.compile();
    }
}
