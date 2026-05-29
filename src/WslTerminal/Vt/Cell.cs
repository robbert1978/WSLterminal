namespace WslTerminal.Vt;

[Flags]
public enum CellFlags : ushort
{
    None      = 0,
    Bold      = 1 << 0,
    Faint     = 1 << 1,
    Italic    = 1 << 2,
    Underline = 1 << 3,
    Blink     = 1 << 4,
    Reverse   = 1 << 5,
    Hidden    = 1 << 6,
    Strike    = 1 << 7,
}

/// <summary>One screen cell. Rune==0 renders as blank. Width: 1 normal,
/// 2 lead of a wide glyph, 0 the trailing continuation slot of a wide glyph.
/// <see cref="Combo"/> holds any combining marks appended to the base rune
/// (e.g. the overlay stroke in a kaomoji); null for the common case.</summary>
public struct Cell
{
    public int Rune;
    public int Fg;
    public int Bg;
    public CellFlags Flags;
    public byte Width;
    public string? Combo;

    public static Cell Blank(int fg, int bg, CellFlags flags) =>
        new() { Rune = 0, Fg = fg, Bg = bg, Flags = flags, Width = 1 };
}

public static class CharWidth
{
    /// <summary>Compact wcwidth: 0 for combining/zero-width, 2 for wide, else 1.</summary>
    public static int Of(int cp)
    {
        if (cp == 0) return 0;
        if (cp < 32 || (cp >= 0x7f && cp < 0xa0)) return 0; // control
        if (IsCombining(cp)) return 0;
        if (IsWide(cp)) return 2;
        return 1;
    }

    private static bool IsCombining(int cp) =>
        (cp >= 0x0300 && cp <= 0x036F) ||   // combining diacritical marks
        (cp >= 0x1AB0 && cp <= 0x1AFF) ||
        (cp >= 0x1DC0 && cp <= 0x1DFF) ||
        (cp >= 0x20D0 && cp <= 0x20FF) ||
        (cp >= 0xFE00 && cp <= 0xFE0F) ||   // variation selectors (e.g. VS16 emoji presentation)
        (cp >= 0xFE20 && cp <= 0xFE2F) ||
        cp == 0x200B || cp == 0x200D || cp == 0xFEFF; // ZWSP, ZWJ, BOM/ZWNBSP

    private static bool IsWide(int cp) =>
        (cp >= 0x1100 && cp <= 0x115F) ||   // Hangul Jamo
        (cp >= 0x2E80 && cp <= 0x303E) ||   // CJK radicals …
        (cp >= 0x3041 && cp <= 0x33FF) ||   // Hiragana … CJK symbols
        (cp >= 0x3400 && cp <= 0x4DBF) ||   // CJK Ext A
        (cp >= 0x4E00 && cp <= 0x9FFF) ||   // CJK Unified
        (cp >= 0xA000 && cp <= 0xA4CF) ||   // Yi
        (cp >= 0xAC00 && cp <= 0xD7A3) ||   // Hangul syllables
        (cp >= 0xF900 && cp <= 0xFAFF) ||   // CJK compat ideographs
        (cp >= 0xFE30 && cp <= 0xFE4F) ||   // CJK compat forms
        (cp >= 0xFF00 && cp <= 0xFF60) ||   // Fullwidth forms
        (cp >= 0xFFE0 && cp <= 0xFFE6) ||
        (cp >= 0x1F300 && cp <= 0x1FAFF) || // emoji & symbols
        (cp >= 0x20000 && cp <= 0x3FFFD);   // CJK Ext B+
}
