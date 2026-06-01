//! WSL Terminal GUI — Rust rewrite.
//!
//! A window that renders the `wslterm-core` terminal grid and pumps the user's
//! keystrokes (via `wslterm-core::input::encode`) into a live WSL session driven
//! by `wslterm-pty`. CPU-rendered (winit + softbuffer + ab_glyph) on purpose: it
//! keeps RAM low and avoids the wgpu/Direct3D managed stack — the whole point of
//! the rewrite.
//!
//! Threading (mirrors the C# app): a background thread owns the mux receiver and
//! feeds bytes into a shared `Arc<Mutex<Terminal>>`, then wakes the UI to render
//! (coalesced to one pending wake). The mux uses a *bounded* channel, so under a
//! flood (e.g. termbench) the reader blocks, back-pressuring wslptyd instead of
//! buffering the whole burst — memory stays bounded. Rendering is decoupled from
//! parsing, so a slow repaint never inflates the input queue.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use wslterm_core::input::{self, Key, Mods};
use wslterm_core::{color, Cell, CellFlags, Terminal};
use wslterm_pty::bootstrap;
use wslterm_pty::mux::MuxEvent;
use wslterm_pty::{WslMux, WslProcess};

const DISTRO: &str = "Ubuntu";
/// Logical font size; multiplied by the monitor's DPI scale to get device px.
const BASE_FONT_PX: f32 = 18.0;
const DEFAULT_FG: u32 = 0xCC_CCCC; // Campbell foreground
const DEFAULT_BG: u32 = 0x0C_0C0C; // Campbell background
const CURSOR_RGB: u32 = 0xCC_CCCC;
const SELECTION_BG: u32 = 0x26_4F78; // VS Code-ish selection blue

/// Lightweight events from the feed thread to the UI (no payload — the data is
/// already in the shared Terminal; this just wakes the loop to render/exit).
enum UserEvent {
    Redraw,
    Closed,
}

