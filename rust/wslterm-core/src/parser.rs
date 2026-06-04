//! Incremental VT/ANSI parser. Ports `src/WslTerminal/Vt/VtParser.cs`.
//! Bytes are UTF-8 decoded and dispatched to the `Screen`. State persists across
//! `parse` calls so split sequences work. Responses (DSR/DA), title, and cwd
//! (OSC 7) are surfaced through `ParserSinks`.

use crate::screen::{MouseTracking, Screen};

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Ground,
    Esc,
    EscInt,
    Csi,
    CsiInt,
    Osc,
    OscEsc,
    Str,
}

/// Side-effect outputs collected during a parse: bytes to send back to the PTY,
/// the latest window title, and the latest OSC-7 working directory.
#[derive(Default)]
pub struct ParserSinks {
    pub respond: Vec<u8>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Text an app asked to put on the system clipboard via OSC 52 (e.g. zellij /
    /// tmux / vim `copy_on_select` / yank). The owner writes it to the OS clipboard.
    pub clipboard: Option<String>,
}

pub struct VtParser {
    state: St,
    cp: u32,
    need: i32,

    params: Vec<i32>,
    params_colon: Vec<bool>,
    pending_colon: bool,
    cur: i32,
    cur_has: bool,
    priv_: char,
    inter: char,

    esc_inter: char,
    // OSC payload accumulated as raw bytes; decoded as UTF-8 only at osc_done.
    // (Pushing `b as char` here would treat each UTF-8 byte as Latin-1, turning a
    // multibyte title like the spinner U+2833 into mojibake "â ³".)
    osc: Vec<u8>,

    g0_special: bool,
    g1_special: bool,
    shift_out: bool,
}

impl VtParser {
    pub fn new() -> Self {
        VtParser {
            state: St::Ground,
            cp: 0,
            need: 0,
            params: Vec::new(),
            params_colon: Vec::new(),
            pending_colon: false,
            cur: 0,
            cur_has: false,
            priv_: '\0',
            inter: '\0',
            esc_inter: '\0',
            osc: Vec::new(),
            g0_special: false,
            g1_special: false,
            shift_out: false,
        }
    }

    pub fn parse(&mut self, s: &mut Screen, sinks: &mut ParserSinks, data: &[u8]) {
        let n = data.len();
        let mut i = 0;
        while i < n {
            // Fast path: ground state, no pending UTF-8 — blast a run of plain
            // printable ASCII straight to the screen.
            if self.state == St::Ground && self.need == 0 {
                let b = data[i];
                if (0x20..0x7f).contains(&b) {
                    let start = i;
                    i += 1;
                    while i < n && (0x20..0x7f).contains(&data[i]) {
                        i += 1;
                    }
                    for k in start..i {
                        self.print(s, data[k] as u32);
                    }
                    continue;
                }
            }
            self.step(s, sinks, data[i]);
            i += 1;
        }
    }

    fn step(&mut self, s: &mut Screen, sinks: &mut ParserSinks, b: u8) {
        match self.state {
            St::Ground => self.ground(s, b),
            St::Esc => self.escape(s, b),
            St::EscInt => self.escape_intermediate(b),
            St::Csi => self.csi(s, sinks, b),
            St::CsiInt => self.csi_intermediate(s, sinks, b),
            St::Osc => self.osc(sinks, b),
            St::OscEsc => self.osc_esc(s, sinks, b),
            St::Str => self.str_consume(b),
        }
    }

    fn ground(&mut self, s: &mut Screen, b: u8) {
        if self.need > 0 {
            if b & 0xC0 == 0x80 {
                self.cp = (self.cp << 6) | (b as u32 & 0x3F);
                self.need -= 1;
                if self.need == 0 {
                    self.print(s, self.cp);
                }
                return;
            }
            self.need = 0; // malformed; reinterpret b
        }

        if b < 0x80 {
            if b < 0x20 || b == 0x7f {
                self.c0(s, b);
                return;
            }
            self.print(s, b as u32);
        } else if b & 0xE0 == 0xC0 {
            self.cp = (b & 0x1F) as u32;
            self.need = 1;
        } else if b & 0xF0 == 0xE0 {
            self.cp = (b & 0x0F) as u32;
            self.need = 2;
        } else if b & 0xF8 == 0xF0 {
            self.cp = (b & 0x07) as u32;
            self.need = 3;
        } else {
            self.print(s, 0xFFFD);
        }
    }

