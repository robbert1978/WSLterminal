//! GPU presentation via Direct3D11 + Direct2D + DirectComposition.
//!
//! We render into a **premultiplied-alpha composition swapchain** that DWM
//! composites directly against the desktop — so the terminal background stays
//! truly see-through (the same look the old `UpdateLayeredWindow` path gave)
//! while drawing moves onto the GPU. The window is created with
//! `WS_EX_NOREDIRECTIONBITMAP` (winit `with_no_redirection_bitmap`) so there is
//! no opaque GDI surface behind our content.
//!
//! The frame is composited in two layers: the CPU framebuffer (window chrome,
//! tab bar, sidebar, editor, cell backgrounds/cursor/selection) is blitted as a
//! D2D bitmap, then the terminal **glyphs are drawn natively with DirectWrite**
//! on top — which gives system font fallback and color emoji for free, on the
//! GPU, instead of the CPU rasterizer.

/// One terminal glyph to draw on the GPU: a char at a pixel origin in an opaque
/// color (`rgb`, 0x00RRGGBB).
#[derive(Clone, Copy)]
pub struct GlyphDraw {
    pub ch: char,
    pub x: f32,
    pub y: f32,
    pub rgb: u32,
}

#[cfg(windows)]
pub use imp::{available, Gpu};

#[cfg(not(windows))]
pub use stub::{available, Gpu};

