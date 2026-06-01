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

/// Resolve a color code (see module docs) to a concrete 0xRRGGBB, using the
/// supplied default fg/bg and the 256-color palette. Ports the relevant part of
/// `Theme.Resolve`. `bold` brightens the low 8 palette colors.
pub fn resolve(code: i32, default_rgb: u32, bold: bool) -> u32 {
    if code == DEFAULT {
        return default_rgb;
    }
    if code & TRUE_COLOR != 0 {
        return (code & 0xFF_FFFF) as u32;
    }
    let mut idx = (code & 0xFF) as usize;
    if bold && idx < 8 {
        idx += 8;
    }
    palette(idx)
}

/// The xterm 256-color palette: 16 ANSI (Campbell), 216-color cube, 24 grays.
pub fn palette(i: usize) -> u32 {
    const ANSI: [u32; 16] = [
        0x0C0C0C, 0xC50F1F, 0x13A10E, 0xC19C00, 0x0037DA, 0x881798, 0x3A96DD, 0xCCCCCC,
        0x767676, 0xE74856, 0x16C60C, 0xF9F1A5, 0x3B78FF, 0xB4009E, 0x61D6D6, 0xF2F2F2,
    ];
    if i < 16 {
        return ANSI[i];
    }
    if (16..232).contains(&i) {
        const STEPS: [u32; 6] = [0, 95, 135, 175, 215, 255];
        let n = i - 16;
        let r = STEPS[(n / 36) % 6];
        let g = STEPS[(n / 6) % 6];
        let b = STEPS[n % 6];
        return (r << 16) | (g << 8) | b;
    }
    if (232..256).contains(&i) {
        let v = 8 + (i - 232) as u32 * 10;
        return (v << 16) | (v << 8) | v;
    }
    0
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