    fn c0(&mut self, s: &mut Screen, b: u8) {
        match b {
            0x07 => {}
            0x08 => s.backspace(),
            0x09 => s.tab(),
            0x0A | 0x0B | 0x0C => s.index(),
            0x0D => s.carriage_return(),
            0x0E => self.shift_out = true,
            0x0F => self.shift_out = false,
            0x1B => self.state = St::Esc,
            _ => {}
        }
    }

    fn print(&mut self, s: &mut Screen, mut cp: u32) {
        let special = if self.shift_out { self.g1_special } else { self.g0_special };
        if special && (0x60..=0x7e).contains(&cp) {
            cp = DEC_SPECIAL[(cp - 0x60) as usize];
        }
        s.put_rune(cp);
    }

    fn escape(&mut self, s: &mut Screen, b: u8) {
        let c = b as char;
        match c {
            '[' => {
                self.reset_csi();
                self.state = St::Csi;
            }
            ']' => {
                self.osc.clear();
                self.state = St::Osc;
            }
            'P' | 'X' | '^' | '_' => self.state = St::Str,
            '(' | ')' | '*' | '+' => {
                self.esc_inter = c;
                self.state = St::EscInt;
            }
            'M' => {
                s.reverse_index();
                self.state = St::Ground;
            }
            'D' => {
                s.index();
                self.state = St::Ground;
            }
            'E' => {
                s.next_line();
                self.state = St::Ground;
            }
            '7' => {
                s.save_cursor();
                self.state = St::Ground;
            }
            '8' => {
                s.restore_cursor();
                self.state = St::Ground;
            }
            'c' => {
                s.full_reset();
                self.state = St::Ground;
            }
            '=' => {
                s.app_keypad = true;
                self.state = St::Ground;
            }
            '>' => {
                s.app_keypad = false;
                self.state = St::Ground;
            }
            'H' => {
                s.set_tab_stop();
                self.state = St::Ground;
            }
            _ => self.state = St::Ground,
        }
    }

    fn escape_intermediate(&mut self, b: u8) {
        let special = b == b'0';
        let ascii = b == b'B' || b == b'A';
        if self.esc_inter == '(' {
            if special {
                self.g0_special = true;
            } else if ascii {
                self.g0_special = false;
            }
        } else if self.esc_inter == ')' {
            if special {
                self.g1_special = true;
            } else if ascii {
                self.g1_special = false;
            }
        }
        self.state = St::Ground;
    }

    fn reset_csi(&mut self) {
        self.params.clear();
        self.params_colon.clear();
        self.pending_colon = false;
        self.cur = 0;
        self.cur_has = false;
        self.priv_ = '\0';
        self.inter = '\0';
    }

    fn flush_param(&mut self, sep_after_is_colon: bool) {
        self.params.push(if self.cur_has { self.cur } else { 0 });
        self.params_colon.push(self.pending_colon);
        self.pending_colon = sep_after_is_colon;
        self.cur = 0;
        self.cur_has = false;
    }

    fn csi(&mut self, s: &mut Screen, sinks: &mut ParserSinks, b: u8) {
        let c = b as char;
        if b < 0x20 {
            self.c0(s, b);
            return;
        }
        if matches!(c, '?' | '<' | '=' | '>') {
            self.priv_ = c;
            return;
        }
        if c.is_ascii_digit() {
            self.cur = self.cur * 10 + (b - b'0') as i32;
            self.cur_has = true;
            return;
        }
        if c == ';' {
            self.flush_param(false);
            return;
        }
        if c == ':' {
            self.flush_param(true);
            return;
        }
        if (0x20..=0x2f).contains(&b) {
            self.inter = c;
            self.state = St::CsiInt;
            return;
        }
        self.flush_param(false);
        self.dispatch_csi(s, sinks, c);
        self.state = St::Ground;
    }