#[cfg(windows)]
mod imp {
    use super::GlyphDraw;
    use std::collections::HashMap;
    use windows::core::{Interface, Result, PCWSTR};
    use windows::Win32::Foundation::{HMODULE, HWND};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F,
        D2D_SIZE_U,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
        ID2D1SolidColorBrush, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET,
        D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        D2D1_INTERPOLATION_MODE_LINEAR,
    };
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::DirectComposition::{
        DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, IDWriteFactory3, IDWriteFactory5,
        IDWriteFontCollection, IDWriteFontCollection1, IDWriteFontFile, IDWriteFontSet,
        IDWriteFontSetBuilder, IDWriteFontSetBuilder1, IDWriteLocalizedStrings, IDWriteTextFormat,
        IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
        DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_WORD_WRAPPING_NO_WRAP,
    };
    use std::path::Path;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
        DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
        DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn create_device() -> Result<ID3D11Device> {
        let mut last = windows::core::Error::from_win32();
        for dt in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            let mut dev: Option<ID3D11Device> = None;
            let r = unsafe {
                D3D11CreateDevice(
                    None,
                    dt,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    None,
                )
            };
            match r {
                Ok(()) => {
                    if let Some(d) = dev {
                        return Ok(d);
                    }
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// True if a D3D11 device can be created (so the GPU path is usable). Cheap
    /// probe done before window creation to decide the window flags.
    pub fn available() -> bool {
        create_device().is_ok()
    }

    pub struct Gpu {
        _device: ID3D11Device,
        swapchain: IDXGISwapChain1,
        dc: ID2D1DeviceContext,
        _dcomp: IDCompositionDevice,
        _target: IDCompositionTarget,
        _visual: IDCompositionVisual,
        dwrite: IDWriteFactory,
        text_format: Option<IDWriteTextFormat>,
        custom_collection: Option<IDWriteFontCollection>, // kept alive for text_format
        layouts: HashMap<char, IDWriteTextLayout>,
        brush: Option<ID2D1SolidColorBrush>,
        cell_w: f32,
        cell_h: f32,
        target_bitmap: Option<ID2D1Bitmap1>,
        src_bitmap: Option<ID2D1Bitmap1>, // persistent CPU->GPU upload bitmap (reused per frame)
        premul: Vec<u8>,
        w: u32,
        h: u32,
    }

    impl Gpu {
        pub fn new(hwnd: isize) -> Result<Gpu> {
            unsafe {
                let device = create_device()?;
                let dxgi_device: IDXGIDevice = device.cast()?;
                let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;

                let desc = DXGI_SWAP_CHAIN_DESC1 {
                    Width: 1,
                    Height: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                    ..Default::default()
                };
                let swapchain = factory.CreateSwapChainForComposition(&device, &desc, None)?;

                let d2d_factory: ID2D1Factory1 =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
                let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                // 1 DIP == 1 physical pixel (main.rs lays everything out in px).
                dc.SetDpi(96.0, 96.0);

                let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

                let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
                let target = dcomp.CreateTargetForHwnd(HWND(hwnd as *mut _), true)?;
                let visual = dcomp.CreateVisual()?;
                visual.SetContent(&swapchain)?;
                target.SetRoot(&visual)?;
                dcomp.Commit()?;

                Ok(Gpu {
                    _device: device,
                    swapchain,
                    dc,
                    _dcomp: dcomp,
                    _target: target,
                    _visual: visual,
                    dwrite,
                    text_format: None,
                    custom_collection: None,
                    layouts: HashMap::new(),
                    brush: None,
                    cell_w: 1.0,
                    cell_h: 1.0,
                    target_bitmap: None,
                    src_bitmap: None,
                    premul: Vec::new(),
                    w: 0,
                    h: 0,
                })
            }
        }

        /// Load a single font file into a private DirectWrite font collection and
        /// return (its family name, the collection). This makes the terminal use
        /// the *exact* file the CPU sidebar loaded, instead of relying on DWrite's
        /// system-collection name lookup (which can land on the wrong face/fallback
        /// for Nerd Fonts whose typographic family differs from the GDI name).
        fn load_font_file(&self, path: &Path) -> Option<(Vec<u16>, IDWriteFontCollection)> {
            unsafe {
                let path_w = to_wide(&path.to_string_lossy());
                let file: IDWriteFontFile =
                    self.dwrite.CreateFontFileReference(PCWSTR(path_w.as_ptr()), None).ok()?;
                let f3: IDWriteFactory3 = self.dwrite.cast().ok()?;
                let builder0: IDWriteFontSetBuilder = f3.CreateFontSetBuilder().ok()?;
                let builder: IDWriteFontSetBuilder1 = builder0.cast().ok()?;
                builder.AddFontFile(&file).ok()?;
                let set: IDWriteFontSet = builder.CreateFontSet().ok()?;
                let f5: IDWriteFactory5 = self.dwrite.cast().ok()?;
                let coll1: IDWriteFontCollection1 = f5.CreateFontCollectionFromFontSet(&set).ok()?;
                let family = coll1.GetFontFamily(0).ok()?;
                let names: IDWriteLocalizedStrings = family.GetFamilyNames().ok()?;
                let len = names.GetStringLength(0).ok()? as usize;
                let mut name = vec![0u16; len + 1];
                names.GetString(0, &mut name).ok()?;
                let base: IDWriteFontCollection = coll1.cast().ok()?;
                Some((name, base))
            }
        }

        /// (Re)build the DirectWrite text format for the given font + cell size,
        /// dropping the cached per-char layouts. Called on init and font change.
        /// `path` is the resolved font file (preferred); falls back to a system
        /// family-name lookup when absent.
        pub fn set_font(&mut self, family: &str, px: f32, cell_w: f32, cell_h: f32, path: Option<&Path>) {
            self.cell_w = cell_w.max(1.0);
            self.cell_h = cell_h.max(1.0);
            self.layouts.clear();
            self.custom_collection = None;

            // Prefer loading the exact file; fall back to the system family name.
            let (name_w, collection) = match path.and_then(|p| self.load_font_file(p)) {
                Some((n, c)) => {
                    let nm = String::from_utf16_lossy(&n[..n.len().saturating_sub(1)]);
                    eprintln!("[wslterm] GPU text font loaded from file -> family '{nm}'");
                    (n, Some(c))
                }
                None => {
                    eprintln!("[wslterm] GPU text font: system family '{family}' (no file)");
                    (to_wide(family), None)
                }
            };
            let locale = to_wide("en-us");
            let fmt = unsafe {
                self.dwrite.CreateTextFormat(
                    PCWSTR(name_w.as_ptr()),
                    collection.as_ref(),
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    px.max(1.0),
                    PCWSTR(locale.as_ptr()),
                )
            };
            match fmt {
                Ok(f) => {
                    unsafe {
                        let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                    }
                    self.text_format = Some(f);
                    self.custom_collection = collection;
                }
                Err(e) => eprintln!("[wslterm] DWrite CreateTextFormat failed: {e:?}"),
            }
            if self.brush.is_none() {
                let black = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
                self.brush = unsafe { self.dc.CreateSolidColorBrush(&black, None).ok() };
            }
        }

        fn layout_for(&mut self, ch: char) -> Option<IDWriteTextLayout> {
            if let Some(l) = self.layouts.get(&ch) {
                return Some(l.clone());
            }
            let fmt = self.text_format.as_ref()?;
            let mut buf = [0u16; 2];
            let s = ch.encode_utf16(&mut buf);
            let layout = unsafe {
                self.dwrite
                    .CreateTextLayout(s, fmt, self.cell_w * 2.0, self.cell_h)
                    .ok()?
            };
            self.layouts.insert(ch, layout.clone());
            Some(layout)
        }

        /// (Re)create the swapchain buffers + the D2D target bitmap for `w`×`h`.
        fn ensure_size(&mut self, w: u32, h: u32) -> Result<()> {
            if w == self.w && h == self.h && self.target_bitmap.is_some() {
                return Ok(());
            }
            unsafe {
                self.dc.SetTarget(None);
                self.target_bitmap = None;
                self.src_bitmap = None;
                self.swapchain.ResizeBuffers(
                    0,
                    w,
                    h,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
                let back: IDXGISurface = self.swapchain.GetBuffer(0)?;
                let props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    ..Default::default()
                };
                self.target_bitmap = Some(self.dc.CreateBitmapFromDxgiSurface(&back, Some(&props))?);

                // Persistent upload bitmap for the CPU framebuffer: created once per
                // size, refilled via CopyFromMemory each frame. Allocating a fresh
                // GPU texture per frame (the old CreateBitmap-in-present) scaled with
                // window area and caused the fullscreen lag.
                let sprops = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                };
                self.src_bitmap = Some(self.dc.CreateBitmap(
                    D2D_SIZE_U { width: w, height: h },
                    None,
                    0,
                    &sprops,
                )?);
            }
            self.w = w;
            self.h = h;
            Ok(())
        }

        /// Present a frame: blit the CPU framebuffer (chrome + cell backgrounds),
        /// then draw the terminal glyphs natively via DirectWrite (color emoji),
        /// then flip the composition swapchain.
        pub fn present(&mut self, fb: &[u32], w: u32, h: u32, glyphs: &[GlyphDraw]) -> Result<()> {
            if w == 0 || h == 0 {
                return Ok(());
            }
            self.ensure_size(w, h)?;
            let n = (w as usize) * (h as usize);
            if fb.len() < n {
                return Ok(());
            }
            self.premul.resize(n * 4, 0);
            for (i, &px) in fb.iter().take(n).enumerate() {
                let a = (px >> 24) & 0xff;
                let r = (((px >> 16) & 0xff) * a / 255) as u8;
                let g = (((px >> 8) & 0xff) * a / 255) as u8;
                let b = ((px & 0xff) * a / 255) as u8;
                let o = i * 4;
                self.premul[o] = b;
                self.premul[o + 1] = g;
                self.premul[o + 2] = r;
                self.premul[o + 3] = a as u8;
            }
            let target: ID2D1Image = match &self.target_bitmap {
                Some(b) => b.cast()?,
                None => return Ok(()),
            };
            unsafe {
                self.dc.SetTarget(&target);
                self.dc.BeginDraw();
                self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));

                // Layer 1: the CPU framebuffer (chrome, backgrounds, cursor, sel).
                // Refill the persistent upload bitmap (no per-frame GPU alloc) and blit.
                if let Some(src) = &self.src_bitmap {
                    src.CopyFromMemory(None, self.premul.as_ptr() as *const core::ffi::c_void, w * 4)?;
                    let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 };
                    self.dc.DrawBitmap(
                        src,
                        Some(&rect),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                }

                // Layer 2: terminal glyphs (system fallback + color emoji).
                if let Some(brush) = self.brush.clone() {
                    for gph in glyphs {
                        let layout = match self.layout_for(gph.ch) {
                            Some(l) => l,
                            None => break, // no text format yet
                        };
                        let col = D2D1_COLOR_F {
                            r: ((gph.rgb >> 16) & 0xff) as f32 / 255.0,
                            g: ((gph.rgb >> 8) & 0xff) as f32 / 255.0,
                            b: (gph.rgb & 0xff) as f32 / 255.0,
                            a: 1.0,
                        };
                        brush.SetColor(&col);
                        self.dc.DrawTextLayout(
                            D2D_POINT_2F { x: gph.x, y: gph.y },
                            &layout,
                            &brush,
                            D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                        );
                    }
                }

                self.dc.EndDraw(None, None)?;
                self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
            }
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod stub {
    use super::GlyphDraw;
    pub fn available() -> bool {
        false
    }
    pub struct Gpu;
    impl Gpu {
        pub fn new(_hwnd: isize) -> Result<Gpu, ()> {
            Err(())
        }
        pub fn set_font(
            &mut self,
            _family: &str,
            _px: f32,
            _cell_w: f32,
            _cell_h: f32,
            _path: Option<&std::path::Path>,
        ) {
        }
        pub fn present(&mut self, _fb: &[u32], _w: u32, _h: u32, _glyphs: &[GlyphDraw]) {}
    }
}
