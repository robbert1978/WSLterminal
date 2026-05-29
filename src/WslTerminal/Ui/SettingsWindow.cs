using System.Linq;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>conhost-style appearance dialog: font family/size, a color scheme,
/// and background/foreground/cursor colors. Apply previews live; OK persists.</summary>
public sealed class SettingsWindow : Window
{
    private readonly Settings _original;
    private readonly Action<Settings> _applyLive;
    private string[] _ansi;

    private readonly ComboBox _font = new() { Width = 230 };
    private readonly ComboBox _size = new() { Width = 70, IsEditable = true };
    private readonly ComboBox _scheme = new() { Width = 230, DisplayMemberPath = nameof(Scheme.Name) };
    private readonly TextBox _bg = new() { Width = 90 };
    private readonly TextBox _fg = new() { Width = 90 };
    private readonly TextBox _cur = new() { Width = 90 };
    private readonly TextBox _sel = new() { Width = 90 };
    private readonly ComboBox _opacity = new() { Width = 70, IsEditable = true };
    private readonly Border _bgSw = Swatch(), _fgSw = Swatch(), _curSw = Swatch(), _selSw = Swatch();

    /// <summary>The settings chosen, valid after the dialog returns true.</summary>
    public Settings? Result { get; private set; }

    public SettingsWindow(Settings current, Action<Settings> applyLive)
    {
        _original = current.Clone();
        _applyLive = applyLive;
        _ansi = (string[])current.Ansi.Clone();

        Title = "Terminal Settings";
        SizeToContent = SizeToContent.Height;
        Width = 380;
        ResizeMode = ResizeMode.NoResize;
        ShowInTaskbar = false;
        WindowStartupLocation = WindowStartupLocation.CenterOwner;
        Background = new SolidColorBrush(Color.FromRgb(0xF0, 0xF0, 0xF0));

        _font.ItemsSource = Fonts.SystemFontFamilies.Select(f => f.Source).OrderBy(s => s).ToList();
        if (!string.IsNullOrEmpty(current.FontFamily) && !_font.Items.Contains(current.FontFamily))
            _font.Items.Add(current.FontFamily);
        _font.SelectedItem = current.FontFamily;

        _size.ItemsSource = new[] { "8", "9", "10", "11", "12", "13", "14", "15", "16", "18", "20", "22", "24", "28", "32" };
        _size.Text = current.FontSize.ToString("0");

        _scheme.ItemsSource = Schemes.All;
        _bg.Text = current.Background; _fg.Text = current.Foreground;
        _cur.Text = current.Cursor; _sel.Text = current.Selection;

        _opacity.ItemsSource = new[] { "100", "95", "90", "85", "80", "75", "70", "60", "50" };
        _opacity.Text = current.Opacity.ToString();

        _bg.TextChanged += (_, _) => UpdateSwatch(_bg, _bgSw);
        _fg.TextChanged += (_, _) => UpdateSwatch(_fg, _fgSw);
        _cur.TextChanged += (_, _) => UpdateSwatch(_cur, _curSw);
        _sel.TextChanged += (_, _) => UpdateSwatch(_sel, _selSw);
        UpdateSwatch(_bg, _bgSw); UpdateSwatch(_fg, _fgSw); UpdateSwatch(_cur, _curSw); UpdateSwatch(_sel, _selSw);

        _scheme.SelectionChanged += (_, _) =>
        {
            if (_scheme.SelectedItem is Scheme s)
            {
                _ansi = (string[])s.Ansi16.Clone();
                _bg.Text = s.Background; _fg.Text = s.Foreground; _cur.Text = s.Cursor;
            }
        };

        Content = BuildLayout();
    }

