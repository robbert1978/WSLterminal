using System.IO;
using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Xml;
using ICSharpCode.AvalonEdit;
using ICSharpCode.AvalonEdit.Highlighting;
using ICSharpCode.AvalonEdit.Highlighting.Xshd;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>
/// Builds a read-only preview control for a WSL file — syntax-highlighted text
/// (AvalonEdit), a rendered image, or an info line for binaries — for hosting in a
/// file-viewer tab. Colors follow the app theme so previews match the terminal.
/// Files are read from Windows over <c>\\wsl.localhost</c>; no extra process.
/// </summary>
internal static class FilePreview
{
    public static FileDocument Create(string distro, string linuxPath, string name)
    {
        Brush fg = Rgb(Theme.Foreground), bg = Rgb(Theme.Background);

        byte[]? bytes = WslFiles.ReadBytes(distro, linuxPath);
        if (bytes is null)
            return NonEditable(Info($"Cannot read:\n{linuxPath}", fg, bg), distro, linuxPath, name);

        if (WslFiles.IsImage(name))
        {
            try
            {
                var bmp = new BitmapImage();
                bmp.BeginInit();
                bmp.CacheOption = BitmapCacheOption.OnLoad;
                bmp.StreamSource = new MemoryStream(bytes);
                bmp.EndInit();
                bmp.Freeze();
                var img = new Image { Source = bmp, Stretch = Stretch.Uniform, StretchDirection = StretchDirection.DownOnly, Margin = new Thickness(8) };
                var sv = new ScrollViewer
                {
                    Content = img,
                    Background = bg,
                    HorizontalScrollBarVisibility = ScrollBarVisibility.Auto,
                    VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
                };
                return NonEditable(sv, distro, linuxPath, name);
            }
            catch { /* fall through to info */ }
        }

        if (WslFiles.LooksBinary(bytes))
            return NonEditable(Info($"{name}\n{WslFiles.HumanSize(bytes.Length)} (binary file)", fg, bg), distro, linuxPath, name);

        // Text. Editable, unless the read was truncated at the cap (saving would
        // overwrite the full file with a partial copy).
        bool truncated = WslFiles.FileLength(distro, linuxPath) > bytes.Length;
        string text = System.Text.Encoding.UTF8.GetString(bytes);
        var editor = new TextEditor
        {
            IsReadOnly = truncated,
            ShowLineNumbers = true,
            WordWrap = false,
            FontFamily = new FontFamily("Cascadia Mono, Consolas, monospace"),
            FontSize = 13,
            Background = bg,
            Foreground = fg,
            Text = text,
            SyntaxHighlighting = HighlightingFor(name, text),
        };
        editor.Options.EnableHyperlinks = false;
        editor.TextArea.Foreground = fg;
        editor.LineNumbersForeground = new SolidColorBrush(Color.FromArgb(0x80,
            (byte)(Theme.Foreground >> 16), (byte)(Theme.Foreground >> 8), (byte)Theme.Foreground));

        if (truncated)
            return NonEditable(WrapTruncated(editor, name, fg, bg), distro, linuxPath, name);

        return new FileDocument(editor, editor, distro, linuxPath, name);
    }

    private static FileDocument NonEditable(FrameworkElement el, string distro, string path, string name)
        => new(el, null, distro, path, name);

    // A read-only editor for an over-cap file, with a banner explaining why.
    private static FrameworkElement WrapTruncated(TextEditor editor, string name, Brush fg, Brush bg)
    {
        var grid = new Grid { Background = bg };
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        var banner = new TextBlock
        {
            Text = $"{name} is large — showing the first part, read-only.",
            Foreground = fg, Opacity = 0.7, Margin = new Thickness(8, 4, 8, 4),
        };
        Grid.SetRow(banner, 0);
        Grid.SetRow(editor, 1);
        grid.Children.Add(banner);
        grid.Children.Add(editor);
        return grid;
    }

    private static Border Info(string msg, Brush fg, Brush bg) => new()
    {
        Background = bg,
        Child = new TextBlock { Text = msg, Foreground = fg, Margin = new Thickness(10), TextWrapping = TextWrapping.Wrap },
    };

    private static bool _registered;

