using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Shell;
using System.Windows.Threading;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>Hosts one or more terminal tabs, each a multiplexed PTY session on
/// the shared per-distro wslptyd server. A tab strip switches between them.</summary>
public sealed class MainWindow : Window
{
    private readonly string _distro;
    private readonly string _serverWinPath;
    private readonly string? _startDir;
    private Settings _settings;
    private readonly bool _translucent;

    private WslMux? _mux;
    private readonly List<TerminalTab> _tabs = new();
    private TerminalTab? _active;
    private volatile bool _closing;

    private Grid _topBar = null!;
    private StackPanel _tabStrip = null!;
    private Grid _content = null!;
    private Border _plus = null!;
    private const double TitleBarHeight = 32;

    public MainWindow(string distro, string serverWinPath, Settings settings, string? startDir = null)
    {
        _distro = distro;
        _serverWinPath = serverWinPath;
        _settings = settings;
        _startDir = startDir;

        Title = $"WSL Terminal — {distro}";
        Width = 960; Height = 600;
        WindowStartupLocation = WindowStartupLocation.CenterScreen;
        Icon = LoadAppIcon();

        _translucent = Math.Clamp(_settings.Opacity, 10, 100) < 100;
        if (_translucent)
        {
            // Borderless + translucent, but WITHOUT WPF's AllowsTransparency — that
            // uses a per-pixel-alpha layered window (the expensive path). Instead
            // DWM composites the window's alpha cheaply; see EnableDwmTranslucency.
            WindowStyle = WindowStyle.None;
            ResizeMode = ResizeMode.CanResize;
            Background = Brushes.Transparent;
            WindowChrome.SetWindowChrome(this, new WindowChrome
            {
                CaptionHeight = TitleBarHeight,
                ResizeBorderThickness = new Thickness(6),
                GlassFrameThickness = new Thickness(0),
                CornerRadius = new CornerRadius(0),
                UseAeroCaptionButtons = false,
            });
            SourceInitialized += (_, _) => EnableDwmTranslucency();
        }
        else
        {
            Background = BgBrush();
        }

        Content = BuildLayout();
        Loaded += OnLoaded;
        Closed += OnClosed;
    }

    // ---- layout ------------------------------------------------------------

    private UIElement BuildLayout()
    {
        _tabStrip = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Bottom };
        _plus = MakeToolButton("+", () => AddTab(_active?.Active.Term.CurrentDirectory));

