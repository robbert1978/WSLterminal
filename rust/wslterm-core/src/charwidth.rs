//! Compact wcwidth. Ports `CharWidth` from `src/WslTerminal/Vt/Cell.cs`.

/// Display width of a code point: 0 for combining/zero-width, 2 for wide, else 1.
pub fn width_of(cp: u32) -> u8 {
    if cp == 0 {
        return 0;
    }
    if cp < 32 || (0x7f..0xa0).contains(&cp) {
        return 0; // control
    }
    if is_combining(cp) {
        return 0;
    }
    if is_wide(cp) {
        return 2;
    }
    1
}

fn is_combining(cp: u32) -> bool {
    (0x0300..=0x036F).contains(&cp)
        || (0x1AB0..=0x1AFF).contains(&cp)
        || (0x1DC0..=0x1DFF).contains(&cp)
        || (0x20D0..=0x20FF).contains(&cp)
        || (0xFE00..=0xFE0F).contains(&cp) // variation selectors (VS16 = emoji presentation)
        || (0xFE20..=0xFE2F).contains(&cp)
        || cp == 0x200B
        || cp == 0x200D
        || cp == 0xFEFF // ZWSP, ZWJ, BOM/ZWNBSP
}

fn is_wide(cp: u32) -> bool {
    (0x1100..=0x115F).contains(&cp)   // Hangul Jamo
        || (0x2E80..=0x303E).contains(&cp) // CJK radicals ...
        || (0x3041..=0x33FF).contains(&cp) // Hiragana ... CJK symbols
        || (0x3400..=0x4DBF).contains(&cp) // CJK Ext A
        || (0x4E00..=0x9FFF).contains(&cp) // CJK Unified
        || (0xA000..=0xA4CF).contains(&cp) // Yi
        || (0xAC00..=0xD7A3).contains(&cp) // Hangul syllables
        || (0xF900..=0xFAFF).contains(&cp) // CJK compat ideographs
        || (0xFE30..=0xFE4F).contains(&cp) // CJK compat forms
        || (0xFF00..=0xFF60).contains(&cp) // Fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1FAFF).contains(&cp) // emoji & symbols
        || (0x20000..=0x3FFFD).contains(&cp) // CJK Ext B+
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn widths() {
        assert_eq!(width_of('a' as u32), 1);
        assert_eq!(width_of(0x4E16), 2); // 世
        assert_eq!(width_of(0x0301), 0); // combining acute
        assert_eq!(width_of(0), 0);
        assert_eq!(width_of(0x1F40D), 2); // 🐍
    }
}
