namespace WslTerminal;

/// <summary>Named color schemes (Windows Terminal defaults). Each provides the
/// background/foreground/cursor and the 16 ANSI colors.</summary>
public sealed record Scheme(string Name, string Background, string Foreground, string Cursor, string[] Ansi16);

public static class Schemes
{
    public static readonly Scheme Campbell = new("Campbell", "#0C0C0C", "#CCCCCC", "#FFFFFF", new[]
    {
        "#0C0C0C", "#C50F1F", "#13A10E", "#C19C00", "#0037DA", "#881798", "#3A96DD", "#CCCCCC",
        "#767676", "#E74856", "#16C60C", "#F9F1A5", "#3B78FF", "#B4009E", "#61D6D6", "#F2F2F2",
    });

    public static readonly Scheme OneHalfDark = new("One Half Dark", "#282C34", "#DCDFE4", "#DCDFE4", new[]
    {
        "#282C34", "#E06C75", "#98C379", "#E5C07B", "#61AFEF", "#C678DD", "#56B6C2", "#DCDFE4",
        "#5A6374", "#E06C75", "#98C379", "#E5C07B", "#61AFEF", "#C678DD", "#56B6C2", "#DCDFE4",
    });

    public static readonly Scheme SolarizedDark = new("Solarized Dark", "#002B36", "#839496", "#839496", new[]
    {
        "#002B36", "#DC322F", "#859900", "#B58900", "#268BD2", "#D33682", "#2AA198", "#EEE8D5",
        "#073642", "#CB4B16", "#586E75", "#657B83", "#839496", "#6C71C4", "#93A1A1", "#FDF6E3",
    });

    public static readonly Scheme TangoDark = new("Tango Dark", "#000000", "#D3D7CF", "#FFFFFF", new[]
    {
        "#000000", "#CC0000", "#4E9A06", "#C4A000", "#3465A4", "#75507B", "#06989A", "#D3D7CF",
        "#555753", "#EF2929", "#8AE234", "#FCE94F", "#729FCF", "#AD7FA8", "#34E2E2", "#EEEEEC",
    });

    public static readonly Scheme[] All = { Campbell, OneHalfDark, SolarizedDark, TangoDark };
}
