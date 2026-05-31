using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>
/// Left sidebar: a file/dir list that follows the active pane's working directory
/// (OSC 7). Double-click (or Enter) a file opens it in a new viewer tab; a
/// directory enters it. Right-click a file for Open / "Insert path at prompt";
/// right-click a directory for Open / "Open in new window"; right-click empty space
/// for "Show hidden items" + font size. Hidden (dot) files are off by default
/// (Ctrl+Shift+H toggles). The panel font defaults to the terminal's size and is
/// adjustable with Ctrl +/-/0. Files are read from Windows over the
/// <c>\\wsl.localhost</c> share — no extra process.
/// </summary>
public sealed class FileSidebar : Grid
{
    private readonly string _distro;
    private readonly ListBox _list = new();
    private readonly TextBlock _header = new();
    private string? _dir;                              // current Linux dir being shown

    private bool _showHidden;                          // dotfiles hidden by default
    private double _fontSize = 12;                     // panel font (points); set to terminal size by host
    private double _baseFontSize = 12;                 // Ctrl+0 resets here (the terminal's size)

    public event Action<string>? InsertPath;          // single-click a file
    public event Action<string>? OpenFile;            // double-click a file (Linux path)
    public event Action<string>? OpenInNewWindow;     // right-click dir -> menu (Linux path)

