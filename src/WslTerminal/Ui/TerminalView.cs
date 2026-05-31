using System.Globalization;
using System.Text;
using System.Windows;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>
/// WPF surface that renders a <see cref="Terminal"/> grid via GlyphRuns and
/// turns keyboard/mouse input into a byte stream for the PTY. Rendering is
/// paced at the monitor refresh rate and dirty-row cached; resizing reports new cols/rows.
/// </summary>
public sealed class TerminalView : FrameworkElement
{
    private readonly Terminal _term;

    // font / metrics
    private GlyphTypeface _gtRegular = null!, _gtBold = null!, _gtItalic = null!, _gtBoldItalic = null!;
    private Typeface _tfRegular = null!, _tfBold = null!, _tfItalic = null!, _tfBoldItalic = null!;
    private readonly Dictionary<int, bool> _glyphCache = new();   // rune -> primary font has glyph

    // Shared across all windows (single UI thread): Direct2D color-emoji rasterizer.
    private static EmojiRenderer? _emoji;
    private static bool _emojiInitTried;
    private string _fontFamily = "Cascadia Mono";
    private double _fontPoints = 11.0;             // user-facing size in points (WT/conhost convention)
    private double _emSize = 11.0 * 96.0 / 72.0;   // WPF DIP em size used for rendering
    private double _baseFontPoints = 11.0;         // Ctrl+0 resets to this
    private static double PtToEm(double pt) => pt * 96.0 / 72.0;
    private double _cellW, _cellH, _baseline;
    private float _ppd = 1.0f;

    // grid snapshot buffer
    private Cell[][] _dest = Array.Empty<Cell[]>();
    private int _cols, _rows;

    private int _scrollOffset;
    private bool _focused;
    private volatile int _dirty = 1;

    // selection, in absolute row indices (0 = oldest scrollback line)
    private bool _selecting, _hasSelection;
    private int _selAr, _selAc, _selBr, _selBc;

    private readonly Dictionary<uint, SolidColorBrush> _brushes = new();

    public int GridCols => _cols;
    public int GridRows => _rows;
    public (double w, double h) CellSize => (_cellW, _cellH);

    public event Action<byte[]>? Input;
    public event Action<int, int>? Resized;
    public event Action? NewWindowRequested;
    public event Action? OpenSettingsRequested;
    public event Action<double>? FontSizeChanged;
    public event Action? NewTabRequested;
    public event Action? ClosePaneRequested;
    public event Action<int>? SwitchTabRequested;   // +1 next, -1 previous
    public event Action<bool>? SplitRequested;      // true = split right (columns), false = split down (rows)
    public event Action? Focused;
    public event Action? ToggleSidebarRequested;    // Ctrl+Shift+E toggles the file sidebar
    public event Action? ToggleHiddenRequested;     // Ctrl+Shift+H toggles hidden files in the sidebar

    public TerminalView(Terminal term)
    {
        _term = term;
        Focusable = true;
        FocusVisualStyle = null;
        SnapsToDevicePixels = true;
        TextOptions.SetTextRenderingMode(this, TextRenderingMode.ClearType);
        TextOptions.SetTextFormattingMode(this, TextFormattingMode.Ideal);

        LoadFonts();

        // Pace repaints at the monitor's refresh rate (e.g. 60/120/144 Hz) so the
        // terminal feels as smooth as the display allows. The _dirty gate means a
        // tick only repaints when something changed, so idle still costs nothing;
        // the dirty-row cache keeps each repaint cheap. (Reads the primary display
        // at startup; doesn't re-read if the window moves to a different monitor.)
        int hz = Math.Clamp(DisplayRefreshHz(), 30, 360);
        var timer = new DispatcherTimer(DispatcherPriority.Render)
        {
            Interval = TimeSpan.FromMilliseconds(1000.0 / hz),
        };
        timer.Tick += (_, _) => { if (Interlocked.Exchange(ref _dirty, 0) != 0) InvalidateVisual(); };
        timer.Start();

        Loaded += (_, _) => Focus();
    }

