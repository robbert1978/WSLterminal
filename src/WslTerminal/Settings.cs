using System.IO;
using System.Text.Json;

namespace WslTerminal;

/// <summary>User-configurable appearance, persisted to
/// %APPDATA%\WslTerminal\settings.json. Colors are "#RRGGBB" strings.</summary>
public sealed class Settings
{
    public string FontFamily { get; set; } = "Cascadia Mono";
    public double FontSize { get; set; } = 11;          // points (like Windows Terminal / conhost)
    public string Background { get; set; } = "#0C0C0C";
    public string Foreground { get; set; } = "#CCCCCC";
    public string Cursor { get; set; } = "#FFFFFF";
    public string Selection { get; set; } = "#264F78";
    public int Opacity { get; set; } = 100;             // window opacity %, 10..100 (100 = opaque)

    /// <summary>The 16 ANSI colors (0-7 normal, 8-15 bright).</summary>
    public string[] Ansi { get; set; } = (string[])Schemes.Campbell.Ansi16.Clone();

    private static readonly JsonSerializerOptions JsonOpts = new() { WriteIndented = true };

    public static string Path =>
        System.IO.Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
            "WslTerminal", "settings.json");

    public static Settings Load()
    {
        try
        {
            if (File.Exists(Path))
                return JsonSerializer.Deserialize<Settings>(File.ReadAllText(Path)) ?? new Settings();
        }
        catch { /* fall back to defaults */ }
        return new Settings();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(System.IO.Path.GetDirectoryName(Path)!);
            File.WriteAllText(Path, JsonSerializer.Serialize(this, JsonOpts));
        }
        catch { /* non-fatal */ }
    }

    public Settings Clone() => new()
    {
        FontFamily = FontFamily,
        FontSize = FontSize,
        Background = Background,
        Foreground = Foreground,
        Cursor = Cursor,
        Selection = Selection,
        Opacity = Opacity,
        Ansi = (string[])Ansi.Clone(),
    };
}
