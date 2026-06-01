//! Terminal screen grid, cursor, pen, scroll region, alt buffer, and scrollback.
//! Ports `src/WslTerminal/Vt/Screen.cs`. Single-threaded (the C# version ran
//! under a lock; here the owner serializes access).

use crate::cell::{Cell, CellFlags};
use crate::charwidth::width_of;
use crate::color;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseTracking {
    None,
    X10,
    Normal,
    ButtonEvent,
    AnyEvent,
}

#[derive(Clone, Copy)]
struct Saved {
    cx: usize,
    cy: usize,
    fg: i32,
    bg: i32,
    fl: CellFlags,
    origin: bool,
}

/// Fixed-capacity ring of scrollback lines: O(1) push, indexed 0 = oldest. Push
/// returns the evicted line for recycling. Ports the C# `Scrollback`.
struct Scrollback {
    ring: Vec<Option<Vec<Cell>>>,
    start: usize,
    count: usize,
}

impl Scrollback {
    fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Scrollback { ring: vec![None; cap], start: 0, count: 0 }
    }
    fn count(&self) -> usize {
        self.count
    }
    fn get(&self, i: usize) -> &Vec<Cell> {
        let cap = self.ring.len();
        self.ring[(self.start + i) % cap].as_ref().unwrap()
    }
    fn push(&mut self, line: Vec<Cell>) -> Option<Vec<Cell>> {
        let cap = self.ring.len();
        if self.count < cap {
            self.ring[(self.start + self.count) % cap] = Some(line);
            self.count += 1;
            None
        } else {
            let evicted = self.ring[self.start].take();
            self.ring[self.start] = Some(line);
            self.start = (self.start + 1) % cap;
            evicted
        }
    }
    fn clear(&mut self) {
        for s in self.ring.iter_mut() {
            *s = None;
        }
        self.start = 0;
        self.count = 0;
    }
}

pub struct Screen {
    pub cols: usize,
    pub rows: usize,

    buf_alt: bool, // false => primary is active, true => alt
    primary: Vec<Vec<Cell>>,
    alt: Vec<Vec<Cell>>,

    pub cx: usize,
    pub cy: usize,
    wrap_pending: bool,

    fg: i32,
    bg: i32,
    flags: CellFlags,

    top: usize,
    bot: usize,

    saved: Saved,
    tabs: Vec<bool>,
    scrollback: Scrollback,
    /// Combining-mark strings; a cell's `combo` is a 1-based index here (0 = none).
    /// Kept off the cell so `Cell` stays `Copy`. Grows only when combining marks
    /// appear (rare); bounded so an adversarial stream can't grow it without end.
    combo_pool: Vec<String>,

    last_base_x: isize,
    last_base_y: isize,

