//! GPU presentation via Direct3D11 + Direct2D + DirectComposition.
//!
//! We render into a **premultiplied-alpha composition swapchain** that DWM
//! composites directly against the desktop — so the terminal background stays
//! truly see-through (the same look the old `UpdateLayeredWindow` path gave)
//! while drawing moves onto the GPU. The window is created with
//! `WS_EX_NOREDIRECTIONBITMAP` (winit `with_no_redirection_bitmap`) so there is
//! no opaque GDI surface behind our content.
//!
//! Phase 1 (this revision) blits the CPU framebuffer through a D2D bitmap to
//! prove the pipeline + transparency end-to-end. Phase 2 will draw text natively
//! via DirectWrite (giving color emoji) instead of uploading a CPU buffer.

#[cfg(windows)]
pub use imp::{available, Gpu};

#[cfg(not(windows))]
pub use stub::{available, Gpu};

#[cfg(windows)]
mod imp {
    use windows::core::{Interface, Result};
    use windows::Win32::Foundation::{HMODULE, HWND};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1Image,
        D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
        D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
        D2D1_INTERPOLATION_MODE_LINEAR,
    };
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::DirectComposition::{
        DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
        DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
        DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    fn create_device() -> Result<ID3D11Device> {
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
            if r.is_ok() {
                if let Some(d) = dev {
                    return Ok(d);
                }
            }
        }
        // Final attempt surfaces the real error.
        let mut dev: Option<ID3D11Device> = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                None,
            )?;
        }
        dev.ok_or_else(|| windows::core::Error::from_win32())
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
        target_bitmap: Option<ID2D1Bitmap1>,
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

                // Direct2D device context targeting the swapchain's back buffer.
                let d2d_factory: ID2D1Factory1 =
                    D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let d2d_device = d2d_factory.CreateDevice(&dxgi_device)?;
                let dc = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
                // 1 DIP == 1 physical pixel (main.rs lays everything out in px).
                dc.SetDpi(96.0, 96.0);

                // DirectComposition: bind the swapchain to the HWND.
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
                    target_bitmap: None,
                    premul: Vec::new(),
                    w: 0,
                    h: 0,
                })
            }
        }

        /// (Re)create the swapchain buffers + the D2D target bitmap for `w`×`h`.
        fn ensure_size(&mut self, w: u32, h: u32) -> Result<()> {
            if w == self.w && h == self.h && self.target_bitmap.is_some() {
                return Ok(());
            }
            unsafe {
                // Release the old target before resizing the swapchain buffers.
                self.dc.SetTarget(None);
                self.target_bitmap = None;
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
            }
            self.w = w;
            self.h = h;
            Ok(())
        }

        /// Present the ARGB framebuffer (alpha in the high byte). Premultiplies
        /// into BGRA (the swapchain is premultiplied-alpha) and blits via D2D.
        pub fn present(&mut self, fb: &[u32], w: u32, h: u32) -> Result<()> {
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
                let size = D2D_SIZE_U { width: w, height: h };
                let props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    ..Default::default()
                };
                let src = self.dc.CreateBitmap(
                    size,
                    Some(self.premul.as_ptr() as *const core::ffi::c_void),
                    w * 4,
                    &props,
                )?;
                let rect = D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 };
                self.dc.DrawBitmap(
                    &src,
                    Some(&rect),
                    1.0,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None,
                    None,
                );
                self.dc.EndDraw(None, None)?;
                self.swapchain.Present(1, DXGI_PRESENT(0)).ok()?;
            }
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod stub {
    pub fn available() -> bool {
        false
    }
    pub struct Gpu;
    impl Gpu {
        pub fn new(_hwnd: isize) -> Result<Gpu, ()> {
            Err(())
        }
        pub fn present(&mut self, _fb: &[u32], _w: u32, _h: u32) {}
    }
}
