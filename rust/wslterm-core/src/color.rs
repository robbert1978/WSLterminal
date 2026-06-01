//! Color handling. Ports `src/WslTerminal/Vt/Theme.cs`.
//!
//! A "color code" is an i32:
//!   -1                    => default fg/bg (theme)
//!   0..=255               => xterm palette index
//!   >= TRUE_COLOR (bit24) => 0xRRGGBB packed in the low 24 bits

pub const DEFAULT: i32 = -1;
pub const TRUE_COLOR: i32 = 1 << 24;

#[inline]
pub fn rgb(r: u8, g: u8, b: u8) -> i32 {
    TRUE_COLOR | ((r as i32) << 16) | ((g as i32) << 8) | (b as i32)
}

/// Parse "#RRGGBB" / "RRGGBB" to 0xRRGGBB, or return `fallback`.
pub fn parse_hex(s: &str, fallback: u32) -> u32 {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(v) = u32::from_str_radix(s, 16) {
            return v;
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_packs_truecolor_bit() {
        let c = rgb(10, 20, 30);
        assert_ne!(c & TRUE_COLOR, 0);
        assert_eq!((c & 0xFF_FFFF) as u32, 0x0A141E);
    }

    #[test]
    fn parse_hex_ok_and_fallback() {
        assert_eq!(parse_hex("#0C0C0C", 0), 0x0C0C0C);
        assert_eq!(parse_hex("FFD3D9", 0), 0xFFD3D9);
        assert_eq!(parse_hex("nope", 0x123456), 0x123456);
        assert_eq!(parse_hex("", 7), 7);
    }
}