/// A glyph rasterized once and cached: 8-bit coverage plus its pixel offset from
/// the cell's top-left. Blitting these is just memory work — no re-outlining per
/// frame, which is what made the renderer slow under floods.
struct Glyph {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

/// Rasterize one char at the given pixel size; `None` if it has no outline (e.g.
/// space). Coords are relative to the cell origin (pen at baseline y = ascent).
fn rasterize_glyph(font: &FontVec, px: f32, ch: char) -> Option<Glyph> {
    let scale = PxScale::from(px);
    let ascent = font.as_scaled(scale).ascent();
    let g = font
        .glyph_id(ch)
        .with_scale_and_position(scale, ab_glyph::point(0.0, ascent));
    let outline = font.outline_glyph(g)?;
    let b = outline.px_bounds();
    let w = b.width().ceil() as usize;
    let h = b.height().ceil() as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let mut cov = vec![0u8; w * h];
    outline.draw(|gx, gy, c| {
        let (xi, yi) = (gx as usize, gy as usize);
        if xi < w && yi < h {
            cov[yi * w + xi] = (c * 255.0) as u8;
        }
    });
    Some(Glyph { left: b.min.x.floor() as i32, top: b.min.y.floor() as i32, w, h, cov })
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    win: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,

    font: FontVec,
    scale: f32,    // monitor DPI scale (1.0, 1.5, 2.0, ...)
    font_px: f32,  // BASE_FONT_PX * scale, in device pixels
    cell_w: usize,
    cell_h: usize,
    ascent: f32,

    term: Arc<Mutex<Terminal>>,
    mux: Option<WslMux>,
    session: u32,
    /// Bytes the emulator owes the PTY (DSR/DA), filled by the feed thread,
    /// flushed by the UI thread (keeps `mux` owned solely by the UI).
    outbox: Arc<Mutex<Vec<u8>>>,
    redraw_pending: Arc<AtomicBool>,

    mods: ModifiersState,
    scroll_off: usize, // rows scrolled up into scrollback (0 = live bottom)
    grid: Vec<Vec<Cell>>, // reused viewport snapshot
    glyph_cache: HashMap<char, Option<Glyph>>, // rasterized once per char per size

    cursor_px: (f64, f64), // last mouse position (device px)
    selecting: bool,       // left button held, dragging a selection
    sel_anchor: Option<(i64, i64)>, // (abs_row, col) where the drag began
    sel: Option<(i64, i64, i64, i64)>, // normalized (r1,c1,r2,c2), abs rows, inclusive

    opacity: f32, // 0.4..=1.0 window opacity (1.0 = fully opaque)
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> App {
        let font = load_monospace_font().expect("no monospace font found (Consolas/Cascadia)");
        let mut app = App {
            proxy,
            win: None,
            context: None,
            surface: None,
            font,
            scale: 1.0,
            font_px: BASE_FONT_PX,
            cell_w: 1,
            cell_h: 1,
            ascent: 0.0,
            term: Arc::new(Mutex::new(Terminal::new(80, 24))),
            mux: None,
            session: 0,
            outbox: Arc::new(Mutex::new(Vec::new())),
            redraw_pending: Arc::new(AtomicBool::new(false)),
            mods: ModifiersState::empty(),
            cursor_px: (0.0, 0.0),
            selecting: false,
            sel_anchor: None,
            sel: None,
            opacity: parse_opacity_env(),
            scroll_off: 0,
            grid: Vec::new(),
            glyph_cache: HashMap::new(),
        };
        app.recompute_metrics();
        app
    }

    /// Recompute font/cell metrics for the current DPI scale.
    fn recompute_metrics(&mut self) {
        self.font_px = BASE_FONT_PX * self.scale;
        let sf = self.font.as_scaled(PxScale::from(self.font_px));
        self.ascent = sf.ascent();
        self.cell_h = (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(1.0) as usize;
        self.cell_w = sf.h_advance(self.font.glyph_id('M')).ceil().max(1.0) as usize;
        self.glyph_cache.clear(); // cached glyphs are size-specific
    }

    fn grid_dims(&self, w: u32, h: u32) -> (usize, usize) {
        let cols = (w as usize / self.cell_w).max(1);
        let rows = (h as usize / self.cell_h).max(1);
        (cols, rows)
    }

    /// Send bytes to the live session (no-op before the mux is up).
    fn send(&self, bytes: &[u8]) {
        if let Some(mux) = &self.mux {
            if !bytes.is_empty() {
                mux.send_data(self.session, bytes);
            }
        }
    }

    fn handle_key(&mut self, ev: &KeyEvent) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();

        // Clipboard shortcuts (don't reach the shell): Ctrl+Shift+C/V, Shift+Ins.
        if let PhysicalKey::Code(code) = ev.physical_key {
            match code {
                KeyCode::KeyC if ctrl && shift => {
                    self.copy_selection();
                    return;
                }
                KeyCode::KeyV if ctrl && shift => {
                    self.paste();
                    return;
                }
                KeyCode::Insert if shift => {
                    self.paste();
                    return;
                }
                // Ctrl +/- adjusts window opacity.
                KeyCode::Equal | KeyCode::NumpadAdd if ctrl => {
                    self.adjust_opacity(0.05);
                    return;
                }
                KeyCode::Minus | KeyCode::NumpadSubtract if ctrl => {
                    self.adjust_opacity(-0.05);
                    return;
                }
                _ => {}
            }
        }

        // Typing snaps the view back to the live bottom.
        if self.scroll_off != 0 {
            self.scroll_off = 0;
            if let Some(win) = &self.win {
                win.request_redraw();
            }
        }
        let mods = Mods {
            ctrl: self.mods.control_key(),
            alt: self.mods.alt_key(),
            shift: self.mods.shift_key(),
        };
        let app_cursor = self.term.lock().unwrap().app_cursor_keys();

        // Function keys are most reliable off the physical key code.
        if let PhysicalKey::Code(code) = ev.physical_key {
            if let Some(n) = function_number(code) {
                if let Some(b) = input::encode(Key::F(n), mods, app_cursor) {
                    self.send(&b);
                    return;
                }
            }
        }

        let key = map_key(&ev.logical_key);
        let encoded = key.and_then(|k| input::encode(k, mods, app_cursor));
        if let Some(bytes) = encoded {
            self.send(&bytes);
            return;
        }
        // Plain printable input: send the layout-resolved text directly.
        if let Some(text) = &ev.text {
            self.send(text.as_bytes());
        }
    }

    /// Map the last mouse position to an (absolute row, column) cell. Absolute
    /// rows count from the oldest scrollback line, matching `Terminal::get_text`.
    fn cell_at_cursor(&self) -> (i64, i64) {
        let col = (self.cursor_px.0 / self.cell_w as f64).floor() as i64;
        let vrow = (self.cursor_px.1 / self.cell_h as f64).floor() as i64;
        let t = self.term.lock().unwrap();
        let cols = t.cols() as i64;
        let top_abs = t.scrollback_count() as i64 - self.scroll_off as i64;
        let total = t.scrollback_count() as i64 + t.rows() as i64;
        let abs = (top_abs + vrow).clamp(0, (total - 1).max(0));
        (abs, col.clamp(0, (cols - 1).max(0)))
    }

    fn begin_selection(&mut self) {
        let (r, c) = self.cell_at_cursor();
        self.sel_anchor = Some((r, c));
        self.sel = Some((r, c, r, c));
        self.selecting = true;
        self.request_redraw();
    }

    fn update_selection(&mut self) {
        if let Some((ar, ac)) = self.sel_anchor {
            let (r, c) = self.cell_at_cursor();
            // Order anchor and current point in reading order.
            self.sel = Some(if (r, c) < (ar, ac) {
                (r, c, ar, ac)
            } else {
                (ar, ac, r, c)
            });
            self.request_redraw();
        }
    }

    fn copy_selection(&self) {
        if let Some((r1, c1, r2, c2)) = self.sel {
            let text = self.term.lock().unwrap().get_text(r1, c1, r2, c2);
            if !text.is_empty() {
                let _ = clipboard_win::set_clipboard_string(&text);
            }
        }
    }

    fn paste(&self) {
        let s = match clipboard_win::get_clipboard_string() {
            Ok(s) => s,
            Err(_) => return,
        };
        let s = s.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.term.lock().unwrap().bracketed_paste();
        let mut out = Vec::new();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(s.as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        self.send(&out);
    }

    fn request_redraw(&self) {
        if let Some(win) = &self.win {
            win.request_redraw();
        }
    }

    fn adjust_opacity(&mut self, delta: f32) {
        self.opacity = (self.opacity + delta).clamp(0.4, 1.0);
        if let Some(win) = &self.win {
            set_window_opacity(win, self.opacity);
        }
    }

    /// Scroll the view by `lines` into scrollback (+ = up into history).
    fn scroll_by(&mut self, lines: i32) {
        let max = self.term.lock().unwrap().scrollback_count() as i32;
        let next = (self.scroll_off as i32 + lines).clamp(0, max) as usize;
        if next != self.scroll_off {
            self.scroll_off = next;
            if let Some(win) = &self.win {
                win.request_redraw();
            }
        }
    }

    /// Re-derive grid size from the window's current physical size.
    fn reflow(&mut self) {
        if let Some(win) = &self.win {
            let s = win.inner_size();
            self.resize_surface(s.width, s.height);
        }
    }

    fn resize_surface(&mut self, w: u32, h: u32) {
        let (cols, rows) = self.grid_dims(w, h);
        self.term.lock().unwrap().resize(cols, rows);
        if let Some(mux) = &self.mux {
            mux.send_resize(self.session, cols as u16, rows as u16);
        }
        if let (Some(surface), Some(nw), Some(nh)) =
            (&mut self.surface, NonZeroU32::new(w), NonZeroU32::new(h))
        {
            let _ = surface.resize(nw, nh);
        }
    }

    fn render(&mut self) {
        let (win, surface) = match (&self.win, &mut self.surface) {
            (Some(w), Some(s)) => (w, s),
            _ => return,
        };
        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));

        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        buffer.fill(DEFAULT_BG);

        // Snapshot the grid under the lock, then rasterize without holding it.
        let (cols, rows, cx, cy, cursor_on, top_abs);
        {
            let t = self.term.lock().unwrap();
            let off = self.scroll_off.min(t.scrollback_count());
            t.capture_viewport(off, &mut self.grid);
            cols = t.cols();
            rows = t.rows();
            cx = t.cx();
            cy = t.cy();
            // Cursor only shows in the live view (not while scrolled into history).
            cursor_on = t.cursor_visible() && off == 0;
            // Absolute row of the top visible line, for selection hit-testing.
            top_abs = t.scrollback_count() as i64 - off as i64;
        }
        let sel = self.sel;

        // Pass 1: make sure every visible glyph is rasterized into the cache.
        // (Direct field access keeps `font`/`glyph_cache` as disjoint borrows.)
        for r in 0..rows.min(self.grid.len()) {
            for c in 0..cols.min(self.grid[r].len()) {
                let rune = self.grid[r][c].rune;
                if rune < 0x20 {
                    continue;
                }
                let ch = char::from_u32(rune).unwrap_or(' ');
                if ch != ' ' && !self.glyph_cache.contains_key(&ch) {
                    let g = rasterize_glyph(&self.font, self.font_px, ch);
                    self.glyph_cache.insert(ch, g);
                }
            }
        }

        // Pass 2: fill backgrounds and blit cached glyph coverage.
        let (cw, ch_px) = (self.cell_w, self.cell_h);
        for r in 0..rows.min(self.grid.len()) {
            let row = &self.grid[r];
            for c in 0..cols.min(row.len()) {
                let cell = &row[c];
                if cell.width == 0 {
                    continue; // trailing slot of a wide glyph
                }
                let is_cursor = cursor_on && r == cy && c == cx;
                let reverse = cell.flags.contains(CellFlags::REVERSE) ^ is_cursor;
                let bold = cell.flags.contains(CellFlags::BOLD);

                let mut fg = color::resolve(cell.fg, DEFAULT_FG, bold);
                let mut bg = color::resolve(cell.bg, DEFAULT_BG, false);
                if reverse {
                    if is_cursor {
                        bg = CURSOR_RGB;
                        fg = DEFAULT_BG;
                    } else {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                }
                if let Some((r1, c1, r2, c2)) = sel {
                    let (ar, ac) = (top_abs + r as i64, c as i64);
                    let after_start = ar > r1 || (ar == r1 && ac >= c1);
                    let before_end = ar < r2 || (ar == r2 && ac <= c2);
                    if after_start && before_end {
                        bg = SELECTION_BG;
                    }
                }

                let x0 = c * cw;
                let y0 = r * ch_px;
                fill_rect(&mut buffer, w, h, x0, y0, cw, ch_px, bg);

                if cell.rune < 0x20 {
                    continue;
                }
                let glyph_ch = char::from_u32(cell.rune).unwrap_or(' ');
                if let Some(Some(g)) = self.glyph_cache.get(&glyph_ch) {
                    for gy in 0..g.h {
                        let py = y0 as i32 + g.top + gy as i32;
                        if py < 0 || py as u32 >= h {
                            continue;
                        }
                        let base = py as usize * w as usize;
                        let row_cov = &g.cov[gy * g.w..gy * g.w + g.w];
                        for gx in 0..g.w {
                            let cov = row_cov[gx];
                            if cov == 0 {
                                continue;
                            }
                            let px = x0 as i32 + g.left + gx as i32;
                            if px < 0 || px as u32 >= w {
                                continue;
                            }
                            let idx = base + px as usize;
                            buffer[idx] = blend(buffer[idx], fg, cov as f32 / 255.0);
                        }
                    }
                }
            }
        }

        let _ = buffer.present();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.win.is_some() {
            return; // already initialized
        }
        let attrs = Window::default_attributes()
            .with_title("WSL Terminal (Rust)")
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        let win = Rc::new(event_loop.create_window(attrs).expect("create window"));
        self.scale = win.scale_factor() as f32;
        self.recompute_metrics();
        set_window_opacity(&win, self.opacity);

        let context = Context::new(win.clone()).expect("softbuffer context");
        let mut surface = Surface::new(&context, win.clone()).expect("softbuffer surface");
        let size = win.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            let _ = surface.resize(w, h);
        }

        let (cols, rows) = self.grid_dims(size.width, size.height);
        self.term = Arc::new(Mutex::new(Terminal::new(cols, rows)));

        // Bring up the live WSL session.
        let server = bootstrap::resolve_server()
            .expect("wslptyd not found (build native/ and place under artifacts/)");
        let command = bootstrap::build_server_command(&server);
        let proc = WslProcess::launch(DISTRO, &command).expect("launch wslg.exe");
        let (mux, rx) = WslMux::start(proc);
        self.session = mux.open(cols as u16, rows as u16, "");
        let session = self.session;
        self.mux = Some(mux);
        eprintln!("[wslterm] window up {cols}x{rows}, session {session} opened on {DISTRO}");

        // Feed thread: parse bytes into the shared Terminal off the UI thread and
        // wake the loop to render. Bounded mux channel back-pressures the reader.
        let term = self.term.clone();
        let outbox = self.outbox.clone();
        let pending = self.redraw_pending.clone();
        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("wsl-feed".into())
            .spawn(move || {
                let mut acc: u64 = 0;
                let mut mark = Instant::now();
                let mut resp_batch: Vec<u8> = Vec::new();
                // Block for the next frame, then drain ALL queued frames under one
                // lock before waking the UI. Batching amortizes lock + wake cost
                // (the per-frame churn that was capping feed throughput) and lets
                // the parser run near full speed under a flood.
                while let Ok(first) = rx.recv() {
                    let mut ev = first;
                    let mut t = term.lock().unwrap();
                    loop {
                        match ev {
                            MuxEvent::Data { id, bytes } if id == session => {
                                acc += bytes.len() as u64;
                                t.feed(&bytes);
                                if !t.respond.is_empty() {
                                    resp_batch.append(&mut t.respond);
                                }
                            }
                            MuxEvent::Exit { id, .. } if id == session => {
                                drop(t);
                                let _ = proxy.send_event(UserEvent::Closed);
                                return;
                            }
                            _ => {}
                        }
                        match rx.try_recv() {
                            Ok(next) => ev = next,
                            Err(_) => break, // nothing more queued right now
                        }
                    }
                    drop(t);

                    if !resp_batch.is_empty() {
                        outbox.lock().unwrap().append(&mut resp_batch);
                    }
                    let dt = mark.elapsed().as_secs_f64();
                    if dt >= 1.0 {
                        if acc > 0 {
                            eprintln!("[feed] {:.1} MB/s", acc as f64 / 1e6 / dt);
                        }
                        acc = 0;
                        mark = Instant::now();
                    }
                    // Coalesce: only wake if no redraw is already queued.
                    if !pending.swap(true, Ordering::AcqRel)
                        && proxy.send_event(UserEvent::Redraw).is_err()
                    {
                        return;
                    }
                }
                let _ = proxy.send_event(UserEvent::Closed);
            })
            .expect("spawn feed thread");

        self.win = Some(win);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw => {
                self.redraw_pending.store(false, Ordering::Release);
                self.scroll_off = 0; // new output snaps to the live bottom
                if !self.selecting {
                    self.sel = None; // absolute coords shift as content scrolls
                }
                // Flush any DSR/DA responses the feed thread produced.
                let out = std::mem::take(&mut *self.outbox.lock().unwrap());
                self.send(&out);
                if let Some(win) = &self.win {
                    win.request_redraw();
                }
            }
            UserEvent::Closed => {
                eprintln!("[wslterm] session ended");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event),
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (y.round() as i32) * 3,
                    MouseScrollDelta::PixelDelta(p) => (p.y / self.cell_h as f64).round() as i32,
                };
                self.scroll_by(lines);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = (position.x, position.y);
                if self.selecting {
                    self.update_selection();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => self.begin_selection(),
                (MouseButton::Left, ElementState::Released) => self.selecting = false,
                (MouseButton::Middle, ElementState::Pressed) => self.paste(),
                _ => {}
            },
            WindowEvent::Resized(size) => {
                self.resize_surface(size.width, size.height);
                if let Some(win) = &self.win {
                    win.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                self.recompute_metrics();
                self.reflow();
                if let Some(win) = &self.win {
                    win.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).expect("run app");
}

/// Initial window opacity from `$WSLTERM_OPACITY` (accepts 0.0..1.0 or 0..100),
/// defaulting to 0.92. Clamped to a usable 0.4..1.0.
fn parse_opacity_env() -> f32 {
    let raw = std::env::var("WSLTERM_OPACITY")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok());
    let v = match raw {
        Some(v) if v > 1.0 => v / 100.0, // treat 0..100 as a percentage
        Some(v) => v,
        None => 0.92,
    };
    v.clamp(0.4, 1.0)
}

/// Apply uniform window translucency via a layered window. The OS composites our
/// painted content at `opacity`, so text and background fade together — the
/// common terminal "transparency" behavior; keeps the normal title bar/resize.
#[cfg(windows)]
fn set_window_opacity(window: &Window, opacity: f32) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA,
        WS_EX_LAYERED,
    };
    let raw = match window.window_handle() {
        Ok(h) => h.as_raw(),
        Err(_) => return,
    };
    let hwnd = match raw {
        RawWindowHandle::Win32(h) => h.hwnd.get() as HWND,
        _ => return,
    };
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        SetLayeredWindowAttributes(hwnd, 0, alpha, LWA_ALPHA);
    }
}