    // Primary display's vertical refresh in Hz (GetDeviceCaps VREFRESH). Returns
    // 0/1 for "default/unknown" on some virtual displays; caller clamps. Falls
    // back to 60 on any failure.
    private static int DisplayRefreshHz()
    {
        try
        {
            IntPtr dc = Native.GetDC(IntPtr.Zero);
            if (dc != IntPtr.Zero)
            {
                int hz = Native.GetDeviceCaps(dc, Native.VREFRESH);
                Native.ReleaseDC(IntPtr.Zero, dc);
                if (hz > 1) return hz;
            }
        }
        catch { /* fall through */ }
        return 60;
    }

    public double FontEmSize => _emSize;

    /// <summary>Apply persisted appearance settings (font + colors) and re-render.
    /// FontSize is in points (WT/conhost convention); converted to DIP for WPF.</summary>
    public void ApplySettings(Settings s)
    {
        Theme.Apply(s);
        _fontFamily = s.FontFamily;
        _fontPoints = Math.Clamp(s.FontSize, 6, 72);
        _baseFontPoints = _fontPoints;
        _emSize = PtToEm(_fontPoints);
        _brushes.Clear();              // colors may have changed
        ReloadFontAndGrid();
    }

    /// <summary>Adjust the font size by <paramref name="delta"/> points (Ctrl +/-, Ctrl+wheel).</summary>
    public void ZoomFont(double delta) => SetFontSize(_fontPoints + delta);

    public void SetFontSize(double points)
    {
        points = Math.Clamp(Math.Round(points), 6, 72);
        if (Math.Abs(points - _fontPoints) < 0.5) return;
        _fontPoints = points;
        _emSize = PtToEm(points);
        ReloadFontAndGrid();
        FontSizeChanged?.Invoke(_fontPoints);     // persist points
    }

    private void ReloadFontAndGrid()
    {
        LoadFonts();
        if (RenderSize.Width > 0 && RenderSize.Height > 0) RecomputeGrid(RenderSize);
        InvalidateVisual();
    }

    private double _bgOpacity = 1.0;   // <1 lets the desktop show through the default background

    /// <summary>Set the terminal background opacity (0..1). Text stays opaque.</summary>
    public void SetBackgroundOpacity(double o) { _bgOpacity = Math.Clamp(o, 0.0, 1.0); InvalidateVisual(); }

    /// <summary>Flag the surface dirty; safe to call from the reader thread.</summary>
    public void MarkDirty() => _dirty = 1;

    /// <summary>Size the grid for a given pixel size without a layout pass (used by tests).</summary>
    public void EnsureGrid(double width, double height) => RecomputeGrid(new Size(width, height));

    private void LoadFonts()
    {
        Typeface MakeTf(FontStyle style, FontWeight weight)
        {
            foreach (var fam in new[] { _fontFamily, "Cascadia Mono", "Cascadia Code", "Consolas", "Lucida Console" })
            {
                if (string.IsNullOrWhiteSpace(fam)) continue;
                var tf = new Typeface(new FontFamily(fam), style, weight, FontStretches.Normal);
                if (tf.TryGetGlyphTypeface(out _)) return tf;
            }
            return new Typeface(new FontFamily("Courier New"), style, weight, FontStretches.Normal);
        }

        _glyphCache.Clear();
        _tfRegular = MakeTf(FontStyles.Normal, FontWeights.Normal);
        _tfBold = MakeTf(FontStyles.Normal, FontWeights.Bold);
        _tfItalic = MakeTf(FontStyles.Italic, FontWeights.Normal);
        _tfBoldItalic = MakeTf(FontStyles.Italic, FontWeights.Bold);
        _tfRegular.TryGetGlyphTypeface(out _gtRegular!);
        _tfBold.TryGetGlyphTypeface(out _gtBold!);
        _tfItalic.TryGetGlyphTypeface(out _gtItalic!);
        _tfBoldItalic.TryGetGlyphTypeface(out _gtBoldItalic!);

        ushort zero = _gtRegular.CharacterToGlyphMap.TryGetValue('0', out var gi) ? gi : (ushort)0;
        _cellW = Math.Round(_gtRegular.AdvanceWidths[zero] * _emSize, MidpointRounding.AwayFromZero);
        if (_cellW <= 0) _cellW = _emSize * 0.6;
        _cellH = Math.Ceiling(_gtRegular.Height * _emSize);
        _baseline = _gtRegular.Baseline * _emSize;
    }

