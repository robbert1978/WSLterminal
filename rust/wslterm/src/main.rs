//! WSL Terminal GUI — Rust rewrite.
//!
//! A window that renders the `wslterm-core` terminal grid and pumps the user's
//! keystrokes (via `wslterm-core::input::encode`) into live WSL sessions driven
//! by `wslterm-pty`. CPU-rendered (winit + softbuffer + ab_glyph) on purpose: it
//! keeps RAM low and avoids the wgpu/Direct3D managed stack — the whole point of
//! the rewrite.
//!
//! One window multiplexes many sessions as tabs over a single wslg+wslptyd. A
//! background feed thread owns the mux receiver, routes each frame to the right
//! session's `Arc<Mutex<Terminal>>` (by id), and wakes the UI to render
//! (coalesced). The mux uses a bounded channel so floods back-pressure wslptyd
//! instead of buffering — memory stays bounded.

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
use wslterm_core::{Cell, CellFlags, Terminal};
use wslterm_pty::bootstrap;
use wslterm_pty::mux::MuxEvent;
use wslterm_pty::{WslMux, WslProcess};

mod settings;
use settings::{Settings, Theme};

const DISTRO: &str = "Ubuntu";
/// points -> device-independent pixels (CSS px); multiplied again by DPI scale.
const PT_TO_PX: f32 = 96.0 / 72.0;

/// Per-session registry shared with the feed thread: session id -> its terminal.
type Registry = Arc<Mutex<HashMap<u32, Arc<Mutex<Terminal>>>>>;
/// Responses (DSR/DA) the feed thread owes the PTY, tagged by session.
type Outbox = Arc<Mutex<Vec<(u32, Vec<u8>)>>>;

/// Lightweight events from the feed thread to the UI.
enum UserEvent {
    Redraw,
    SessionExit(u32),
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

/// One terminal tab: its session + emulator state + view (scroll/selection).
struct Tab {
    session: u32,
    term: Arc<Mutex<Terminal>>,
    scroll_off: usize,
    selecting: bool,
    sel_anchor: Option<(i64, i64)>,
    sel: Option<(i64, i64, i64, i64)>,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    win: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,

    font: FontVec,
    scale: f32,
    font_pts: f32,
    font_pts_base: f32,
    font_px: f32,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    theme: Theme,
    opacity: f32,

    mux: Option<WslMux>,
    registry: Registry,
    outbox: Outbox,
    redraw_pending: Arc<AtomicBool>,

    tabs: Vec<Tab>,
    active: usize,
    start_dir: Option<String>, // cwd for the first tab (from --cd)

    mods: ModifiersState,
    cursor_px: (f64, f64),
    grid: Vec<Vec<Cell>>,
    glyph_cache: HashMap<char, Option<Glyph>>,

    // Tab-bar hit-test ranges (device px), recomputed each render.
    chip_ranges: Vec<(f32, f32)>,
    plus_range: (f32, f32),
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>, start_dir: Option<String>) -> App {
        let cfg = Settings::load();
        let font = load_monospace_font(&cfg.font_family)
            .expect("no monospace font found (Consolas/Cascadia)");
        let mut app = App {
            proxy,
            win: None,
            context: None,
            surface: None,
            font,
            scale: 1.0,
            font_pts: cfg.font_pts,
            font_pts_base: cfg.font_pts,
            font_px: cfg.font_pts * PT_TO_PX,
            cell_w: 1,
            cell_h: 1,
            ascent: 0.0,
            theme: cfg.theme,
            opacity: parse_opacity_env().unwrap_or(cfg.opacity),
            mux: None,
            registry: Arc::new(Mutex::new(HashMap::new())),
            outbox: Arc::new(Mutex::new(Vec::new())),
            redraw_pending: Arc::new(AtomicBool::new(false)),
            tabs: Vec::new(),
            active: 0,
            start_dir,
            mods: ModifiersState::empty(),
            cursor_px: (0.0, 0.0),
            grid: Vec::new(),
            glyph_cache: HashMap::new(),
            chip_ranges: Vec::new(),
            plus_range: (0.0, 0.0),
        };
        app.recompute_metrics();
        app
    }