    public FileSidebar(string distro)
    {
        _distro = distro;

        RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });                   // header
        RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) }); // list

        // --- header (folder path + refresh) ---
        var headerBar = new DockPanel { LastChildFill = true, Margin = new Thickness(6, 4, 4, 4) };
        var refresh = new Button
        {
            Content = "⟳", Width = 22, Height = 20, Padding = new Thickness(0),
            ToolTip = "Refresh", FontSize = 12, Cursor = Cursors.Hand,
            Background = Brushes.Transparent, BorderThickness = new Thickness(0),
        };
        refresh.Click += (_, _) => Reload();
        DockPanel.SetDock(refresh, Dock.Right);
        _header.VerticalAlignment = VerticalAlignment.Center;
        _header.TextTrimming = TextTrimming.CharacterEllipsis;
        _header.FontWeight = FontWeights.SemiBold;
        _header.Text = "(no folder)";
        headerBar.Children.Add(refresh);
        headerBar.Children.Add(_header);
        SetRow(headerBar, 0);
        Children.Add(headerBar);

        // --- file list ---
        _list.BorderThickness = new Thickness(0);
        ScrollViewer.SetHorizontalScrollBarVisibility(_list, ScrollBarVisibility.Disabled);
        // Rows paint no background of their own, so the opaque list background shows
        // through; selection color comes from the list resources in ApplyTheme.
        var itemStyle = new Style(typeof(ListBoxItem));
        itemStyle.Setters.Add(new Setter(Control.BackgroundProperty, Brushes.Transparent));
        itemStyle.Setters.Add(new Setter(Control.BorderThicknessProperty, new Thickness(0)));
        itemStyle.Setters.Add(new Setter(Control.HorizontalContentAlignmentProperty, HorizontalAlignment.Stretch));
        _list.ItemContainerStyle = itemStyle;
        // Catch double-click on the TUNNELING preview-down: a ListBoxItem handles
        // MouseDoubleClick for selection and can swallow it, so the ListBox-level
        // MouseDoubleClick may never fire. PreviewMouseLeftButtonDown tunnels to the
        // ListBox first, so ClickCount==2 here reliably means "activate this row".
        _list.PreviewMouseLeftButtonDown += OnListPreviewDown;
        _list.KeyDown += OnListKeyDown;                    // Enter activates; Ctrl+Shift+H / Ctrl+/-/0
        _list.ContextMenu = BuildListMenu();              // right-click empty space
        SetRow(_list, 1);
        Children.Add(_list);
    }

    /// <summary>Set the panel's base font size from the terminal's size in POINTS.
    /// WPF FontSize is in DIP, and the terminal renders glyphs at points*96/72 DIP,
    /// so we convert here to match the terminal's visual size. Ctrl+0 resets here.</summary>
    public void InitFontSize(double points)
    {
        _baseFontSize = Math.Clamp(points * 96.0 / 72.0, 8, 96);   // points -> WPF DIP
        SetFontSize(_baseFontSize);
    }

    /// <summary>Set the current panel font size in WPF DIP.</summary>
    public void SetFontSize(double dip)
    {
        _fontSize = Math.Clamp(dip, 8, 96);
        ApplyFont();
        Reload();   // rows are rebuilt with the new size
    }

    public void BumpFontSize(double delta) => SetFontSize(_fontSize + delta);

    private void ApplyFont()
    {
        _header.FontSize = _fontSize;
        _list.FontSize = _fontSize;
    }

    public bool ShowHidden
    {
        get => _showHidden;
        set { if (_showHidden != value) { _showHidden = value; Reload(); } }
    }

    public void ToggleHidden() => ShowHidden = !_showHidden;

    public void SetDir(string? linuxDir)
    {
        if (string.IsNullOrEmpty(linuxDir) || linuxDir == _dir) return;
        _dir = linuxDir;
        Reload();
    }

    public void Reload()
    {
        if (_dir is null) return;
        _header.Text = _dir;
        _header.ToolTip = _dir;
        _list.Items.Clear();

        string trimmed = _dir.TrimEnd('/');
        if (trimmed.Length > 0)
            _list.Items.Add(MakeRow("📁", "..", ParentDir(trimmed), true, -1));   // ".." always shown

        foreach (var e in WslFiles.List(_distro, _dir))
        {
            if (!_showHidden && e.Name.StartsWith(".")) continue;   // hide dotfiles by default
            _list.Items.Add(MakeRow(e.IsDir ? "📁" : Icon(e.Name), e.Name, e.LinuxPath, e.IsDir, e.Size));
        }
    }

    private static string ParentDir(string linuxDir)
    {
        int i = linuxDir.LastIndexOf('/');
        return i <= 0 ? "/" : linuxDir[..i];
    }

    private ListBoxItem MakeRow(string icon, string name, string linuxPath, bool isDir, long size)
    {
        var sp = new StackPanel { Orientation = Orientation.Horizontal };
        sp.Children.Add(new TextBlock { Text = icon, Margin = new Thickness(0, 0, 6, 0), FontSize = _fontSize });
        sp.Children.Add(new TextBlock { Text = name, VerticalAlignment = VerticalAlignment.Center });
        if (!isDir && size >= 0)
            sp.Children.Add(new TextBlock
            {
                Text = "  " + WslFiles.HumanSize(size),
                Opacity = 0.5, FontSize = _fontSize * 0.82, VerticalAlignment = VerticalAlignment.Center,
            });

        var item = new ListBoxItem
        {
            Content = sp,
            Tag = new RowData(name, linuxPath, isDir),
            Padding = new Thickness(4, 2, 4, 2),
        };

        var menu = new ContextMenu();
        if (isDir)
        {
            var open = new MenuItem { Header = "Open" };
            open.Click += (_, _) => SetDir(linuxPath);
            var openNew = new MenuItem { Header = "Open in new window" };
            openNew.Click += (_, _) => OpenInNewWindow?.Invoke(linuxPath);
            menu.Items.Add(open);
            menu.Items.Add(openNew);
        }
        else
        {
            var open = new MenuItem { Header = "Open" };
            open.Click += (_, _) => OpenFile?.Invoke(linuxPath);
            var insert = new MenuItem { Header = "Insert path at prompt" };
            insert.Click += (_, _) => InsertPath?.Invoke(linuxPath);
            menu.Items.Add(open);
            menu.Items.Add(insert);
        }
        menu.Items.Add(new Separator());
        AddViewItems(menu);     // every row's menu also offers the view options
        item.ContextMenu = menu;
        return item;
    }

    // The view options (show-hidden + font size) shared by the empty-space menu and
    // each row's menu. The hidden item is checkable and re-syncs when the menu opens.
    private void AddViewItems(ContextMenu menu)
    {
        var hidden = new MenuItem { Header = "Show hidden items", IsCheckable = true, InputGestureText = "Ctrl+Shift+H" };
        hidden.Click += (_, _) => ShowHidden = hidden.IsChecked;
        menu.Opened += (_, _) => hidden.IsChecked = _showHidden;
        menu.Items.Add(hidden);

        var bigger = new MenuItem { Header = "Increase font size", InputGestureText = "Ctrl++" };
        bigger.Click += (_, _) => BumpFontSize(+1);
        var smaller = new MenuItem { Header = "Decrease font size", InputGestureText = "Ctrl+-" };
        smaller.Click += (_, _) => BumpFontSize(-1);
        menu.Items.Add(bigger);
        menu.Items.Add(smaller);
    }

    private ContextMenu BuildListMenu()
    {
        var menu = new ContextMenu();
        var refresh = new MenuItem { Header = "Refresh" };
        refresh.Click += (_, _) => Reload();
        menu.Items.Add(refresh);
        menu.Items.Add(new Separator());
        AddViewItems(menu);
        return menu;
    }

    private sealed record RowData(string Name, string LinuxPath, bool IsDir);

    private void OnListPreviewDown(object sender, MouseButtonEventArgs e)
    {
        if (e.ClickCount < 2) return;            // single click: let the row select
        if (ItemUnder(e.OriginalSource) is not { } row) return;
        e.Handled = true;
        Activate(row);
    }

    private void OnListKeyDown(object sender, KeyEventArgs e)
    {
        bool ctrl = (Keyboard.Modifiers & ModifierKeys.Control) != 0;
        bool shift = (Keyboard.Modifiers & ModifierKeys.Shift) != 0;

        if (ctrl && shift && e.Key == Key.H) { ToggleHidden(); e.Handled = true; return; }
        if (ctrl && e.Key is Key.OemPlus or Key.Add) { BumpFontSize(+1); e.Handled = true; return; }
        if (ctrl && e.Key is Key.OemMinus or Key.Subtract) { BumpFontSize(-1); e.Handled = true; return; }
        if (ctrl && e.Key is Key.D0 or Key.NumPad0) { SetFontSize(_baseFontSize); e.Handled = true; return; }

        if (e.Key == Key.Enter &&
            (_list.SelectedItem as ListBoxItem)?.Tag is RowData row) { Activate(row); e.Handled = true; }
    }

    // Double-click / Enter: open a file in a viewer tab, or enter a directory.
    private void Activate(RowData row)
    {
        if (row.IsDir) SetDir(row.LinuxPath);
        else OpenFile?.Invoke(row.LinuxPath);
    }

    private RowData? ItemUnder(object? source)
    {
        if (source is not DependencyObject d) return null;
        while (d is not null and not ListBoxItem) d = VisualTreeHelper.GetParent(d);
        return (d as ListBoxItem)?.Tag as RowData;
    }

    private static string Icon(string name) => Path.GetExtension(name).ToLowerInvariant() switch
    {
        ".png" or ".jpg" or ".jpeg" or ".gif" or ".bmp" or ".ico" or ".webp" => "🖼",
        ".md" or ".txt" or ".log" => "📄",
        ".sh" or ".bash" or ".zsh" => "📜",
        ".json" or ".yml" or ".yaml" or ".xml" or ".toml" or ".ini" => "⚙",
        ".zip" or ".gz" or ".tar" or ".xz" or ".7z" => "📦",
        _ => "📄",
    };

    // ---- test hooks (--sidebartest) ---------------------------------------
    internal int ItemCount => _list.Items.Count;
    internal string HeaderText => _header.Text;
    internal bool TestShowHidden => _showHidden;
    internal void TestToggleHidden() => ToggleHidden();
    internal double TestFontSize => _fontSize;
    internal void TestBumpFont(double d) => BumpFontSize(d);
    // Runs the same activation path a double-click would, exercising the OpenFile wiring.
    internal void TestActivateFile(string linuxPath)
    {
        string name = linuxPath.TrimEnd('/');
        int i = name.LastIndexOf('/'); if (i >= 0) name = name[(i + 1)..];
        Activate(new RowData(name, linuxPath, false));
    }

    /// <summary>Apply the app theme colors so the sidebar matches the terminal.
    /// The sidebar is always OPAQUE — even when the window is translucent, the file
    /// panel should be solid dark (not see-through), so we paint it with the theme
    /// background at full alpha rather than letting the window show through.</summary>
    public void ApplyTheme()
    {
        uint fg = Theme.Foreground, bg = Theme.Background;
        var fgBrush = Rgb(fg);
        var bgBrush = Rgb(bg);                                  // opaque (full alpha)
        var rowSelBrush = new SolidColorBrush(Color.FromArgb(0x55, 0x3B, 0x78, 0xFF));

        Background = bgBrush;                                   // whole-sidebar solid background
        _header.Foreground = fgBrush;

        _list.Foreground = fgBrush;
        _list.Background = bgBrush;
        _list.Resources[SystemColors.HighlightBrushKey] = rowSelBrush;
        _list.Resources[SystemColors.HighlightTextBrushKey] = fgBrush;
        _list.Resources[SystemColors.InactiveSelectionHighlightBrushKey] = rowSelBrush;
        _list.Resources[SystemColors.InactiveSelectionHighlightTextBrushKey] = fgBrush;
    }

    private static SolidColorBrush Rgb(uint c) =>
        new(Color.FromRgb((byte)(c >> 16), (byte)(c >> 8), (byte)c));
}
