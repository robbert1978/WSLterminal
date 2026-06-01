//! Thread-unsafe front end over `Screen` + `VtParser`. Ports the relevant parts
//! of `src/WslTerminal/Vt/Terminal.cs`. The GUI/owner serializes access (the C#
//! version used a lock); responses and cwd are drained after each `feed`.

use crate::cell::Cell;
use crate::parser::{ParserSinks, VtParser};
use crate::screen::Screen;

pub struct Terminal {
    screen: Screen,
    parser: VtParser,
    sinks: ParserSinks,
    cwd: Option<String>,
    title: Option<String>,
    /// Bytes the emulator owes the PTY (DSR/DA replies). Drain after `feed`.
    pub respond: Vec<u8>,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Terminal {
            screen: Screen::new(cols, rows),
            parser: VtParser::new(),
            sinks: ParserSinks::default(),
            cwd: None,
            title: None,
            respond: Vec::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.sinks.respond.clear();
        self.sinks.title = None;
        self.sinks.cwd = None;
        self.parser.parse(&mut self.screen, &mut self.sinks, data);
        if !self.sinks.respond.is_empty() {
            self.respond.extend_from_slice(&self.sinks.respond);
        }
        if let Some(t) = self.sinks.title.take() {
            self.title = Some(t);
        }
        if let Some(c) = self.sinks.cwd.take() {
            self.cwd = Some(c);
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }

    pub fn cols(&self) -> usize {
        self.screen.cols
    }
    pub fn rows(&self) -> usize {
        self.screen.rows
    }
    pub fn scrollback_count(&self) -> usize {
        self.screen.scrollback_count()
    }
    pub fn current_directory(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }
    pub fn app_cursor_keys(&self) -> bool {
        self.screen.app_cursor_keys
    }
    pub fn cursor_visible(&self) -> bool {
        self.screen.cursor_visible
    }
    pub fn bracketed_paste(&self) -> bool {
        self.screen.bracketed_paste
    }

    /// Snapshot the visible viewport into a rows×cols grid for rendering.
    pub fn capture_viewport(&self, off: usize, dest: &mut Vec<Vec<Cell>>) {
        self.screen.copy_viewport(off, dest);
    }

    pub fn get_text(&self, r1: i64, c1: i64, r2: i64, c2: i64) -> String {
        self.screen.get_text(r1, c1, r2, c2)
    }
    pub fn word_span(&self, abs_row: i64, col: i64) -> Option<(usize, usize)> {
        self.screen.word_span(abs_row, col)
    }

    // direct cursor access (parity with C# Cx/Cy used by tests)
    pub fn cx(&self) -> usize {
        self.screen.cx
    }
    pub fn cy(&self) -> usize {
        self.screen.cy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::CellFlags;
    use crate::color;

    // Mirrors VtSelfTest.Case: feed bytes, snapshot the grid, run an assertion.
    fn grid(cols: usize, rows: usize, feed: &str) -> (Terminal, Vec<Vec<Cell>>) {
        let mut t = Terminal::new(cols, rows);
        t.feed(feed.as_bytes());
        let mut g = Vec::new();
        t.capture_viewport(0, &mut g);
        (t, g)
    }

    #[test]
    fn plain_text() {
        let (_t, g) = grid(20, 6, "hello");
        assert_eq!(g[0][0].rune, 'h' as u32);
        assert_eq!(g[0][1].rune, 'e' as u32);
        assert_eq!(g[0][4].rune, 'o' as u32);
    }

    #[test]
    fn sgr_fg_and_reset() {
        let (_t, g) = grid(20, 6, "\x1b[31mR\x1b[0mG");
        assert_eq!(g[0][0].rune, 'R' as u32);
        assert_eq!(g[0][0].fg, 1);
        assert_eq!(g[0][1].rune, 'G' as u32);
        assert_eq!(g[0][1].fg, color::DEFAULT);
    }

    #[test]
    fn truecolor_sgr() {
        let (_t, g) = grid(20, 6, "\x1b[38;2;10;20;30mX");
        assert_eq!(g[0][0].rune, 'X' as u32);
        assert_eq!(g[0][0].fg, color::rgb(10, 20, 30));
    }

    #[test]
    fn sgr_4_3_styled_underline() {
        let (_t, g) = grid(20, 6, "\x1b[4:3mU");
        assert!(g[0][0].flags.contains(CellFlags::UNDERLINE));
        assert!(!g[0][0].flags.contains(CellFlags::ITALIC));
    }

    #[test]
    fn sgr_4_0_underline_off() {
        let (_t, g) = grid(20, 6, "\x1b[4:3mA\x1b[4:0mB");
        assert!(g[0][0].flags.contains(CellFlags::UNDERLINE));
        assert!(!g[0][1].flags.contains(CellFlags::UNDERLINE));
    }

    #[test]
    fn sgr_24_underline_off() {
        let (_t, g) = grid(20, 6, "\x1b[4mA\x1b[24mB");
        assert!(g[0][0].flags.contains(CellFlags::UNDERLINE));
        assert!(!g[0][1].flags.contains(CellFlags::UNDERLINE));
    }

    #[test]
    fn sgr_colon_truecolor() {
        let (_t, g) = grid(20, 6, "\x1b[38:2:10:20:30mX");
        assert_eq!(g[0][0].rune, 'X' as u32);
        assert_eq!(g[0][0].fg, color::rgb(10, 20, 30));
        assert!(g[0][0].flags.is_empty());
    }

    #[test]
    fn csi_gt4m_is_not_underline() {
        let (_t, g) = grid(20, 6, "\x1b[>4mX");
        assert_eq!(g[0][0].rune, 'X' as u32);
        assert!(!g[0][0].flags.contains(CellFlags::UNDERLINE));
    }

    #[test]
    fn cr_erase_line_write() {
        let (_t, g) = grid(20, 6, "ABC\r\x1b[KZ");
        assert_eq!(g[0][0].rune, 'Z' as u32);
        assert_eq!(g[0][1].rune, 0);
        assert_eq!(g[0][2].rune, 0);
    }

    #[test]
    fn cup_row_col() {
        let (_t, g) = grid(20, 6, "\x1b[2;3HX");
        assert_eq!(g[1][2].rune, 'X' as u32);
    }

    #[test]
    fn autowrap() {
        let (_t, g) = grid(4, 4, "ABCDE");
        assert_eq!(g[0][0].rune, 'A' as u32);
        assert_eq!(g[0][3].rune, 'D' as u32);
        assert_eq!(g[1][0].rune, 'E' as u32);
    }

    #[test]
    fn utf8_wide_char() {
        let (_t, g) = grid(20, 6, "世a");
        assert_eq!(g[0][0].rune, 0x4E16);
        assert_eq!(g[0][0].width, 2);
        assert_eq!(g[0][1].width, 0);
        assert_eq!(g[0][2].rune, 'a' as u32);
    }

    #[test]
    fn dec_line_drawing() {
        let (_t, g) = grid(20, 6, "\x1b(0q\x1b(B");
        assert_eq!(g[0][0].rune, 0x2500);
    }

    #[test]
    fn scroll_and_scrollback() {
        let (t, g) = grid(10, 3, "1\r\n2\r\n3\r\n4");
        assert_eq!(g[0][0].rune, '2' as u32);
        assert_eq!(g[1][0].rune, '3' as u32);
        assert_eq!(g[2][0].rune, '4' as u32);
        assert!(t.scrollback_count() >= 1);
    }

    #[test]
    fn insert_delete_chars() {
        let (_t, g) = grid(10, 3, "ABCD\x1b[1G\x1b[2@");
        assert_eq!(g[0][0].rune, 0);
        assert_eq!(g[0][1].rune, 0);
        assert_eq!(g[0][2].rune, 'A' as u32);
        assert_eq!(g[0][3].rune, 'B' as u32);
    }

    #[test]
    fn dsr_cursor_pos_reply() {
        let mut t = Terminal::new(20, 6);
        t.feed(b"\x1b[6n");
        assert_eq!(t.respond, b"\x1b[1;1R");
    }

    #[test]
    fn alt_screen_isolation() {
        let (_t, g) = grid(10, 4, "main\x1b[?1049h\x1b[2J\x1b[?1049l");
        assert_eq!(g[0][0].rune, 'm' as u32);
        assert_eq!(g[0][3].rune, 'n' as u32);
    }

    #[test]
    fn osc7_working_directory() {
        let (t, _g) = grid(20, 4, "\x1b]7;file://Agartha/home/robbert/proj\x1b\\");
        assert_eq!(t.current_directory(), Some("/home/robbert/proj"));
    }

    #[test]
    fn selection_word() {
        let (t, _g) = grid(20, 4, "hello world");
        assert_eq!(t.get_text(0, 0, 0, 4), "hello");
    }

    #[test]
    fn selection_trims_trailing() {
        let (t, _g) = grid(20, 4, "hello world");
        assert_eq!(t.get_text(0, 6, 0, 19), "world");
    }

    #[test]
    fn selection_multiline_crlf() {
        let (t, _g) = grid(20, 4, "ab\r\ncd");
        assert_eq!(t.get_text(0, 0, 1, 1), "ab\r\ncd");
    }

    #[test]
    fn double_click_word_span() {
        let (t, _g) = grid(20, 2, "foo bar");
        assert_eq!(t.word_span(0, 1), Some((0, 2)));
    }
}
