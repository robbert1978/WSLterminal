//! WSL Terminal GUI — Rust rewrite, milestone 1.
//!
//! A real window that renders the `wslterm-core` terminal grid and pumps the
//! user's keystrokes (via `wslterm-core::input::encode`) into a live WSL session
//! driven by `wslterm-pty`. CPU-rendered (winit + softbuffer + ab_glyph) on
//! purpose: it keeps RAM low and avoids the wgpu/Direct3D managed stack — the
//! whole point of the rewrite. Color, SGR, scrollback view, tabs, panes, the
//! sidebar and the editor come in later milestones; this one proves the stack.
//!
//! NOTE: still a console subsystem app so panics/stderr are visible while we
//! bring the GUI up. A later milestone switches to the windowless subsystem.

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

use wslterm_core::input::{self, Key, Mods};
use wslterm_core::{color, Cell, CellFlags, Terminal};
use wslterm_pty::bootstrap;
use wslterm_pty::mux::MuxEvent;
use wslterm_pty::{WslMux, WslProcess};

const DISTRO: &str = "Ubuntu";
const FONT_PX: f32 = 17.0;
const DEFAULT_FG: u32 = 0xCC_CCCC; // Campbell foreground
const DEFAULT_BG: u32 = 0x0C_0C0C; // Campbell background
const CURSOR_RGB: u32 = 0xCC_CCCC;

/// Events delivered to the winit loop from the mux reader thread.
enum UserEvent {
    Mux(MuxEvent),
    Closed,
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    win: Option<Rc<Window>>,
    context: Option<Context<Rc<Window>>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,

    font: FontVec,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,

    term: Terminal,
    mux: Option<WslMux>,
    session: u32,
    mods: ModifiersState,

    grid: Vec<Vec<Cell>>, // reused viewport snapshot
    rx: Option<Receiver<MuxEvent>>,
}

impl App {
    fn new(proxy: EventLoopProxy<UserEvent>) -> App {
        let font = load_monospace_font().expect("no monospace font found (Consolas/Cascadia)");
        let scale = PxScale::from(FONT_PX);
        let sf = font.as_scaled(scale);
        let ascent = sf.ascent();
        let cell_h = (sf.ascent() - sf.descent() + sf.line_gap()).ceil().max(1.0) as usize;
        let cell_w = sf.h_advance(font.glyph_id('M')).ceil().max(1.0) as usize;
        App {
            proxy,
            win: None,
            context: None,
            surface: None,
            font,
            cell_w,
            cell_h,
            ascent,
            term: Terminal::new(80, 24),
            mux: None,
            session: 0,
            mods: ModifiersState::empty(),
            grid: Vec::new(),
            rx: None,
        }
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
        let mods = Mods {
            ctrl: self.mods.control_key(),
            alt: self.mods.alt_key(),
            shift: self.mods.shift_key(),
        };
        let app_cursor = self.term.app_cursor_keys();

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

    fn resize_surface(&mut self, w: u32, h: u32) {
        let (cols, rows) = self.grid_dims(w, h);
        self.term.resize(cols, rows);
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
        // Clear to background.
        buffer.fill(DEFAULT_BG);

        self.term.capture_viewport(0, &mut self.grid);
        let cols = self.term.cols();
        let rows = self.term.rows();
        let (cx, cy) = (self.term.cx(), self.term.cy());
        let cursor_on = self.term.cursor_visible();

        let scale = PxScale::from(FONT_PX);
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

                let x0 = c * self.cell_w;
                let y0 = r * self.cell_h;
                fill_rect(&mut buffer, w, h, x0, y0, self.cell_w, self.cell_h, bg);

                let ch = char::from_u32(cell.rune).unwrap_or(' ');
                if cell.rune >= 0x20 && ch != ' ' {
                    let baseline = y0 as f32 + self.ascent;
                    let glyph = self
                        .font
                        .glyph_id(ch)
                        .with_scale_and_position(scale, ab_glyph::point(x0 as f32, baseline));
                    if let Some(outline) = self.font.outline_glyph(glyph) {
                        let bounds = outline.px_bounds();
                        outline.draw(|gx, gy, cov| {
                            let px = bounds.min.x as i32 + gx as i32;
                            let py = bounds.min.y as i32 + gy as i32;
                            if px < 0 || py < 0 || px as u32 >= w || py as u32 >= h {
                                return;
                            }
                            let idx = py as usize * w as usize + px as usize;
                            buffer[idx] = blend(buffer[idx], fg, cov);
                        });
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

        let context = Context::new(win.clone()).expect("softbuffer context");
        let mut surface = Surface::new(&context, win.clone()).expect("softbuffer surface");
        let size = win.inner_size();
        if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
            let _ = surface.resize(w, h);
        }

        let (cols, rows) = self.grid_dims(size.width, size.height);
        self.term = Terminal::new(cols, rows);

        // Bring up the live WSL session.
        let server = bootstrap::resolve_server()
            .expect("wslptyd not found (build native/ and place under artifacts/)");
        let command = bootstrap::build_server_command(&server);
        let proc = WslProcess::launch(DISTRO, &command).expect("launch wslg.exe");
        let (mux, rx) = WslMux::start(proc);
        self.session = mux.open(cols as u16, rows as u16, "");
        self.mux = Some(mux);
        eprintln!(
            "[wslterm] window up {cols}x{rows}, session {} opened on {DISTRO}",
            self.session
        );

        // Forward mux events into the winit loop.
        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("mux-forward".into())
            .spawn(move || {
                while let Ok(ev) = rx.recv() {
                    if proxy.send_event(UserEvent::Mux(ev)).is_err() {
                        return;
                    }
                }
                let _ = proxy.send_event(UserEvent::Closed);
            })
            .expect("spawn forwarder");
        let _ = self.rx.take(); // rx moved into the thread

        self.win = Some(win);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Mux(MuxEvent::Data { id, bytes }) if id == self.session => {
                self.term.feed(&bytes);
                if !self.term.respond.is_empty() {
                    let resp = std::mem::take(&mut self.term.respond);
                    self.send(&resp);
                }
                if let Some(win) = &self.win {
                    win.request_redraw();
                }
            }
            UserEvent::Mux(MuxEvent::Exit { id, code }) if id == self.session => {
                eprintln!("[wslterm] session {id} exited (code {code})");
                event_loop.exit();
            }
            UserEvent::Closed => {
                eprintln!("[wslterm] mux closed");
                event_loop.exit();
            }
            _ => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(&event),
            WindowEvent::Resized(size) => {
                self.resize_surface(size.width, size.height);
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