    fn recompute_metrics(&mut self) {
        self.font_px = self.font_pts * PT_TO_PX * self.scale;
        let sf = self.font.as_scaled(PxScale::from(self.font_px));
        self.ascent = sf.ascent();
        self.cell_h = (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(1.0) as usize;
        self.cell_w = sf.h_advance(self.font.glyph_id('M')).ceil().max(1.0) as usize;
        self.glyph_cache.clear();
    }

    /// Height of the tab strip, in device px.
    fn tab_bar_h(&self) -> usize {
        self.cell_h + (8.0 * self.scale).round() as usize
    }

    /// Grid dimensions for the terminal area (below the tab bar).
    fn grid_dims(&self, w: u32, h: u32) -> (usize, usize) {
        let avail_h = (h as usize).saturating_sub(self.tab_bar_h());
        let cols = (w as usize / self.cell_w).max(1);
        let rows = (avail_h / self.cell_h).max(1);
        (cols, rows)
    }

    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }
    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }
    fn active_term(&self) -> Arc<Mutex<Terminal>> {
        self.tabs[self.active].term.clone()
    }

    /// Send bytes to the active session.
    fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let (Some(mux), Some(tab)) = (&self.mux, self.tabs.get(self.active)) {
            mux.send_data(tab.session, bytes);
        }
    }

    /// Open a new tab, inheriting the active tab's working directory.
    fn add_tab(&mut self) {
        let (cols, rows) = match &self.win {
            Some(w) => {
                let s = w.inner_size();
                self.grid_dims(s.width, s.height)
            }
            None => (80, 24),
        };
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.term.lock().unwrap().current_directory().map(String::from))
            .unwrap_or_default();
        let term = Arc::new(Mutex::new(Terminal::new(cols, rows)));
        let session = match &self.mux {
            Some(mux) => mux.open(cols as u16, rows as u16, &cwd),
            None => return,
        };
        self.registry.lock().unwrap().insert(session, term.clone());
        self.tabs.push(Tab {
            session,
            term,
            scroll_off: 0,
            selecting: false,
            sel_anchor: None,
            sel: None,
        });
        self.active = self.tabs.len() - 1;
        self.request_redraw();
    }

    /// Close the tab at `idx`. `exited` = the session already died (don't re-close
    /// the PTY). Exits the app when the last tab closes.
    fn close_tab_at(&mut self, idx: usize, exited: bool, event_loop: &ActiveEventLoop) {
        if idx >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(idx);
        self.registry.lock().unwrap().remove(&tab.session);
        if !exited {
            if let Some(mux) = &self.mux {
                mux.close(tab.session);
            }
        }
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.request_redraw();
    }

    /// Open a new top-level window (a fresh wslterm process) in `dir`.
    fn spawn_new_window(&self) {
        let dir = self
            .tabs
            .get(self.active)
            .and_then(|t| t.term.lock().unwrap().current_directory().map(String::from));
        spawn_window(dir);
    }

    fn switch_tab(&mut self, delta: i32) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as i32;
        self.active = (self.active as i32 + delta).rem_euclid(n) as usize;
        self.request_redraw();
    }

    fn handle_key(&mut self, ev: &KeyEvent, event_loop: &ActiveEventLoop) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();

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
                KeyCode::KeyT if ctrl && shift => {
                    self.add_tab();
                    return;
                }
                KeyCode::KeyN if ctrl && shift => {
                    self.spawn_new_window();
                    return;
                }
                KeyCode::KeyW if ctrl && shift => {
                    self.close_tab_at(self.active, false, event_loop);
                    return;
                }
                KeyCode::Tab if ctrl => {
                    self.switch_tab(if shift { -1 } else { 1 });
                    return;
                }
                KeyCode::Equal | KeyCode::NumpadAdd if ctrl => {
                    self.zoom_font(1.0);
                    return;
                }
                KeyCode::Minus | KeyCode::NumpadSubtract if ctrl => {
                    self.zoom_font(-1.0);
                    return;
                }
                KeyCode::Digit0 | KeyCode::Numpad0 if ctrl => {
                    self.reset_font();
                    return;
                }
                _ => {}
            }
        }

        // Typing snaps the active view to the live bottom.
        if self.active_tab().scroll_off != 0 {
            self.active_tab_mut().scroll_off = 0;
            self.request_redraw();
        }
        let mods = Mods { ctrl, alt: self.mods.alt_key(), shift };
        let app_cursor = self.active_term().lock().unwrap().app_cursor_keys();

        if let PhysicalKey::Code(code) = ev.physical_key {
            if let Some(n) = function_number(code) {
                if let Some(b) = input::encode(Key::F(n), mods, app_cursor) {
                    self.send(&b);
                    return;
                }
            }
        }

        let key = map_key(&ev.logical_key);
        if let Some(bytes) = key.and_then(|k| input::encode(k, mods, app_cursor)) {
            self.send(&bytes);
            return;
        }
        if let Some(text) = &ev.text {
            self.send(text.as_bytes());
        }
    }

    /// Map the mouse position to an (absolute row, column) cell in the active tab.
    fn cell_at_cursor(&self) -> (i64, i64) {
        let bar = self.tab_bar_h() as f64;
        let col = (self.cursor_px.0 / self.cell_w as f64).floor() as i64;
        let vrow = ((self.cursor_px.1 - bar) / self.cell_h as f64).floor() as i64;
        let t = self.active_term();
        let t = t.lock().unwrap();
        let cols = t.cols() as i64;
        let off = self.active_tab().scroll_off as i64;
        let top_abs = t.scrollback_count() as i64 - off;
        let total = t.scrollback_count() as i64 + t.rows() as i64;
        let abs = (top_abs + vrow).clamp(0, (total - 1).max(0));
        (abs, col.clamp(0, (cols - 1).max(0)))
    }

    fn begin_selection(&mut self) {
        let cell = self.cell_at_cursor();
        let tab = self.active_tab_mut();
        tab.sel_anchor = Some(cell);
        tab.sel = Some((cell.0, cell.1, cell.0, cell.1));
        tab.selecting = true;
        self.request_redraw();
    }

    fn update_selection(&mut self) {
        let anchor = self.active_tab().sel_anchor;
        if let Some((ar, ac)) = anchor {
            let (r, c) = self.cell_at_cursor();
            let sel = if (r, c) < (ar, ac) { (r, c, ar, ac) } else { (ar, ac, r, c) };
            self.active_tab_mut().sel = Some(sel);
            self.request_redraw();
        }
    }

    fn copy_selection(&self) {
        if let Some((r1, c1, r2, c2)) = self.active_tab().sel {
            let text = self.active_term().lock().unwrap().get_text(r1, c1, r2, c2);
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
        let bracketed = self.active_term().lock().unwrap().bracketed_paste();
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

    fn zoom_font(&mut self, delta_pts: f32) {
        self.font_pts = (self.font_pts + delta_pts).clamp(6.0, 72.0);
        self.recompute_metrics();
        self.reflow();
        self.request_redraw();
    }

    fn reset_font(&mut self) {
        self.font_pts = self.font_pts_base;
        self.recompute_metrics();
        self.reflow();
        self.request_redraw();
    }

    fn scroll_by(&mut self, lines: i32) {
        let max = self.active_term().lock().unwrap().scrollback_count() as i32;
        let cur = self.active_tab().scroll_off as i32;
        let next = (cur + lines).clamp(0, max) as usize;
        if next != self.active_tab().scroll_off {
            self.active_tab_mut().scroll_off = next;
            self.request_redraw();
        }
    }

    fn reflow(&mut self) {
        if let Some(win) = &self.win {
            let s = win.inner_size();
            self.resize_surface(s.width, s.height);
        }
    }

    fn resize_surface(&mut self, w: u32, h: u32) {
        let (cols, rows) = self.grid_dims(w, h);
        for tab in &self.tabs {
            tab.term.lock().unwrap().resize(cols, rows);
            if let Some(mux) = &self.mux {
                mux.send_resize(tab.session, cols as u16, rows as u16);
            }
        }
        if let (Some(surface), Some(nw), Some(nh)) =
            (&mut self.surface, NonZeroU32::new(w), NonZeroU32::new(h))
        {
            let _ = surface.resize(nw, nh);
        }
    }

    /// Index of the tab chip under device-x, if any.
    fn chip_at(&self, x: f32) -> Option<usize> {
        self.chip_ranges
            .iter()
            .position(|&(x0, x1)| x >= x0 && x < x1)
    }

    /// Handle a left-click in the tab bar: switch tab or open a new one.
    fn tab_bar_click(&mut self) {
        let x = self.cursor_px.0 as f32;
        if x >= self.plus_range.0 && x < self.plus_range.1 {
            self.add_tab();
        } else if let Some(i) = self.chip_at(x) {
            self.active = i;
            self.request_redraw();
        }
    }

    fn render(&mut self) {
        // Clone the Rc so we drop the borrow on self.win before touching surface
        // and other fields (avoids &self-method-vs-&mut-surface borrow conflicts).
        let win = match &self.win {
            Some(w) => w.clone(),
            None => return,
        };
        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let bar_h = self.tab_bar_h();
        let (cw, ch_px) = (self.cell_w, self.cell_h);
        let pad = (6.0 * self.scale).round().max(2.0) as usize;
        let text_top = ((bar_h.saturating_sub(ch_px)) / 2) as i32;

        // Tab titles (gathered before borrowing the surface).
        let titles: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                let g = t.term.lock().unwrap();
                g.title()
                    .map(str::to_string)
                    .or_else(|| g.current_directory().map(basename))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "wsl".into())
            })
            .collect();

        let surface = match &mut self.surface {
            Some(s) => s,
            None => return,
        };
        let mut buffer = match surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        let buf: &mut [u32] = &mut buffer;
        for px in buf.iter_mut() {
            *px = self.theme.bg;
        }

        // --- tab bar -------------------------------------------------------
        let chrome = mix(self.theme.bg, self.theme.fg, 0.10);
        let chip_active = mix(self.theme.bg, self.theme.fg, 0.22);
        fill_rect(buf, w, h, 0, 0, w as usize, bar_h, chrome);

        self.chip_ranges.clear();
        let active = self.active;
        let mut x = pad;
        for (i, title) in titles.iter().enumerate() {
            let text: String = title.chars().take(18).collect();
            let chip_w = (text.chars().count() * cw + pad * 2).clamp(40, 240);
            let x0 = x;
            let x1 = (x + chip_w).min(w as usize);
            if i == active {
                fill_rect(buf, w, h, x0, 2, x1 - x0, bar_h.saturating_sub(4), chip_active);
            }
            let fg = if i == active {
                self.theme.fg
            } else {
                mix(self.theme.bg, self.theme.fg, 0.55)
            };
            let mut gx = x0 + pad;
            for ch in text.chars() {
                ensure_glyph(&self.font, &mut self.glyph_cache, self.font_px, ch);
                blit_char(buf, w, h, &self.glyph_cache, ch, gx, text_top, fg);
                gx += cw;
            }
            self.chip_ranges.push((x0 as f32, x1 as f32));
            x = x1 + (2.0 * self.scale) as usize;
        }
        // '+' new-tab button
        let px0 = x;
        let px1 = (x + cw + pad * 2).min(w as usize);
        ensure_glyph(&self.font, &mut self.glyph_cache, self.font_px, '+');
        blit_char(buf, w, h, &self.glyph_cache, '+', px0 + pad, text_top,
            mix(self.theme.bg, self.theme.fg, 0.7));
        self.plus_range = (px0 as f32, px1 as f32);

        // --- terminal area -------------------------------------------------
        let (cols, rows, cx, cy, cursor_on, top_abs);
        let sel;
        {
            let tab = &self.tabs[self.active];
            let t = tab.term.lock().unwrap();
            let scroll = tab.scroll_off.min(t.scrollback_count());
            t.capture_viewport(scroll, &mut self.grid);
            cols = t.cols();
            rows = t.rows();
            cx = t.cx();
            cy = t.cy();
            cursor_on = t.cursor_visible() && scroll == 0;
            top_abs = t.scrollback_count() as i64 - scroll as i64;
            sel = tab.sel;
        }

        // Pass 1: ensure glyphs cached.
        for r in 0..rows.min(self.grid.len()) {
            for c in 0..cols.min(self.grid[r].len()) {
                let rune = self.grid[r][c].rune;
                if rune >= 0x20 {
                    if let Some(ch) = char::from_u32(rune) {
                        ensure_glyph(&self.font, &mut self.glyph_cache, self.font_px, ch);
                    }
                }
            }
        }

        // Pass 2: fill backgrounds + blit glyphs.
        for r in 0..rows.min(self.grid.len()) {
            let row = &self.grid[r];
            for c in 0..cols.min(row.len()) {
                let cell = &row[c];
                if cell.width == 0 {
                    continue;
                }
                let is_cursor = cursor_on && r == cy && c == cx;
                let reverse = cell.flags.contains(CellFlags::REVERSE) ^ is_cursor;
                let bold = cell.flags.contains(CellFlags::BOLD);

                let mut fg = self.theme.resolve(cell.fg, self.theme.fg, bold);
                let mut bg = self.theme.resolve(cell.bg, self.theme.bg, false);
                if reverse {
                    if is_cursor {
                        bg = self.theme.cursor;
                        fg = self.theme.bg;
                    } else {
                        std::mem::swap(&mut fg, &mut bg);
                    }
                }
                if let Some((r1, c1, r2, c2)) = sel {
                    let (ar, ac) = (top_abs + r as i64, c as i64);
                    let after = ar > r1 || (ar == r1 && ac >= c1);
                    let before = ar < r2 || (ar == r2 && ac <= c2);
                    if after && before {
                        bg = self.theme.selection;
                    }
                }

                let x0 = c * cw;
                let y0 = bar_h + r * ch_px;
                fill_rect(buf, w, h, x0, y0, cw, ch_px, bg);

                if cell.rune >= 0x20 {
                    if let Some(ch) = char::from_u32(cell.rune) {
                        blit_char(buf, w, h, &self.glyph_cache, ch, x0, y0 as i32, fg);
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
            return;
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

        // Bring up the live WSL server.
        let server = bootstrap::resolve_server()
            .expect("wslptyd not found (build native/ and place under artifacts/)");
        let command = bootstrap::build_server_command(&server);
        let proc = WslProcess::launch(DISTRO, &command).expect("launch wslg.exe");
        let (mux, rx) = WslMux::start(proc);

        // First tab (in --cd directory if given).
        let term = Arc::new(Mutex::new(Terminal::new(cols, rows)));
        let session = mux.open(cols as u16, rows as u16, self.start_dir.as_deref().unwrap_or(""));
        self.registry.lock().unwrap().insert(session, term.clone());
        self.tabs.push(Tab {
            session,
            term,
            scroll_off: 0,
            selecting: false,
            sel_anchor: None,
            sel: None,
        });
        self.active = 0;
        self.mux = Some(mux);
        eprintln!("[wslterm] window up {cols}x{rows}, first session {session} on {DISTRO}");

        // Feed thread: route frames to the right session's terminal by id.
        let registry = self.registry.clone();
        let outbox = self.outbox.clone();
        let pending = self.redraw_pending.clone();
        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("wsl-feed".into())
            .spawn(move || {
                let mut acc: u64 = 0;
                let mut mark = Instant::now();
                while let Ok(first) = rx.recv() {
                    let mut ev = Some(first);
                    let mut resp: Vec<(u32, Vec<u8>)> = Vec::new();
                    while let Some(e) = ev.take() {
                        match e {
                            MuxEvent::Data { id, bytes } => {
                                acc += bytes.len() as u64;
                                let term = registry.lock().unwrap().get(&id).cloned();
                                if let Some(term) = term {
                                    let mut t = term.lock().unwrap();
                                    t.feed(&bytes);
                                    if !t.respond.is_empty() {
                                        resp.push((id, std::mem::take(&mut t.respond)));
                                    }
                                }
                            }
                            MuxEvent::Exit { id, .. } => {
                                let _ = proxy.send_event(UserEvent::SessionExit(id));
                            }
                        }
                        ev = rx.try_recv().ok();
                    }
                    if !resp.is_empty() {
                        outbox.lock().unwrap().extend(resp);
                    }
                    let dt = mark.elapsed().as_secs_f64();
                    if dt >= 1.0 {
                        if acc > 0 {
                            eprintln!("[feed] {:.1} MB/s", acc as f64 / 1e6 / dt);
                        }
                        acc = 0;
                        mark = Instant::now();
                    }
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
                // New output snaps the active tab to the bottom (unless dragging).
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.scroll_off = 0;
                    if !tab.selecting {
                        tab.sel = None;
                    }
                }
                // Flush DSR/DA responses to their sessions.
                let out = std::mem::take(&mut *self.outbox.lock().unwrap());
                if let Some(mux) = &self.mux {
                    for (id, bytes) in out {
                        mux.send_data(id, &bytes);
                    }
                }
                self.request_redraw();
            }
            UserEvent::SessionExit(id) => {
                if let Some(idx) = self.tabs.iter().position(|t| t.session == id) {
                    self.close_tab_at(idx, true, event_loop);
                }
            }
            UserEvent::Closed => {
                eprintln!("[wslterm] mux ended");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event, event_loop),
            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / self.cell_h as f64,
                };
                if self.mods.control_key() {
                    self.zoom_font(if y > 0.0 { 1.0 } else { -1.0 });
                } else {
                    self.scroll_by((y.round() as i32) * 3);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = (position.x, position.y);
                if self.active_tab().selecting {
                    self.update_selection();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    if self.cursor_px.1 < self.tab_bar_h() as f64 {
                        self.tab_bar_click();
                    } else {
                        self.begin_selection();
                    }
                }
                (MouseButton::Left, ElementState::Released) => {
                    self.active_tab_mut().selecting = false;
                }
                (MouseButton::Middle, ElementState::Pressed) => {
                    if self.cursor_px.1 < self.tab_bar_h() as f64 {
                        if let Some(idx) = self.chip_at(self.cursor_px.0 as f32) {
                            self.close_tab_at(idx, false, event_loop);
                        }
                    } else {
                        self.paste(); // X11-style middle-click paste in the terminal
                    }
                }
                _ => {}
            },
            WindowEvent::Resized(size) => {
                self.resize_surface(size.width, size.height);
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale = scale_factor as f32;
                self.recompute_metrics();
                self.reflow();
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }
}

fn main() {
    let start_dir = parse_cd_arg();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy, start_dir);
    event_loop.run_app(&mut app).expect("run app");
}

/// `--cd <wsl-dir>`: the directory the first tab should start in (used when a
/// new window is spawned from "Open in new window" / Ctrl+Shift+N).
fn parse_cd_arg() -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == "--cd" {
            return args.next().filter(|s| !s.is_empty());
        }
    }
    None
}

