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

/// A pixel rectangle in window coordinates.
#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}
impl Rect {
    fn contains(&self, px: f64, py: f64) -> bool {
        let (px, py) = (px as i64, py as i64);
        px >= self.x as i64
            && px < (self.x + self.w) as i64
            && py >= self.y as i64
            && py < (self.y + self.h) as i64
    }
}

/// One terminal pane: its session + emulator state + view (scroll/selection).
struct Pane {
    session: u32,
    term: Arc<Mutex<Terminal>>,
    scroll_off: usize,
    selecting: bool,
    sel_anchor: Option<(i64, i64)>,
    sel: Option<(i64, i64, i64, i64)>,
}

impl Pane {
    fn new(session: u32, term: Arc<Mutex<Terminal>>) -> Pane {
        Pane { session, term, scroll_off: 0, selecting: false, sel_anchor: None, sel: None }
    }
}

/// A tab's pane layout: a single pane or a recursive split. `Empty` is a
/// transient placeholder used while swapping subtrees in place.
enum Layout {
    Empty,
    Leaf(Pane),
    Split { side_by_side: bool, ratio: f32, a: Box<Layout>, b: Box<Layout> },
}

const DIVIDER: usize = 1; // px between split panes

impl Layout {
    /// Assign each leaf a pixel rect within `area` (depth-first, left/top first).
    fn leaf_rects(&self, area: Rect, out: &mut Vec<(u32, Rect)>) {
        match self {
            Layout::Empty => {}
            Layout::Leaf(p) => out.push((p.session, area)),
            Layout::Split { side_by_side, ratio, a, b } => {
                if *side_by_side {
                    let wa = ((area.w as f32) * ratio) as usize;
                    let ra = Rect { x: area.x, y: area.y, w: wa.saturating_sub(DIVIDER), h: area.h };
                    let rb = Rect {
                        x: area.x + wa,
                        y: area.y,
                        w: area.w.saturating_sub(wa),
                        h: area.h,
                    };
                    a.leaf_rects(ra, out);
                    b.leaf_rects(rb, out);
                } else {
                    let ha = ((area.h as f32) * ratio) as usize;
                    let ra = Rect { x: area.x, y: area.y, w: area.w, h: ha.saturating_sub(DIVIDER) };
                    let rb = Rect {
                        x: area.x,
                        y: area.y + ha,
                        w: area.w,
                        h: area.h.saturating_sub(ha),
                    };
                    a.leaf_rects(ra, out);
                    b.leaf_rects(rb, out);
                }
            }
        }
    }

    fn find(&self, session: u32) -> Option<&Pane> {
        match self {
            Layout::Empty => None,
            Layout::Leaf(p) => (p.session == session).then_some(p),
            Layout::Split { a, b, .. } => a.find(session).or_else(|| b.find(session)),
        }
    }
    fn find_mut(&mut self, session: u32) -> Option<&mut Pane> {
        match self {
            Layout::Empty => None,
            Layout::Leaf(p) => (p.session == session).then_some(p),
            Layout::Split { a, b, .. } => {
                if let Some(p) = a.find_mut(session) {
                    Some(p)
                } else {
                    b.find_mut(session)
                }
            }
        }
    }
    fn first_session(&self) -> u32 {
        match self {
            Layout::Empty => 0,
            Layout::Leaf(p) => p.session,
            Layout::Split { a, .. } => a.first_session(),
        }
    }
    fn collect_sessions(&self, out: &mut Vec<u32>) {
        match self {
            Layout::Empty => {}
            Layout::Leaf(p) => out.push(p.session),
            Layout::Split { a, b, .. } => {
                a.collect_sessions(out);
                b.collect_sessions(out);
            }
        }
    }
}

/// Split the leaf with `target` into (old leaf, `newp`); takes `newp` once.
fn split_layout(node: Layout, target: u32, side_by_side: bool, newp: &mut Option<Pane>) -> Layout {
    match node {
        Layout::Empty => Layout::Empty,
        Layout::Leaf(p) => {
            if p.session == target && newp.is_some() {
                let b = Layout::Leaf(newp.take().unwrap());
                Layout::Split {
                    side_by_side,
                    ratio: 0.5,
                    a: Box::new(Layout::Leaf(p)),
                    b: Box::new(b),
                }
            } else {
                Layout::Leaf(p)
            }
        }
        Layout::Split { side_by_side: s, ratio, a, b } => Layout::Split {
            side_by_side: s,
            ratio,
            a: Box::new(split_layout(*a, target, side_by_side, newp)),
            b: Box::new(split_layout(*b, target, side_by_side, newp)),
        },
    }
}