    private SolidColorBrush BackgroundBrush()
    {
        uint c = Theme.Background;
        byte a = (byte)Math.Round(_bgOpacity * 255);
        return new SolidColorBrush(Color.FromArgb(a, (byte)(c >> 16), (byte)(c >> 8), (byte)c));
    }

    private SolidColorBrush Brush(uint rgb)
    {
        if (_brushes.TryGetValue(rgb, out var b)) return b;
        var c = Color.FromRgb((byte)(rgb >> 16), (byte)(rgb >> 8), (byte)rgb);
        b = new SolidColorBrush(c);
        b.Freeze();
        _brushes[rgb] = b;
        return b;
    }

    // ---- layout ------------------------------------------------------------

    protected override Size MeasureOverride(Size availableSize)
    {
        if (double.IsInfinity(availableSize.Width)) availableSize.Width = _cellW * 80;
        if (double.IsInfinity(availableSize.Height)) availableSize.Height = _cellH * 24;
        return availableSize;
    }

    protected override void OnRenderSizeChanged(SizeChangedInfo info)
    {
        base.OnRenderSizeChanged(info);
        RecomputeGrid(info.NewSize);
    }

    private void RecomputeGrid(Size size)
    {
        int cols = Math.Max(1, (int)(size.Width / _cellW));
        int rows = Math.Max(1, (int)(size.Height / _cellH));
        if (cols == _cols && rows == _rows && _dest.Length == rows) return;

        _cols = cols; _rows = rows;
        _dest = new Cell[rows][];
        for (int r = 0; r < rows; r++) _dest[r] = new Cell[cols];

        _term.Resize(cols, rows);
        Resized?.Invoke(cols, rows);
        _dirty = 1;
    }

    // ---- rendering ---------------------------------------------------------

    protected override void OnRender(DrawingContext dc)
    {
        _ppd = (float)VisualTreeHelper.GetDpi(this).PixelsPerDip;

        // Ensure the grid matches the current size even if OnRenderSizeChanged
        // didn't fire (e.g. a manually arranged, unconnected visual).
        if (RenderSize.Width > 0 && RenderSize.Height > 0) RecomputeGrid(RenderSize);

        // full-surface background (carries window opacity); cheap, drawn every frame
        dc.DrawRectangle(BackgroundBrush(), null, new Rect(0, 0, RenderSize.Width, RenderSize.Height));
        if (_dest.Length == 0) return;

        ViewportInfo vp = _term.CaptureViewport(_scrollOffset, _dest);

        // Render every row each painted frame. (A previous dirty-row cache reused
        // per-row drawings to save layout work, but it served stale rows under heavy
        // TUI redraw — e.g. Claude Code — causing overlapping/garbled output. Frames
        // are already gated by the _dirty flag and paced to the monitor refresh, so a
        // full per-row render stays cheap; correctness first.)
        for (int r = 0; r < _rows && r < _dest.Length; r++)
            RenderRow(dc, r, _dest[r]);

        // cursor: live overlay, redrawn every frame (only when viewing the live bottom)
        if (_scrollOffset == 0 && vp.CursorVisible &&
            vp.CursorX >= 0 && vp.CursorX < _cols && vp.CursorY >= 0 && vp.CursorY < _rows)
        {
            DrawCursor(dc, vp.CursorX, vp.CursorY, _dest[vp.CursorY][vp.CursorX]);
        }
    }