#[cfg(not(windows))]
fn set_window_opacity(_window: &Window, _opacity: f32) {}

/// Load a system monospace font (Consolas, then Cascadia Mono, then Lucida).
fn load_monospace_font() -> Option<FontVec> {
    let candidates = [
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\CascadiaCode.ttf",
        r"C:\Windows\Fonts\lucon.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = FontVec::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }
    None
}

fn map_key(k: &WKey) -> Option<Key> {
    match k {
        WKey::Named(n) => match n {
            NamedKey::Enter => Some(Key::Enter),
            NamedKey::Tab => Some(Key::Tab),
            NamedKey::Escape => Some(Key::Escape),
            NamedKey::Backspace => Some(Key::Backspace),
            NamedKey::Space => Some(Key::Space),
            NamedKey::ArrowUp => Some(Key::Up),
            NamedKey::ArrowDown => Some(Key::Down),
            NamedKey::ArrowLeft => Some(Key::Left),
            NamedKey::ArrowRight => Some(Key::Right),
            NamedKey::Home => Some(Key::Home),
            NamedKey::End => Some(Key::End),
            NamedKey::Insert => Some(Key::Insert),
            NamedKey::Delete => Some(Key::Delete),
            NamedKey::PageUp => Some(Key::PageUp),
            NamedKey::PageDown => Some(Key::PageDown),
            _ => None,
        },
        WKey::Character(s) => s.chars().next().map(Key::Char),
        _ => None,
    }
}

fn function_number(code: KeyCode) -> Option<u8> {
    Some(match code {
        KeyCode::F1 => 1,
        KeyCode::F2 => 2,
        KeyCode::F3 => 3,
        KeyCode::F4 => 4,
        KeyCode::F5 => 5,
        KeyCode::F6 => 6,
        KeyCode::F7 => 7,
        KeyCode::F8 => 8,
        KeyCode::F9 => 9,
        KeyCode::F10 => 10,
        KeyCode::F11 => 11,
        KeyCode::F12 => 12,
        _ => return None,
    })
}

/// Fill a clipped rectangle in the 0RGB pixel buffer.
fn fill_rect(buf: &mut [u32], w: u32, h: u32, x: usize, y: usize, rw: usize, rh: usize, rgb: u32) {
    let x1 = (x + rw).min(w as usize);
    let y1 = (y + rh).min(h as usize);
    for py in y..y1 {
        let base = py * w as usize;
        for px in x..x1 {
            buf[base + px] = rgb;
        }
    }
}

/// Alpha-blend `fg` over `dst` by coverage (0..1). Both are 0RGB.
fn blend(dst: u32, fg: u32, cov: f32) -> u32 {
    let cov = cov.clamp(0.0, 1.0);
    let inv = 1.0 - cov;
    let dr = ((dst >> 16) & 0xff) as f32;
    let dg = ((dst >> 8) & 0xff) as f32;
    let db = (dst & 0xff) as f32;
    let fr = ((fg >> 16) & 0xff) as f32;
    let fgc = ((fg >> 8) & 0xff) as f32;
    let fb = (fg & 0xff) as f32;
    let r = (fr * cov + dr * inv) as u32;
    let g = (fgc * cov + dg * inv) as u32;
    let b = (fb * cov + db * inv) as u32;
    (r << 16) | (g << 8) | b
}
