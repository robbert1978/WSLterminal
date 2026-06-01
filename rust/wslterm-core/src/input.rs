//! Keys -> terminal byte sequences. Ports `src/WslTerminal/Ui/InputEncoder.cs`.
//!
//! Framework-neutral: the GUI maps its own key events to `Key` + `Mods` and calls
//! `encode`. Returns `None` for plain printable input (the GUI sends the typed
//! text directly, respecting layout/IME), matching the C# `Encode` returning null.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods { ctrl: false, alt: false, shift: false };
    fn code(self) -> i32 {
        1 + if self.shift { 1 } else { 0 } + if self.alt { 2 } else { 0 } + if self.ctrl { 4 } else { 0 }
    }
}

/// Keys the encoder cares about. `Char(c)` carries a letter/punctuation key for
/// Ctrl/Alt combos; plain typing returns `None` and is handled by text input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    F(u8),
    Enter,
    Tab,
    Escape,
    Backspace,
    Space,
    /// A character key (already lowercased for letters); used for Ctrl/Alt combos.
    Char(char),
}

/// Encode a key press to bytes, or `None` to let text input handle it.
pub fn encode(key: Key, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    let m = mods.code();
    let (ctrl, alt, shift) = (mods.ctrl, mods.alt, mods.shift);

    match key {
        Key::Up => return Some(cursor('A', m, app_cursor)),
        Key::Down => return Some(cursor('B', m, app_cursor)),
        Key::Right => return Some(cursor('C', m, app_cursor)),
        Key::Left => return Some(cursor('D', m, app_cursor)),
        Key::Home => return Some(cursor('H', m, app_cursor)),
        Key::End => return Some(cursor('F', m, app_cursor)),
        Key::Insert => return Some(tilde(2, m)),
        Key::Delete => return Some(tilde(3, m)),
        Key::PageUp => return Some(tilde(5, m)),
        Key::PageDown => return Some(tilde(6, m)),
        Key::F(n) => return Some(function(n, m)),
        // Enter = CR; Alt+Enter or Shift+Enter = ESC CR (meta-Enter -> newline).
        Key::Enter => return Some(if alt || shift { esc("\r") } else { ascii("\r") }),
        Key::Tab => return Some(if shift { ascii("\x1b[Z") } else { ascii("\t") }),
        Key::Escape => return Some(ascii("\x1b")),
        Key::Backspace => {
            return Some(if alt {
                esc("\x7f")
            } else if ctrl {
                ascii("\x08")
            } else {
                ascii("\x7f")
            })
        }
        Key::Space if ctrl => return Some(vec![0]),
        _ => {}
    }

    if let Key::Char(c) = key {
        if c.is_ascii_alphabetic() {
            let lower = c.to_ascii_lowercase();
            if ctrl {
                let ctl = (lower as u8 - b'a') + 1; // ^A=1..^Z=26
                return Some(if alt { vec![0x1b, ctl] } else { vec![ctl] });
            }
            if alt {
                let ch = if shift { lower.to_ascii_uppercase() } else { lower };
                return Some(esc(&ch.to_string()));
            }
        }
        if ctrl {
            // common control punctuation
            match c {
                '[' => return Some(vec![0x1b]),
                ']' => return Some(vec![0x1d]),
                '\\' => return Some(vec![0x1c]),
                '-' | '_' => return Some(vec![0x1f]),
                _ => {}
            }
        }
    }

    None // let text input produce the character
}

fn cursor(fin: char, m: i32, app: bool) -> Vec<u8> {
    if m > 1 {
        ascii(&format!("\x1b[1;{m}{fin}"))
    } else if app {
        ascii(&format!("\x1bO{fin}"))
    } else {
        ascii(&format!("\x1b[{fin}"))
    }
}

fn tilde(n: i32, m: i32) -> Vec<u8> {
    if m > 1 {
        ascii(&format!("\x1b[{n};{m}~"))
    } else {
        ascii(&format!("\x1b[{n}~"))
    }
}

