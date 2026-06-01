//! Per-pixel translucent presentation via `UpdateLayeredWindow`. The window is
//! borderless + WS_EX_LAYERED; we render into a 32-bit top-down DIB and blit it
//! with per-pixel alpha, so the terminal background can be translucent while the
//! tab bar / sidebar / text stay fully opaque. The app renders into an ARGB
//! framebuffer (alpha in the high byte); `present` premultiplies into the DIB.

#[cfg(windows)]
pub use imp::{work_area, Layered};

#[cfg(not(windows))]
pub use stub::{work_area, Layered};

#[cfg(windows)]
mod imp {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, UpdateLayeredWindow, GWL_EXSTYLE,
        ULW_ALPHA, WS_EX_LAYERED,
    };

    /// The work area (excluding the taskbar) of the monitor the window is on,
    /// as (x, y, width, height) in physical pixels.
    pub fn work_area(hwnd: isize) -> Option<(i32, i32, u32, u32)> {
        unsafe {
            let mon = MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(mon, &mut mi) == 0 {
                return None;
            }
            let r = mi.rcWork;
            Some((r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32))
        }
    }

    pub struct Layered {
        hwnd: HWND,
        screen_dc: HDC,
        mem_dc: HDC,
        dib: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u32,
        w: i32,
        h: i32,
    }

    impl Layered {
        /// `hwnd` is the raw window handle (isize). Adds WS_EX_LAYERED.
        pub fn new(hwnd: isize) -> Layered {
            let hwnd = hwnd as HWND;
            unsafe {
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
                let screen_dc = GetDC(0 as HWND);
                let mem_dc = CreateCompatibleDC(screen_dc);
                Layered {
                    hwnd,
                    screen_dc,
                    mem_dc,
                    dib: 0,
                    old: 0,
                    bits: null_mut(),
                    w: 0,
                    h: 0,
                }
            }
        }

        fn ensure(&mut self, w: i32, h: i32) {
            if w == self.w && h == self.h && self.dib != 0 {
                return;
            }
            unsafe {
                if self.dib != 0 {
                    SelectObject(self.mem_dc, self.old);
                    DeleteObject(self.dib as HGDIOBJ);
                }
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB as u32,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                };
                let mut bits: *mut core::ffi::c_void = null_mut();
                let dib = CreateDIBSection(self.mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, 0, 0);
                self.old = SelectObject(self.mem_dc, dib as HGDIOBJ);
                self.dib = dib;
                self.bits = bits as *mut u32;
                self.w = w;
                self.h = h;
            }
        }

        /// Premultiply `fb` (ARGB, alpha in high byte) into the DIB and blit it
        /// to the window with per-pixel alpha. `fb.len()` must be >= w*h.
        pub fn present(&mut self, fb: &[u32], w: u32, h: u32) {
            if w == 0 || h == 0 {
                return;
            }
            self.ensure(w as i32, h as i32);
            if self.bits.is_null() {
                return;
            }
            let n = (w as usize) * (h as usize);
            unsafe {
                let dst = std::slice::from_raw_parts_mut(self.bits, n);
                for (d, &px) in dst.iter_mut().zip(fb.iter()).take(n) {
                    let a = (px >> 24) & 0xff;
                    let r = (((px >> 16) & 0xff) * a / 255) & 0xff;
                    let g = (((px >> 8) & 0xff) * a / 255) & 0xff;
                    let b = ((px & 0xff) * a / 255) & 0xff;
                    *d = (a << 24) | (r << 16) | (g << 8) | b;
                }
                let size = SIZE { cx: w as i32, cy: h as i32 };
                let src = POINT { x: 0, y: 0 };
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                // Position the layered content at the window's actual screen
                // top-left. Passing null ("keep position") leaves stale content
                // after the window moves (e.g. maximize repositions to -8,top),
                // exposing the window's white default background.
                let mut wr: RECT = std::mem::zeroed();
                let dst = if GetWindowRect(self.hwnd, &mut wr) != 0 {
                    POINT { x: wr.left, y: wr.top }
                } else {
                    POINT { x: 0, y: 0 }
                };
                UpdateLayeredWindow(
                    self.hwnd,
                    self.screen_dc,
                    &dst,
                    &size,
                    self.mem_dc,
                    &src,
                    0,
                    &blend,
                    ULW_ALPHA,
                );
            }
        }
    }

    impl Drop for Layered {
        fn drop(&mut self) {
            unsafe {
                if self.dib != 0 {
                    SelectObject(self.mem_dc, self.old);
                    DeleteObject(self.dib as HGDIOBJ);
                }
                DeleteDC(self.mem_dc);
                ReleaseDC(0 as HWND, self.screen_dc);
            }
        }
    }
}

#[cfg(not(windows))]
mod stub {
    pub struct Layered;
    impl Layered {
        pub fn new(_hwnd: isize) -> Layered {
            Layered
        }
        pub fn present(&mut self, _fb: &[u32], _w: u32, _h: u32) {}
    }
    pub fn work_area(_hwnd: isize) -> Option<(i32, i32, u32, u32)> {
        None
    }
}