        var tabArea = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Center };
        if (_translucent)
            tabArea.Children.Add(new Image
            {
                Source = LoadAppIcon(),
                Width = 16, Height = 16,
                Margin = new Thickness(8, 0, 6, 0),
                VerticalAlignment = VerticalAlignment.Center,
            });
        tabArea.Children.Add(_tabStrip);
        tabArea.Children.Add(_plus);

        _topBar = new Grid { Height = TitleBarHeight, Background = BgBrush() };
        _topBar.ColumnDefinitions.Add(new ColumnDefinition());
        _topBar.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        Grid.SetColumn(tabArea, 0);
        _topBar.Children.Add(tabArea);

        if (_translucent)
        {
            var caption = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Top };
            caption.Children.Add(CaptionButton("—", () => WindowState = WindowState.Minimized, false));
            caption.Children.Add(CaptionButton("▢", () => WindowState =
                WindowState == WindowState.Maximized ? WindowState.Normal : WindowState.Maximized, false));
            caption.Children.Add(CaptionButton("✕", Close, true));
            Grid.SetColumn(caption, 1);
            _topBar.Children.Add(caption);
        }

        _content = new Grid();
        var root = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(_topBar, Dock.Top);
        root.Children.Add(_topBar);
        root.Children.Add(_content);
        return root;
    }

    // ---- tabs --------------------------------------------------------------

    private void AddTab(string? cwd)
    {
        var tab = new TerminalTab();
        MakeChip(tab);
        Pane pane = CreatePane(tab, cwd);
        tab.Root = pane;
        tab.Active = pane;
        _tabs.Add(tab);
        RebuildStrip();
        SelectTab(tab);
    }

    // One leaf pane: terminal + view (wrapped in a Border for the active highlight)
    // + its own session on the shared server. All event wiring is per-pane.
    private Pane CreatePane(TerminalTab tab, string? cwd)
    {
        var term = new Terminal(80, 24);
        var view = new TerminalView(term);
        view.ApplySettings(_settings);
        if (_translucent) view.SetBackgroundOpacity(Math.Clamp(_settings.Opacity, 10, 100) / 100.0);
        var host = new Border { BorderThickness = new Thickness(1), BorderBrush = Brushes.Transparent, Child = view };

        MuxSession? session = null;
        try { session = _mux?.Open(80, 24, cwd); }
        catch (Exception ex) { Log("pane open failed: " + ex.Message); }

        var pane = new Pane(term, view, session, host);

        view.Input += b => session?.SendData(b);
        view.Resized += (c, r) => session?.SendResize(c, r);
        term.Respond += b => session?.SendData(b);
        term.TitleChanged += t => Dispatcher.BeginInvoke(() =>
        {
            pane.Title = string.IsNullOrWhiteSpace(t) ? "shell" : t;
            if (tab.Active == pane) UpdateTabTitle(tab);
        });

        view.Focused += () => SetActivePane(tab, pane);
        view.NewWindowRequested += SpawnNewWindow;
        view.OpenSettingsRequested += OpenSettings;
        view.NewTabRequested += () => AddTab(pane.Term.CurrentDirectory);
        view.ClosePaneRequested += () => ClosePane(tab, pane);
        view.SwitchTabRequested += SwitchTab;
        view.SplitRequested += columns => SplitPane(tab, pane, columns);
        view.FontSizeChanged += sz => { _settings.FontSize = sz; _settings.Save(); ApplyFontToAll(sz); };

        if (session is not null)
        {
            session.DataReceived += d => { term.Feed(d); view.MarkDirty(); };
            session.Exited += _ => Dispatcher.BeginInvoke(() => { if (!_closing) ClosePane(tab, pane); });
        }
        else
        {
            term.Feed(System.Text.Encoding.UTF8.GetBytes($"\r\n  Failed to start a WSL session in '{_distro}'.\r\n"));
            view.MarkDirty();
        }

        tab.Panes.Add(pane);
        session?.SendResize(80, 24);
        return pane;
    }

    // Alt+Shift+= splits right (columns); Alt+Shift+- splits down (rows). The new
    // pane opens in the active pane's directory.
    private void SplitPane(TerminalTab tab, Pane pane, bool columns)
    {
        SplitNode? oldParent = pane.Parent;
        Pane newPane = CreatePane(tab, pane.Term.CurrentDirectory);

        DetachFromParent(pane.Element);
        Grid grid = BuildSplitGrid(columns, pane.Element, newPane.Element);
        var split = new SplitNode(columns, pane, newPane, grid) { Parent = oldParent };
        pane.Parent = split;
        newPane.Parent = split;

        if (oldParent is null)
        {
            tab.Root = split;
            if (_active == tab) ShowRoot(tab);
        }
        else
        {
            ReplaceChild(oldParent, pane, split);
        }
        SetActivePane(tab, newPane);
    }

    // Close one pane; its sibling takes its place. The tab's last pane closes the tab.
    private void ClosePane(TerminalTab tab, Pane pane)
    {
        if (!tab.Panes.Contains(pane)) return;
        try { pane.Session?.Close(); } catch { }
        tab.Panes.Remove(pane);
        if (tab.Panes.Count == 0) { CloseTab(tab); return; }

        SplitNode parent = pane.Parent!;
        PaneNode sibling = parent.A == pane ? parent.B : parent.A;
        DetachFromParent(sibling.Element);
        SplitNode? grand = parent.Parent;
        if (grand is null)
        {
            tab.Root = sibling; sibling.Parent = null;
            if (_active == tab) ShowRoot(tab);
        }
        else
        {
            ReplaceChild(grand, parent, sibling);
        }
        if (tab.Active == pane) SetActivePane(tab, FirstLeaf(sibling));
    }

    private void SetActivePane(TerminalTab tab, Pane pane)
    {
        tab.Active = pane;
        foreach (var p in tab.Panes)
            p.Host.BorderBrush = p == pane && tab.Panes.Count > 1 ? AccentBrush() : Brushes.Transparent;
        UpdateTabTitle(tab);
        if (!pane.View.IsKeyboardFocusWithin)
            Dispatcher.BeginInvoke(new Action(() => pane.View.Focus()), DispatcherPriority.Input);
    }

    private void UpdateTabTitle(TerminalTab tab)
    {
        tab.Title = tab.Active.Title;
        tab.Label.Text = tab.Title;
        if (_active == tab) Title = $"{tab.Title} — {_distro}";
    }

    private static Pane FirstLeaf(PaneNode node) => node is Pane p ? p : FirstLeaf(((SplitNode)node).A);

    private static void DetachFromParent(FrameworkElement el)
    {
        if (el.Parent is Panel panel) panel.Children.Remove(el);
    }

    private void ReplaceChild(SplitNode parent, PaneNode oldChild, PaneNode newChild)
    {
        int row = Grid.GetRow(oldChild.Element), col = Grid.GetColumn(oldChild.Element);
        parent.Grid.Children.Remove(oldChild.Element);
        Grid.SetRow(newChild.Element, row);
        Grid.SetColumn(newChild.Element, col);
        parent.Grid.Children.Add(newChild.Element);
        if (parent.A == oldChild) parent.A = newChild; else parent.B = newChild;
        newChild.Parent = parent;
    }

    private Grid BuildSplitGrid(bool columns, FrameworkElement first, FrameworkElement second)
    {
        var g = new Grid();
        var splitter = new GridSplitter
        {
            Background = new SolidColorBrush(Color.FromArgb(0x40, 0xFF, 0xFF, 0xFF)),
            ResizeBehavior = GridResizeBehavior.PreviousAndNext,
            HorizontalAlignment = HorizontalAlignment.Stretch,
            VerticalAlignment = VerticalAlignment.Stretch,
        };
        if (columns)
        {
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            g.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            splitter.Width = 3; splitter.ResizeDirection = GridResizeDirection.Columns;
            Grid.SetColumn(first, 0); Grid.SetColumn(splitter, 1); Grid.SetColumn(second, 2);
        }
        else
        {
            g.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
            g.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            g.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
            splitter.Height = 3; splitter.ResizeDirection = GridResizeDirection.Rows;
            Grid.SetRow(first, 0); Grid.SetRow(splitter, 1); Grid.SetRow(second, 2);
        }
        WindowChrome.SetIsHitTestVisibleInChrome(splitter, true);
        g.Children.Add(first); g.Children.Add(splitter); g.Children.Add(second);
        return g;
    }

    private static SolidColorBrush AccentBrush() => new(Color.FromRgb(0x3B, 0x78, 0xFF));

    private void ShowRoot(TerminalTab tab)
    {
        _content.Children.Clear();
        _content.Children.Add(tab.Root.Element);
    }

    private void SelectTab(TerminalTab tab)
    {
        _active = tab;
        ShowRoot(tab);
        RefreshChips();
        Title = $"{tab.Title} — {_distro}";
        Dispatcher.BeginInvoke(new Action(() => tab.Active.View.Focus()), DispatcherPriority.Input);
    }

    private void CloseTab(TerminalTab tab)
    {
        int idx = _tabs.IndexOf(tab);
        if (idx < 0) return;
        foreach (var p in tab.Panes) { try { p.Session?.Close(); } catch { } }
        _tabs.RemoveAt(idx);
        if (_active == tab) _content.Children.Clear();

        if (_tabs.Count == 0) { Close(); return; }
        RebuildStrip();
        if (_active == tab) SelectTab(_tabs[Math.Min(idx, _tabs.Count - 1)]);
    }

    private void SwitchTab(int delta)
    {
        if (_tabs.Count < 2 || _active is null) return;
        int i = (_tabs.IndexOf(_active) + delta + _tabs.Count) % _tabs.Count;
        SelectTab(_tabs[i]);
    }

    private void RebuildStrip()
    {
        _tabStrip.Children.Clear();
        foreach (var t in _tabs) _tabStrip.Children.Add(t.Chip);
        RefreshChips();
    }

    private void RefreshChips()
    {
        foreach (var t in _tabs)
        {
            bool active = t == _active;
            t.Chip.Background = active ? new SolidColorBrush(Color.FromArgb(0x33, 0xFF, 0xFF, 0xFF)) : Brushes.Transparent;
            t.Label.FontWeight = active ? FontWeights.SemiBold : FontWeights.Normal;
        }
    }

    private void MakeChip(TerminalTab tab)
    {
        var label = new TextBlock
        {
            Text = tab.Title,
            Foreground = FgBrush(),
            VerticalAlignment = VerticalAlignment.Center,
            TextTrimming = TextTrimming.CharacterEllipsis,
            MaxWidth = 160,
            FontSize = 12,
        };
        var close = new Border
        {
            Width = 18, Height = 18,
            Margin = new Thickness(6, 0, 0, 0),
            CornerRadius = new CornerRadius(3),
            Background = Brushes.Transparent,
            Child = new TextBlock { Text = "✕", FontSize = 9, Foreground = FgBrush(), HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center },
        };
        close.MouseEnter += (_, _) => close.Background = new SolidColorBrush(Color.FromArgb(0x66, 0xFF, 0xFF, 0xFF));
        close.MouseLeave += (_, _) => close.Background = Brushes.Transparent;
        close.MouseLeftButtonUp += (_, e) => { e.Handled = true; CloseTab(tab); };
        WindowChrome.SetIsHitTestVisibleInChrome(close, true);

        var sp = new StackPanel { Orientation = Orientation.Horizontal, VerticalAlignment = VerticalAlignment.Center };
        sp.Children.Add(label);
        sp.Children.Add(close);

        var chip = new Border
        {
            Height = TitleBarHeight - 6,
            MinWidth = 100,
            Margin = new Thickness(2, 3, 0, 0),
            Padding = new Thickness(10, 0, 6, 0),
            CornerRadius = new CornerRadius(4, 4, 0, 0),
            Background = Brushes.Transparent,
            Child = sp,
        };
        WindowChrome.SetIsHitTestVisibleInChrome(chip, true);
        chip.MouseLeftButtonUp += (_, _) => SelectTab(tab);
        chip.MouseDown += (_, e) => { if (e.ChangedButton == MouseButton.Middle) { CloseTab(tab); e.Handled = true; } };

        tab.Chip = chip;
        tab.Label = label;
    }

    private Border MakeToolButton(string glyph, Action onClick)
    {
        var b = new Border
        {
            Width = 28, Height = TitleBarHeight - 6,
            Margin = new Thickness(4, 3, 0, 0),
            CornerRadius = new CornerRadius(4),
            Background = Brushes.Transparent,
            Child = new TextBlock { Text = glyph, FontSize = 14, Foreground = FgBrush(), HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center },
        };
        WindowChrome.SetIsHitTestVisibleInChrome(b, true);
        b.MouseEnter += (_, _) => b.Background = new SolidColorBrush(Color.FromArgb(0x33, 0xFF, 0xFF, 0xFF));
        b.MouseLeave += (_, _) => b.Background = Brushes.Transparent;
        b.MouseLeftButtonUp += (_, _) => onClick();
        return b;
    }

    // ---- chrome helpers ----------------------------------------------------

    private Brush BgBrush()
    {
        uint c = Theme.Background;
        return new SolidColorBrush(Color.FromRgb((byte)(c >> 16), (byte)(c >> 8), (byte)c));
    }

    private Brush FgBrush()
    {
        uint c = Theme.Foreground;
        return new SolidColorBrush(Color.FromRgb((byte)(c >> 16), (byte)(c >> 8), (byte)c));
    }

    private static ImageSource? LoadAppIcon()
    {
        try { return BitmapFrame.Create(new Uri("pack://application:,,,/wsl.ico", UriKind.Absolute)); }
        catch { return null; }
    }

    private Border CaptionButton(string glyph, Action onClick, bool isClose)
    {
        var normal = Brushes.Transparent;
        var hover = isClose ? new SolidColorBrush(Color.FromRgb(0xC4, 0x2B, 0x1C))
                            : (Brush)new SolidColorBrush(Color.FromArgb(0x33, 0xFF, 0xFF, 0xFF));
        var b = new Border
        {
            Width = 44, Height = TitleBarHeight,
            Background = normal,
            Child = new TextBlock { Text = glyph, Foreground = FgBrush(), HorizontalAlignment = HorizontalAlignment.Center, VerticalAlignment = VerticalAlignment.Center, FontSize = 12 },
        };
        WindowChrome.SetIsHitTestVisibleInChrome(b, true);
        b.MouseEnter += (_, _) => b.Background = hover;
        b.MouseLeave += (_, _) => b.Background = normal;
        b.MouseLeftButtonUp += (_, _) => onClick();
        return b;
    }

    // Cheap PLAIN translucency without AllowsTransparency (no layered window): a
    // transparent WPF composition surface, with DWM honoring the per-pixel alpha
    // across the whole client area via an extended frame — so the terminal's
    // alpha background reveals the real desktop behind it (not an acrylic blur).
    private void EnableDwmTranslucency()
    {
        if (PresentationSource.FromVisual(this) is not HwndSource src) return;

        if (src.CompositionTarget is not null)
            src.CompositionTarget.BackgroundColor = Colors.Transparent;

        // Sheet-of-glass: extend the (now-empty) frame over the whole client area
        // so DWM composites the window's alpha against what's behind it.
        var margins = new Native.MARGINS { cxLeftWidth = -1, cxRightWidth = -1, cyTopHeight = -1, cyBottomHeight = -1 };
        Native.DwmExtendFrameIntoClientArea(src.Handle, margins);

        src.AddHook(MinMaxHook);   // keep the borderless maximize-size fix
    }

    private IntPtr MinMaxHook(IntPtr hwnd, int msg, IntPtr wParam, IntPtr lParam, ref bool handled)
    {
        const int WM_GETMINMAXINFO = 0x0024;
        if (msg != WM_GETMINMAXINFO) return IntPtr.Zero;
        IntPtr monitor = MonitorFromWindow(hwnd, 2);
        var mi = new MONITORINFO { cbSize = Marshal.SizeOf<MONITORINFO>() };
        if (GetMonitorInfo(monitor, ref mi))
        {
            var mmi = Marshal.PtrToStructure<MINMAXINFO>(lParam);
            mmi.ptMaxPosition.x = mi.rcWork.left - mi.rcMonitor.left;
            mmi.ptMaxPosition.y = mi.rcWork.top - mi.rcMonitor.top;
            mmi.ptMaxSize.x = mi.rcWork.right - mi.rcWork.left;
            mmi.ptMaxSize.y = mi.rcWork.bottom - mi.rcWork.top;
            Marshal.StructureToPtr(mmi, lParam, true);
        }
        return IntPtr.Zero;
    }

    [DllImport("user32.dll")] private static extern IntPtr MonitorFromWindow(IntPtr hwnd, int flags);
    [DllImport("user32.dll")] private static extern bool GetMonitorInfo(IntPtr hMonitor, ref MONITORINFO mi);
    [StructLayout(LayoutKind.Sequential)] private struct RECT { public int left, top, right, bottom; }
    [StructLayout(LayoutKind.Sequential)] private struct MONITORINFO { public int cbSize; public RECT rcMonitor; public RECT rcWork; public int dwFlags; }
    [StructLayout(LayoutKind.Sequential)] private struct POINT { public int x, y; }
    [StructLayout(LayoutKind.Sequential)] private struct MINMAXINFO { public POINT ptReserved, ptMaxSize, ptMaxPosition, ptMinTrackSize, ptMaxTrackSize; }

    // ---- window actions ----------------------------------------------------

    private void SpawnNewWindow()
    {
        try { new MainWindow(_distro, _serverWinPath, Settings.Load(), _active?.Active.Term.CurrentDirectory).Show(); }
        catch (Exception ex) { Log("new window failed: " + ex.Message); }
    }

    private void OpenSettings()
    {
        var dlg = new SettingsWindow(_settings, ApplyAppearance) { Owner = this };
        if (dlg.ShowDialog() == true && dlg.Result is not null) { _settings = dlg.Result; _settings.Save(); }
        _active?.Active.View.Focus();
    }

    private void ApplyAppearance(Settings s)
    {
        double op = _translucent ? Math.Clamp(s.Opacity, 10, 100) / 100.0 : 1.0;
        foreach (var t in _tabs)
        {
            foreach (var p in t.Panes)
            {
                p.View.ApplySettings(s);
                p.View.SetBackgroundOpacity(op);
                p.View.MarkDirty();
            }
            t.Label.Foreground = FgBrush();
        }
        _topBar.Background = BgBrush();
        Background = _translucent ? Brushes.Transparent : BgBrush();
        RefreshChips();
    }

    private void ApplyFontToAll(double points)
    {
        foreach (var t in _tabs)
            foreach (var p in t.Panes)
                p.View.SetFontSize(points);   // SetFontSize no-ops if unchanged
    }

    private void OnLoaded(object? sender, RoutedEventArgs e)
    {
        try { _mux = WslMuxManager.Get(_distro, _serverWinPath); }
        catch (Exception ex) { Log("mux start failed: " + ex.Message); }
        AddTab(_startDir);
    }

    private void OnClosed(object? sender, EventArgs e)
    {
        _closing = true;
        foreach (var t in _tabs)
            foreach (var p in t.Panes) { try { p.Session?.Close(); } catch { } }
    }

    /// <summary>Render the active tab's surface to a PNG + pixel stats (--shottest).</summary>
    public (long nonBg, long colored, int w, int h) CaptureStats(string pngPath)
    {
        var view = _active?.Active.View;
        if (view is null) return (0, 0, 0, 0);
        int w = (int)view.ActualWidth, h = (int)view.ActualHeight;
        if (w <= 0 || h <= 0) return (0, 0, 0, 0);

        var rtb = new RenderTargetBitmap(w, h, 96, 96, PixelFormats.Pbgra32);
        rtb.Render(view);
        var enc = new PngBitmapEncoder();
        enc.Frames.Add(BitmapFrame.Create(rtb));
        using (var fs = File.Create(pngPath)) enc.Save(fs);

        int stride = w * 4;
        var px = new byte[h * stride];
        rtb.CopyPixels(px, stride, 0);
        long nonBg = 0, colored = 0;
        for (int i = 0; i < px.Length; i += 4)
        {
            byte b = px[i], g = px[i + 1], r = px[i + 2];
            if (Math.Abs(r - 0x0C) > 10 || Math.Abs(g - 0x0C) > 10 || Math.Abs(b - 0x0C) > 10) nonBg++;
            int mx = Math.Max(r, Math.Max(g, b)), mn = Math.Min(r, Math.Min(g, b));
            if (mx - mn > 40) colored++;
        }
        return (nonBg, colored, w, h);
    }

    // ---- test hooks (--tabtest) -------------------------------------------
    internal void TestNewTab() => AddTab(_active?.Active.Term.CurrentDirectory);
    internal int TestTabCount => _tabs.Count;
    internal void TestSwitch(int d) => SwitchTab(d);
    internal void TestCloseActive() { if (_active is not null) CloseTab(_active); }
    internal void TestSplit(bool columns) { if (_active is not null) SplitPane(_active, _active.Active, columns); }
    internal int TestPaneCount => _active?.Panes.Count ?? 0;
    internal void TestClosePane() { if (_active is not null) ClosePane(_active, _active.Active); }

    internal void CaptureWindow(string png)
    {
        var root = (FrameworkElement)Content;
        int w = (int)root.ActualWidth, h = (int)root.ActualHeight;
        if (w <= 0 || h <= 0) return;
        var rtb = new RenderTargetBitmap(w, h, 96, 96, PixelFormats.Pbgra32);
        rtb.Render(root);
        var enc = new PngBitmapEncoder();
        enc.Frames.Add(BitmapFrame.Create(rtb));
        using var fs = File.Create(png);
        enc.Save(fs);
    }

    private static void Log(string msg)
    {
        try
        {
            File.AppendAllText(Path.Combine(Path.GetTempPath(), "wslterminal.log"),
                $"{DateTime.Now:HH:mm:ss.fff} {msg}{Environment.NewLine}");
        }
        catch { }
    }
}