    private void RenderRow(DrawingContext dc, int row, Cell[] cells)
    {
        double y = row * _cellH;

        // backgrounds: coalesce equal-bg spans (skip default background)
        int c = 0;
        while (c < _cols)
        {
            (uint bg, _) = Colors(cells[c]);
            int start = c;
            while (c < _cols && Colors(cells[c]).bg == bg) c++;
            if (bg != Theme.Background)
                dc.DrawRectangle(Brush(bg), null, new Rect(start * _cellW, y, (c - start) * _cellW, _cellH));
        }

        // selection highlight (drawn over backgrounds, under glyphs)
        if (SelSpan(row, out int selS, out int selE))
            dc.DrawRectangle(Brush(Theme.Selection), null,
                new Rect(selS * _cellW, y, (selE - selS + 1) * _cellW, _cellH));

        // foreground: fast GlyphRun batches for cells the primary font has;
        // FormattedText (WPF font fallback) for cells it lacks or that carry
        // combining marks — that's what makes emoji / kaomoji / CJK render.
        c = 0;
        while (c < _cols)
        {
            Cell cell = cells[c];
            if (cell.Width == 0) { c++; continue; }              // wide-char continuation

            if (IsComplex(cell))
            {
                DrawComplexCell(dc, c, y, cell);
                c += Math.Max((byte)1, cell.Width);
                continue;
            }

            (uint _, uint fg0) = Colors(cell);
            GlyphTypeface gt = VariantOf(cell.Flags);
            bool underline = (cell.Flags & CellFlags.Underline) != 0;
            bool strike = (cell.Flags & CellFlags.Strike) != 0;
            int start = c;
            var indices = new List<ushort>();
            var advances = new List<double>();

            while (c < _cols)
            {
                Cell cc = cells[c];
                if (cc.Width == 0) { c++; continue; }
                if (IsComplex(cc)) break;
                (uint _, uint fg) = Colors(cc);
                if (fg != fg0 || VariantOf(cc.Flags) != gt ||
                    ((cc.Flags & CellFlags.Underline) != 0) != underline ||
                    ((cc.Flags & CellFlags.Strike) != 0) != strike)
                    break;

                int rune = (cc.Flags & CellFlags.Hidden) != 0 || cc.Rune == 0 ? ' ' : cc.Rune;
                ushort gi = gt.CharacterToGlyphMap.TryGetValue(rune, out var g) ? g
                          : (gt.CharacterToGlyphMap.TryGetValue(' ', out var sg) ? sg : (ushort)0);
                indices.Add(gi);
                advances.Add(_cellW * Math.Max((byte)1, cc.Width));
                c++;
            }

            if (indices.Count > 0)
            {
                var origin = new Point(start * _cellW, y + _baseline);
                try
                {
                    var run = new GlyphRun(gt, 0, false, _emSize, _ppd, indices,
                        origin, advances, null, null, null, null, null, null);
                    dc.DrawGlyphRun(Brush(fg0), run);
                }
                catch { /* skip un-renderable run */ }
                double runW = 0; foreach (var a in advances) runW += a;
                DrawDecorations(dc, fg0, underline, strike, origin.X, y, runW);
            }
        }
    }

    private bool IsComplex(Cell cell)
    {
        if (cell.Combo != null) return true;
        if (cell.Rune == 0 || (cell.Flags & CellFlags.Hidden) != 0) return false;
        return !PrimaryHasGlyph(cell.Rune);
    }

    private bool PrimaryHasGlyph(int rune)
    {
        if (_glyphCache.TryGetValue(rune, out bool has)) return has;
        has = _gtRegular.CharacterToGlyphMap.ContainsKey(rune);
        _glyphCache[rune] = has;
        return has;
    }

    private Typeface TypefaceVariant(CellFlags f)
    {
        bool bold = (f & CellFlags.Bold) != 0, italic = (f & CellFlags.Italic) != 0;
        return bold ? (italic ? _tfBoldItalic : _tfBold) : (italic ? _tfItalic : _tfRegular);
    }

    // Render one cell that the primary font can't handle (or that has combining
    // marks) via FormattedText, which does WPF font fallback. Emoji render
    // monochrome — WPF has no color-font support; that needs DirectWrite.
    private static bool IsEmojiCell(Cell cell)
    {
        int r = cell.Rune;
        if (r >= 0x1F000 && r <= 0x1FAFF) return true;            // emoji & pictographs
        if (r >= 0x1F1E6 && r <= 0x1F1FF) return true;            // regional indicators (flags)
        if (cell.Combo is not null)
            foreach (char ch in cell.Combo)
                if (ch == '️') return true;             // VS16 -> emoji presentation
        return false;
    }

    private static EmojiRenderer? Emoji()
    {
        if (!_emojiInitTried)
        {
            _emojiInitTried = true;
            try { _emoji = new EmojiRenderer(); } catch { _emoji = null; }
        }
        return _emoji;
    }