/// Launch another wslterm window (a detached, windowless child process).
#[cfg(windows)]
fn spawn_window(dir: Option<String>) {
    use std::os::windows::process::CommandExt;
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = std::process::Command::new(exe);
        if let Some(d) = dir {
            if !d.is_empty() {
                cmd.arg("--cd").arg(d);
            }
        }
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (no console flash)
        let _ = cmd.spawn();
    }
}

#[cfg(not(windows))]
fn spawn_window(_dir: Option<String>) {}

/// Last path component of a (WSL) path, for tab titles.
fn basename(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rsplit('/').next() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "/".to_string(),
    }
}

/// Linear blend between two 0RGB colors by `t` (0 = a, 1 = b).
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |sh: u32| {
        let av = ((a >> sh) & 0xff) as f32;
        let bv = ((b >> sh) & 0xff) as f32;
        (av + (bv - av) * t) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Rasterize `ch` into the cache if not already present (free fn so it borrows
/// only `font`/`cache`, not all of `self`, while the surface buffer is alive).
fn ensure_glyph(font: &FontVec, cache: &mut HashMap<char, Option<Glyph>>, px: f32, ch: char) {
    if ch != ' ' && !ch.is_control() && !cache.contains_key(&ch) {
        cache.insert(ch, rasterize_glyph(font, px, ch));
    }
}

/// Blit a cached glyph with its top-left cell at (x, cell_top) in color `fg`.
/// `g.left`/`g.top` are the glyph box offsets from the cell origin (the glyph was
/// rasterized with the pen at y = ascent), so pixel pos = cell origin + offset.
fn blit_char(
    buf: &mut [u32],
    w: u32,
    h: u32,
    cache: &HashMap<char, Option<Glyph>>,
    ch: char,
    x: usize,
    cell_top: i32,
    fg: u32,
) {
    let g = match cache.get(&ch) {
        Some(Some(g)) => g,
        _ => return,
    };
    for gy in 0..g.h {
        let py = cell_top + g.top + gy as i32;
        if py < 0 || py as u32 >= h {
            continue;
        }
        let base = py as usize * w as usize;
        let cov_row = &g.cov[gy * g.w..gy * g.w + g.w];
        for gx in 0..g.w {
            let cov = cov_row[gx];
            if cov == 0 {
                continue;
            }
            let px = x as i32 + g.left + gx as i32;
            if px < 0 || px as u32 >= w {
                continue;
            }
            let idx = base + px as usize;
            if idx < buf.len() {
                buf[idx] = blend(buf[idx], fg, cov as f32 / 255.0);
            }
        }
    }
}

/// Optional `$WSLTERM_OPACITY` override (0.0..1.0 or 0..100); `None` if unset.
fn parse_opacity_env() -> Option<f32> {
    let v = std::env::var("WSLTERM_OPACITY").ok()?.trim().parse::<f32>().ok()?;
    let v = if v > 1.0 { v / 100.0 } else { v };
    Some(v.clamp(0.4, 1.0))
}

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

fn load_monospace_font(family: &str) -> Option<FontVec> {
    let fam = family.to_ascii_lowercase();
    let mut candidates: Vec<&str> = Vec::new();
    if fam.contains("cascadia") {
        candidates.push(r"C:\Windows\Fonts\CascadiaMono.ttf");
        candidates.push(r"C:\Windows\Fonts\CascadiaCode.ttf");
    }
    if fam.contains("consol") {
        candidates.push(r"C:\Windows\Fonts\consola.ttf");
    }
    candidates.extend_from_slice(&[
        r"C:\Windows\Fonts\consola.ttf",
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\CascadiaCode.ttf",
        r"C:\Windows\Fonts\lucon.ttf",
    ]);
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