    fn csi_intermediate(&mut self, s: &mut Screen, sinks: &mut ParserSinks, b: u8) {
        let c = b as char;
        if (0x20..=0x2f).contains(&b) {
            self.inter = c;
            return;
        }
        self.flush_param(false);
        self.dispatch_csi(s, sinks, c);
        self.state = St::Ground;
    }

    fn p(&self, i: usize, def: i32) -> i32 {
        if i >= self.params.len() {
            return def;
        }
        if self.params[i] == 0 && def != 0 {
            def
        } else {
            self.params[i]
        }
    }

    fn dispatch_csi(&mut self, s: &mut Screen, sinks: &mut ParserSinks, f: char) {
        if self.priv_ == '?' {
            self.dec_mode(s, f);
            return;
        }
        // Private '>' '<' '=' CSI is not standard; only secondary DA is handled.
        // \e[>4m (XTMODKEYS) must NOT fall through to SGR (underline bleed bug).
        if matches!(self.priv_, '>' | '<' | '=') {
            if f == 'c' {
                self.primary_da(sinks);
            }
            return;
        }

        match f {
            'A' => s.cursor_up(self.p(0, 1) as usize),
            'B' | 'e' => s.cursor_down(self.p(0, 1) as usize),
            'C' | 'a' => s.cursor_fwd(self.p(0, 1) as usize),
            'D' => s.cursor_back(self.p(0, 1) as usize),
            'E' => {
                s.carriage_return();
                s.cursor_down(self.p(0, 1) as usize);
            }
            'F' => {
                s.carriage_return();
                s.cursor_up(self.p(0, 1) as usize);
            }
            'G' | '`' => s.cursor_col(self.p(0, 1) as i64 - 1),
            'd' => s.cursor_row(self.p(0, 1) as i64 - 1),
            'H' | 'f' => s.cursor_to(self.p(0, 1) as i64 - 1, self.p(1, 1) as i64 - 1),
            'J' => s.erase_in_display(self.p(0, 0)),
            'K' => s.erase_in_line(self.p(0, 0)),
            'L' => s.insert_lines(self.p(0, 1) as usize),
            'M' => s.delete_lines(self.p(0, 1) as usize),
            'P' => s.delete_chars(self.p(0, 1) as usize),
            '@' => s.insert_chars(self.p(0, 1) as usize),
            'X' => s.erase_chars(self.p(0, 1) as usize),
            'S' => s.scroll_up(self.p(0, 1) as usize),
            'T' => s.scroll_down(self.p(0, 1) as usize),
            'r' => {
                let bottom = if self.params.len() > 1 { self.p(1, 0) } else { 0 };
                s.set_scroll_region(self.p(0, 0), bottom);
            }
            'm' => s.set_graphics(&self.params, &self.params_colon),
            'h' => self.std_mode(s, true),
            'l' => self.std_mode(s, false),
            'n' => self.device_status(s, sinks, self.p(0, 0)),
            'c' => self.primary_da(sinks),
            'g' => {
                if self.p(0, 0) == 3 {
                    s.clear_all_tabs();
                } else {
                    s.clear_tab_stop();
                }
            }
            's' => s.save_cursor(),
            'u' => s.restore_cursor(),
            _ => {}
        }
    }

    fn std_mode(&self, s: &mut Screen, on: bool) {
        for i in 0..self.params.len().max(1) {
            if self.p(i, 0) == 4 {
                s.insert_mode = on;
            }
        }
    }

    fn dec_mode(&self, s: &mut Screen, f: char) {
        let on = f == 'h';
        if f != 'h' && f != 'l' {
            return;
        }
        for i in 0..self.params.len().max(1) {
            match self.p(i, 0) {
                1 => s.app_cursor_keys = on,
                6 => {
                    s.origin_mode = on;
                    s.cursor_to(0, 0);
                }
                7 => s.auto_wrap = on,
                25 => s.cursor_visible = on,
                9 => s.mouse = if on { MouseTracking::X10 } else { MouseTracking::None },
                1000 => s.mouse = if on { MouseTracking::Normal } else { MouseTracking::None },
                1002 => s.mouse = if on { MouseTracking::ButtonEvent } else { MouseTracking::None },
                1003 => s.mouse = if on { MouseTracking::AnyEvent } else { MouseTracking::None },
                1006 => s.mouse_sgr = on,
                47 | 1047 | 1049 => s.set_alt_screen(on),
                2004 => s.bracketed_paste = on,
                _ => {}
            }
        }
    }