    private UIElement BuildLayout()
    {
        var ok = new Button { Content = "OK", Width = 80, IsDefault = true, Margin = new Thickness(6, 0, 0, 0) };
        var cancel = new Button { Content = "Cancel", Width = 80, IsCancel = true, Margin = new Thickness(6, 0, 0, 0) };
        var apply = new Button { Content = "Apply", Width = 80, Margin = new Thickness(6, 0, 0, 0) };
        ok.Click += (_, _) => { Result = Snapshot(); _applyLive(Result); DialogResult = true; Close(); };
        cancel.Click += (_, _) => { _applyLive(_original); DialogResult = false; Close(); };
        apply.Click += (_, _) => _applyLive(Snapshot());

        var buttons = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            HorizontalAlignment = HorizontalAlignment.Right,
            Margin = new Thickness(0, 14, 0, 0),
        };
        buttons.Children.Add(ok);
        buttons.Children.Add(cancel);
        buttons.Children.Add(apply);

        var root = new StackPanel { Margin = new Thickness(14) };
        root.Children.Add(Row("Font", _font));
        root.Children.Add(Row("Size", _size));
        root.Children.Add(Separator());
        root.Children.Add(Row("Color scheme", _scheme));
        root.Children.Add(Row("Background", WithSwatch(_bg, _bgSw)));
        root.Children.Add(Row("Foreground", WithSwatch(_fg, _fgSw)));
        root.Children.Add(Row("Cursor", WithSwatch(_cur, _curSw)));
        root.Children.Add(Row("Selection", WithSwatch(_sel, _selSw)));
        root.Children.Add(Separator());
        root.Children.Add(Row("Opacity %", _opacity));
        root.Children.Add(new TextBlock
        {
            Text = "Opacity change takes effect on relaunch.",
            FontSize = 10, Foreground = Brushes.Gray, Margin = new Thickness(110, 0, 0, 0),
        });
        root.Children.Add(buttons);
        return root;
    }

    /// <summary>Test hook so --settingstest can verify all fields round-trip.</summary>
    internal Settings SnapshotForTest() => Snapshot();

    private Settings Snapshot()
    {
        double size = double.TryParse(_size.Text, out double v) ? v : _original.FontSize;
        int opacity = int.TryParse(_opacity.Text, out int o) ? Math.Clamp(o, 10, 100) : _original.Opacity;
        return new Settings
        {
            FontFamily = _font.SelectedItem as string ?? _font.Text,
            FontSize = Math.Clamp(size, 6, 72),
            Background = _bg.Text.Trim(),
            Foreground = _fg.Text.Trim(),
            Cursor = _cur.Text.Trim(),
            Selection = _sel.Text.Trim(),
            Opacity = opacity,
            Ansi = (string[])_ansi.Clone(),
        };
    }

    private static Border Swatch() => new()
    {
        Width = 26,
        Height = 18,
        Margin = new Thickness(8, 0, 0, 0),
        BorderBrush = Brushes.Gray,
        BorderThickness = new Thickness(1),
        VerticalAlignment = VerticalAlignment.Center,
    };

    private static void UpdateSwatch(TextBox box, Border swatch)
    {
        uint rgb = Theme.ParseHex(box.Text, 0x000000);
        swatch.Background = new SolidColorBrush(Color.FromRgb((byte)(rgb >> 16), (byte)(rgb >> 8), (byte)rgb));
    }

    private static UIElement WithSwatch(TextBox box, Border swatch)
    {
        var sp = new StackPanel { Orientation = Orientation.Horizontal };
        sp.Children.Add(box);
        sp.Children.Add(swatch);
        return sp;
    }

    private static UIElement Row(string label, UIElement control)
    {
        var dp = new DockPanel { Margin = new Thickness(0, 4, 0, 4) };
        var l = new TextBlock { Text = label, Width = 110, VerticalAlignment = VerticalAlignment.Center };
        DockPanel.SetDock(l, Dock.Left);
        dp.Children.Add(l);
        dp.Children.Add(control);
        return dp;
    }

    private static UIElement Separator() =>
        new Border { Height = 1, Background = Brushes.LightGray, Margin = new Thickness(0, 8, 0, 8) };
}