    private void DrawComplexCell(DrawingContext dc, int col, double y, Cell cell)
    {
        (uint _, uint fg) = Colors(cell);
        int w = Math.Max((byte)1, cell.Width);
        string text = (cell.Rune == 0 ? " " : char.ConvertFromUtf32(cell.Rune)) + (cell.Combo ?? "");

        // Color emoji via Direct2D (cached bitmap); everything else via FormattedText.
        if (IsEmojiCell(cell) && Emoji() is { } emoji)
        {
            int pxW = Math.Max(1, (int)Math.Round(w * _cellW * _ppd));
            int pxH = Math.Max(1, (int)Math.Round(_cellH * _ppd));
            var bmp = emoji.Get(text, pxW, pxH, (float)(_emSize * _ppd));
            if (bmp is not null)
            {
                dc.DrawImage(bmp, new Rect(col * _cellW, y, w * _cellW, _cellH));
                return;
            }
        }

        try
        {
            var ft = new FormattedText(text, CultureInfo.InvariantCulture, FlowDirection.LeftToRight,
                TypefaceVariant(cell.Flags), _emSize, Brush(fg), _ppd)
            {
                MaxTextWidth = w * _cellW,
                MaxTextHeight = _cellH,
                Trimming = TextTrimming.None,
            };
            dc.DrawText(ft, new Point(col * _cellW, y + (_baseline - ft.Baseline)));  // align baselines
        }
        catch { /* unrenderable */ }
        DrawDecorations(dc, fg, (cell.Flags & CellFlags.Underline) != 0,
            (cell.Flags & CellFlags.Strike) != 0, col * _cellW, y, w * _cellW);
    }

    private void DrawDecorations(DrawingContext dc, uint fg, bool underline, bool strike, double x, double y, double w)
    {
        if (underline)
        {
            double uy = Math.Round(y + _baseline + 1.5);
            dc.DrawLine(new Pen(Brush(fg), 1), new Point(x, uy), new Point(x + w, uy));
        }
        if (strike)
        {
            double sy = Math.Round(y + _cellH * 0.5);
            dc.DrawLine(new Pen(Brush(fg), 1), new Point(x, sy), new Point(x + w, sy));
        }
    }

    private void DrawCursor(DrawingContext dc, int cx, int cy, Cell cell)
    {
        var rect = new Rect(cx * _cellW, cy * _cellH, _cellW, _cellH);
        if (!_focused)
        {
            dc.DrawRectangle(null, new Pen(Brush(Theme.CursorColor), 1), rect);
            return;
        }
        dc.DrawRectangle(Brush(Theme.CursorColor), null, rect);
        int rune = cell.Rune == 0 ? ' ' : cell.Rune;
        GlyphTypeface gt = VariantOf(cell.Flags);
        if (gt.CharacterToGlyphMap.TryGetValue(rune, out var gi))
        {
            try
            {
                var run = new GlyphRun(gt, 0, false, _emSize, _ppd,
                    new[] { gi }, new Point(rect.X, rect.Y + _baseline),
                    new[] { _cellW }, null, null, null, null, null, null);
                dc.DrawGlyphRun(Brush(Theme.Background), run);
            }
            catch { }
        }
    }

    private GlyphTypeface VariantOf(CellFlags f)
    {
        bool bold = (f & CellFlags.Bold) != 0;
        bool italic = (f & CellFlags.Italic) != 0;
        return bold ? (italic ? _gtBoldItalic : _gtBold) : (italic ? _gtItalic : _gtRegular);
    }

    private static (uint bg, uint fg) Colors(Cell cell)
    {
        bool bold = (cell.Flags & CellFlags.Bold) != 0;
        uint fg = Theme.Resolve(cell.Fg, true, bold);
        uint bg = Theme.Resolve(cell.Bg, false, false);
        if ((cell.Flags & CellFlags.Reverse) != 0) (fg, bg) = (bg, fg);
        return (bg, fg);
    }

    // ---- input -------------------------------------------------------------

    protected override void OnGotKeyboardFocus(KeyboardFocusChangedEventArgs e) { _focused = true; Focused?.Invoke(); InvalidateVisual(); }
    protected override void OnLostKeyboardFocus(KeyboardFocusChangedEventArgs e) { _focused = false; InvalidateVisual(); }