    // Register the extra .xshd definitions AvalonEdit doesn't ship, loaded once from
    // embedded resources. Each failure is non-fatal (that type falls back to plain text).
    private static void EnsureRegistered()
    {
        if (_registered) return;
        _registered = true;
        Register("Shell.xshd",      "Shell",      new[] { ".sh", ".bash", ".zsh", ".ksh" });
        Register("Yaml.xshd",       "YAML",       new[] { ".yml", ".yaml" });
        Register("Rust.xshd",       "Rust",       new[] { ".rs" });
        Register("Go.xshd",         "Go",         new[] { ".go" });
        Register("TypeScript.xshd", "TypeScript", new[] { ".ts", ".tsx", ".mts", ".cts" });
        Register("Toml.xshd",       "TOML",       new[] { ".toml" });
        Register("Ini.xshd",        "INI",        new[] { ".ini", ".conf", ".cfg", ".properties", ".editorconfig", ".gitconfig" });
        Register("Dockerfile.xshd", "Dockerfile", new[] { ".dockerfile" });
        Register("Ruby.xshd",       "Ruby",       new[] { ".rb", ".gemspec", ".rake" });
        Register("Lua.xshd",        "Lua",        new[] { ".lua" });
    }

    private static void Register(string suffix, string name, string[] exts)
    {
        try
        {
            var asm = typeof(FilePreview).Assembly;
            string? res = asm.GetManifestResourceNames()
                .FirstOrDefault(n => n.EndsWith(suffix, StringComparison.OrdinalIgnoreCase));
            if (res is null) return;
            using var s = asm.GetManifestResourceStream(res);
            if (s is null) return;
            using var reader = XmlReader.Create(s);
            var def = HighlightingLoader.Load(reader, HighlightingManager.Instance);
            HighlightingManager.Instance.RegisterHighlighting(name, exts, def);
        }
        catch { /* optional: fall back to plain text */ }
    }

    // Resolve a highlighting definition for a file. Tries the extension (built-in
    // defs + our registered shell/YAML), then a few aliases, then a shebang sniff
    // for extensionless scripts; null => render as plain text.
    public static IHighlightingDefinition? HighlightingFor(string name, string? text = null)
    {
        EnsureRegistered();
        var hm = HighlightingManager.Instance;

        // special filenames (no usable extension): Dockerfile, Makefile, dotfiles...
        string baseName = Path.GetFileName(name);
        var byName = baseName.ToLowerInvariant() switch
        {
            "dockerfile" or "containerfile"       => hm.GetDefinition("Dockerfile"),
            ".bashrc" or ".bash_profile" or ".bash_aliases" or ".zshrc" or ".profile" or ".zprofile"
                                                  => hm.GetDefinition("Shell"),
            ".gitconfig" or ".npmrc" or ".editorconfig" => hm.GetDefinition("INI"),
            ".gitignore" or ".dockerignore" or ".gitattributes" => null,   // plain
            "makefile" or "gnumakefile"           => null,   // no make grammar; plain text
            _ => null,
        };
        if (byName is not null) return byName;
        if (baseName.StartsWith("Dockerfile", StringComparison.OrdinalIgnoreCase)) return hm.GetDefinition("Dockerfile");

        var byExt = hm.GetDefinitionByExtension(Path.GetExtension(name));
        if (byExt is not null) return byExt;

        var def = Path.GetExtension(name).ToLowerInvariant() switch
        {
            ".py" or ".pyw"                       => hm.GetDefinition("Python"),
            ".mjs" or ".cjs" or ".jsx"            => hm.GetDefinition("JavaScript"),
            ".xml" or ".csproj" or ".props" or ".targets" or ".plist" or ".svg" => hm.GetDefinition("XML"),
            ".c" or ".h" or ".cpp" or ".hpp" or ".cc" or ".hh" or ".cxx" or ".hxx" => hm.GetDefinition("C++"),
            ".ps1" or ".psm1" or ".psd1"          => hm.GetDefinition("PowerShell"),
            ".htm" or ".xhtml"                    => hm.GetDefinition("HTML"),
            ".markdown"                           => hm.GetDefinition("MarkDown"),
            ".java" or ".kt" or ".kts" or ".scala" or ".groovy" => hm.GetDefinition("Java"),
            ".sql"                                => hm.GetDefinition("TSQL"),
            ".diff" or ".patch"                   => hm.GetDefinition("Patch"),
            _ => null,
        };
        if (def is not null) return def;

        // extensionless scripts: pick by shebang (#!/usr/bin/env bash, etc.)
        if (text is not null && text.StartsWith("#!"))
        {
            int nl = text.IndexOf('\n');
            string first = nl < 0 ? text : text[..nl];
            if (first.Contains("python")) return hm.GetDefinition("Python");
            if (first.Contains("node"))   return hm.GetDefinition("JavaScript");
            if (first.Contains("pwsh") || first.Contains("powershell")) return hm.GetDefinition("PowerShell");
            return hm.GetDefinition("Shell");   // sh/bash/zsh/ksh/env ...
        }
        return null;   // unknown -> plain text (still readable, with line numbers)
    }

    private static SolidColorBrush Rgb(uint c) => new(Color.FromRgb((byte)(c >> 16), (byte)(c >> 8), (byte)c));
}