fn function(n: u8, m: i32) -> Vec<u8> {
    // F1-F4 use SS3 P/Q/R/S unmodified; F5+ and all modified use CSI ~ codes.
    let (ss3, tilde_n): (Option<char>, i32) = match n {
        1 => (Some('P'), 11),
        2 => (Some('Q'), 12),
        3 => (Some('R'), 13),
        4 => (Some('S'), 14),
        5 => (None, 15),
        6 => (None, 17),
        7 => (None, 18),
        8 => (None, 19),
        9 => (None, 20),
        10 => (None, 21),
        11 => (None, 23),
        12 => (None, 24),
        _ => (None, 0),
    };
    match ss3 {
        Some(c) if m <= 1 => ascii(&format!("\x1bO{c}")),
        _ => {
            if m > 1 {
                ascii(&format!("\x1b[{tilde_n};{m}~"))
            } else {
                ascii(&format!("\x1b[{tilde_n}~"))
            }
        }
    }
}

fn ascii(s: &str) -> Vec<u8> {
    s.bytes().collect()
}
fn esc(s: &str) -> Vec<u8> {
    let mut v = vec![0x1b];
    v.extend(s.bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(ctrl: bool, alt: bool, shift: bool) -> Mods {
        Mods { ctrl, alt, shift }
    }

    #[test]
    fn enter_plain_vs_shift_alt() {
        assert_eq!(encode(Key::Enter, Mods::NONE, false), Some(vec![0x0d]));
        assert_eq!(encode(Key::Enter, m(false, false, true), false), Some(vec![0x1b, 0x0d])); // shift+enter
        assert_eq!(encode(Key::Enter, m(false, true, false), false), Some(vec![0x1b, 0x0d])); // alt+enter
    }

    #[test]
    fn arrows_normal_and_app() {
        assert_eq!(encode(Key::Up, Mods::NONE, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode(Key::Up, Mods::NONE, true), Some(b"\x1bOA".to_vec()));
        // modified -> CSI 1;mod
        assert_eq!(encode(Key::Right, m(true, false, false), false), Some(b"\x1b[1;5C".to_vec()));
    }

    #[test]
    fn ctrl_letters() {
        assert_eq!(encode(Key::Char('c'), m(true, false, false), false), Some(vec![3])); // ^C
        assert_eq!(encode(Key::Char('A'), m(true, false, false), false), Some(vec![1])); // ^A (case-insensitive)
        assert_eq!(encode(Key::Char('a'), m(true, true, false), false), Some(vec![0x1b, 1])); // alt+^A
    }

    #[test]
    fn alt_letter_is_meta_prefixed() {
        assert_eq!(encode(Key::Char('x'), m(false, true, false), false), Some(b"\x1bx".to_vec()));
        assert_eq!(encode(Key::Char('x'), m(false, true, true), false), Some(b"\x1bX".to_vec()));
    }

    #[test]
    fn tab_and_backtab() {
        assert_eq!(encode(Key::Tab, Mods::NONE, false), Some(b"\t".to_vec()));
        assert_eq!(encode(Key::Tab, m(false, false, true), false), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode(Key::F(1), Mods::NONE, false), Some(b"\x1bOP".to_vec()));
        assert_eq!(encode(Key::F(5), Mods::NONE, false), Some(b"\x1b[15~".to_vec()));
        assert_eq!(encode(Key::F(12), Mods::NONE, false), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn ctrl_space_and_backspace() {
        assert_eq!(encode(Key::Space, m(true, false, false), false), Some(vec![0]));
        assert_eq!(encode(Key::Backspace, Mods::NONE, false), Some(vec![0x7f]));
        assert_eq!(encode(Key::Backspace, m(false, true, false), false), Some(vec![0x1b, 0x7f]));
    }

    #[test]
    fn plain_char_returns_none() {
        assert_eq!(encode(Key::Char('a'), Mods::NONE, false), None);
    }
}
