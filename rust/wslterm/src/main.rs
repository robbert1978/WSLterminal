// Release builds are GUI-subsystem so Windows never allocates a console window
// for our stdout/stderr (which showed up as a stray terminal window). Debug
// builds keep the console for development diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Icon, ResizeDirection, Window, WindowId};

use wslterm_core::color;
use wslterm_core::input::{self, Key, Mods};
use wslterm_core::{Cell, CellFlags, MouseTracking, Terminal};
use wslterm_pty::bootstrap;
use wslterm_pty::mux::MuxEvent;
use wslterm_pty::{WslMux, WslProcess};

mod background;
mod gpu;
mod layered;
mod settings;
mod wslfiles;
use background::BackgroundImage;
use gpu::{Gpu, GlyphDraw};
use layered::{work_area, Layered};
use settings::{Settings, Theme};

/// Opaque alpha in the high byte (for the framebuffer ARGB encoding).
const OPAQUE: u32 = 0xFF00_0000;

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

/// Rasterize one char from `font` at pixel size `px`, positioned on the baseline
/// `ascent` (passed in so fallback glyphs align to the primary font's baseline).
/// `None` if it has no outline (e.g. space, or a color/bitmap-only emoji glyph).
fn rasterize_glyph(font: &FontVec, px: f32, ascent: f32, ch: char) -> Option<Glyph> {
    let scale = PxScale::from(px);
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

/// Cached geometry of a pane's scrollbar, recomputed each render and used for
/// hit-testing/dragging. `max_off` is the pane's scrollback line count (the
/// range of `scroll_off`); `scroll_off == 0` puts the thumb at the bottom.
#[derive(Clone, Copy)]
struct ScrollBar {
    session: u32,
    track: Rect,
    thumb_y: usize,
    thumb_h: usize,
    max_off: usize,
}

impl ScrollBar {
    fn thumb_contains(&self, py: f64) -> bool {
        py >= self.thumb_y as f64 && py < (self.thumb_y + self.thumb_h) as f64
    }
    /// Map a desired thumb-top (px) back to a `scroll_off` value.
    fn off_for_thumb_top(&self, thumb_top: f64) -> usize {
        let travel = self.track.h.saturating_sub(self.thumb_h);
        if travel == 0 || self.max_off == 0 {
            return 0;
        }
        let t = (thumb_top - self.track.y as f64).clamp(0.0, travel as f64);
        let from_top = (t * self.max_off as f64 / travel as f64).round() as usize;
        self.max_off.saturating_sub(from_top)
    }
}

/// A hyperlink under the cursor (Ctrl-hover): its pane, absolute row, the column
/// span `[c0, c1]` to underline, and the URL to open on Ctrl-click.
#[derive(Clone, PartialEq)]
struct HoverUrl {
    session: u32,
    row: i64,
    c0: i64,
    c1: i64,
    url: String,
}

/// Scrollback search state (Ctrl+Shift+F): the query, the matches in `session`
/// (`(abs_row, start_col, end_col)`, top→bottom), and the current match index.
struct Search {
    query: String,
    matches: Vec<(i64, usize, usize)>,
    current: usize,
    session: u32,
}

/// One terminal pane: its session + emulator state + view (scroll/selection).
struct Pane {
    session: u32,
    term: Arc<Mutex<Terminal>>,
    scroll_off: usize,
    /// `term.scrolled_total()` seen at the last frame; diffed to keep a
    /// scrolled-back viewport pinned to the same content as new output arrives.
    last_scrolled: u64,
    selecting: bool,
    sel_anchor: Option<(i64, i64)>,
    sel: Option<(i64, i64, i64, i64)>,
}

impl Pane {
    fn new(session: u32, term: Arc<Mutex<Terminal>>) -> Pane {
        Pane { session, term, scroll_off: 0, last_scrolled: 0, selecting: false, sel_anchor: None, sel: None }
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
const SIDEBAR_MIN_W: usize = 160;
const SIDEBAR_DEFAULT_COLS: usize = 24;

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

/// Move the divider of the *nearest* ancestor split of the requested orientation
/// containing the focused pane, in the arrow direction (Alt+Shift+Arrow). Like
/// Windows Terminal, the shared border moves regardless of which side the focused
/// pane is on, so both arrows work for both panes. `want_sbs` picks the
/// orientation (true = columns/left-right, false = rows); `delta` shifts the
/// split's ratio (Right/Down = +, Left/Up = −). Returns `(focus_in_subtree, adjusted)`.
fn resize_split(node: &mut Layout, focus: u32, want_sbs: bool, delta: f32) -> (bool, bool) {
    match node {
        Layout::Empty => (false, false),
        Layout::Leaf(p) => (p.session == focus, false),
        Layout::Split { side_by_side, ratio, a, b } => {
            // Recurse into the child holding the focus only (don't touch the other).
            let (in_a, adj_a) = resize_split(a, focus, want_sbs, delta);
            let (in_sub, adj) =
                if in_a { (true, adj_a) } else { resize_split(b, focus, want_sbs, delta) };
            if in_sub {
                if !adj && *side_by_side == want_sbs {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    return (true, true);
                }
                return (true, adj);
            }
            (false, false)
        }
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

/// An open file in the editor overlay (replaces the terminal area while open).
struct Doc {
    path: String, // linux path
    name: String,
    lines: Vec<Vec<char>>, // editable, per line
    cy: usize,             // cursor line
    cx: usize,             // cursor column (char index in line)
    scroll: usize,         // first visible line
    dirty: bool,
    readonly: bool,
    win_path: Option<std::path::PathBuf>, // Some => a Windows file (settings)
    is_settings: bool,
    /// Per-line syntax-highlight runs: (rgb, char count). Empty = no highlighting.
    hl: Vec<Vec<(u32, usize)>>,
}

/// Bundled syntaxes / theme for the editor (loaded once).
fn syntax_set() -> &'static syntect::parsing::SyntaxSet {
    static S: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    S.get_or_init(syntect::parsing::SyntaxSet::load_defaults_nonewlines)
}
fn hl_theme() -> &'static syntect::highlighting::Theme {
    static T: OnceLock<syntect::highlighting::Theme> = OnceLock::new();
    T.get_or_init(|| {
        let mut ts = syntect::highlighting::ThemeSet::load_defaults();
        ts.themes
            .remove("base16-ocean.dark")
            .unwrap_or_else(|| ts.themes.values().next().cloned().unwrap())
    })
}

impl Doc {
    /// Recompute per-line syntax-highlight runs (by file extension). Skipped for
    /// very large files to keep editing snappy.
    fn rehighlight(&mut self) {
        self.hl.clear();
        if self.lines.len() > 20_000 {
            return;
        }
        use syntect::easy::HighlightLines;
        let ss = syntax_set();
        let ext = self.name.rsplit('.').next().unwrap_or("");
        let syntax = ss
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| ss.find_syntax_plain_text());
        let mut h = HighlightLines::new(syntax, hl_theme());
        for line in &self.lines {
            let s: String = line.iter().collect();
            let mut runs = Vec::new();
            if let Ok(ranges) = h.highlight_line(&s, ss) {
                for (style, text) in ranges {
                    let c = style.foreground;
                    let rgb = ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32;
                    let n = text.chars().count();
                    if n > 0 {
                        runs.push((rgb, n));
                    }
                }
            }
            self.hl.push(runs);
        }
    }

    fn text(&self) -> String {
        self.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }
    fn save(&mut self, distro: &str) -> bool {
        let ok = if let Some(p) = &self.win_path {
            std::fs::write(p, self.text()).is_ok()
        } else {
            wslfiles::write_text(distro, &self.path, &self.text())
        };
        if ok {
            self.dirty = false;
        }
        ok
    }
    fn clamp_cx(&mut self) {
        let len = self.lines.get(self.cy).map(|l| l.len()).unwrap_or(0);
        if self.cx > len {
            self.cx = len;
        }
    }
    fn insert_char(&mut self, c: char) {
        if self.readonly {
            return;
        }
        let line = &mut self.lines[self.cy];
        let i = self.cx.min(line.len());
        line.insert(i, c);
        self.cx = i + 1;
        self.dirty = true;
    }
    fn newline(&mut self) {
        if self.readonly {
            return;
        }
        let at = self.cx.min(self.lines[self.cy].len());
        let tail: Vec<char> = self.lines[self.cy].split_off(at);
        self.lines.insert(self.cy + 1, tail);
        self.cy += 1;
        self.cx = 0;
        self.dirty = true;
    }
    fn backspace(&mut self) {
        if self.readonly {
            return;
        }
        if self.cx > 0 {
            self.lines[self.cy].remove(self.cx - 1);
            self.cx -= 1;
        } else if self.cy > 0 {
            let cur = self.lines.remove(self.cy);
            self.cy -= 1;
            self.cx = self.lines[self.cy].len();
            self.lines[self.cy].extend(cur);
        }
        self.dirty = true;
    }
    fn move_cursor(&mut self, dr: i32, dc: i32) {
        if dr != 0 {
            self.cy = (self.cy as i32 + dr).clamp(0, self.lines.len() as i32 - 1) as usize;
            self.clamp_cx();
        }
        if dc != 0 {
            let len = self.lines[self.cy].len();
            if dc < 0 && self.cx == 0 && self.cy > 0 {
                self.cy -= 1;
                self.cx = self.lines[self.cy].len();
            } else if dc > 0 && self.cx >= len && self.cy + 1 < self.lines.len() {
                self.cy += 1;
                self.cx = 0;
            } else {
                self.cx = (self.cx as i32 + dc).clamp(0, len as i32) as usize;
            }
        }
    }
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    win: Option<Rc<Window>>,
    gpu: Option<Gpu>,          // GPU present path (DirectComposition); preferred
    layered: Option<Layered>,  // CPU fallback (UpdateLayeredWindow)
    gpu_text: bool,            // draw terminal glyphs via DirectWrite (GPU path)
    term_glyphs: Vec<GlyphDraw>, // terminal glyphs collected this frame for the GPU
    fb: Vec<u32>, // ARGB framebuffer (alpha in high byte)

    // Frame pacing: coalesce feed-driven redraws to the monitor refresh so heavy
    // output (termbench) presents smoothly instead of as fast as the loop spins.
    want_frame: bool,
    last_frame: Instant,
    frame_interval: std::time::Duration,
    // Custom maximize (avoids Windows' WS_MAXIMIZE phantom border, which a
    // layered window can't paint -> white strips). Resize to the work area.
    maximized: bool,
    restore_rect: Option<(i32, i32, u32, u32)>,

    font: FontVec,
    fallback: Vec<FontVec>, // for glyphs the primary font lacks (CJK, Cyrillic, symbols)
    font_family: String,
    font_path: Option<std::path::PathBuf>, // resolved font file (shared by CPU + GPU text)
    scale: f32,
    font_pts: f32,
    font_pts_base: f32,
    font_px: f32,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    theme: Theme,
    opacity: f32,
    editor: String, // command to open files (in a new terminal tab)
    settings_session: Option<u32>, // the edit.exe tab editing settings.json; reload on its exit
    background: BackgroundImage,

    mux: Option<WslMux>,
    registry: Registry,
    outbox: Outbox,
    redraw_pending: Arc<AtomicBool>,

    tabs: Vec<Tab>,
    active_term: usize, // index into `tabs` of the current terminal tab
    docs: Vec<Doc>,     // open file/editor tabs
    active_doc: Option<usize>, // Some(i) => doc tab is foreground; None => terminal
    start_dir: Option<String>, // cwd for the first tab (from --cd)

    mods: ModifiersState,
    cursor_px: (f64, f64),
    grid: Vec<Vec<Cell>>,
    glyph_cache: HashMap<char, Option<Glyph>>,

    // Hit-test caches (device px), recomputed each render.
    chip_ranges: Vec<(f32, f32)>,
    chip_targets: Vec<ChipTarget>, // what each chip selects (parallel to chip_ranges)
    plus_range: (f32, f32),
    sidebar_btn: (f32, f32),  // sidebar toggle button x-range
    win_btns: [(f32, f32); 3], // minimize / maximize / close x-ranges
    pane_rects: Vec<(u32, Rect)>, // active tab's leaf rects
    scrollbars: Vec<ScrollBar>,   // active tab's per-pane scrollbar geometry
    scrollbar_drag: Option<(u32, f64)>, // (session, grab offset within thumb, px)
    sb_hover: Option<u32>, // session whose scrollbar the cursor is over (hover-expands the bar)
    hover_url: Option<HoverUrl>, // hyperlink under the cursor while Ctrl is held
    search: Option<Search>, // scrollback search overlay (Ctrl+Shift+F)

    // Mouse reporting: when a focused app enables DEC mouse tracking, button/
    // motion/wheel events are encoded and sent to the PTY instead of driving
    // local selection/scroll. `mouse_held` is the button (0/1/2) currently down
    // in a report, `mouse_session` the pane it was pressed in (so drags keep
    // reporting there), and `mouse_cell` the last reported cell (motion throttle).
    mouse_held: Option<u8>,
    mouse_session: Option<u32>,
    mouse_cell: (i64, i64),

    // File sidebar.
    sidebar_open: bool,
    sidebar_width: usize,
    sidebar_resizing: bool,
    show_hidden: bool,
    sidebar_dir: String, // directory currently shown (may be browsed, != shell cwd)
    last_sidebar_cwd: String, // last shell cwd we followed; only re-follow when it changes
    sidebar_entries: Vec<wslfiles::Entry>,
    sidebar_scroll: usize,
    sidebar_menu: Option<SidebarMenu>, // right-click context menu
}

/// What a tab-bar chip selects.
#[derive(Clone, Copy)]
enum ChipTarget {
    Term(usize),
    Doc(usize),
}

/// Right-click context menu over a sidebar entry.
struct SidebarMenu {
    entry: usize,             // index into sidebar_entries
    items: Vec<&'static str>, // menu labels
    rect: Rect,               // popup rect (device px)
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>, start_dir: Option<String>) -> App {
        let cfg = Settings::load();
        let font = load_monospace_font(&cfg.font_family)
            .expect("no monospace font found (Consolas/Cascadia)");
        let mut app = App {
            proxy,
            win: None,
            gpu: None,
            layered: None,
            gpu_text: false,
            term_glyphs: Vec::new(),
            fb: Vec::new(),
            want_frame: false,
            last_frame: Instant::now(),
            frame_interval: std::time::Duration::from_millis(16),
            maximized: false,
            restore_rect: None,
            font,
            fallback: load_fallback_fonts(),
            font_path: font_file_path(&cfg.font_family),
            font_family: cfg.font_family.clone(),
            scale: 1.0,
            font_pts: cfg.font_pts,
            font_pts_base: cfg.font_pts,
            font_px: cfg.font_pts * PT_TO_PX,
            cell_w: 1,
            cell_h: 1,
            ascent: 0.0,
            theme: cfg.theme,
            opacity: parse_opacity_env().unwrap_or(cfg.opacity),
            editor: cfg.editor,
            settings_session: None,
            background: BackgroundImage::load(cfg.background),
            mux: None,
            registry: Arc::new(Mutex::new(HashMap::new())),
            outbox: Arc::new(Mutex::new(Vec::new())),
            redraw_pending: Arc::new(AtomicBool::new(false)),
            tabs: Vec::new(),
            active_term: 0,
            docs: Vec::new(),
            active_doc: None,
            start_dir,
            mods: ModifiersState::empty(),
            cursor_px: (0.0, 0.0),
            grid: Vec::new(),
            glyph_cache: HashMap::new(),
            chip_ranges: Vec::new(),
            chip_targets: Vec::new(),
            plus_range: (0.0, 0.0),
            sidebar_btn: (0.0, 0.0),
            win_btns: [(0.0, 0.0); 3],
            pane_rects: Vec::new(),
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            sb_hover: None,
            hover_url: None,
            search: None,
            mouse_held: None,
            mouse_session: None,
            mouse_cell: (-1, -1),
            sidebar_open: false,
            sidebar_width: 0,
            sidebar_resizing: false,
            show_hidden: false,
            sidebar_dir: String::new(),
            last_sidebar_cwd: String::new(),
            sidebar_entries: Vec::new(),
            sidebar_scroll: 0,
            sidebar_menu: None,
        };
        app.recompute_metrics();
        app
    }

    /// Sidebar width in device px (0 when closed).
    fn sidebar_w(&self) -> usize {
        if self.sidebar_open {
            let w = if self.sidebar_width == 0 {
                self.default_sidebar_w()
            } else {
                self.sidebar_width
            };
            self.clamp_sidebar_w(w)
        } else {
            0
        }
    }

    fn default_sidebar_w(&self) -> usize {
        (self.cell_w * SIDEBAR_DEFAULT_COLS).clamp(SIDEBAR_MIN_W, 420)
    }

    fn max_sidebar_w(&self) -> usize {
        let reserve = (self.cell_w * 20).max(240);
        self.win
            .as_ref()
            .map(|w| (w.inner_size().width as usize).saturating_sub(reserve).max(1))
            .unwrap_or(840)
    }

    fn clamp_sidebar_w(&self, w: usize) -> usize {
        let max = self.max_sidebar_w();
        let min = SIDEBAR_MIN_W.min(max);
        w.clamp(min, max)
    }

    fn sidebar_resize_handle_w(&self) -> f64 {
        (6.0 * self.scale as f64).round().max(4.0)
    }

    fn sidebar_resize_hit(&self, px: f64, py: f64) -> bool {
        let sb = self.sidebar_w();
        if sb == 0 || py < self.tab_bar_h() as f64 {
            return false;
        }
        let h = self.sidebar_resize_handle_w();
        px >= sb as f64 - h && px < sb as f64 + h
    }

    fn resize_sidebar_to_cursor(&mut self) {
        self.sidebar_width = self.clamp_sidebar_w(self.cursor_px.0.round().max(0.0) as usize);
        self.reflow();
        self.request_redraw();
    }

    /// List an explicit directory into the sidebar (browsing). This does NOT
    /// touch the shell's working directory — the panel is an independent file
    /// browser. No-op when the sidebar is closed.
    fn list_sidebar_dir(&mut self, dir: String) {
        if !self.sidebar_open {
            return;
        }
        let dir = if dir.is_empty() { "/".to_string() } else { dir };
        let mut entries = wslfiles::list(DISTRO, &dir, self.show_hidden);
        // Prepend a ".." parent entry (unless at root), like the C# sidebar.
        if dir != "/" {
            let trimmed = dir.trim_end_matches('/');
            let parent = match trimmed.rsplit_once('/') {
                Some((p, _)) if !p.is_empty() => p.to_string(),
                _ => "/".to_string(),
            };
            entries.insert(0, wslfiles::Entry { name: "..".into(), linux_path: parent, is_dir: true });
        }
        self.sidebar_entries = entries;
        self.sidebar_dir = dir;
        self.sidebar_scroll = 0;
    }

    /// Re-list the directory the sidebar is currently showing (e.g. after
    /// toggling hidden files). Falls back to the shell cwd if nothing is shown.
    fn refresh_sidebar(&mut self) {
        let dir = if self.sidebar_dir.is_empty() {
            self.focused_cwd()
        } else {
            self.sidebar_dir.clone()
        };
        self.list_sidebar_dir(dir);
    }

    /// Edit settings.json (Ctrl+,) with Windows `edit.exe` in a new terminal tab
    /// (via WSL interop). Creates the file with current values if missing; settings
    /// are reloaded + applied when the editor tab closes (see `close_session`).
    fn open_settings(&mut self) {
        let path = match Settings::path() {
            Some(p) => p,
            None => return,
        };
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, self.settings_json());
        }
        // Open the session in the file's dir (as a /mnt path) so interop hands
        // edit.exe the translated Windows cwd; it then opens the relative filename.
        let dir = path
            .parent()
            .map(|p| bootstrap::windows_to_wsl_path(&p.to_string_lossy()))
            .unwrap_or_default();
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("settings.json");
        let esc = file.replace('\'', "'\\''");
        let cmd = format!("'/mnt/c/WINDOWS/system32/edit.exe' '{esc}'");
        self.settings_session = self.open_editor_tab(&dir, &cmd);
    }

    /// Serialize the current appearance to the settings.json schema.
    fn settings_json(&self) -> String {
        let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
        let hx = |c: u32| format!("\"#{:06X}\"", c & 0xFF_FFFF);
        let ansi: Vec<String> = self.theme.ansi.iter().map(|c| hx(*c)).collect();
        let bg = self.background.config();
        let bg_path = bg
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!(
            "{{\n  \"FontFamily\": {},\n  \"FontSize\": {},\n  \"Background\": {},\n  \"Foreground\": {},\n  \"Cursor\": {},\n  \"Selection\": {},\n  \"Opacity\": {},\n  \"Editor\": {},\n  \"BackgroundImage\": {},\n  \"BackgroundImageOpacity\": {},\n  \"BackgroundImageFit\": {},\n  \"Ansi\": [{}]\n}}\n",
            js(&self.font_family),
            self.font_pts_base,
            hx(self.theme.bg),
            hx(self.theme.fg),
            hx(self.theme.cursor),
            hx(self.theme.selection),
            (self.opacity * 100.0).round() as u32,
            js(&self.editor),
            js(&bg_path),
            (bg.opacity * 100.0).round() as u32,
            js(bg.fit.as_str()),
            ansi.join(", ")
        )
    }

    /// Re-load settings.json and apply colors/opacity/font live.
    fn apply_settings(&mut self) {
        let cfg = Settings::load();
        self.theme = cfg.theme;
        self.opacity = cfg.opacity;
        self.editor = cfg.editor;
        self.background = BackgroundImage::load(cfg.background);
        self.font_pts = cfg.font_pts;
        self.font_pts_base = cfg.font_pts;
        if cfg.font_family != self.font_family {
            if let Some(f) = load_monospace_font(&cfg.font_family) {
                self.font = f;
            }
            self.font_path = font_file_path(&cfg.font_family);
            self.font_family = cfg.font_family;
        }
        self.recompute_metrics();
        self.sync_gpu_font();
        self.reflow();
        self.request_redraw();
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_open = !self.sidebar_open;
        if self.sidebar_open {
            if self.sidebar_width == 0 {
                self.sidebar_width = self.default_sidebar_w();
            }
            // Open at the shell's current directory and sync the follow baseline.
            let cwd = self.focused_cwd();
            self.last_sidebar_cwd = cwd.clone();
            self.list_sidebar_dir(cwd);
        }
        self.reflow(); // terminal area width changed
        self.request_redraw();
    }

    fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh_sidebar(); // re-list the directory currently shown
        self.request_redraw();
    }

    /// Sidebar entry index under the mouse, if any.
    fn sidebar_entry_at_cursor(&self) -> Option<usize> {
        let bar = self.tab_bar_h();
        if (self.cursor_px.1 as usize) < bar {
            return None;
        }
        let idx = (self.cursor_px.1 as usize - bar) / self.cell_h + self.sidebar_scroll;
        (idx < self.sidebar_entries.len()).then_some(idx)
    }

    /// Click in the sidebar: cd into a directory (shell follows) or open a file.
    fn sidebar_click(&mut self) {
        let idx = match self.sidebar_entry_at_cursor() {
            Some(i) => i,
            None => return,
        };
        let (is_dir, lp) = {
            let e = &self.sidebar_entries[idx];
            (e.is_dir, e.linux_path.clone())
        };
        if is_dir {
            // Browse into the folder in the panel only — do NOT cd the shell.
            // The panel follows the shell's cwd only on a manual `cd`.
            self.list_sidebar_dir(lp);
        } else {
            // Open the file in the configured editor in a new terminal tab.
            self.open_in_editor(&lp);
        }
        self.request_redraw();
    }

    /// Open a right-click context menu over the sidebar entry under the cursor.
    fn open_sidebar_menu(&mut self) {
        let idx = match self.sidebar_entry_at_cursor() {
            Some(i) => i,
            None => {
                self.sidebar_menu = None;
                return;
            }
        };
        let is_dir = self.sidebar_entries[idx].is_dir;
        let items: Vec<&'static str> = if is_dir {
            vec!["Open in new window", "Open"]
        } else {
            vec!["Open", "Insert path at prompt"]
        };
        let pad = (6.0 * self.scale).round().max(2.0) as usize;
        let mw = items.iter().map(|s| s.len()).max().unwrap_or(8) * self.cell_w + pad * 2;
        let line_h = self.cell_h + 4;
        let rect = Rect {
            x: self.cursor_px.0 as usize,
            y: self.cursor_px.1 as usize,
            w: mw,
            h: items.len() * line_h + 4,
        };
        self.sidebar_menu = Some(SidebarMenu { entry: idx, items, rect });
        self.request_redraw();
    }

    /// Handle a click while the sidebar context menu is open. Returns true if the
    /// click was consumed (menu was open).
    fn menu_click(&mut self) -> bool {
        let menu = match self.sidebar_menu.take() {
            Some(m) => m,
            None => return false,
        };
        self.request_redraw();
        let (mx, my) = self.cursor_px;
        if !menu.rect.contains(mx, my) {
            return true; // clicked outside: dismissed
        }
        let line_h = self.cell_h + 4;
        let row = (my as usize - menu.rect.y).saturating_sub(2) / line_h;
        let item = match menu.items.get(row) {
            Some(s) => *s,
            None => return true,
        };
        let (is_dir, lp) = match self.sidebar_entries.get(menu.entry) {
            Some(e) => (e.is_dir, e.linux_path.clone()),
            None => return true,
        };
        match item {
            "Open in new window" => spawn_window(Some(lp)),
            "Open" => {
                if is_dir {
                    self.list_sidebar_dir(lp); // browse in the panel; don't cd the shell
                } else {
                    self.open_in_editor(&lp); // open in the configured editor (new tab)
                }
            }
            "Insert path at prompt" => {
                let quoted = format!("'{}'", lp.replace('\'', "'\\''"));
                self.send(quoted.as_bytes());
            }
            _ => {}
        }
        true
    }

    /// Editor key handling while a document is open.
    fn doc_key(&mut self, ev: &KeyEvent) {
        let ctrl = self.mods.control_key();
        let mut save = false;
        let mut edited = false;
        if let Some(doc) = self.cur_doc_mut() {
            if let PhysicalKey::Code(code) = ev.physical_key {
                match code {
                    KeyCode::KeyS if ctrl => save = true,
                    KeyCode::ArrowUp => doc.move_cursor(-1, 0),
                    KeyCode::ArrowDown => doc.move_cursor(1, 0),
                    KeyCode::ArrowLeft => doc.move_cursor(0, -1),
                    KeyCode::ArrowRight => doc.move_cursor(0, 1),
                    KeyCode::Home => doc.cx = 0,
                    KeyCode::End => doc.cx = doc.lines[doc.cy].len(),
                    KeyCode::PageUp => doc.move_cursor(-20, 0),
                    KeyCode::PageDown => doc.move_cursor(20, 0),
                    KeyCode::Backspace => {
                        doc.backspace();
                        edited = true;
                    }
                    KeyCode::Enter | KeyCode::NumpadEnter => {
                        doc.newline();
                        edited = true;
                    }
                    KeyCode::Tab => {
                        for _ in 0..4 {
                            doc.insert_char(' ');
                        }
                        edited = true;
                    }
                    _ => {
                        if let Some(text) = &ev.text {
                            for c in text.chars() {
                                if !c.is_control() {
                                    doc.insert_char(c);
                                    edited = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        if edited {
            if let Some(d) = self.cur_doc_mut() {
                d.rehighlight();
            }
        }
        let mut reload_settings = false;
        if save {
            if let Some(d) = self.cur_doc_mut() {
                d.save(DISTRO);
                reload_settings = d.is_settings;
            }
        }
        if reload_settings {
            self.apply_settings();
        }
        self.doc_scroll_to_cursor();
        self.request_redraw();
    }

    fn doc_scroll_to_cursor(&mut self) {
        let bar = self.tab_bar_h();
        let ch = self.cell_h;
        let wh = self.win.as_ref().map(|w| w.inner_size().height as usize).unwrap_or(0);
        let rows = (wh.saturating_sub(bar + ch) / ch).max(1);
        if let Some(doc) = self.cur_doc_mut() {
            if doc.cy < doc.scroll {
                doc.scroll = doc.cy;
            } else if doc.cy >= doc.scroll + rows {
                doc.scroll = doc.cy + 1 - rows;
            }
        }
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

    /// Grid dimensions for the terminal area (below the tab bar, right of sidebar).
    fn grid_dims(&self, w: u32, h: u32) -> (usize, usize) {
        let area = self.terminal_area(w, h);
        let (text_w, ..) = sb_layout(area, w as usize, self.scale, self.cell_w);
        let cols = (text_w / self.cell_w).max(1);
        let rows = (area.h / self.cell_h).max(1);
        (cols, rows)
    }

    // ---- focused-pane accessors ----------------------------------------
    fn focused_session(&self) -> u32 {
        let tab = &self.tabs[self.active_term];
        if tab.root.find(tab.focus).is_some() {
            tab.focus
        } else {
            tab.root.first_session()
        }
    }
    fn focused(&self) -> &Pane {
        let s = self.focused_session();
        self.tabs[self.active_term].root.find(s).expect("focused pane")
    }
    fn focused_term(&self) -> Arc<Mutex<Terminal>> {
        self.focused().term.clone()
    }
    fn pane(&self, session: u32) -> Option<&Pane> {
        self.tabs.get(self.active_term).and_then(|t| t.root.find(session))
    }
    fn pane_mut(&mut self, session: u32) -> Option<&mut Pane> {
        let active = self.active_term;
        self.tabs.get_mut(active).and_then(|t| t.root.find_mut(session))
    }

    // ---- document (editor) tabs ----------------------------------------
    fn cur_doc_mut(&mut self) -> Option<&mut Doc> {
        match self.active_doc {
            Some(i) => self.docs.get_mut(i),
            None => None,
        }
    }
    /// Close the active document tab (back to the terminal view).
    fn close_active_doc(&mut self) {
        if let Some(i) = self.active_doc.take() {
            if i < self.docs.len() {
                self.docs.remove(i);
            }
            self.request_redraw();
        }
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
        let sb = self.sidebar_w();
        // Reserve a few px at the very bottom so the last row isn't flush against
        // the window edge — easier to read, especially when maximized/fullscreen.
        let bottom_pad = (4.0 * self.scale).round().max(3.0) as usize;
        Rect {
            x: sb,
            y: bar,
            w: (w as usize).saturating_sub(sb),
            h: (h as usize).saturating_sub(bar).saturating_sub(bottom_pad),
        }
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
            .get(self.active_term)
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
        self.active_term = self.tabs.len() - 1;
        self.active_doc = None;
        self.reflow();
        self.request_redraw();
    }

    /// Open a new terminal tab whose session immediately `exec`s `exec_cmd` (in
    /// `cwd`). `exec` replaces the shell, so quitting the program ends the session
    /// and the tab closes itself. Returns the session id.
    fn open_editor_tab(&mut self, cwd: &str, exec_cmd: &str) -> Option<u32> {
        let (cols, rows) = match &self.win {
            Some(w) => {
                let s = w.inner_size();
                self.grid_dims(s.width, s.height)
            }
            None => (80, 24),
        };
        let (session, term) = self.open_session(cols, rows, cwd)?;
        let cmd = format!("exec {exec_cmd}\r");
        if let Some(mux) = &self.mux {
            mux.send_data(session, cmd.as_bytes());
        }
        self.tabs.push(Tab { root: Layout::Leaf(Pane::new(session, term)), focus: session });
        self.active_term = self.tabs.len() - 1;
        self.active_doc = None;
        self.reflow();
        self.request_redraw();
        Some(session)
    }

    /// Open `linux_path` in the configured editor in a new terminal tab (in the
    /// file's directory).
    fn open_in_editor(&mut self, linux_path: &str) {
        let dir = match linux_path.rsplit_once('/') {
            Some((p, _)) if !p.is_empty() => p.to_string(),
            _ => String::new(),
        };
        let esc = linux_path.replace('\'', "'\\''");
        let editor = self.editor.clone();
        self.open_editor_tab(&dir, &format!("{editor} '{esc}'"));
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
        let tab = &mut self.tabs[self.active_term];
        let root = std::mem::replace(&mut tab.root, Layout::Empty);
        tab.root = split_layout(root, target, side_by_side, &mut newp);
        tab.focus = session;
        self.reflow();
        self.request_redraw();
    }

    /// Resize the focused pane (Windows-Terminal-style Alt+Shift+Arrow): move the
    /// nearest divider of the given orientation in the arrow direction (works
    /// whichever side the focused pane is on). `want_sbs` is the orientation
    /// (columns vs rows); `positive` is Right/Down (+ratio) vs Left/Up (−ratio).
    fn resize_focused(&mut self, want_sbs: bool, positive: bool) {
        const STEP: f32 = 0.04;
        let focus = self.focused_session();
        let delta = if positive { STEP } else { -STEP };
        let tab = &mut self.tabs[self.active_term];
        let (_, adjusted) = resize_split(&mut tab.root, focus, want_sbs, delta);
        if adjusted {
            self.reflow();
            self.request_redraw();
        }
    }

    /// Close a session's pane (collapsing its split); `exited` skips re-closing
    /// the PTY. Closes the tab when its last pane goes, and exits on the last tab.
    fn close_session(&mut self, session: u32, exited: bool, event_loop: &ActiveEventLoop) {
        // The settings editor (edit.exe) tab closed -> reload + apply settings.
        if self.settings_session == Some(session) {
            self.settings_session = None;
            self.apply_settings();
        }
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
                if self.active_term >= self.tabs.len() {
                    self.active_term = self.tabs.len() - 1;
                }
            }
        }
        self.reflow();
        self.request_redraw();
    }

    /// Custom maximize: resize the (borderless) window to the monitor work area,
    /// restoring the previous rect on toggle. Avoids WS_MAXIMIZE, whose phantom
    /// border a layered window can't paint (white strips).
    fn toggle_maximize(&mut self) {
        let win = match &self.win {
            Some(w) => w.clone(),
            None => return,
        };
        if self.maximized {
            if let Some((x, y, w, h)) = self.restore_rect.take() {
                let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
                win.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
            self.maximized = false;
        } else {
            if let (Ok(p), s) = (win.outer_position(), win.inner_size()) {
                self.restore_rect = Some((p.x, p.y, s.width, s.height));
            }
            if let Some((x, y, w, h)) = hwnd_of(&win).and_then(work_area) {
                win.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
                let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
            }
            self.maximized = true;
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
        if self.active_term >= self.tabs.len() {
            self.active_term = self.tabs.len() - 1;
        }
        self.reflow();
        self.request_redraw();
    }

    /// Cycle through the combined tab strip: terminal tabs then document tabs.
    fn switch_tab(&mut self, delta: i32) {
        let nterm = self.tabs.len();
        let total = nterm + self.docs.len();
        if total == 0 {
            return;
        }
        let cur = match self.active_doc {
            Some(d) => nterm + d,
            None => self.active_term.min(nterm.saturating_sub(1)),
        };
        let next = (cur as i32 + delta).rem_euclid(total as i32) as usize;
        if next < nterm {
            self.active_term = next;
            self.active_doc = None;
        } else {
            self.active_doc = Some(next - nterm);
        }
        self.request_redraw();
    }

    fn handle_key(&mut self, ev: &KeyEvent, event_loop: &ActiveEventLoop) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key();
        let shift = self.mods.shift_key();
        let alt = self.mods.alt_key();

        // While the search overlay is open it captures all key input.
        if self.search.is_some() {
            self.search_key(ev, shift);
            return;
        }

        if let PhysicalKey::Code(code) = ev.physical_key {
            match code {
                KeyCode::KeyF if ctrl && shift => {
                    self.open_search();
                    return;
                }
                // Alt+Shift +/- split the focused pane (columns / rows).
                KeyCode::Equal | KeyCode::NumpadAdd if alt && shift => {
                    self.split_focused(true);
                    return;
                }
                KeyCode::Minus | KeyCode::NumpadSubtract if alt && shift => {
                    self.split_focused(false);
                    return;
                }
                // Alt+Shift+Arrow resizes the focused pane (grow toward the arrow).
                KeyCode::ArrowRight if alt && shift => {
                    self.resize_focused(true, true);
                    return;
                }
                KeyCode::ArrowLeft if alt && shift => {
                    self.resize_focused(true, false);
                    return;
                }
                KeyCode::ArrowDown if alt && shift => {
                    self.resize_focused(false, true);
                    return;
                }
                KeyCode::ArrowUp if alt && shift => {
                    self.resize_focused(false, false);
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
                KeyCode::KeyE if ctrl && shift => {
                    self.toggle_sidebar();
                    return;
                }
                KeyCode::KeyH if ctrl && shift => {
                    self.toggle_hidden();
                    return;
                }
                KeyCode::KeyW if ctrl && shift => {
                    if self.active_doc.is_some() {
                        self.close_active_doc();
                    } else {
                        let s = self.focused_session();
                        self.close_session(s, false, event_loop);
                    }
                    return;
                }
                KeyCode::Tab if ctrl => {
                    self.switch_tab(if shift { -1 } else { 1 });
                    return;
                }
                // Jump between shell prompts (needs OSC 133 shell integration).
                KeyCode::ArrowUp if ctrl && shift => {
                    self.jump_prompt(-1);
                    return;
                }
                KeyCode::ArrowDown if ctrl && shift => {
                    self.jump_prompt(1);
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
                KeyCode::Comma if ctrl => {
                    self.open_settings();
                    return;
                }
                KeyCode::F11 => {
                    self.toggle_maximize();
                    return;
                }
                // Shift+PageUp/Down scroll the local scrollback by a screen
                // (plain PageUp/Down still go to the app, e.g. less/vim).
                KeyCode::PageUp if shift => {
                    let r = self.focused_term().lock().unwrap().rows() as i32;
                    self.scroll_by(r);
                    return;
                }
                KeyCode::PageDown if shift => {
                    let r = self.focused_term().lock().unwrap().rows() as i32;
                    self.scroll_by(-r);
                    return;
                }
                _ => {}
            }
        }

        // Super/Win combos are OS-level (e.g. Win+Shift+S screen capture): don't
        // forward them to the PTY/editor and don't snap the view to the bottom —
        // just let the OS hotkey through. Super is never sent to the terminal.
        if self.mods.super_key() {
            return;
        }

        // While the editor overlay is open it takes all remaining input.
        if self.active_doc.is_some() {
            self.doc_key(ev);
            return;
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
    fn scrollbar_hit(&self, px: f64, py: f64) -> Option<ScrollBar> {
        self.scrollbars.iter().find(|s| s.track.contains(px, py)).copied()
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

    /// The hyperlink under the cursor, if any (used for Ctrl-hover/Ctrl-click).
    fn url_under_cursor(&self) -> Option<HoverUrl> {
        let session = self.pane_at_cursor()?;
        let rect = self.rect_of(session)?;
        let (row, col) = self.cell_in(session, rect);
        let term = self.pane(session)?.term.clone();
        let (c0, c1, url) = term.lock().unwrap().url_at(row, col)?;
        Some(HoverUrl { session, row, c0: c0 as i64, c1: c1 as i64, url })
    }

    // ---- mouse reporting (DEC mouse tracking) --------------------------
    /// The mouse mode + SGR-encoding flag a pane's app has enabled.
    fn mouse_mode(&self, session: u32) -> (MouseTracking, bool) {
        self.pane(session)
            .map(|p| {
                let t = p.term.lock().unwrap();
                (t.mouse(), t.mouse_sgr())
            })
            .unwrap_or((MouseTracking::None, false))
    }

    /// xterm modifier bits for a mouse report: shift 4 / alt 8 / ctrl 16.
    fn mouse_mod_bits(&self) -> u32 {
        let mut m = 0;
        if self.mods.shift_key() {
            m += 4;
        }
        if self.mods.alt_key() {
            m += 8;
        }
        if self.mods.control_key() {
            m += 16;
        }
        m
    }

    /// 0-based viewport (col, row) of the cursor within `rect`, clamped to the
    /// pane's grid — what a mouse report needs (not absolute scrollback rows).
    fn mouse_cell_at(&self, session: u32, rect: Rect) -> (i64, i64) {
        let col = ((self.cursor_px.0 - rect.x as f64) / self.cell_w as f64).floor() as i64;
        let row = ((self.cursor_px.1 - rect.y as f64) / self.cell_h as f64).floor() as i64;
        let (cols, rows) = match self.pane(session) {
            Some(p) => {
                let t = p.term.lock().unwrap();
                (t.cols() as i64, t.rows() as i64)
            }
            None => return (0, 0),
        };
        (col.clamp(0, (cols - 1).max(0)), row.clamp(0, (rows - 1).max(0)))
    }

    /// Encode one mouse report (SGR 1006 or legacy X10/normal) and send it to the
    /// PTY. `cb` is the button code with motion (32) / wheel (64) bits and the
    /// modifier bits already folded in (but not the legacy +32 char offset, nor
    /// the X10 release low-bits). `col`/`row` are 0-based viewport cells.
    fn send_mouse(&self, session: u32, sgr: bool, cb: u32, col: i64, row: i64, release: bool) {
        let x = col.max(0) + 1;
        let y = row.max(0) + 1;
        let bytes: Vec<u8> = if sgr {
            let term = if release { 'm' } else { 'M' };
            format!("\x1b[<{cb};{x};{y}{term}").into_bytes()
        } else {
            // ESC [ M  Cb Cx Cy, each a byte = value + 32. A release reports the
            // generic "button 3" in the low two bits; legacy coords cap at 223.
            let b = if release { (cb & !0b11) | 3 } else { cb };
            let enc = |v: i64| (32 + v).clamp(32, 255) as u8;
            vec![0x1b, b'[', b'M', (32 + b).min(255) as u8, enc(x), enc(y)]
        };
        if let Some(mux) = &self.mux {
            mux.send_data(session, &bytes);
        }
    }

    /// Report a button press (0=left/1=middle/2=right) to a tracking app under
    /// the cursor. Holding Shift forces local selection (xterm convention).
    /// Returns true if the event was reported (caller skips local handling).
    fn report_mouse_press(&mut self, btn: u8) -> bool {
        let session = match self.pane_at_cursor() {
            Some(s) => s,
            None => return false,
        };
        let (mode, sgr) = self.mouse_mode(session);
        if mode == MouseTracking::None || self.mods.shift_key() {
            return false;
        }
        self.tabs[self.active_term].focus = session; // a click still focuses the pane
        if let Some(rect) = self.rect_of(session) {
            let (col, row) = self.mouse_cell_at(session, rect);
            let cb = btn as u32 + self.mouse_mod_bits();
            self.send_mouse(session, sgr, cb, col, row, false);
            self.mouse_held = Some(btn);
            self.mouse_session = Some(session);
            self.mouse_cell = (col, row);
        }
        true
    }

    /// Report a button release matching a prior press. Returns true if consumed.
    fn report_mouse_release(&mut self, btn: u8) -> bool {
        let session = match self.mouse_session {
            Some(s) => s,
            None => return false,
        };
        let (mode, sgr) = self.mouse_mode(session);
        // X10 (mode 9) and press-only apps never get a release report; still
        // consume it so no stray selection lingers.
        if matches!(mode, MouseTracking::Normal | MouseTracking::ButtonEvent | MouseTracking::AnyEvent)
        {
            if let Some(rect) = self.rect_of(session) {
                let (col, row) = self.mouse_cell_at(session, rect);
                let cb = btn as u32 + self.mouse_mod_bits();
                self.send_mouse(session, sgr, cb, col, row, true);
            }
        }
        self.mouse_held = None;
        self.mouse_session = None;
        true
    }

    /// Report pointer motion if the app wants it: AnyEvent (1003) reports every
    /// move; ButtonEvent (1002) only while a button is down. Throttled to one
    /// report per cell. Returns true if consumed.
    fn report_mouse_motion(&mut self) -> bool {
        let session = match self.pane_at_cursor() {
            Some(s) => s,
            None => return false,
        };
        let (mode, sgr) = self.mouse_mode(session);
        let want = match mode {
            MouseTracking::AnyEvent => true,
            MouseTracking::ButtonEvent => self.mouse_held.is_some(),
            _ => false,
        };
        if !want || self.mods.shift_key() {
            return false;
        }
        if let Some(rect) = self.rect_of(session) {
            let (col, row) = self.mouse_cell_at(session, rect);
            if (col, row) == self.mouse_cell {
                return true; // same cell — already reported
            }
            self.mouse_cell = (col, row);
            // Motion sets bit 32; a held button keeps its number, else "button 3".
            let cb = self.mouse_held.unwrap_or(3) as u32 + 32 + self.mouse_mod_bits();
            self.send_mouse(session, sgr, cb, col, row, false);
        }
        true
    }

    /// Report a wheel notch (`up`) to a tracking app under the cursor as button
    /// 64 (up) / 65 (down). Returns true if consumed.
    fn report_mouse_wheel(&mut self, up: bool, notches: u32) -> bool {
        let session = match self.pane_at_cursor() {
            Some(s) => s,
            None => return false,
        };
        let (mode, sgr) = self.mouse_mode(session);
        if mode == MouseTracking::None || self.mods.shift_key() {
            return false;
        }
        if let Some(rect) = self.rect_of(session) {
            let (col, row) = self.mouse_cell_at(session, rect);
            let cb = (if up { 64 } else { 65 }) + self.mouse_mod_bits();
            for _ in 0..notches.max(1) {
                self.send_mouse(session, sgr, cb, col, row, false);
            }
        }
        true
    }

    fn begin_selection(&mut self) {
        let session = match self.pane_at_cursor() {
            Some(s) => s,
            None => return,
        };
        self.tabs[self.active_term].focus = session; // click focuses the pane
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
        self.sync_gpu_font();
        self.reflow();
        self.request_redraw();
    }

    fn reset_font(&mut self) {
        self.font_pts = self.font_pts_base;
        self.recompute_metrics();
        self.sync_gpu_font();
        self.reflow();
        self.request_redraw();
    }

    /// Push the current font family + cell metrics to the GPU text renderer
    /// (no-op on the CPU/layered path).
    fn sync_gpu_font(&mut self) {
        if let Some(g) = &mut self.gpu {
            g.set_font(
                &self.font_family,
                self.font_px,
                self.cell_w as f32,
                self.cell_h as f32,
                self.font_path.as_deref(),
            );
        }
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

    // ---- scrollback search (Ctrl+Shift+F) ------------------------------
    fn open_search(&mut self) {
        let session = self.focused_session();
        self.search = Some(Search { query: String::new(), matches: Vec::new(), current: 0, session });
        self.request_redraw();
    }

    /// Route a key to the search overlay (it owns input while open).
    fn search_key(&mut self, ev: &KeyEvent, shift: bool) {
        if let PhysicalKey::Code(code) = ev.physical_key {
            match code {
                KeyCode::Escape => {
                    self.search = None;
                    self.request_redraw();
                    return;
                }
                KeyCode::Enter | KeyCode::NumpadEnter => {
                    self.search_step(if shift { -1 } else { 1 });
                    return;
                }
                KeyCode::ArrowDown => {
                    self.search_step(1);
                    return;
                }
                KeyCode::ArrowUp => {
                    self.search_step(-1);
                    return;
                }
                KeyCode::Backspace => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.pop();
                    }
                    self.search_run();
                    return;
                }
                _ => {}
            }
        }
        // Otherwise, append any typed (non-control) text to the query.
        if let Some(text) = &ev.text {
            let t: String = text.chars().filter(|c| !c.is_control()).collect();
            if !t.is_empty() {
                if let Some(s) = self.search.as_mut() {
                    s.query.push_str(&t);
                }
                self.search_run();
            }
        }
    }

    /// Re-run the query, point at the newest (bottom-most) match, and scroll to it.
    fn search_run(&mut self) {
        let (session, query) = match &self.search {
            Some(s) => (s.session, s.query.clone()),
            None => return,
        };
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            self.pane(session).map(|p| p.term.lock().unwrap().search(&query)).unwrap_or_default()
        };
        if let Some(s) = self.search.as_mut() {
            s.current = matches.len().saturating_sub(1);
            s.matches = matches;
        }
        self.scroll_to_match();
        self.request_redraw();
    }

    /// Move to the next (`+1`, newer) / previous (`-1`, older) match, wrapping.
    fn search_step(&mut self, dir: i32) {
        if let Some(s) = self.search.as_mut() {
            let n = s.matches.len();
            if n == 0 {
                return;
            }
            s.current = ((s.current.min(n - 1) as i32 + dir).rem_euclid(n as i32)) as usize;
        }
        self.scroll_to_match();
        self.request_redraw();
    }

    /// Scroll the searched pane so the current match is centered in the viewport.
    fn scroll_to_match(&mut self) {
        let (session, abs_row) = match &self.search {
            Some(s) if !s.matches.is_empty() => {
                (s.session, s.matches[s.current.min(s.matches.len() - 1)].0)
            }
            _ => return,
        };
        let (sbc, rows) = match self.pane(session) {
            Some(p) => {
                let t = p.term.lock().unwrap();
                (t.scrollback_count() as i64, t.rows() as i64)
            }
            None => return,
        };
        let off = (sbc - abs_row + rows / 2).clamp(0, sbc);
        if let Some(p) = self.pane_mut(session) {
            p.scroll_off = off as usize;
        }
    }

    /// Jump the focused pane to the previous (`-1`, older) / next (`+1`, newer)
    /// shell prompt (OSC 133 mark), placing it at the top of the viewport.
    fn jump_prompt(&mut self, dir: i32) {
        let s = self.focused_session();
        let (marks, sbc, off) = match self.pane(s) {
            Some(p) => {
                let t = p.term.lock().unwrap();
                (t.prompt_marks(), t.scrollback_count() as i64, p.scroll_off as i64)
            }
            None => return,
        };
        if marks.is_empty() {
            return;
        }
        let top_abs = sbc - off; // absolute row currently at the top of the view
        let rows_it = marks.iter().map(|(r, _)| *r);
        let target = if dir < 0 {
            rows_it.filter(|&r| r < top_abs).max()
        } else {
            rows_it.filter(|&r| r > top_abs).min()
        };
        if let Some(abs) = target {
            let new_off = (sbc - abs).clamp(0, sbc);
            if let Some(p) = self.pane_mut(s) {
                p.scroll_off = new_off as usize;
            }
            self.request_redraw();
        }
    }

    /// Show a resize cursor when hovering a window edge (borderless feedback).
    fn update_resize_cursor(&self) {
        let win = match &self.win {
            Some(w) => w,
            None => return,
        };
        let s = win.inner_size();
        let icon = if self.sidebar_resizing || self.sidebar_resize_hit(self.cursor_px.0, self.cursor_px.1) {
            CursorIcon::EwResize
        } else {
            match resize_dir_at(self.cursor_px.0, self.cursor_px.1, s.width, s.height, self.scale) {
                Some(ResizeDirection::North | ResizeDirection::South) => CursorIcon::NsResize,
                Some(ResizeDirection::East | ResizeDirection::West) => CursorIcon::EwResize,
                Some(ResizeDirection::NorthEast | ResizeDirection::SouthWest) => CursorIcon::NeswResize,
                Some(ResizeDirection::NorthWest | ResizeDirection::SouthEast) => CursorIcon::NwseResize,
                None => CursorIcon::Default,
            }
        };
        win.set_cursor(icon);
    }

    /// Resize the surface and every pane's PTY to match its rect.
    fn resize_surface(&mut self, w: u32, h: u32) {
        let area = self.terminal_area(w, h);
        let (cw, ch_px) = (self.cell_w, self.cell_h);
        for ti in 0..self.tabs.len() {
            let mut rects = Vec::new();
            self.tabs[ti].root.leaf_rects(area, &mut rects);
            for (session, rect) in rects {
                let (text_w, ..) = sb_layout(rect, w as usize, self.scale, cw);
                let cols = (text_w / cw).max(1);
                let rows = (rect.h / ch_px).max(1);
                if let Some(p) = self.tabs[ti].root.find(session) {
                    p.term.lock().unwrap().resize(cols, rows);
                }
                if let Some(mux) = &self.mux {
                    mux.send_resize(session, cols as u16, rows as u16);
                }
            }
        }
    }

    fn chip_at(&self, x: f32) -> Option<usize> {
        self.chip_ranges.iter().position(|&(x0, x1)| x >= x0 && x < x1)
    }

    fn tab_bar_click(&mut self) {
        let x = self.cursor_px.0 as f32;
        if x >= self.sidebar_btn.0 && x < self.sidebar_btn.1 {
            self.toggle_sidebar();
        } else if x >= self.plus_range.0 && x < self.plus_range.1 {
            self.add_tab();
        } else if let Some(i) = self.chip_at(x) {
            match self.chip_targets.get(i).copied() {
                Some(ChipTarget::Term(t)) => {
                    self.active_term = t;
                    self.active_doc = None;
                }
                Some(ChipTarget::Doc(d)) => self.active_doc = Some(d),
                None => {}
            }
            self.request_redraw();
        }
    }

    fn render(&mut self) {
        let win = match &self.win {
            Some(w) => w.clone(),
            None => return,
        };
        // All panes/tabs gone (e.g. the connection dropped under load and every
        // session was torn down) — the app is exiting. Skip this frame so we
        // don't index an empty `tabs`. Keep `active_term` in range defensively.
        if self.tabs.is_empty() {
            return;
        }
        if self.active_term >= self.tabs.len() {
            self.active_term = self.tabs.len() - 1;
        }
        let size = win.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let bar_h = self.tab_bar_h();
        let (cw, ch_px) = (self.cell_w, self.cell_h);
        let pad = (6.0 * self.scale).round().max(2.0) as usize;
        let text_top = ((bar_h.saturating_sub(ch_px)) / 2) as i32;

        // Build the combined chip list: terminal tabs, then document tabs.
        let mut chips: Vec<(String, ChipTarget, bool)> = Vec::new(); // (label, target, active)
        for (i, t) in self.tabs.iter().enumerate() {
            let s = t.root.first_session();
            let label = match t.root.find(if t.root.find(t.focus).is_some() { t.focus } else { s }) {
                Some(p) => {
                    let g = p.term.lock().unwrap();
                    g.title()
                        .map(str::to_string)
                        .or_else(|| g.current_directory().map(basename))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "wsl".into())
                }
                None => "wsl".into(),
            };
            let active = self.active_doc.is_none() && i == self.active_term;
            chips.push((label, ChipTarget::Term(i), active));
        }
        for (j, d) in self.docs.iter().enumerate() {
            let mut label = d.name.clone();
            if d.dirty {
                label.push('*');
            }
            chips.push((label, ChipTarget::Doc(j), self.active_doc == Some(j)));
        }

        // Lay out panes for the active tab (also used for mouse hit-testing).
        // terminal_area reserves the sidebar (left) and a small bottom margin.
        let sb = self.sidebar_w();
        let area = self.terminal_area(w, h);
        self.pane_rects.clear();
        self.scrollbars.clear();
        self.tabs[self.active_term].root.leaf_rects(area, &mut self.pane_rects);
        let focus = self.focused_session();
        let rects = self.pane_rects.clone();

        // Render into our ARGB framebuffer (alpha in the high byte). Everything
        // is opaque (0xFF) except terminal-pane backgrounds, which use the
        // configured opacity so only the terminal shows the desktop through it.
        let npx = (w as usize) * (h as usize);
        self.fb.clear();
        self.fb.resize(npx, OPAQUE | self.theme.bg);
        self.term_glyphs.clear(); // GPU path collects pane glyphs here this frame
        let buf: &mut [u32] = &mut self.fb;
        let op = (self.opacity.clamp(0.0, 1.0) * 255.0).round() as u32; // pane bg alpha
        let has_background = self.background.is_active();

        // --- tab bar -------------------------------------------------------
        let chrome = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.10);
        let chip_active = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.22);
        fill_rect(buf, w, h, 0, 0, w as usize, bar_h, chrome);

        self.chip_ranges.clear();
        self.chip_targets.clear();

        // Sidebar toggle button on the LEFT (like the C# app): a panel-with-
        // divider icon, highlighted when the sidebar is open.
        {
            let bw = bar_h;
            if self.sidebar_open {
                fill_rect(buf, w, h, 0, 2, bw, bar_h.saturating_sub(4), chip_active);
            }
            let fc = OPAQUE
                | if self.sidebar_open {
                    self.theme.selection
                } else {
                    mix(self.theme.bg, self.theme.fg, 0.7)
                };
            let s = (bar_h as f32 * 0.55) as usize;
            let ix = (bw - s) / 2;
            let iy = (bar_h - s) / 2;
            let t = (1.5 * self.scale).round().max(1.0) as usize;
            fill_rect(buf, w, h, ix, iy, s, t, fc);
            fill_rect(buf, w, h, ix, iy + s - t, s, t, fc);
            fill_rect(buf, w, h, ix, iy, t, s, fc);
            fill_rect(buf, w, h, ix + s - t, iy, t, s, fc);
            let dvx = ix + s * 35 / 100;
            fill_rect(buf, w, h, dvx, iy, t, s, fc);
            if self.sidebar_open {
                fill_rect(buf, w, h, ix + t, iy + t, dvx.saturating_sub(ix + t), s.saturating_sub(2 * t), fc);
            }
            self.sidebar_btn = (0.0, bw as f32);
        }

        let mut x = bar_h + pad; // tab chips start right of the sidebar button
        let chips_right = (w as usize).saturating_sub(bar_h * 4); // leave room for win controls + '+'
        for (label, target, active) in &chips {
            if x >= chips_right {
                break; // no room for more chips (narrow window)
            }
            let text: String = label.chars().take(18).collect();
            let chip_w = (text.chars().count() * cw + pad * 2).clamp(40, 240);
            let x0 = x;
            let x1 = (x + chip_w).min(chips_right);
            // Tile geometry (a small top/bottom inset inside the bar).
            let cy0 = 2usize;
            let chh = bar_h.saturating_sub(4);
            let chw = x1.saturating_sub(x0);
            if *active {
                fill_rect(buf, w, h, x0, cy0, chw, chh, chip_active);
            }
            // A visible 1–2px border around every tile so tabs are distinct even
            // when inactive. The active tab gets the accent color; others a
            // muted line.
            {
                let t = (1.0 * self.scale).round().max(1.0) as usize;
                let bc = OPAQUE
                    | if *active {
                        self.theme.selection
                    } else {
                        mix(self.theme.bg, self.theme.fg, 0.30)
                    };
                fill_rect(buf, w, h, x0, cy0, chw, t, bc); // top
                fill_rect(buf, w, h, x0, cy0 + chh.saturating_sub(t), chw, t, bc); // bottom
                fill_rect(buf, w, h, x0, cy0, t, chh, bc); // left
                fill_rect(buf, w, h, x0 + chw.saturating_sub(t), cy0, t, chh, bc); // right
            }
            let fg = if *active {
                self.theme.fg
            } else {
                mix(self.theme.bg, self.theme.fg, 0.55)
            };
            let mut gx = x0 + pad;
            for ch in text.chars() {
                ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                blit_char(buf, w, h, &self.glyph_cache, ch, gx, text_top, fg);
                gx += cw;
            }
            self.chip_ranges.push((x0 as f32, x1 as f32));
            self.chip_targets.push(*target);
            x = x1 + (2.0 * self.scale) as usize;
        }
        let px0 = x;
        let px1 = (x + cw + pad * 2).min(w as usize);
        ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, '+');
        blit_char(buf, w, h, &self.glyph_cache, '+', px0 + pad, text_top,
            mix(self.theme.bg, self.theme.fg, 0.7));
        self.plus_range = (px0 as f32, px1 as f32);

        // Window controls (right edge): minimize, maximize, close.
        {
            let bw = bar_h;
            let close_x = (w as usize).saturating_sub(bw);
            let max_x = close_x.saturating_sub(bw);
            let min_x = max_x.saturating_sub(bw);
            let cy = bar_h / 2;
            let fg = mix(self.theme.bg, self.theme.fg, 0.75);
            let half = (bw as f32 * 0.22) as usize;
            let t = (1.5 * self.scale).round().max(1.0) as usize;
            // minimize: a bottom bar
            fill_rect(buf, w, h, min_x + bw / 2 - half, cy + half, half * 2, t, OPAQUE | fg);
            // maximize: a square outline
            let mx = max_x + bw / 2 - half;
            let my = cy - half;
            let sq = half * 2;
            fill_rect(buf, w, h, mx, my, sq, t, OPAQUE | fg);
            fill_rect(buf, w, h, mx, my + sq - t, sq, t, OPAQUE | fg);
            fill_rect(buf, w, h, mx, my, t, sq, OPAQUE | fg);
            fill_rect(buf, w, h, mx + sq - t, my, t, sq, OPAQUE | fg);
            // close: an X (drawn as a filled box tinted red on the glyph)
            let cxx = close_x + bw / 2;
            ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, 'x');
            blit_char(buf, w, h, &self.glyph_cache, 'x', cxx - cw / 2, text_top, 0xE0_6C75);
            self.win_btns = [
                (min_x as f32, (min_x + bw) as f32),
                (max_x as f32, (max_x + bw) as f32),
                (close_x as f32, (close_x + bw) as f32),
            ];
            // Keep the sidebar toggle left of the window controls.
            let toggled_left = min_x.saturating_sub(bw + pad);
            let _ = toggled_left; // (sidebar button already placed; layout is fixed width)
        }

        // --- content: panes (terminal tab) or editor (document tab) -------
        let divider = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.18);
        let dim = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.45);
        // Scrollback-search highlights for the searched pane: abs_row -> spans.
        let search_sess = self.search.as_ref().map(|s| s.session);
        let mut search_hl: HashMap<i64, Vec<(usize, usize, bool)>> = HashMap::new();
        if let Some(s) = &self.search {
            for (i, &(row, c0, c1)) in s.matches.iter().enumerate() {
                search_hl.entry(row).or_default().push((c0, c1, i == s.current));
            }
        }
        // When the search bar is open, terminal glyphs must not draw over it
        // (the GPU paints glyphs above the chrome framebuffer).
        let search_bar_top: Option<usize> =
            self.search.as_ref().map(|_| (h as usize).saturating_sub(ch_px + pad));
        if self.active_doc.is_none() {
        for (session, rect) in &rects {
            if has_background {
                fill_rect(buf, w, h, rect.x, rect.y, rect.w, rect.h, (op << 24) | self.theme.bg);
                self.background.paint_rect(buf, w, h, rect.x, rect.y, rect.w, rect.h);
            }
            // Snapshot this pane's state, then release its borrow.
            let (term, scroll_off, sel) = {
                let p = match self.tabs[self.active_term].root.find(*session) {
                    Some(p) => p,
                    None => continue,
                };
                (p.term.clone(), p.scroll_off, p.sel)
            };
            let (cols, rows, cx, cy, cursor_on, top_abs, scroll, sb_count, alt);
            let failed_rows: Vec<i64>;
            {
                let t = term.lock().unwrap();
                sb_count = t.scrollback_count();
                scroll = scroll_off.min(sb_count);
                t.capture_viewport(scroll, &mut self.grid);
                cols = t.cols();
                rows = t.rows();
                cx = t.cx();
                cy = t.cy();
                cursor_on = t.cursor_visible() && scroll == 0;
                top_abs = sb_count as i64 - scroll as i64;
                alt = t.in_alt();
                // Failed-command rows (OSC 133;D exit != 0) for scrollbar ticks.
                failed_rows = t
                    .prompt_marks()
                    .into_iter()
                    .filter(|(_, e)| matches!(e, Some(c) if *c != 0))
                    .map(|(r, _)| r)
                    .collect();
            }
            let rcols = (rect.w / cw).min(cols);
            let rrows = (rect.h / ch_px).min(rows);

            // CPU path pre-rasterizes glyphs into the cache; the GPU path draws
            // them natively via DirectWrite (no cache needed).
            if !self.gpu_text {
                for r in 0..rrows.min(self.grid.len()) {
                    for c in 0..rcols.min(self.grid[r].len()) {
                        let rune = self.grid[r][c].rune;
                        if rune >= 0x20 {
                            if let Some(ch) = char::from_u32(rune) {
                                ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                            }
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
                    let default_bg = cell.bg == color::DEFAULT;
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
                    let mut selected = false;
                    if let Some((r1, c1, r2, c2)) = sel {
                        let (ar, ac) = (top_abs + r as i64, c as i64);
                        let after = ar > r1 || (ar == r1 && ac >= c1);
                        let before = ar < r2 || (ar == r2 && ac <= c2);
                        if after && before {
                            bg = self.theme.selection;
                            selected = true;
                        }
                    }
                    // Scrollback-search match highlight (current match brighter).
                    if search_sess == Some(*session) {
                        if let Some(spans) = search_hl.get(&(top_abs + r as i64)) {
                            if let Some(&(.., cur)) =
                                spans.iter().find(|&&(c0, c1, _)| c >= c0 && c <= c1)
                            {
                                bg = if cur { 0xFF_CC00 } else { mix(self.theme.bg, 0xFF_CC00, 0.5) };
                                fg = 0x1A_1A1A; // dark text for contrast on amber
                                selected = true; // force the bg to paint over images
                            }
                        }
                    }
                    let x0 = rect.x + c * cw;
                    let y0 = rect.y + r * ch_px;
                    // Terminal background uses the configured opacity; the cursor
                    // cell stays opaque so the block reads clearly.
                    let a = if is_cursor || reverse { 255 } else { op };
                    if !has_background || !default_bg || is_cursor || reverse || selected {
                        fill_rect(buf, w, h, x0, y0, cw, ch_px, (a << 24) | bg);
                    }
                    if cell.rune >= 0x20 && search_bar_top.map_or(true, |t| y0 + ch_px <= t) {
                        if let Some(ch) = char::from_u32(cell.rune) {
                            if self.gpu_text {
                                self.term_glyphs.push(GlyphDraw {
                                    ch,
                                    x: x0 as f32,
                                    y: y0 as f32,
                                    rgb: fg,
                                });
                            } else {
                                blit_char(buf, w, h, &self.glyph_cache, ch, x0, y0 as i32, fg);
                            }
                        }
                    }
                }
            }

            // Ctrl-hover hyperlink: underline the URL span on its row.
            if let Some(hl) = &self.hover_url {
                if hl.session == *session && hl.row >= top_abs {
                    let vis = (hl.row - top_abs) as usize;
                    if vis < rrows {
                        let off = (2.0 * self.scale).round().max(1.0) as usize;
                        let uy = rect.y + vis * ch_px + ch_px.saturating_sub(off);
                        let ux0 = rect.x + (hl.c0 as usize) * cw;
                        let ux1 = (rect.x + (hl.c1 as usize + 1) * cw).min(rect.x + rect.w);
                        let uth = (1.5 * self.scale).round().max(1.0) as usize;
                        fill_rect(buf, w, h, ux0, uy, ux1.saturating_sub(ux0), uth,
                            OPAQUE | self.theme.selection);
                    }
                }
            }

            // Divider lines between split panes (compared against the pane AREA,
            // not the window — the reserved bottom margin must not draw a divider).
            if rect.x + rect.w < area.x + area.w {
                fill_rect(buf, w, h, rect.x + rect.w, rect.y, DIVIDER, rect.h, divider);
            }
            if rect.y + rect.h < area.y + area.h {
                fill_rect(buf, w, h, rect.x, rect.y + rect.h, rect.w, DIVIDER, divider);
            }
            // Focused pane: a thin accent border (like the focused tab) when split.
            if *session == focus && rects.len() > 1 {
                let t = (1.0 * self.scale).round().max(1.0) as usize;
                let col = OPAQUE | self.theme.selection;
                fill_rect(buf, w, h, rect.x, rect.y, rect.w, t, col); // top
                fill_rect(buf, w, h, rect.x, rect.y + rect.h.saturating_sub(t), rect.w, t, col); // bottom
                fill_rect(buf, w, h, rect.x, rect.y, t, rect.h, col); // left
                fill_rect(buf, w, h, rect.x + rect.w.saturating_sub(t), rect.y, t, rect.h, col); // right
            }

            // Scrollbar in the strip reserved at the pane's right edge (see
            // sb_layout — text never draws under it). Primary screen only;
            // alt-screen apps own the whole grid. It hover-expands: a slim resting
            // bar grows to fill the reserved strip (≈ Windows Terminal's width)
            // while the cursor is over it or dragging. The grab zone is always the
            // full strip, so it's easy to catch even when drawn slim. A gutter is
            // left beyond the bar (the resize margin at the window's right edge,
            // else the divider), so the very edge still resizes the window.
            let (text_w, band_x, bar_right, wide_w) = sb_layout(*rect, w as usize, self.scale, cw);
            let thin_w = (5.0 * self.scale).round().max(4.0) as usize;
            // Draw only when there's history, on the primary screen, and the pane
            // is actually wide enough to have reserved a real strip (else text_w
            // was clamped and there's no room for the bar).
            if !alt && sb_count > 0 && rect.h > 0 && text_w + wide_w < rect.w {
                let total = sb_count + rows;
                let min_thumb = ch_px.max(16);
                let thumb_h = ((rect.h * rows) / total.max(1)).clamp(min_thumb.min(rect.h), rect.h);
                let travel = rect.h.saturating_sub(thumb_h);
                let from_top = sb_count.saturating_sub(scroll); // 0 = oldest .. sb_count = live
                let thumb_y = rect.y + (travel * from_top) / sb_count.max(1);
                let dragging = self.scrollbar_drag.map(|(s, _)| s == *session).unwrap_or(false);
                // Expanded while dragging or while the cursor is within the band.
                let (cx, cy) = self.cursor_px;
                let hovering = cx >= band_x as f64
                    && cx < bar_right as f64
                    && cy >= rect.y as f64
                    && cy < (rect.y + rect.h) as f64;
                let draw_w = if dragging || hovering { wide_w } else { thin_w };
                let draw_x = bar_right - draw_w; // right-anchored: slim bar hugs the band's edge
                let track_col = (150u32 << 24) | mix(self.theme.bg, self.theme.fg, 0.14);
                let thumb_col =
                    OPAQUE | mix(self.theme.bg, self.theme.fg, if dragging { 0.62 } else { 0.42 });
                fill_rect(buf, w, h, draw_x, rect.y, draw_w, rect.h, track_col);
                fill_rect(buf, w, h, draw_x, thumb_y, draw_w, thumb_h, thumb_col);
                // Red ticks at failed-command prompts (OSC 133), positioned by
                // their fraction down the whole buffer.
                let tick_h = (2.0 * self.scale).round().max(1.0) as usize;
                for &fr in &failed_rows {
                    if fr >= 0 && (fr as usize) < total {
                        let ty = rect.y + (rect.h * fr as usize) / total.max(1);
                        fill_rect(buf, w, h, draw_x, ty, draw_w, tick_h, OPAQUE | 0xE0_3030);
                    }
                }
                // Hit zone is the full wide band (not the drawn width), so the slim
                // resting bar is still easy to catch; it ends at the resize gutter,
                // so the very edge still resizes the window.
                self.scrollbars.push(ScrollBar {
                    session: *session,
                    track: Rect { x: band_x, y: rect.y, w: wide_w, h: rect.h },
                    thumb_y,
                    thumb_h,
                    max_off: sb_count,
                });
            }
        }
        } // end: panes (no doc)

        // --- editor (document tab) ----------------------------------------
        if let Some(doc) = self.active_doc.and_then(|i| self.docs.get(i)) {
            let ax = sb;
            let aw = (w as usize).saturating_sub(sb);
            let ah = (h as usize).saturating_sub(bar_h);
            fill_rect(buf, w, h, ax, bar_h, aw, ah, OPAQUE | self.theme.bg);
            // header: filename (+ * if dirty / [ro])
            let mut header = doc.name.clone();
            if doc.readonly {
                header.push_str("  [read-only]");
            } else if doc.dirty {
                header.push_str("  *");
            }
            header.push_str("   (Ctrl+S save · Ctrl+Shift+W close tab)");
            fill_rect(buf, w, h, ax, bar_h, aw, ch_px, OPAQUE | mix(self.theme.bg, self.theme.fg, 0.12));
            let mut gx = ax + pad;
            for ch in header.chars().take(aw / cw) {
                ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                blit_char(buf, w, h, &self.glyph_cache, ch, gx, bar_h as i32, self.theme.fg);
                gx += cw;
            }
            // text rows below the header
            let total = doc.lines.len();
            let gutter = (format!("{total}").len() + 1).max(3);
            let gpx = gutter * cw;
            let top = bar_h + ch_px;
            let rows = ah.saturating_sub(ch_px) / ch_px;
            for vi in 0..rows {
                let li = doc.scroll + vi;
                if li >= total {
                    break;
                }
                let y = (top + vi * ch_px) as i32;
                // line number (right-aligned in the gutter)
                let num = format!("{:>w$} ", li + 1, w = gutter - 1);
                let mut gx = ax;
                for ch in num.chars() {
                    ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                    blit_char(buf, w, h, &self.glyph_cache, ch, gx, y, dim);
                    gx += cw;
                }
                // line text (syntax-colored when highlight runs are available)
                let line = &doc.lines[li];
                let mut colors: Vec<u32> = Vec::with_capacity(line.len());
                if let Some(runs) = doc.hl.get(li) {
                    for &(rgb, n) in runs {
                        for _ in 0..n {
                            colors.push(rgb);
                        }
                    }
                }
                let mut gx = ax + gpx;
                for (i, &ch) in line.iter().enumerate() {
                    if gx + cw > ax + aw {
                        break;
                    }
                    let color = colors.get(i).copied().unwrap_or(self.theme.fg);
                    ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                    blit_char(buf, w, h, &self.glyph_cache, ch, gx, y, color);
                    gx += cw;
                }
                // caret
                if li == doc.cy && !doc.readonly {
                    let cxpx = ax + gpx + doc.cx * cw;
                    let caret_w = (cw / 8).max(2);
                    fill_rect(buf, w, h, cxpx, y as usize, caret_w, ch_px, OPAQUE | self.theme.cursor);
                }
            }
        }

        // --- file sidebar --------------------------------------------------
        if sb > 0 {
            let sb_bg = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.05);
            fill_rect(buf, w, h, 0, bar_h, sb, (h as usize).saturating_sub(bar_h), sb_bg);
            let line_h = ch_px;
            let visible = (h as usize).saturating_sub(bar_h) / line_h;
            let dir_color = self.theme.ansi[12]; // bright blue
            let icon_w = cw + cw / 2; // room for a small folder/file glyph
            for (vi, ent) in
                self.sidebar_entries.iter().skip(self.sidebar_scroll).take(visible).enumerate()
            {
                let y = bar_h + vi * line_h;
                let color = if ent.is_dir { dir_color } else { self.theme.fg };
                // icon
                if ent.is_dir {
                    draw_folder_icon(buf, w, h, 5, y, ch_px, dir_color);
                } else {
                    draw_file_icon(buf, w, h, 5, y, ch_px, mix(self.theme.bg, self.theme.fg, 0.6));
                }
                // name (no trailing slash now that there's an icon)
                let mut gx = 4 + icon_w;
                for ch in ent.name.chars().take(sb.saturating_sub(gx) / cw) {
                    ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                    blit_char(buf, w, h, &self.glyph_cache, ch, gx, y as i32, color);
                    gx += cw;
                }
            }
            let grip = if self.sidebar_resizing { OPAQUE | self.theme.selection } else { divider };
            fill_rect(buf, w, h, sb.saturating_sub(DIVIDER), bar_h, DIVIDER,
                (h as usize).saturating_sub(bar_h), grip);
        }

        // --- sidebar context menu (popup) ---------------------------------
        if let Some(menu) = &self.sidebar_menu {
            let r = menu.rect;
            let menu_bg = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.16);
            let border = OPAQUE | mix(self.theme.bg, self.theme.fg, 0.4);
            fill_rect(buf, w, h, r.x, r.y, r.w, r.h, menu_bg);
            fill_rect(buf, w, h, r.x, r.y, r.w, 1, border);
            fill_rect(buf, w, h, r.x, r.y + r.h - 1, r.w, 1, border);
            fill_rect(buf, w, h, r.x, r.y, 1, r.h, border);
            fill_rect(buf, w, h, r.x + r.w - 1, r.y, 1, r.h, border);
            let line_h = ch_px + 4;
            for (i, item) in menu.items.iter().enumerate() {
                let iy = (r.y + 2 + i * line_h) as i32;
                let mut gx = r.x + pad;
                for ch in item.chars() {
                    ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                    blit_char(buf, w, h, &self.glyph_cache, ch, gx, iy, self.theme.fg);
                    gx += cw;
                }
            }
        }

        // --- scrollback search bar (bottom strip overlay) -----------------
        if let Some((query, total, nth)) = self.search.as_ref().map(|s| {
            let total = s.matches.len();
            let nth = if total == 0 { 0 } else { s.current + 1 };
            (s.query.clone(), total, nth)
        }) {
            let bh = ch_px + pad;
            let by = (h as usize).saturating_sub(bh);
            let aw = (w as usize).saturating_sub(sb);
            fill_rect(buf, w, h, sb, by, aw, bh, chrome);
            let accent = (1.5 * self.scale).round().max(1.0) as usize;
            fill_rect(buf, w, h, sb, by, aw, accent, OPAQUE | self.theme.selection);
            let ty = (by + pad / 2) as i32;
            // Left: "Find: <query>" + a caret.
            let label = format!("Find: {query}");
            let mut gx = sb + pad;
            for ch in label.chars() {
                if gx + cw > sb + aw {
                    break;
                }
                ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                blit_char(buf, w, h, &self.glyph_cache, ch, gx, ty, self.theme.fg);
                gx += cw;
            }
            fill_rect(buf, w, h, gx, by + pad / 2, (cw / 8).max(2), ch_px, OPAQUE | self.theme.cursor);
            // Right: match count (or "no matches"). Esc / Enter / Shift+Enter hint.
            let count = if total == 0 && !query.is_empty() {
                "no matches".to_string()
            } else {
                format!("{nth}/{total}   Enter/Shift+Enter: next/prev   Esc: close")
            };
            let cwidth = count.chars().count() * cw;
            let mut gx = (sb + aw).saturating_sub(cwidth + pad);
            let ccol = if total == 0 && !query.is_empty() {
                mix(self.theme.bg, 0xFF_5555, 0.9)
            } else {
                mix(self.theme.bg, self.theme.fg, 0.6)
            };
            for ch in count.chars() {
                ensure_glyph(&self.font, &self.fallback, &mut self.glyph_cache, self.font_px, ch);
                blit_char(buf, w, h, &self.glyph_cache, ch, gx, ty, ccol);
                gx += cw;
            }
        }

        if let Some(g) = &mut self.gpu {
            if let Err(e) = g.present(&self.fb, w, h, &self.term_glyphs) {
                eprintln!("[wslterm] GPU present failed: {e:?}");
            }
        } else if let Some(layered) = &mut self.layered {
            layered.present(&self.fb, w, h);
        }
        self.last_frame = Instant::now();
        self.want_frame = false;
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        // Prefer the GPU path (DirectComposition). It needs a window with no
        // opaque redirection surface so DWM composites our premultiplied-alpha
        // swapchain straight against the desktop; the CPU fallback instead uses a
        // layered window (UpdateLayeredWindow) and must NOT set those flags.
        let use_gpu = gpu::available();
        // Borderless either way (we draw our own title/tab bar chrome).
        let mut attrs = Window::default_attributes()
            .with_title("WSL Terminal")
            .with_window_icon(load_window_icon())
            .with_decorations(false)
            .with_resizable(true)
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
        if use_gpu {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs = attrs.with_transparent(true).with_no_redirection_bitmap(true);
        }
        let win = Rc::new(event_loop.create_window(attrs).expect("create window"));
        self.scale = win.scale_factor() as f32;
        self.recompute_metrics();
        // Pace presents to the monitor refresh (fallback 60 Hz).
        self.frame_interval = win
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|&mhz| mhz > 0)
            .map(|mhz| std::time::Duration::from_secs_f64(1000.0 / mhz as f64))
            .unwrap_or(std::time::Duration::from_millis(16));

        if let Some(hwnd) = hwnd_of(&win) {
            if use_gpu {
                match Gpu::new(hwnd) {
                    Ok(mut g) => {
                        g.set_font(
                            &self.font_family,
                            self.font_px,
                            self.cell_w as f32,
                            self.cell_h as f32,
                            self.font_path.as_deref(),
                        );
                        self.gpu = Some(g);
                        self.gpu_text = true;
                    }
                    Err(e) => {
                        eprintln!("[wslterm] GPU init failed ({e:?}); falling back to layered");
                        self.layered = Some(Layered::new(hwnd));
                    }
                }
            } else {
                self.layered = Some(Layered::new(hwnd));
            }
        }
        let size = win.inner_size();
        let (cols, rows) = self.grid_dims(size.width, size.height);

        // Bring up the live WSL server (vsock daemon, else wslg pipe).
        let (mux, rx) = bring_up_mux(DISTRO);

        // First tab (in --cd directory if given).
        let term = Arc::new(Mutex::new(Terminal::new(cols, rows)));
        let session = mux.open(cols as u16, rows as u16, self.start_dir.as_deref().unwrap_or(""));
        self.registry.lock().unwrap().insert(session, term.clone());
        self.tabs.push(Tab { root: Layout::Leaf(Pane::new(session, term)), focus: session });
        self.active_term = 0;
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
                    let mut clip: Option<String> = None;
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
                                    if let Some(c) = t.take_clipboard() {
                                        clip = Some(c); // OSC 52 (zellij/tmux/vim copy)
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
                    // OSC 52: push the latest clipboard request to the OS clipboard
                    // (the per-terminal lock is already released here).
                    if let Some(text) = clip {
                        let _ = clipboard_win::set_clipboard_string(&text);
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
        self.request_redraw(); // paint chrome immediately (before first output)
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw => {
                self.redraw_pending.store(false, Ordering::Release);
                // Keep each scrolled-back pane pinned to the same content as new
                // output arrives: advance scroll_off by however many lines just
                // scrolled into history (so the view doesn't get yanked to the
                // bottom). Panes already at the live bottom (scroll_off == 0) stay
                // there. Typing is the explicit way back to live (see handle_key).
                for tab in &mut self.tabs {
                    let mut sids = Vec::new();
                    tab.root.collect_sessions(&mut sids);
                    for sid in sids {
                        if let Some(p) = tab.root.find_mut(sid) {
                            let (total, sbc) = {
                                let t = p.term.lock().unwrap();
                                (t.scrolled_total(), t.scrollback_count())
                            };
                            let delta = total.saturating_sub(p.last_scrolled);
                            if p.scroll_off > 0 && delta > 0 {
                                p.scroll_off = (p.scroll_off + delta as usize).min(sbc);
                            }
                            p.last_scrolled = total;
                        }
                    }
                }
                // Clear the focused pane's stale selection on new output.
                let s = self.focused_session();
                if let Some(p) = self.pane_mut(s) {
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
                // Follow the shell's cwd in the sidebar, but only when it actually
                // changes (a manual `cd`). Clicking a folder browses the panel
                // independently, so we must not snap it back to the cwd here.
                if self.sidebar_open {
                    let cwd = self.focused_cwd();
                    if cwd != self.last_sidebar_cwd {
                        self.last_sidebar_cwd = cwd.clone();
                        self.list_sidebar_dir(cwd);
                    }
                }
                // Pace the actual present to the monitor refresh (about_to_wait).
                self.want_frame = true;
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

    /// Frame pacing: when output is pending, present at most once per monitor
    /// refresh — repaint when the interval elapses, otherwise sleep until then.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.want_frame {
            let target = self.last_frame + self.frame_interval;
            if Instant::now() >= target {
                if let Some(win) = &self.win {
                    win.request_redraw();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(target));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state();
                // Releasing Ctrl drops the hyperlink affordance immediately.
                if !self.mods.control_key() && self.hover_url.take().is_some() {
                    self.update_resize_cursor();
                    self.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event, event_loop),
            WindowEvent::MouseWheel { delta, .. } => {
                let y = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / self.cell_h as f64,
                };
                let step = (y.round() as i32) * 3;
                let over_sidebar = self.sidebar_w() > 0
                    && self.cursor_px.0 < self.sidebar_w() as f64
                    && self.cursor_px.1 >= self.tab_bar_h() as f64;
                if self.mods.control_key() {
                    self.zoom_font(if y > 0.0 { 1.0 } else { -1.0 });
                } else if over_sidebar {
                    let max = self.sidebar_entries.len().saturating_sub(1) as i32;
                    self.sidebar_scroll = (self.sidebar_scroll as i32 - step).clamp(0, max) as usize;
                    self.request_redraw();
                } else if let Some(doc) = self.cur_doc_mut() {
                    let max = doc.lines.len().saturating_sub(1) as i32;
                    doc.scroll = (doc.scroll as i32 - step).clamp(0, max) as usize;
                    self.request_redraw();
                } else if self.report_mouse_wheel(y > 0.0, (y.round().abs() as u32).max(1)) {
                    // forwarded to a mouse-tracking app (zellij, vim, …)
                } else {
                    self.scroll_by(step);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_px = (position.x, position.y);
                if self.sidebar_resizing {
                    self.resize_sidebar_to_cursor();
                    self.update_resize_cursor();
                    return;
                }
                // Dragging a scrollbar thumb maps cursor-y straight to scroll_off.
                if let Some((session, grab)) = self.scrollbar_drag {
                    if let Some(sb) = self.scrollbars.iter().find(|s| s.session == session).copied() {
                        let off = sb.off_for_thumb_top(self.cursor_px.1 - grab);
                        if let Some(p) = self.pane_mut(session) {
                            p.scroll_off = off;
                        }
                        self.request_redraw();
                    }
                    return;
                }
                self.update_resize_cursor();
                // Hover-expand: redraw when the cursor enters or leaves a scrollbar.
                let hov = self.scrollbar_hit(self.cursor_px.0, self.cursor_px.1).map(|s| s.session);
                if hov != self.sb_hover {
                    self.sb_hover = hov;
                    self.request_redraw();
                }
                // Ctrl-hover a hyperlink: highlight it and show the hand cursor.
                let url = if self.mods.control_key()
                    && self.active_doc.is_none()
                    && self.scrollbar_drag.is_none()
                {
                    self.url_under_cursor()
                } else {
                    None
                };
                if url != self.hover_url {
                    self.hover_url = url;
                    self.request_redraw();
                }
                if self.hover_url.is_some() {
                    if let Some(win) = &self.win {
                        win.set_cursor(CursorIcon::Pointer);
                    }
                }
                // Forward motion to a mouse-tracking app before any local selection.
                if self.active_doc.is_none() && self.report_mouse_motion() {
                    return;
                }
                if self.active_doc.is_none()
                    && self.tabs.get(self.active_term).is_some()
                    && self.focused().selecting
                {
                    self.update_selection();
                }
            }
            WindowEvent::MouseInput { state, button, .. } => match (button, state) {
                (MouseButton::Left, ElementState::Pressed) => {
                    // A click anywhere dismisses/handles an open context menu first.
                    if self.menu_click() {
                        return;
                    }
                    // Ctrl-click opens a hyperlink under the cursor (before selection).
                    if self.mods.control_key() {
                        if let Some(hl) = self.url_under_cursor() {
                            open_url(&hl.url);
                            return;
                        }
                    }
                    let (px, py) = self.cursor_px;
                    let size = self.win.as_ref().map(|w| w.inner_size());
                    // 1) Resize from a window edge (borderless window).
                    if let Some(s) = size {
                        if let Some(dir) = resize_dir_at(px, py, s.width, s.height, self.scale) {
                            if let Some(win) = &self.win {
                                let _ = win.drag_resize_window(dir);
                            }
                            return;
                        }
                    }
                    if self.sidebar_resize_hit(px, py) {
                        self.sidebar_resizing = true;
                        self.resize_sidebar_to_cursor();
                        self.update_resize_cursor();
                        return;
                    }
                    if py < self.tab_bar_h() as f64 {
                        let x = px as f32;
                        // 2) Window controls.
                        if x >= self.win_btns[2].0 {
                            event_loop.exit();
                        } else if x >= self.win_btns[1].0 && x < self.win_btns[1].1 {
                            self.toggle_maximize();
                        } else if x >= self.win_btns[0].0 && x < self.win_btns[0].1 {
                            if let Some(win) = &self.win {
                                win.set_minimized(true);
                            }
                        } else if x >= self.sidebar_btn.0 && x < self.sidebar_btn.1 {
                            self.toggle_sidebar();
                        } else if (x >= self.plus_range.0 && x < self.plus_range.1)
                            || self.chip_at(x).is_some()
                        {
                            self.tab_bar_click();
                        } else if let Some(win) = &self.win {
                            // 3) Empty tab-bar area drags the window.
                            let _ = win.drag_window();
                        }
                    } else if self.sidebar_w() > 0
                        && px < self.sidebar_w() as f64 - self.sidebar_resize_handle_w()
                    {
                        self.sidebar_click();
                    } else if self.active_doc.is_none() {
                        // A press on a pane's scrollbar starts a thumb drag or
                        // pages the view; otherwise it begins a text selection —
                        // unless a mouse-tracking app wants the click forwarded.
                        if self.report_mouse_press(0) {
                            self.request_redraw();
                        } else if let Some(sb) = self.scrollbar_hit(px, py) {
                            self.tabs[self.active_term].focus = sb.session;
                            if sb.thumb_contains(py) {
                                self.scrollbar_drag = Some((sb.session, py - sb.thumb_y as f64));
                            } else {
                                let rows = self
                                    .pane(sb.session)
                                    .map(|p| p.term.lock().unwrap().rows() as i32)
                                    .unwrap_or(1);
                                self.scroll_by(if (py as usize) < sb.thumb_y { rows } else { -rows });
                            }
                            self.request_redraw();
                        } else {
                            self.begin_selection();
                        }
                    }
                }
                (MouseButton::Left, ElementState::Released) => {
                    self.sidebar_resizing = false;
                    let was_dragging = self.scrollbar_drag.take().is_some();
                    if !self.report_mouse_release(0) {
                        let s = self.focused_session();
                        if let Some(p) = self.pane_mut(s) {
                            p.selecting = false;
                        }
                    }
                    if was_dragging {
                        // Re-evaluate hover so the bar shrinks if the drag ended off it.
                        self.sb_hover =
                            self.scrollbar_hit(self.cursor_px.0, self.cursor_px.1).map(|s| s.session);
                        self.request_redraw();
                    }
                }
                (MouseButton::Middle, ElementState::Pressed) => {
                    if self.cursor_px.1 < self.tab_bar_h() as f64 {
                        // Close the tab (terminal or document) under the middle click.
                        if let Some(idx) = self.chip_at(self.cursor_px.0 as f32) {
                            match self.chip_targets.get(idx).copied() {
                                Some(ChipTarget::Term(t)) => self.close_whole_tab(t, event_loop),
                                Some(ChipTarget::Doc(d)) => {
                                    if d < self.docs.len() {
                                        self.docs.remove(d);
                                    }
                                    if self.active_doc == Some(d) {
                                        self.active_doc = None;
                                    } else if let Some(a) = self.active_doc {
                                        if a > d {
                                            self.active_doc = Some(a - 1);
                                        }
                                    }
                                    self.request_redraw();
                                }
                                None => {}
                            }
                        }
                    } else if !self.report_mouse_press(1) {
                        self.paste(); // X11-style middle-click paste in the terminal
                    }
                }
                (MouseButton::Middle, ElementState::Released) => {
                    self.report_mouse_release(1);
                }
                (MouseButton::Right, ElementState::Pressed) => {
                    let in_sidebar = self.sidebar_w() > 0
                        && self.cursor_px.0 < self.sidebar_w() as f64 - self.sidebar_resize_handle_w()
                        && self.cursor_px.1 >= self.tab_bar_h() as f64;
                    if in_sidebar {
                        self.open_sidebar_menu();
                    } else {
                        self.report_mouse_press(2);
                    }
                }
                (MouseButton::Right, ElementState::Released) => {
                    self.report_mouse_release(2);
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
    install_panic_log();
    let start_dir = parse_cd_arg();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy, start_dir);
    event_loop.run_app(&mut app).expect("run app");
}

/// Log any panic (message + location + backtrace) to a file so crashes — like
/// the reported full-screen one — can be diagnosed after the fact.
fn install_panic_log() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = std::env::var("APPDATA")
            .map(|a| std::path::PathBuf::from(a).join("WslTerminal").join("panic.log"))
            .unwrap_or_else(|_| std::path::PathBuf::from("wslterm-panic.log"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let bt = std::backtrace::Backtrace::force_capture();
        let line = format!("=== panic ===\n{info}\nbacktrace:\n{bt}\n");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
        }
        eprintln!("{line}");
        default(info);
    }));
}

/// `--cd <dir>`: the directory the first tab should start in. Accepts either a
/// Linux path (used when a new window is spawned from "Open in new window" /
/// Ctrl+Shift+N) or a **Windows** path — so an Explorer context menu can pass
/// `%V` directly (see `windows_to_wsl_path`). Anything that can't be resolved to
/// an absolute Linux path is dropped, so the tab falls back to the home (`~`).
fn parse_cd_arg() -> Option<String> {
    let mut args = std::env::args();
    while let Some(a) = args.next() {
        if a == "--cd" {
            return args
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| windows_to_wsl_path(&s))
                .filter(|s| s.starts_with('/'));
        }
    }
    None
}

/// Translate a Windows path into its WSL/Linux equivalent so `--cd %V` from an
/// Explorer context menu works:
///   - drive paths map under `/mnt`:    `C:\Users` -> `/mnt/c/Users`,
///     `F:\test` -> `/mnt/f/test`, `C:\` -> `/mnt/c`;
///   - WSL UNC shares map back to their Linux path:
///     `\\wsl.localhost\Ubuntu\home\me` (or `\\wsl$\...`) -> `/home/me`.
/// A path that is already POSIX (starts with `/`) is returned unchanged; an
/// unrecognized form is returned as-is (the caller then discards it).
fn windows_to_wsl_path(p: &str) -> String {
    let s = p.trim();
    if s.starts_with('/') {
        return s.to_string(); // already a Linux path (internal spawn)
    }
    // WSL UNC share: strip `\\wsl.localhost\<distro>\` (or `\\wsl$\<distro>\`),
    // leaving the Linux-absolute remainder. Normalize separators to backslashes.
    let norm = s.replace('/', "\\");
    for prefix in ["\\\\wsl.localhost\\", "\\\\wsl$\\"] {
        if let Some(rest) = norm.strip_prefix(prefix) {
            // rest = "<distro>\<linux path>"; drop the distro component.
            let after = rest.splitn(2, '\\').nth(1).unwrap_or("");
            return format!("/{}", after.replace('\\', "/").trim_start_matches('/'));
        }
    }
    // Drive path: `X:\...`, `X:/...`, or bare `X:`.
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic() {
        let drive = (b[0] as char).to_ascii_lowercase();
        let rest = s[2..].replace('\\', "/");
        let rest = rest.trim_start_matches('/').trim_end_matches('/');
        return if rest.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{rest}")
        };
    }
    s.to_string() // unrecognized — caller discards (falls back to ~)
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

/// Open a URL in the default browser (Ctrl-click on a hyperlink). Restricted to
/// http(s) — the detector only matches those, but double-check before launching.
#[cfg(windows)]
fn open_url(url: &str) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let op: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            0 as HWND,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open_url(_url: &str) {}

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

/// Rasterize `ch` into the cache if not already present, using the primary font
/// or the first fallback font that has the glyph (so CJK/Cyrillic/symbol glyphs
/// missing from the primary still render). All glyphs share the primary baseline.
fn ensure_glyph(
    primary: &FontVec,
    fallback: &[FontVec],
    cache: &mut HashMap<char, Option<Glyph>>,
    px: f32,
    ch: char,
) {
    if ch == ' ' || ch.is_control() || cache.contains_key(&ch) {
        return;
    }
    let ascent = primary.as_scaled(PxScale::from(px)).ascent();
    let font = if primary.glyph_id(ch).0 != 0 {
        primary
    } else {
        fallback.iter().find(|f| f.glyph_id(ch).0 != 0).unwrap_or(primary)
    };
    cache.insert(ch, rasterize_glyph(font, px, ascent, ch));
}

/// Load fallback fonts for glyphs the primary monospace font lacks. Color emoji
/// (CBDT/COLR) still won't render in color — that needs a color-glyph renderer.
fn load_fallback_fonts() -> Vec<FontVec> {
    let mut v = Vec::new();
    let candidates: &[(&str, bool)] = &[
        (r"C:\Windows\Fonts\segoeui.ttf", false),  // Latin/Cyrillic/Greek/diacritics
        (r"C:\Windows\Fonts\seguisym.ttf", false), // symbols, arrows, misc
        (r"C:\Windows\Fonts\msgothic.ttc", true),  // CJK incl. (half-width) katakana
        (r"C:\Windows\Fonts\seguiemj.ttf", false), // emoji (monochrome outline at best)
    ];
    for &(path, ttc) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let font = if ttc {
                FontVec::try_from_vec_and_index(bytes, 0)
            } else {
                FontVec::try_from_vec(bytes)
            };
            if let Ok(f) = font {
                v.push(f);
            }
        }
    }
    v
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
                let dpx = buf[idx];
                let nrgb = blend(dpx, fg, cov as f32 / 255.0);
                // Text raises the pixel alpha toward opaque (crisp text over a
                // translucent terminal background).
                let da = (dpx >> 24) & 0xff;
                let na = da + (255 - da) * cov as u32 / 255;
                buf[idx] = (na << 24) | nrgb;
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

/// Bring up the WSL backend. Prefer a direct **vsock** connection to a running
/// `wslptyd`; if none is listening, start it detached (once, via `wslg`) and
/// retry briefly (covers VM boot + the daemon's bind race); finally fall back to
/// the legacy `wslg` stdin/stdout pipe if vsock is unavailable.
fn bring_up_mux(distro: &str) -> (WslMux, std::sync::mpsc::Receiver<MuxEvent>) {
    let server = bootstrap::resolve_server()
        .expect("wslptyd not found (build native/ and place under artifacts/)");
    let port = bootstrap::VSOCK_PORT;

    // 1) Already-running daemon? Connect instantly — no process spawn.
    if let Ok(mux) = wslterm_pty::vsock::start_mux(port) {
        eprintln!("[wslterm] connected to wslptyd over vsock:{port}");
        return mux;
    }
    // 2) Not up: start it (fire-and-forget), then poll the connect for a while —
    //    long enough to cover a cold WSL VM boot plus the daemon's bind.
    eprintln!("[wslterm] vsock connect failed; bootstrapping wslptyd on vsock:{port}");
    let cmd = bootstrap::build_vsock_command(&server, port);
    if wslterm_pty::process::spawn_bootstrap(distro, &cmd).is_ok() {
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(mux) = wslterm_pty::vsock::start_mux(port) {
                eprintln!("[wslterm] connected to wslptyd over vsock:{port} (after bootstrap)");
                return mux;
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
    // 3) Fall back to the legacy wslg pipe transport.
    eprintln!("[wslterm] vsock unavailable; falling back to the wslg pipe");
    let command = bootstrap::build_server_command(&server);
    let proc = WslProcess::launch(distro, &command).expect("launch wslg.exe");
    WslMux::from_process(proc)
}

/// Raw Win32 HWND (as isize) for the window, if available.
fn hwnd_of(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Decode the bundled WSL icon (largest frame) into a winit window icon.
fn load_window_icon() -> Option<Icon> {
    const ICON: &[u8] = include_bytes!("../wsl.ico");
    let dir = ico::IconDir::read(std::io::Cursor::new(ICON)).ok()?;
    let entry = dir.entries().iter().max_by_key(|e| e.width())?;
    let img = entry.decode().ok()?;
    Icon::from_rgba(img.rgba_data().to_vec(), img.width(), img.height()).ok()
}

const FALLBACK_FONT_PATHS: [&str; 4] = [
    r"C:\Windows\Fonts\consola.ttf",
    r"C:\Windows\Fonts\CascadiaMono.ttf",
    r"C:\Windows\Fonts\CascadiaCode.ttf",
    r"C:\Windows\Fonts\lucon.ttf",
];

fn load_monospace_font(family: &str) -> Option<FontVec> {
    let path = font_file_path(family)?;
    let bytes = std::fs::read(&path).ok()?;
    FontVec::try_from_vec(bytes).ok()
}

/// Resolve the configured family to a concrete font file (the same file both the
/// CPU renderer and the GPU/DirectWrite renderer load, so they stay identical).
fn font_file_path(family: &str) -> Option<std::path::PathBuf> {
    if let Some(p) = find_family_font_path(family) {
        return Some(p);
    }
    FALLBACK_FONT_PATHS
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.exists())
}

/// Lowercase + keep only alphanumerics, for loose font-name matching.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Find a font file whose name matches `family` by scanning the per-user and
/// system font folders, preferring the Regular weight. Pure filename matching —
/// no font-table parsing — which covers Nerd Fonts and the like (their filenames
/// embed the family name). Returns the validated, loadable file path.
fn find_family_font_path(family: &str) -> Option<std::path::PathBuf> {
    let key = squash(family);
    if key.len() < 3 {
        return None;
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(la) = std::env::var("LOCALAPPDATA") {
        dirs.push(std::path::PathBuf::from(la).join(r"Microsoft\Windows\Fonts"));
    }
    if let Ok(win) = std::env::var("WINDIR") {
        dirs.push(std::path::PathBuf::from(win).join("Fonts"));
    } else {
        dirs.push(std::path::PathBuf::from(r"C:\Windows\Fonts"));
    }

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for dir in dirs {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
            if !matches!(ext.as_deref(), Some("ttf") | Some("otf")) {
                continue; // skip .ttc collections (ab_glyph wants a single face)
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if squash(stem).contains(&key) {
                candidates.push(p);
            }
        }
    }
    // Prefer a Regular face (filename has no weight/style qualifier).
    candidates.sort_by_key(|p| {
        let s = squash(p.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        let styled = ["bold", "italic", "oblique", "light", "thin", "medium", "semibold", "black"]
            .iter()
            .any(|w| s.contains(w));
        styled as u8
    });
    for p in candidates {
        if let Ok(bytes) = std::fs::read(&p) {
            if FontVec::try_from_vec(bytes).is_ok() {
                return Some(p); // validated loadable; return the path
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

/// Scrollbar layout for a pane: `(text_w, band_x, bar_right, bar_w)`. Text is
/// laid out in `[rect.x, rect.x + text_w)`; the scrollbar band occupies the
/// reserved strip `[band_x, bar_right]` to its right, so the bar never draws over
/// text. The reservation is constant w.r.t. scrollback (so columns don't reflow
/// when history appears) and leaves a gutter beyond the bar — the resize margin
/// at the window's right edge, else the inter-pane divider. `win_w` is the window
/// width (to detect the resizable right edge).
fn sb_layout(rect: Rect, win_w: usize, scale: f32, cell_w: usize) -> (usize, usize, usize, usize) {
    let bar_w = (14.0 * scale).round().max(12.0) as usize;
    let at_edge = rect.x + rect.w >= win_w;
    let gutter = if at_edge { (8.0 * scale).round().max(5.0) as usize } else { DIVIDER };
    let reserve = bar_w + gutter;
    let text_w = rect.w.saturating_sub(reserve).max(cell_w);
    let band_x = rect.x + text_w;
    let bar_right = (band_x + bar_w).min(rect.x + rect.w);
    (text_w, band_x, bar_right, bar_w)
}

/// If the cursor is within the resize border of a (borderless) window edge,
/// which direction to resize. `None` = not on an edge.
fn resize_dir_at(px: f64, py: f64, w: u32, h: u32, scale: f32) -> Option<ResizeDirection> {
    let m = (8.0 * scale as f64).max(5.0);
    let (w, h) = (w as f64, h as f64);
    let (left, right) = (px < m, px > w - m);
    let (top, bottom) = (py < m, py > h - m);
    use ResizeDirection::*;
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => NorthWest,
        (true, _, _, true) => NorthEast,
        (_, true, true, _) => SouthWest,
        (_, true, _, true) => SouthEast,
        (true, ..) => North,
        (_, true, ..) => South,
        (_, _, true, _) => West,
        (_, _, _, true) => East,
        _ => return None,
    })
}

/// A small filled folder glyph (for directory rows in the sidebar).
fn draw_folder_icon(buf: &mut [u32], w: u32, h: u32, x: usize, y: usize, cell_h: usize, rgb: u32) {
    let c = OPAQUE | (rgb & 0xFF_FFFF);
    let s = (cell_h as f32 * 0.6) as usize;
    let iy = y + cell_h.saturating_sub(s) / 2;
    let tab_h = (s / 4).max(1);
    fill_rect(buf, w, h, x, iy, s / 2, tab_h, c); // tab
    fill_rect(buf, w, h, x, iy + tab_h, s, s - tab_h, c); // body
}

/// A small outlined "page" glyph (for file rows in the sidebar).
fn draw_file_icon(buf: &mut [u32], w: u32, h: u32, x: usize, y: usize, cell_h: usize, rgb: u32) {
    let c = OPAQUE | (rgb & 0xFF_FFFF);
    let s = (cell_h as f32 * 0.6) as usize;
    let pw = (s * 3 / 4).max(3);
    let iy = y + cell_h.saturating_sub(s) / 2;
    let t = 1usize;
    fill_rect(buf, w, h, x, iy, pw, t, c); // top
    fill_rect(buf, w, h, x, iy + s - t, pw, t, c); // bottom
    fill_rect(buf, w, h, x, iy, t, s, c); // left
    fill_rect(buf, w, h, x + pw - t, iy, t, s, c); // right
    fill_rect(buf, w, h, x + t + 1, iy + s / 3, pw.saturating_sub(2 * t + 2), t, c); // line
    fill_rect(buf, w, h, x + t + 1, iy + s / 2, pw.saturating_sub(2 * t + 2), t, c); // line
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

#[cfg(test)]
mod tests {
    use super::windows_to_wsl_path as w;

    #[test]
    fn drive_paths() {
        assert_eq!(w(r"C:\Users"), "/mnt/c/Users");
        assert_eq!(w(r"F:\test"), "/mnt/f/test");
        assert_eq!(w(r"C:\Users\me\Some Dir"), "/mnt/c/Users/me/Some Dir");
        assert_eq!(w(r"d:\Games\steam"), "/mnt/d/Games/steam");
    }

    #[test]
    fn drive_roots_and_trailing() {
        assert_eq!(w(r"C:\"), "/mnt/c");
        assert_eq!(w("C:"), "/mnt/c");
        assert_eq!(w(r"C:\Users\"), "/mnt/c/Users");
        assert_eq!(w("C:/Users"), "/mnt/c/Users"); // forward slashes too
    }

    #[test]
    fn wsl_unc_shares() {
        assert_eq!(w(r"\\wsl.localhost\Ubuntu\home\me"), "/home/me");
        assert_eq!(w(r"\\wsl$\Ubuntu\home\me\proj"), "/home/me/proj");
        assert_eq!(w(r"\\wsl.localhost\Debian\"), "/");
    }

    #[test]
    fn linux_paths_pass_through() {
        assert_eq!(w("/home/me/proj"), "/home/me/proj"); // internal spawn cwd
        assert_eq!(w("  /var/log  "), "/var/log");
    }

    #[test]
    fn unrecognized_returned_as_is() {
        // Caller discards anything not starting with '/'.
        assert!(!w(r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}").starts_with('/'));
    }
}