    // absolute row of the first visible line (0 = oldest scrollback line)
    private int TopAbs => _term.ScrollbackCount - _scrollOffset;

    private (int col, int row) CellAt(Point p)
    {
        int col = _cellW > 0 ? (int)(p.X / _cellW) : 0;
        int row = _cellH > 0 ? (int)(p.Y / _cellH) : 0;
        return (Math.Clamp(col, 0, Math.Max(0, _cols - 1)), Math.Clamp(row, 0, Math.Max(0, _rows - 1)));
    }

    protected override void OnMouseLeftButtonDown(MouseButtonEventArgs e)
    {
        Focus();
        var (col, row) = CellAt(e.GetPosition(this));
        int abs = TopAbs + row;
        if (e.ClickCount == 2 && _term.WordSpan(abs, col, out int ws, out int we))
        {
            _selAr = _selBr = abs; _selAc = ws; _selBc = we; _hasSelection = true; _selecting = false;
        }
        else if (e.ClickCount >= 3)
        {
            _selAr = _selBr = abs; _selAc = 0; _selBc = _cols - 1; _hasSelection = true; _selecting = false;
        }
        else
        {
            _selAr = _selBr = abs; _selAc = _selBc = col; _hasSelection = false; _selecting = true;
            CaptureMouse();
        }
        InvalidateVisual();
        e.Handled = true;
    }

    protected override void OnMouseMove(System.Windows.Input.MouseEventArgs e)
    {
        if (_selecting && e.LeftButton == MouseButtonState.Pressed)
        {
            var (col, row) = CellAt(e.GetPosition(this));
            _selBr = TopAbs + row; _selBc = col;
            _hasSelection = !(_selAr == _selBr && _selAc == _selBc);
            InvalidateVisual();
        }
    }

    protected override void OnMouseLeftButtonUp(MouseButtonEventArgs e)
    {
        if (_selecting) { _selecting = false; ReleaseMouseCapture(); InvalidateVisual(); }
    }

    protected override void OnMouseRightButtonUp(MouseButtonEventArgs e)
    {
        if (_hasSelection) { CopySelection(); ClearSelection(); }
        else Paste();
        e.Handled = true;
    }

    private void CopySelection()
    {
        if (!_hasSelection) return;
        string text = _term.GetText(_selAr, _selAc, _selBr, _selBc);
        if (string.IsNullOrEmpty(text)) return;
        try { Clipboard.SetText(text); } catch { /* clipboard busy */ }
    }

    private void ClearSelection()
    {
        if (!_hasSelection && !_selecting) return;
        _hasSelection = false; _selecting = false;
        InvalidateVisual();
    }

    /// <summary>Test hook: set a selection in absolute coordinates (used by --rendertest).</summary>
    internal void SelectForTest(int ar, int ac, int br, int bc)
    {
        _selAr = ar; _selAc = ac; _selBr = br; _selBc = bc; _hasSelection = true;
        InvalidateVisual();
    }

    private bool SelSpan(int viewRow, out int s, out int e)
    {
        s = e = 0;
        if (!_hasSelection) return false;
        int r1 = _selAr, c1 = _selAc, r2 = _selBr, c2 = _selBc;
        if (r1 > r2 || (r1 == r2 && c1 > c2)) { (r1, c1, r2, c2) = (r2, c2, r1, c1); }
        int abs = TopAbs + viewRow;
        if (abs < r1 || abs > r2) return false;
        s = abs == r1 ? c1 : 0;
        e = abs == r2 ? c2 : _cols - 1;
        s = Math.Clamp(s, 0, _cols - 1);
        e = Math.Clamp(e, 0, _cols - 1);
        return s <= e;
    }

    protected override void OnTextInput(TextCompositionEventArgs e)
    {
        if (!string.IsNullOrEmpty(e.Text))
        {
            ClearSelection();
            ResetScroll();
            Send(Encoding.UTF8.GetBytes(e.Text));
            e.Handled = true;
        }
    }