/// Remove the leaf with `session`, collapsing its parent split. Returns the new
/// tree (None if the tab is now empty) and the removed pane via `out`.
fn remove_layout(node: Layout, session: u32, out: &mut Option<Pane>) -> Option<Layout> {
    match node {
        Layout::Empty => None,
        Layout::Leaf(p) => {
            if p.session == session {
                *out = Some(p);
                None
            } else {
                Some(Layout::Leaf(p))
            }
        }
        Layout::Split { side_by_side, ratio, a, b } => {
            let na = remove_layout(*a, session, out);
            let nb = remove_layout(*b, session, out);
            match (na, nb) {
                (Some(a), Some(b)) => Some(Layout::Split {
                    side_by_side,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

/// One tab: a pane layout tree + the focused pane's session id.
struct Tab {
    root: Layout,
    focus: u32,
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

    // Hit-test caches (device px), recomputed each render.
    chip_ranges: Vec<(f32, f32)>,
    plus_range: (f32, f32),
    pane_rects: Vec<(u32, Rect)>, // active tab's leaf rects
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
            pane_rects: Vec::new(),
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

    // ---- focused-pane accessors ----------------------------------------
    fn focused_session(&self) -> u32 {
        let tab = &self.tabs[self.active];
        if tab.root.find(tab.focus).is_some() {
            tab.focus
        } else {
            tab.root.first_session()
        }
    }
    fn focused(&self) -> &Pane {
        let s = self.focused_session();
        self.tabs[self.active].root.find(s).expect("focused pane")
    }
    fn focused_term(&self) -> Arc<Mutex<Terminal>> {
        self.focused().term.clone()
    }
    fn pane(&self, session: u32) -> Option<&Pane> {
        self.tabs.get(self.active).and_then(|t| t.root.find(session))
    }
    fn pane_mut(&mut self, session: u32) -> Option<&mut Pane> {
        let active = self.active;
        self.tabs.get_mut(active).and_then(|t| t.root.find_mut(session))
    }

    /// Send bytes to the focused session.
    fn send(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Some(mux) = &self.mux {
            mux.send_data(self.focused_session(), bytes);
        }
    }

    fn terminal_area(&self, w: u32, h: u32) -> Rect {
        let bar = self.tab_bar_h();
        Rect { x: 0, y: bar, w: w as usize, h: (h as usize).saturating_sub(bar) }
    }

    /// Open a session + terminal and register it.
    fn open_session(&self, cols: usize, rows: usize, cwd: &str) -> Option<(u32, Arc<Mutex<Terminal>>)> {
        let mux = self.mux.as_ref()?;
        let term = Arc::new(Mutex::new(Terminal::new(cols.max(1), rows.max(1))));
        let session = mux.open(cols as u16, rows as u16, cwd);
        self.registry.lock().unwrap().insert(session, term.clone());
        Some((session, term))
    }

    fn focused_cwd(&self) -> String {
        self.tabs
            .get(self.active)
            .and_then(|_| self.pane(self.focused_session()))
            .and_then(|p| p.term.lock().unwrap().current_directory().map(String::from))
            .unwrap_or_default()
    }

    /// Open a new tab, inheriting the focused pane's working directory.
    fn add_tab(&mut self) {
        let (cols, rows) = match &self.win {
            Some(w) => {
                let s = w.inner_size();
                self.grid_dims(s.width, s.height)
            }
            None => (80, 24),
        };
        let cwd = self.focused_cwd();
        let (session, term) = match self.open_session(cols, rows, &cwd) {
            Some(x) => x,
            None => return,
        };
        self.tabs.push(Tab { root: Layout::Leaf(Pane::new(session, term)), focus: session });
        self.active = self.tabs.len() - 1;
        self.reflow();
        self.request_redraw();
    }

    /// Split the focused pane (side_by_side = columns, else rows).
    fn split_focused(&mut self, side_by_side: bool) {
        let target = self.focused_session();
        let (cols, rows) = {
            let t = self.focused_term();
            let t = t.lock().unwrap();
            (t.cols(), t.rows())
        };
        let cwd = self.focused_cwd();
        let (session, term) = match self.open_session(cols, rows, &cwd) {
            Some(x) => x,
            None => return,
        };
        let mut newp = Some(Pane::new(session, term));
        let tab = &mut self.tabs[self.active];
        let root = std::mem::replace(&mut tab.root, Layout::Empty);
        tab.root = split_layout(root, target, side_by_side, &mut newp);
        tab.focus = session;
        self.reflow();
        self.request_redraw();
    }

    /// Close a session's pane (collapsing its split); `exited` skips re-closing
    /// the PTY. Closes the tab when its last pane goes, and exits on the last tab.
    fn close_session(&mut self, session: u32, exited: bool, event_loop: &ActiveEventLoop) {
        let ti = match self.tabs.iter().position(|t| t.root.find(session).is_some()) {
            Some(i) => i,
            None => return,
        };
        if !exited {
            if let Some(mux) = &self.mux {
                mux.close(session);
            }
        }
        self.registry.lock().unwrap().remove(&session);
        let tab = &mut self.tabs[ti];
        let root = std::mem::replace(&mut tab.root, Layout::Empty);
        let mut removed = None;
        match remove_layout(root, session, &mut removed) {
            Some(r) => {
                tab.root = r;
                if tab.focus == session {
                    tab.focus = tab.root.first_session();
                }
            }
            None => {
                self.tabs.remove(ti);
                if self.tabs.is_empty() {
                    event_loop.exit();
                    return;
                }
                if self.active >= self.tabs.len() {
                    self.active = self.tabs.len() - 1;
                }
            }
        }
        self.reflow();
        self.request_redraw();
    }

    /// Open a new top-level window (a fresh wslterm process) in the focused cwd.
    fn spawn_new_window(&self) {
        let dir = self.focused_cwd();
        spawn_window(if dir.is_empty() { None } else { Some(dir) });
    }

    /// Close an entire tab (all its panes).
    fn close_whole_tab(&mut self, idx: usize, event_loop: &ActiveEventLoop) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut sessions = Vec::new();
        self.tabs[idx].root.collect_sessions(&mut sessions);
        for s in &sessions {
            if let Some(mux) = &self.mux {
                mux.close(*s);
            }
            self.registry.lock().unwrap().remove(s);
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.reflow();
        self.request_redraw();
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
        let alt = self.mods.alt_key();

        if let PhysicalKey::Code(code) = ev.physical_key {
            match code {
                // Alt+Shift +/- split the focused pane (columns / rows).
                KeyCode::Equal | KeyCode::NumpadAdd if alt && shift => {
                    self.split_focused(true);
                    return;
                }
                KeyCode::Minus | KeyCode::NumpadSubtract if alt && shift => {
                    self.split_focused(false);
                    return;
                }
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
                    let s = self.focused_session();
                    self.close_session(s, false, event_loop);
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

        // Typing snaps the focused view to the live bottom.
        let s = self.focused_session();
        if self.pane(s).map(|p| p.scroll_off).unwrap_or(0) != 0 {
            if let Some(p) = self.pane_mut(s) {
                p.scroll_off = 0;
            }
            self.request_redraw();
        }
        let mods = Mods { ctrl, alt, shift };
        let app_cursor = self.focused_term().lock().unwrap().app_cursor_keys();

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

    fn pane_at_cursor(&self) -> Option<u32> {
        self.pane_rects
            .iter()
            .find(|(_, r)| r.contains(self.cursor_px.0, self.cursor_px.1))
            .map(|(s, _)| *s)
    }
    fn rect_of(&self, session: u32) -> Option<Rect> {
        self.pane_rects.iter().find(|(s, _)| *s == session).map(|(_, r)| *r)
    }

    /// (absolute row, col) of the cursor within a pane's rect.
    fn cell_in(&self, session: u32, rect: Rect) -> (i64, i64) {
        let col = ((self.cursor_px.0 - rect.x as f64) / self.cell_w as f64).floor() as i64;
        let vrow = ((self.cursor_px.1 - rect.y as f64) / self.cell_h as f64).floor() as i64;
        let (off, term) = match self.pane(session) {
            Some(p) => (p.scroll_off as i64, p.term.clone()),
            None => return (0, 0),
        };
        let t = term.lock().unwrap();
        let cols = t.cols() as i64;
        let top_abs = t.scrollback_count() as i64 - off;
        let total = t.scrollback_count() as i64 + t.rows() as i64;
        let abs = (top_abs + vrow).clamp(0, (total - 1).max(0));
        (abs, col.clamp(0, (cols - 1).max(0)))
    }

    fn begin_selection(&mut self) {
        let session = match self.pane_at_cursor() {
            Some(s) => s,
            None => return,
        };
        self.tabs[self.active].focus = session; // click focuses the pane
        if let Some(rect) = self.rect_of(session) {
            let (r, c) = self.cell_in(session, rect);
            if let Some(p) = self.pane_mut(session) {
                p.sel_anchor = Some((r, c));
                p.sel = Some((r, c, r, c));
                p.selecting = true;
            }
        }
        self.request_redraw();
    }

    fn update_selection(&mut self) {
        let session = self.focused_session();
        let anchor = self.pane(session).and_then(|p| if p.selecting { p.sel_anchor } else { None });
        if let (Some((ar, ac)), Some(rect)) = (anchor, self.rect_of(session)) {
            let (r, c) = self.cell_in(session, rect);
            let sel = if (r, c) < (ar, ac) { (r, c, ar, ac) } else { (ar, ac, r, c) };
            if let Some(p) = self.pane_mut(session) {
                p.sel = Some(sel);
            }
            self.request_redraw();
        }
    }

    fn copy_selection(&self) {
        if let Some((r1, c1, r2, c2)) = self.focused().sel {
            let text = self.focused_term().lock().unwrap().get_text(r1, c1, r2, c2);
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
        let bracketed = self.focused_term().lock().unwrap().bracketed_paste();
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
        let s = self.focused_session();
        let (max, cur) = match self.pane(s) {
            Some(p) => (p.term.lock().unwrap().scrollback_count() as i32, p.scroll_off as i32),
            None => return,
        };
        let next = (cur + lines).clamp(0, max) as usize;
        if next as i32 != cur {
            if let Some(p) = self.pane_mut(s) {
                p.scroll_off = next;
            }
            self.request_redraw();
        }
    }

    fn reflow(&mut self) {
        if let Some(win) = &self.win {
            let s = win.inner_size();
            self.resize_surface(s.width, s.height);
        }
    }

    /// Resize the surface and every pane's PTY to match its rect.
    fn resize_surface(&mut self, w: u32, h: u32) {
        let area = self.terminal_area(w, h);
        let (cw, ch_px) = (self.cell_w, self.cell_h);
        for ti in 0..self.tabs.len() {
            let mut rects = Vec::new();
            self.tabs[ti].root.leaf_rects(area, &mut rects);
            for (session, rect) in rects {
                let cols = (rect.w / cw).max(1);
                let rows = (rect.h / ch_px).max(1);
                if let Some(p) = self.tabs[ti].root.find(session) {
                    p.term.lock().unwrap().resize(cols, rows);
                }
                if let Some(mux) = &self.mux {
                    mux.send_resize(session, cols as u16, rows as u16);
                }
            }
        }
        if let (Some(surface), Some(nw), Some(nh)) =
            (&mut self.surface, NonZeroU32::new(w), NonZeroU32::new(h))
        {
            let _ = surface.resize(nw, nh);
        }
    }

    fn chip_at(&self, x: f32) -> Option<usize> {
        self.chip_ranges.iter().position(|&(x0, x1)| x >= x0 && x < x1)
    }

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

        let titles: Vec<String> = self
            .tabs
            .iter()
            .map(|t| {
                let s = t.root.first_session();
                match t.root.find(if t.root.find(t.focus).is_some() { t.focus } else { s }) {
                    Some(p) => {
                        let g = p.term.lock().unwrap();
                        g.title()
                            .map(str::to_string)
                            .or_else(|| g.current_directory().map(basename))
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "wsl".into())
                    }
                    None => "wsl".into(),
                }
            })
            .collect();

        // Lay out panes for the active tab (also used for mouse hit-testing).
        let area = Rect { x: 0, y: bar_h, w: w as usize, h: (h as usize).saturating_sub(bar_h) };
        self.pane_rects.clear();
        self.tabs[self.active].root.leaf_rects(area, &mut self.pane_rects);
        let focus = self.focused_session();
        let rects = self.pane_rects.clone();

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
        let px0 = x;
        let px1 = (x + cw + pad * 2).min(w as usize);
        ensure_glyph(&self.font, &mut self.glyph_cache, self.font_px, '+');
        blit_char(buf, w, h, &self.glyph_cache, '+', px0 + pad, text_top,
            mix(self.theme.bg, self.theme.fg, 0.7));
        self.plus_range = (px0 as f32, px1 as f32);

        // --- panes ---------------------------------------------------------
        let divider = mix(self.theme.bg, self.theme.fg, 0.18);
        for (session, rect) in &rects {
            // Snapshot this pane's state, then release its borrow.
            let (term, scroll_off, sel) = {
                let p = match self.tabs[self.active].root.find(*session) {
                    Some(p) => p,
                    None => continue,
                };
                (p.term.clone(), p.scroll_off, p.sel)
            };
            let (cols, rows, cx, cy, cursor_on, top_abs);
            {
                let t = term.lock().unwrap();
                let scroll = scroll_off.min(t.scrollback_count());
                t.capture_viewport(scroll, &mut self.grid);
                cols = t.cols();
                rows = t.rows();
                cx = t.cx();
                cy = t.cy();
                cursor_on = t.cursor_visible() && scroll == 0;
                top_abs = t.scrollback_count() as i64 - scroll as i64;
            }
            let rcols = (rect.w / cw).min(cols);
            let rrows = (rect.h / ch_px).min(rows);

            for r in 0..rrows.min(self.grid.len()) {
                for c in 0..rcols.min(self.grid[r].len()) {
                    let rune = self.grid[r][c].rune;
                    if rune >= 0x20 {
                        if let Some(ch) = char::from_u32(rune) {
                            ensure_glyph(&self.font, &mut self.glyph_cache, self.font_px, ch);
                        }
                    }
                }
            }

            for r in 0..rrows.min(self.grid.len()) {
                let row = &self.grid[r];
                for c in 0..rcols.min(row.len()) {
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
                    let x0 = rect.x + c * cw;
                    let y0 = rect.y + r * ch_px;
                    fill_rect(buf, w, h, x0, y0, cw, ch_px, bg);
                    if cell.rune >= 0x20 {
                        if let Some(ch) = char::from_u32(cell.rune) {
                            blit_char(buf, w, h, &self.glyph_cache, ch, x0, y0 as i32, fg);
                        }
                    }
                }
            }

            // Divider lines on the right/bottom edge of non-full panes.
            if rect.x + rect.w < w as usize {
                fill_rect(buf, w, h, rect.x + rect.w, rect.y, DIVIDER, rect.h, divider);
            }
            if rect.y + rect.h < h as usize {
                fill_rect(buf, w, h, rect.x, rect.y + rect.h, rect.w, DIVIDER, divider);
            }
            // Focused pane: thin accent line along its top edge.
            if *session == focus && rects.len() > 1 {
                fill_rect(buf, w, h, rect.x, rect.y, rect.w, (2.0 * self.scale) as usize,
                    self.theme.selection);
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
        self.tabs.push(Tab { root: Layout::Leaf(Pane::new(session, term)), focus: session });
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
                // New output snaps the focused pane to the bottom (unless dragging).
                let s = self.focused_session();
                if let Some(p) = self.pane_mut(s) {
                    p.scroll_off = 0;
                    if !p.selecting {
                        p.sel = None;
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
                self.close_session(id, true, event_loop);
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
                if self.tabs.get(self.active).is_some() && self.focused().selecting {
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
                    let s = self.focused_session();
                    if let Some(p) = self.pane_mut(s) {
                        p.selecting = false;
                    }
                }
                (MouseButton::Middle, ElementState::Pressed) => {
                    if self.cursor_px.1 < self.tab_bar_h() as f64 {
                        // Close the tab under the middle click.
                        if let Some(idx) = self.chip_at(self.cursor_px.0 as f32) {
                            self.close_whole_tab(idx, event_loop);
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