    fn device_status(&self, s: &Screen, sinks: &mut ParserSinks, n: i32) {
        if n == 5 {
            sinks.respond.extend_from_slice(b"\x1b[0n");
        } else if n == 6 {
            sinks.respond.extend_from_slice(format!("\x1b[{};{}R", s.cy + 1, s.cx + 1).as_bytes());
        }
    }

    fn primary_da(&self, sinks: &mut ParserSinks) {
        if self.priv_ == '>' {
            sinks.respond.extend_from_slice(b"\x1b[>0;276;0c");
        } else {
            sinks.respond.extend_from_slice(b"\x1b[?1;2c");
        }
    }

    fn osc(&mut self, sinks: &mut ParserSinks, b: u8) {
        if b == 0x07 {
            self.osc_done(sinks);
            return;
        }
        if b == 0x1b {
            self.state = St::OscEsc;
            return;
        }
        self.osc.push(b);
    }

    fn osc_esc(&mut self, s: &mut Screen, sinks: &mut ParserSinks, b: u8) {
        if b == b'\\' {
            self.osc_done(sinks);
            return;
        }
        self.state = St::Ground;
        self.step(s, sinks, 0x1b);
        self.step(s, sinks, b);
    }

    fn osc_done(&mut self, sinks: &mut ParserSinks) {
        let bytes = std::mem::take(&mut self.osc);
        self.state = St::Ground;
        // Decode the whole payload as UTF-8 once (lossy: malformed bytes become
        // U+FFFD rather than corrupting the rest). The "num;text" split and the
        // OSC-7 file:// prefix are ASCII, so slicing the decoded &str is safe.
        let s = String::from_utf8_lossy(&bytes);
        let (num, text) = match s.find(';') {
            Some(semi) => (&s[..semi], &s[semi + 1..]),
            None => (s.as_ref(), ""),
        };
        match num {
            "0" | "1" | "2" => sinks.title = Some(text.to_string()),
            "7" => {
                if let Some(cwd) = parse_file_uri(text) {
                    sinks.cwd = Some(cwd);
                }
            }
            // OSC 52: clipboard. `text` is "<targets>;<base64|?>". A "?" payload is
            // a read request (we don't serve it); otherwise decode and surface the
            // text for the owner to put on the system clipboard.
            "52" => {
                if let Some(semi) = text.find(';') {
                    let data = &text[semi + 1..];
                    if data != "?" {
                        if let Some(b) = base64_decode(data) {
                            sinks.clipboard = Some(String::from_utf8_lossy(&b).into_owned());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn str_consume(&mut self, b: u8) {
        if b == 0x07 {
            self.state = St::Ground;
        } else if b == 0x1b {
            self.state = St::OscEsc;
            self.osc.clear();
        }
    }
}

// Decode standard base64 (OSC 52 payload). Ignores '=' padding and any
// whitespace; returns None on an invalid character.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        buf = (buf << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

// OSC 7 reports the working dir as file://HOST/PATH (percent-encoded).
fn parse_file_uri(s: &str) -> Option<String> {
    const PFX: &str = "file://";
    let rest = s.strip_prefix(PFX)?;
    let slash = rest.find('/')?;
    Some(percent_decode(&rest[slash..]))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// DEC special graphics: maps 0x60..0x7e to box-drawing/line glyphs.
const DEC_SPECIAL: [u32; 31] = [
    0x25C6, 0x2592, 0x2409, 0x240C, 0x240D, 0x240A, 0x00B0, 0x00B1, // ` a b c d e f g
    0x2424, 0x240B, 0x2518, 0x2510, 0x250C, 0x2514, 0x253C, 0x23BA, // h i j k l m n o
    0x23BB, 0x2500, 0x23BC, 0x23BD, 0x251C, 0x2524, 0x2534, 0x252C, // p q r s t u v w
    0x2502, 0x2264, 0x2265, 0x03C0, 0x2260, 0x00A3, 0x00B7, // x y z { | } ~
];