    protected override void OnPreviewKeyDown(KeyEventArgs e)
    {
        var mods = Keyboard.Modifiers;
        bool ctrl = mods.HasFlag(ModifierKeys.Control);
        bool shift = mods.HasFlag(ModifierKeys.Shift);
        bool alt = mods.HasFlag(ModifierKeys.Alt);
        // Alt combos arrive as Key.System with the real key in SystemKey.
        Key key = e.Key == Key.System ? e.SystemKey : e.Key;

        // split panes (Windows Terminal: Alt+Shift+- down, Alt+Shift+= right)
        if (alt && shift && key is Key.OemMinus or Key.Subtract) { SplitRequested?.Invoke(false); e.Handled = true; return; }
        if (alt && shift && key is Key.OemPlus or Key.Add) { SplitRequested?.Invoke(true); e.Handled = true; return; }

        // app shortcuts (checked before key encoding so e.g. Ctrl+Shift+N isn't sent as ^N)
        if (ctrl && shift && key == Key.C && _hasSelection) { CopySelection(); e.Handled = true; return; }
        if (ctrl && shift && key == Key.N) { NewWindowRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && shift && key == Key.T) { NewTabRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && shift && key == Key.W) { ClosePaneRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && shift && key == Key.E) { ToggleSidebarRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && shift && key == Key.H) { ToggleHiddenRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && key == Key.Tab) { SwitchTabRequested?.Invoke(shift ? -1 : 1); e.Handled = true; return; }
        if (ctrl && key == Key.OemComma) { OpenSettingsRequested?.Invoke(); e.Handled = true; return; }
        if (ctrl && key is Key.OemPlus or Key.Add) { ZoomFont(+1); e.Handled = true; return; }
        if (ctrl && key is Key.OemMinus or Key.Subtract) { ZoomFont(-1); e.Handled = true; return; }
        if (ctrl && key is Key.D0 or Key.NumPad0) { SetFontSize(_baseFontPoints); e.Handled = true; return; }

        // paste / scroll shortcuts
        if (ctrl && shift && key == Key.V) { Paste(); e.Handled = true; return; }
        if (key == Key.Insert && shift) { Paste(); e.Handled = true; return; }
        if (key == Key.PageUp && shift) { Scroll(_rows - 1); e.Handled = true; return; }
        if (key == Key.PageDown && shift) { Scroll(-(_rows - 1)); e.Handled = true; return; }

        byte[]? seq = InputEncoder.Encode(key, mods, _term.AppCursorKeys);
        if (seq is not null)
        {
            ClearSelection();
            ResetScroll();
            Send(seq);
            e.Handled = true;
        }
    }

    private void Paste()
    {
        if (!Clipboard.ContainsText()) return;
        string text = Clipboard.GetText().Replace("\r\n", "\r").Replace('\n', '\r');
        var bytes = Encoding.UTF8.GetBytes(text);
        if (_term.BracketedPaste)
        {
            var pre = Encoding.ASCII.GetBytes("\x1b[200~");
            var post = Encoding.ASCII.GetBytes("\x1b[201~");
            var all = new byte[pre.Length + bytes.Length + post.Length];
            Buffer.BlockCopy(pre, 0, all, 0, pre.Length);
            Buffer.BlockCopy(bytes, 0, all, pre.Length, bytes.Length);
            Buffer.BlockCopy(post, 0, all, pre.Length + bytes.Length, post.Length);
            bytes = all;
        }
        ResetScroll();
        Send(bytes);
    }

    protected override void OnMouseWheel(MouseWheelEventArgs e)
    {
        if (Keyboard.Modifiers.HasFlag(ModifierKeys.Control))   // Ctrl+wheel = zoom
        {
            ZoomFont(e.Delta > 0 ? +1 : -1);
            e.Handled = true;
            return;
        }
        Scroll(e.Delta / 120 * 3);
        e.Handled = true;
    }

    private void Scroll(int lines)
    {
        int max = _term.ScrollbackCount;
        _scrollOffset = Math.Clamp(_scrollOffset + lines, 0, max);
        InvalidateVisual();
    }

    private void ResetScroll() { if (_scrollOffset != 0) { _scrollOffset = 0; InvalidateVisual(); } }

    private void Send(byte[] data) => Input?.Invoke(data);
}
