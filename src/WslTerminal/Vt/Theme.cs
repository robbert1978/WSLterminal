using System.Globalization;

namespace WslTerminal.Vt;

/// <summary>
/// Color handling. A "color code" is an int:
///   -1                         => default fg/bg (theme)
///   0..255                     => xterm palette index
///   &gt;= TrueColor (bit 24)   => 0xRRGGBB packed in the low 24 bits
/// </summary>
public static class Theme
{
    public const int Default = -1;
    public const int TrueColor = 1 << 24;

    public static int Rgb(byte r, byte g, byte b) => TrueColor | (r << 16) | (g << 8) | b;

    // Campbell scheme (matches modern Windows Terminal defaults).
    public static uint Background = 0x0C0C0C;
    public static uint Foreground = 0xCCCCCC;
    public static uint CursorColor = 0xFFFFFF;
    public static uint Selection = 0x264F78;   // highlight behind selected text

    private static uint[] Pal = BuildPalette();

    /// <summary>Parse "#RRGGBB" / "RRGGBB" to 0xRRGGBB, or return fallback.</summary>
    public static uint ParseHex(string? s, uint fallback)
    {
        if (string.IsNullOrWhiteSpace(s)) return fallback;
        s = s.Trim().TrimStart('#');
        return s.Length == 6 && uint.TryParse(s, NumberStyles.HexNumber, CultureInfo.InvariantCulture, out uint v)
            ? v : fallback;
    }

    /// <summary>Apply user settings (colors). Font is applied by the view.</summary>
    public static void Apply(Settings s)
    {
        Background = ParseHex(s.Background, Background);
        Foreground = ParseHex(s.Foreground, Foreground);
        CursorColor = ParseHex(s.Cursor, CursorColor);
        Selection = ParseHex(s.Selection, Selection);
        if (s.Ansi is { Length: >= 16 })
            for (int i = 0; i < 16; i++) Pal[i] = ParseHex(s.Ansi[i], Pal[i]);
    }

    private static uint[] BuildPalette()
    {
        var p = new uint[256];
        uint[] ansi =
        {
            0x0C0C0C, 0xC50F1F, 0x13A10E, 0xC19C00, 0x0037DA, 0x881798, 0x3A96DD, 0xCCCCCC,
            0x767676, 0xE74856, 0x16C60C, 0xF9F1A5, 0x3B78FF, 0xB4009E, 0x61D6D6, 0xF2F2F2,
        };
        Array.Copy(ansi, p, 16);

        // 6x6x6 color cube (216 colors), indices 16..231.
        int[] steps = { 0, 95, 135, 175, 215, 255 };
        int i = 16;
        for (int r = 0; r < 6; r++)
            for (int g = 0; g < 6; g++)
                for (int b = 0; b < 6; b++)
                    p[i++] = (uint)((steps[r] << 16) | (steps[g] << 8) | steps[b]);

        // Grayscale ramp, indices 232..255.
        for (int s = 0; s < 24; s++)
        {
            int v = 8 + s * 10;
            p[232 + s] = (uint)((v << 16) | (v << 8) | v);
        }
        return p;
    }

    /// <summary>Resolve a color code to 0xRRGGBB, brightening base colors when bold.</summary>
    public static uint Resolve(int code, bool foreground, bool bold)
    {
        if (code == Default) return foreground ? Foreground : Background;
        if ((code & TrueColor) != 0) return (uint)(code & 0xFFFFFF);
        int idx = code & 0xFF;
        if (bold && idx < 8) idx += 8;            // bold => bright variant
        return Pal[idx];
    }
}