    // modes
    pub auto_wrap: bool,
    pub origin_mode: bool,
    pub cursor_visible: bool,
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub insert_mode: bool,
    pub mouse: MouseTracking,
    pub mouse_sgr: bool,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let mut s = Screen {
            cols: 0,
            rows: 0,
            buf_alt: false,
            primary: Vec::new(),
            alt: Vec::new(),
            cx: 0,
            cy: 0,
            wrap_pending: false,
            fg: color::DEFAULT,
            bg: color::DEFAULT,
            flags: CellFlags::default(),
            top: 0,
            bot: 0,
            saved: Saved { cx: 0, cy: 0, fg: color::DEFAULT, bg: color::DEFAULT, fl: CellFlags::default(), origin: false },
            tabs: Vec::new(),
            scrollback: Scrollback::new(5000),
            combo_pool: Vec::new(),
            last_base_x: -1,
            last_base_y: -1,
            auto_wrap: true,
            origin_mode: false,
            cursor_visible: true,
            app_cursor_keys: false,
            app_keypad: false,
            bracketed_paste: false,
            insert_mode: false,
            mouse: MouseTracking::None,
            mouse_sgr: false,
        };
        s.resize(cols, rows);
        s
    }

    #[inline]
    pub fn scrollback_count(&self) -> usize {
        self.scrollback.count()
    }
    #[inline]
    pub fn in_alt(&self) -> bool {
        self.buf_alt
    }
    #[inline]
    fn buf(&self) -> &Vec<Vec<Cell>> {
        if self.buf_alt { &self.alt } else { &self.primary }
    }
    #[inline]
    fn buf_mut(&mut self) -> &mut Vec<Vec<Cell>> {
        if self.buf_alt { &mut self.alt } else { &mut self.primary }
    }

    fn blank_cell(&self) -> Cell {
        Cell { rune: 0, fg: color::DEFAULT, bg: self.bg, flags: CellFlags::default(), width: 1, combo: 0 }
    }
    fn blank_line(&self) -> Vec<Cell> {
        vec![self.blank_cell(); self.cols]
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);

        let rebuild = |old: &Vec<Vec<Cell>>| -> Vec<Vec<Cell>> {
            let old_cols = if old.is_empty() { 0 } else { old[0].len() };
            let mut nb = Vec::with_capacity(rows);
            for r in 0..rows {
                let mut line = Vec::with_capacity(cols);
                for c in 0..cols {
                    if r < old.len() && c < old_cols {
                        line.push(old[r][c].clone());
                    } else {
                        line.push(Cell { rune: 0, fg: color::DEFAULT, bg: color::DEFAULT, flags: CellFlags::default(), width: 1, combo: 0 });
                    }
                }
                nb.push(line);
            }
            nb
        };

        self.primary = rebuild(&self.primary);
        self.alt = rebuild(&self.alt);
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bot = rows - 1;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.wrap_pending = false;
        self.tabs = (0..cols).map(|i| i % 8 == 0).collect();
    }

    // ---- writing -----------------------------------------------------------

    pub fn put_rune(&mut self, cp: u32) {
        let w = width_of(cp);
        if w == 0 {
            // combining / zero-width: attach to the base cell
            if self.last_base_x >= 0 && self.last_base_y >= 0 {
                let (bx, by) = (self.last_base_x as usize, self.last_base_y as usize);
                if by < self.rows && bx < self.cols {
                    let base = self.buf()[by][bx]; // Cell is Copy
                    if base.rune != 0 {
                        if let Some(ch) = char::from_u32(cp) {
                            if base.combo == 0 {
                                if self.combo_pool.len() < 1_000_000 {
                                    self.combo_pool.push(ch.to_string());
                                    let id = self.combo_pool.len() as u32; // 1-based
                                    self.buf_mut()[by][bx].combo = id;
                                }
                            } else if let Some(s) = self.combo_pool.get_mut((base.combo - 1) as usize) {
                                if s.chars().count() < 16 {
                                    s.push(ch);
                                }
                            }
                        }
                    }
                }
            }
            return;
        }

        if self.wrap_pending {
            self.wrap_pending = false;
            self.cx = 0;
            self.index();
        }
        if w == 2 && self.cx == self.cols - 1 && self.auto_wrap {
            self.cx = 0;
            self.index();
        }

        let (fg, bg, flags) = (self.fg, self.bg, self.flags);
        let cx = self.cx;
        let cols = self.cols;
        let insert = self.insert_mode;
        let cy = self.cy;
        let row = &mut self.buf_mut()[cy];
        if insert {
            let mut c = cols - 1;
            while c >= cx + w as usize {
                row[c] = row[c - w as usize].clone();
                c -= 1;
            }
        }
        row[cx] = Cell { rune: cp, fg, bg, flags, width: w, combo: 0 };
        if w == 2 && cx + 1 < cols {
            row[cx + 1] = Cell { rune: 0, fg, bg, flags, width: 0, combo: 0 };
        }

        self.last_base_x = cx as isize;
        self.last_base_y = self.cy as isize;
        self.cx += w as usize;
        if self.cx >= self.cols {
            self.cx = self.cols - 1;
            if self.auto_wrap {
                self.wrap_pending = true;
            }
        }
    }

    pub fn carriage_return(&mut self) {
        self.cx = 0;
        self.wrap_pending = false;
    }
    pub fn backspace(&mut self) {
        if self.cx > 0 {
            self.cx -= 1;
        }
        self.wrap_pending = false;
    }
    pub fn tab(&mut self) {
        self.wrap_pending = false;
        if self.cx >= self.cols - 1 {
            return;
        }
        loop {
            self.cx += 1;
            if self.cx >= self.cols - 1 || self.tabs[self.cx] {
                break;
            }
        }
    }
    pub fn set_tab_stop(&mut self) {
        if self.cx < self.cols {
            self.tabs[self.cx] = true;
        }
    }
    pub fn clear_tab_stop(&mut self) {
        if self.cx < self.cols {
            self.tabs[self.cx] = false;
        }
    }
    pub fn clear_all_tabs(&mut self) {
        for t in self.tabs.iter_mut() {
            *t = false;
        }
    }

    pub fn index(&mut self) {
        self.wrap_pending = false;
        if self.cy == self.bot {
            self.scroll_up(1);
        } else if self.cy < self.rows - 1 {
            self.cy += 1;
        }
    }
    pub fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cy == self.top {
            self.scroll_down(1);
        } else if self.cy > 0 {
            self.cy -= 1;
        }
    }
    pub fn next_line(&mut self) {
        self.carriage_return();
        self.index();
    }

    pub fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        for _ in 0..n {
            let top = self.top;
            let leaving = std::mem::take(&mut self.buf_mut()[top]);
            // Recycle a row: the line evicted from scrollback (when keeping history),
            // or the leaving line itself.
            let reuse = if !self.buf_alt && self.top == 0 {
                self.scrollback.push(leaving).unwrap_or_else(|| Vec::new())
            } else {
                leaving
            };
            let (top, bot) = (self.top, self.bot);
            for r in top..bot {
                self.buf_mut().swap(r, r + 1);
            }
            let cleared = self.clear_line(reuse);
            self.buf_mut()[bot] = cleared;
        }
    }

    fn clear_line(&self, mut line: Vec<Cell>) -> Vec<Cell> {
        let blank = self.blank_cell();
        if line.len() != self.cols {
            line = vec![blank; self.cols];
        } else {
            line.fill(blank); // Cell is Copy: a fast value-fill, no per-cell drop
        }
        line
    }

    pub fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bot - self.top + 1);
        for _ in 0..n {
            let (top, bot) = (self.top, self.bot);
            let mut r = bot;
            while r > top {
                self.buf_mut().swap(r, r - 1);
                r -= 1;
            }
            let bl = self.blank_line();
            self.buf_mut()[top] = bl;
        }
    }

    // ---- cursor movement ---------------------------------------------------

    fn region_top(&self) -> usize {
        if self.origin_mode { self.top } else { 0 }
    }
    fn region_bot(&self) -> usize {
        if self.origin_mode { self.bot } else { self.rows - 1 }
    }

    pub fn cursor_up(&mut self, n: usize) {
        let rt = self.region_top();
        self.cy = self.cy.saturating_sub(n.max(1)).max(rt);
        self.wrap_pending = false;
    }
    pub fn cursor_down(&mut self, n: usize) {
        self.cy = (self.cy + n.max(1)).min(self.region_bot());
        self.wrap_pending = false;
    }
    pub fn cursor_fwd(&mut self, n: usize) {
        self.cx = (self.cx + n.max(1)).min(self.cols - 1);
        self.wrap_pending = false;
    }
    pub fn cursor_back(&mut self, n: usize) {
        self.cx = self.cx.saturating_sub(n.max(1));
        self.wrap_pending = false;
    }
    pub fn cursor_col(&mut self, col: i64) {
        self.cx = col.clamp(0, self.cols as i64 - 1) as usize;
        self.wrap_pending = false;
    }
    pub fn cursor_row(&mut self, row: i64) {
        let rt = self.region_top() as i64;
        let target = if self.origin_mode { rt + row } else { row };
        self.cy = target.clamp(0, self.rows as i64 - 1) as usize;
        self.wrap_pending = false;
    }
    pub fn cursor_to(&mut self, row: i64, col: i64) {
        let (top, bot) = (self.region_top() as i64, self.region_bot() as i64);
        self.cy = (top + row).clamp(top, bot) as usize;
        self.cx = col.clamp(0, self.cols as i64 - 1) as usize;
        self.wrap_pending = false;
    }

    // ---- erasing -----------------------------------------------------------

    pub fn erase_in_line(&mut self, mode: i32) {
        let blank = self.blank_cell();
        let (cx, cols, cy) = (self.cx, self.cols, self.cy);
        let row = &mut self.buf_mut()[cy];
        match mode {
            0 => for c in cx..cols { row[c] = blank.clone(); },
            1 => for c in 0..=cx.min(cols - 1) { row[c] = blank.clone(); },
            2 => for c in 0..cols { row[c] = blank.clone(); },
            _ => {}
        }
        self.wrap_pending = false;
    }

    pub fn erase_in_display(&mut self, mode: i32) {
        let blank = self.blank_cell();
        match mode {
            0 => {
                self.erase_in_line(0);
                let (cy, rows) = (self.cy, self.rows);
                for r in (cy + 1)..rows {
                    self.fill_row(r, &blank);
                }
            }
            1 => {
                for r in 0..self.cy {
                    self.fill_row(r, &blank);
                }
                self.erase_in_line(1);
            }
            2 => {
                for r in 0..self.rows {
                    self.fill_row(r, &blank);
                }
            }
            3 => self.scrollback.clear(),
            _ => {}
        }
        self.wrap_pending = false;
    }

    fn fill_row(&mut self, r: usize, blank: &Cell) {
        let cols = self.cols;
        let row = &mut self.buf_mut()[r];
        for c in 0..cols {
            row[c] = blank.clone();
        }
    }

    pub fn erase_chars(&mut self, n: usize) {
        let blank = self.blank_cell();
        let (cx, cols, cy) = (self.cx, self.cols, self.cy);
        let row = &mut self.buf_mut()[cy];
        let end = (cx + n.max(1)).min(cols);
        for c in cx..end {
            row[c] = blank.clone();
        }
    }

    pub fn insert_chars(&mut self, n: usize) {
        let n = n.clamp(1, self.cols - self.cx);
        let blank = self.blank_cell();
        let (cx, cols, cy) = (self.cx, self.cols, self.cy);
        let row = &mut self.buf_mut()[cy];
        let mut c = cols - 1;
        while c >= cx + n {
            row[c] = row[c - n].clone();
            c -= 1;
        }
        for c in cx..cx + n {
            row[c] = blank.clone();
        }
    }

    pub fn delete_chars(&mut self, n: usize) {
        let n = n.clamp(1, self.cols - self.cx);
        let blank = self.blank_cell();
        let (cx, cols, cy) = (self.cx, self.cols, self.cy);
        let row = &mut self.buf_mut()[cy];
        for c in cx..cols - n {
            row[c] = row[c + n].clone();
        }
        for c in cols - n..cols {
            row[c] = blank.clone();
        }
    }

    pub fn insert_lines(&mut self, n: usize) {
        if self.cy < self.top || self.cy > self.bot {
            return;
        }
        let n = n.clamp(1, self.bot - self.cy + 1);
        for _ in 0..n {
            let (cy, bot) = (self.cy, self.bot);
            let mut r = bot;
            while r > cy {
                self.buf_mut().swap(r, r - 1);
                r -= 1;
            }
            let bl = self.blank_line();
            self.buf_mut()[cy] = bl;
        }
    }

    pub fn delete_lines(&mut self, n: usize) {
        if self.cy < self.top || self.cy > self.bot {
            return;
        }
        let n = n.clamp(1, self.bot - self.cy + 1);
        for _ in 0..n {
            let (cy, bot) = (self.cy, self.bot);
            for r in cy..bot {
                self.buf_mut().swap(r, r + 1);
            }
            let bl = self.blank_line();
            self.buf_mut()[bot] = bl;
        }
    }

    // ---- scroll region / cursor save --------------------------------------

    pub fn set_scroll_region(&mut self, top: i32, bottom: i32) {
        let mut top = if top <= 0 { 1 } else { top };
        let mut bottom = if bottom <= 0 || bottom > self.rows as i32 { self.rows as i32 } else { bottom };
        if top >= bottom {
            top = 1;
            bottom = self.rows as i32;
        }
        self.top = (top - 1) as usize;
        self.bot = (bottom - 1) as usize;
        self.cursor_to(0, 0);
    }

    pub fn save_cursor(&mut self) {
        self.saved = Saved { cx: self.cx, cy: self.cy, fg: self.fg, bg: self.bg, fl: self.flags, origin: self.origin_mode };
    }
    pub fn restore_cursor(&mut self) {
        self.cx = self.saved.cx.min(self.cols - 1);
        self.cy = self.saved.cy.min(self.rows - 1);
        self.fg = self.saved.fg;
        self.bg = self.saved.bg;
        self.flags = self.saved.fl;
        self.origin_mode = self.saved.origin;
        self.wrap_pending = false;
    }

    pub fn full_reset(&mut self) {
        self.fg = color::DEFAULT;
        self.bg = color::DEFAULT;
        self.flags = CellFlags::default();
        self.auto_wrap = true;
        self.origin_mode = false;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.app_keypad = false;
        self.bracketed_paste = false;
        self.insert_mode = false;
        self.mouse = MouseTracking::None;
        self.mouse_sgr = false;
        self.top = 0;
        self.bot = self.rows - 1;
        self.cx = 0;
        self.cy = 0;
        self.wrap_pending = false;
        if self.buf_alt {
            self.set_alt_screen(false);
        }
        self.erase_in_display(2);
        self.scrollback.clear();
        self.combo_pool.clear(); // no live cell references a combo after a reset
    }

    // ---- alternate screen --------------------------------------------------

    pub fn set_alt_screen(&mut self, on: bool) {
        if on == self.buf_alt {
            return;
        }
        if on {
            self.save_cursor();
            self.buf_alt = true;
            self.erase_in_display(2);
            self.cx = 0;
            self.cy = 0;
            self.wrap_pending = false;
        } else {
            self.buf_alt = false;
            self.restore_cursor();
        }
    }

    // ---- SGR ---------------------------------------------------------------

    pub fn set_graphics(&mut self, p: &[i32], colon: &[bool]) {
        if p.is_empty() {
            self.reset_pen();
            return;
        }
        let mut i = 0;
        while i < p.len() {
            // Skip ':' sub-parameters — they belong to the preceding code
            // (the "3" in 4:3, the channels of 38:2:r:g:b). This was the
            // underline-leak fix.
            if i < colon.len() && colon[i] {
                i += 1;
                continue;
            }
            let n = p[i];
            match n {
                0 => self.reset_pen(),
                1 => self.flags.insert(CellFlags::BOLD),
                2 => self.flags.insert(CellFlags::FAINT),
                3 => self.flags.insert(CellFlags::ITALIC),
                4 => {
                    // 4 = underline on; 4:x = styled underline (x=0 off, else on)
                    if i + 1 < p.len() && i + 1 < colon.len() && colon[i + 1] && p[i + 1] == 0 {
                        self.flags.remove(CellFlags::UNDERLINE);
                    } else {
                        self.flags.insert(CellFlags::UNDERLINE);
                    }
                }
                5 | 6 => self.flags.insert(CellFlags::BLINK),
                7 => self.flags.insert(CellFlags::REVERSE),
                8 => self.flags.insert(CellFlags::HIDDEN),
                9 => self.flags.insert(CellFlags::STRIKE),
                21 | 22 => {
                    self.flags.remove(CellFlags::BOLD);
                    self.flags.remove(CellFlags::FAINT);
                }
                23 => self.flags.remove(CellFlags::ITALIC),
                24 => self.flags.remove(CellFlags::UNDERLINE),
                25 => self.flags.remove(CellFlags::BLINK),
                27 => self.flags.remove(CellFlags::REVERSE),
                28 => self.flags.remove(CellFlags::HIDDEN),
                29 => self.flags.remove(CellFlags::STRIKE),
                39 => self.fg = color::DEFAULT,
                49 => self.bg = color::DEFAULT,
                38 => i = ext_color(p, i, &mut self.fg),
                48 => i = ext_color(p, i, &mut self.bg),
                _ => {
                    if (30..=37).contains(&n) {
                        self.fg = n - 30;
                    } else if (40..=47).contains(&n) {
                        self.bg = n - 40;
                    } else if (90..=97).contains(&n) {
                        self.fg = (n - 90) + 8;
                    } else if (100..=107).contains(&n) {
                        self.bg = (n - 100) + 8;
                    }
                }
            }
            i += 1;
        }
    }

    fn reset_pen(&mut self) {
        self.fg = color::DEFAULT;
        self.bg = color::DEFAULT;
        self.flags = CellFlags::default();
    }

    // ---- snapshot for rendering -------------------------------------------

    /// Copy the visible viewport (scrolled back by `off` lines) into `dest`
    /// (rows × cols). `dest` is resized to match.
    pub fn copy_viewport(&self, off: usize, dest: &mut Vec<Vec<Cell>>) {
        let off = off.min(self.scrollback.count());
        let top_abs = self.scrollback.count() - off;
        dest.clear();
        for r in 0..self.rows {
            let abs = top_abs + r;
            let src = if abs < self.scrollback.count() {
                self.scrollback.get(abs)
            } else {
                &self.buf()[abs - self.scrollback.count()]
            };
            dest.push(src.clone());
        }
    }

    // ---- text extraction --------------------------------------------------

    pub fn total_rows(&self) -> usize {
        self.scrollback.count() + self.rows
    }

    fn abs_line(&self, abs: i64) -> Option<&Vec<Cell>> {
        if abs < 0 {
            return None;
        }
        let abs = abs as usize;
        if abs < self.scrollback.count() {
            Some(self.scrollback.get(abs))
        } else {
            let i = abs - self.scrollback.count();
            if i < self.rows {
                Some(&self.buf()[i])
            } else {
                None
            }
        }
    }

    pub fn get_text(&self, mut r1: i64, mut c1: i64, mut r2: i64, mut c2: i64) -> String {
        if r1 > r2 || (r1 == r2 && c1 > c2) {
            std::mem::swap(&mut r1, &mut r2);
            std::mem::swap(&mut c1, &mut c2);
        }
        let mut out = String::new();
        for r in r1..=r2 {
            let start = if r == r1 { c1.max(0) } else { 0 };
            let end = if r == r2 { c2.min(self.cols as i64 - 1) } else { self.cols as i64 - 1 };
            let mut row = String::new();
            if let Some(line) = self.abs_line(r) {
                let mut c = start;
                while c <= end {
                    let cell = &line[c as usize];
                    if cell.width == 0 {
                        c += 1;
                        continue;
                    }
                    if cell.rune == 0 {
                        row.push(' ');
                    } else if let Some(ch) = char::from_u32(cell.rune) {
                        row.push(ch);
                    }
                    if cell.combo != 0 {
                        if let Some(s) = self.combo_pool.get((cell.combo - 1) as usize) {
                            row.push_str(s);
                        }
                    }
                    c += 1;
                }
            }
            out.push_str(row.trim_end_matches(' '));
            if r < r2 {
                out.push_str("\r\n");
            }
        }
        out
    }

    pub fn word_span(&self, abs_row: i64, col: i64) -> Option<(usize, usize)> {
        let line = self.abs_line(abs_row)?;
        if col < 0 || col >= self.cols as i64 {
            return None;
        }
        let col = col as usize;
        let mut s = col;
        let mut e = col;
        if !is_word_char(line[col].rune) {
            return Some((s, e)); // single non-word cell
        }
        while s > 0 && is_word_char(line[s - 1].rune) {
            s -= 1;
        }
        while e < self.cols - 1 && is_word_char(line[e + 1].rune) {
            e += 1;
        }
        Some((s, e))
    }
}

fn ext_color(p: &[i32], i: usize, slot: &mut i32) -> usize {
    if i + 1 >= p.len() {
        return i;
    }
    let kind = p[i + 1];
    if kind == 5 && i + 2 < p.len() {
        *slot = p[i + 2] & 0xFF;
        return i + 2;
    }
    if kind == 2 && i + 4 < p.len() {
        *slot = color::rgb(p[i + 2] as u8, p[i + 3] as u8, p[i + 4] as u8);
        return i + 4;
    }
    i + 1
}

fn is_word_char(cp: u32) -> bool {
    if cp > 0x7f {
        return true;
    }
    if cp <= b' ' as u32 {
        return false;
    }
    let ch = cp as u8 as char;
    ch.is_alphanumeric() || "._-/~+@:".contains(ch)
}
