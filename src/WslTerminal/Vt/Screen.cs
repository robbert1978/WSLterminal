namespace WslTerminal.Vt;

/// <summary>
/// The terminal screen: a grid of cells with a cursor, current pen, scroll
/// region, primary/alternate buffers, and primary-buffer scrollback. The
/// VtParser calls these operations; the UI snapshots the grid for rendering.
/// All access is expected to happen under Terminal.Sync.
/// </summary>
public sealed class Screen
{
    public int Cols { get; private set; }
    public int Rows { get; private set; }

    private Cell[][] _buf = Array.Empty<Cell[]>();   // active buffer (points at primary or alt)
    private Cell[][] _primary = Array.Empty<Cell[]>();
    private Cell[][] _alt = Array.Empty<Cell[]>();
    private bool _inAlt;

    public int Cx { get; private set; }
    public int Cy { get; private set; }
    private bool _wrapPending;

    // current pen
    private int _fg = Theme.Default, _bg = Theme.Default;
    private CellFlags _flags;

    // scroll region (0-based, inclusive)
    private int _top, _bot;

    // saved cursor (DECSC / DECRC) per buffer
    private (int cx, int cy, int fg, int bg, CellFlags fl, bool origin) _saved;

    private bool[] _tabs = Array.Empty<bool>();

    private readonly Scrollback _scrollback = new(5000);

    // modes exposed to the input encoder / UI
    public bool AutoWrap = true;
    public bool OriginMode;
    public bool CursorVisible = true;
    public bool AppCursorKeys;
    public bool AppKeypad;
    public bool BracketedPaste;
    public bool InsertMode;
    public MouseTracking Mouse = MouseTracking.None;
    public bool MouseSgr;

    public int ScrollbackCount => _scrollback.Count;

    public Screen(int cols, int rows) => Resize(cols, rows);

    // ---- buffer helpers ----------------------------------------------------

    private Cell BlankCell() => new() { Rune = 0, Fg = Theme.Default, Bg = _bg, Flags = CellFlags.None, Width = 1 };

    private Cell[] BlankLine()
    {
        var line = new Cell[Cols];
        var blank = BlankCell();
        for (int i = 0; i < Cols; i++) line[i] = blank;
        return line;
    }

    public void Resize(int cols, int rows)
    {
        cols = Math.Max(1, cols);
        rows = Math.Max(1, rows);

        Cell[][] Rebuild(Cell[][] old)
        {
            var nb = new Cell[rows][];
            for (int r = 0; r < rows; r++)
            {
                nb[r] = new Cell[cols];
                for (int c = 0; c < cols; c++)
                    nb[r][c] = (r < old.Length && c < (old.Length > 0 ? old[0].Length : 0))
                        ? old[r][c]
                        : new Cell { Rune = 0, Fg = Theme.Default, Bg = Theme.Default, Width = 1 };
            }
            return nb;
        }

        _primary = Rebuild(_primary);
        _alt = Rebuild(_alt);
        Cols = cols;
        Rows = rows;
        _buf = _inAlt ? _alt : _primary;

        _top = 0;
        _bot = rows - 1;
        Cx = Math.Min(Cx, cols - 1);
        Cy = Math.Min(Cy, rows - 1);
        _wrapPending = false;

        _tabs = new bool[cols];
        for (int i = 0; i < cols; i++) _tabs[i] = (i % 8) == 0;
    }

    // ---- writing -----------------------------------------------------------

    private int _lastBaseX = -1, _lastBaseY = -1;   // where to attach combining marks

    public void PutRune(int cp)
    {
        int w = CharWidth.Of(cp);
        if (w == 0)                                  // combining / zero-width: attach to the base
        {
            if (_lastBaseX >= 0 && _lastBaseY >= 0 && _lastBaseY < Rows && _lastBaseX < Cols)
            {
                ref Cell baseCell = ref _buf[_lastBaseY][_lastBaseX];
                if (baseCell.Rune != 0 && (baseCell.Combo?.Length ?? 0) < 16)
                    baseCell.Combo += char.ConvertFromUtf32(cp);
            }
            return;
        }

        if (_wrapPending)
        {
            _wrapPending = false;
            Cx = 0;
            Index();
        }
        if (w == 2 && Cx == Cols - 1 && AutoWrap)
        {
            Cx = 0;
            Index();
        }

        var row = _buf[Cy];
        if (InsertMode)
        {
            for (int c = Cols - 1; c >= Cx + w; c--) row[c] = row[c - w];
        }
        row[Cx] = new Cell { Rune = cp, Fg = _fg, Bg = _bg, Flags = _flags, Width = (byte)w };
        if (w == 2 && Cx + 1 < Cols)
            row[Cx + 1] = new Cell { Rune = 0, Fg = _fg, Bg = _bg, Flags = _flags, Width = 0 };

        _lastBaseX = Cx; _lastBaseY = Cy;            // combining marks attach here
        Cx += w;
        if (Cx >= Cols)
        {
            Cx = Cols - 1;
            if (AutoWrap) _wrapPending = true;
        }
    }

    public void CarriageReturn() { Cx = 0; _wrapPending = false; }

    public void Backspace() { if (Cx > 0) Cx--; _wrapPending = false; }

    public void Tab()
    {
        _wrapPending = false;
        if (Cx >= Cols - 1) return;
        do { Cx++; } while (Cx < Cols - 1 && !_tabs[Cx]);
    }

    public void SetTabStop() { if (Cx >= 0 && Cx < Cols) _tabs[Cx] = true; }
    public void ClearTabStop() { if (Cx >= 0 && Cx < Cols) _tabs[Cx] = false; }
    public void ClearAllTabs() { for (int i = 0; i < Cols; i++) _tabs[i] = false; }

    /// <summary>LF / IND: move down one line, scrolling the region if at its bottom.</summary>
    public void Index()
    {
        _wrapPending = false;
        if (Cy == _bot) ScrollUp(1);
        else if (Cy < Rows - 1) Cy++;
    }

    /// <summary>RI: move up one line, scrolling down if at the region top.</summary>
    public void ReverseIndex()
    {
        _wrapPending = false;
        if (Cy == _top) ScrollDown(1);
        else if (Cy > 0) Cy--;
    }

    public void NextLine() { CarriageReturn(); Index(); }

    public void ScrollUp(int n)
    {
        n = Math.Clamp(n, 0, _bot - _top + 1);
        for (int k = 0; k < n; k++)
        {
            Cell[] leaving = _buf[_top];
            // Recycle a row array for the new blank bottom line: the line evicted
            // from the (ring) scrollback, or the leaving line itself when it isn't
            // kept. Avoids an allocation per scroll once the scrollback is full.
            Cell[] reuse = (!_inAlt && _top == 0) ? (_scrollback.Push(leaving) ?? new Cell[Cols]) : leaving;
            for (int r = _top; r < _bot; r++) _buf[r] = _buf[r + 1];
            _buf[_bot] = ClearLine(reuse);
        }
    }

    private Cell[] ClearLine(Cell[] line)
    {
        if (line.Length != Cols) line = new Cell[Cols];
        var blank = BlankCell();
        for (int i = 0; i < line.Length; i++) line[i] = blank;
        return line;
    }

    public void ScrollDown(int n)
    {
        n = Math.Clamp(n, 0, _bot - _top + 1);
        for (int k = 0; k < n; k++)
        {
            for (int r = _bot; r > _top; r--) _buf[r] = _buf[r - 1];
            _buf[_top] = BlankLine();
        }
    }

    // ---- cursor movement ---------------------------------------------------

    private int RegionTop => OriginMode ? _top : 0;
    private int RegionBot => OriginMode ? _bot : Rows - 1;

    public void CursorUp(int n)    { Cy = Math.Max(RegionTop, Cy - Math.Max(1, n)); _wrapPending = false; }
    public void CursorDown(int n)  { Cy = Math.Min(RegionBot, Cy + Math.Max(1, n)); _wrapPending = false; }
    public void CursorFwd(int n)   { Cx = Math.Min(Cols - 1, Cx + Math.Max(1, n)); _wrapPending = false; }
    public void CursorBack(int n)  { Cx = Math.Max(0, Cx - Math.Max(1, n)); _wrapPending = false; }
    public void CursorCol(int col) { Cx = Math.Clamp(col, 0, Cols - 1); _wrapPending = false; }
    public void CursorRow(int row) { Cy = Math.Clamp(RegionTop + (OriginMode ? row : row - RegionTop), 0, Rows - 1); _wrapPending = false; }

    public void CursorTo(int row, int col)
    {
        int top = RegionTop, bot = RegionBot;
        Cy = Math.Clamp(top + row, top, bot);
        Cx = Math.Clamp(col, 0, Cols - 1);
        _wrapPending = false;
    }

    // ---- erasing -----------------------------------------------------------

    public void EraseInLine(int mode)
    {
        var row = _buf[Cy];
        var blank = BlankCell();
        switch (mode)
        {
            case 0: for (int c = Cx; c < Cols; c++) row[c] = blank; break;
            case 1: for (int c = 0; c <= Cx && c < Cols; c++) row[c] = blank; break;
            case 2: for (int c = 0; c < Cols; c++) row[c] = blank; break;
        }
        _wrapPending = false;
    }

    public void EraseInDisplay(int mode)
    {
        var blank = BlankCell();
        switch (mode)
        {
            case 0:
                EraseInLine(0);
                for (int r = Cy + 1; r < Rows; r++) FillRow(r, blank);
                break;
            case 1:
                for (int r = 0; r < Cy; r++) FillRow(r, blank);
                EraseInLine(1);
                break;
            case 2:
                for (int r = 0; r < Rows; r++) FillRow(r, blank);
                break;
            case 3:
                _scrollback.Clear();
                break;
        }
        _wrapPending = false;
    }

    private void FillRow(int r, Cell blank) { var row = _buf[r]; for (int c = 0; c < Cols; c++) row[c] = blank; }

    public void EraseChars(int n)
    {
        var row = _buf[Cy];
        var blank = BlankCell();
        for (int c = Cx; c < Cols && c < Cx + Math.Max(1, n); c++) row[c] = blank;
    }

    public void InsertChars(int n)
    {
        n = Math.Clamp(n, 1, Cols - Cx);
        var row = _buf[Cy];
        var blank = BlankCell();
        for (int c = Cols - 1; c >= Cx + n; c--) row[c] = row[c - n];
        for (int c = Cx; c < Cx + n; c++) row[c] = blank;
    }

    public void DeleteChars(int n)
    {
        n = Math.Clamp(n, 1, Cols - Cx);
        var row = _buf[Cy];
        var blank = BlankCell();
        for (int c = Cx; c < Cols - n; c++) row[c] = row[c + n];
        for (int c = Cols - n; c < Cols; c++) row[c] = blank;
    }

    public void InsertLines(int n)
    {
        if (Cy < _top || Cy > _bot) return;
        n = Math.Clamp(n, 1, _bot - Cy + 1);
        for (int k = 0; k < n; k++)
        {
            for (int r = _bot; r > Cy; r--) _buf[r] = _buf[r - 1];
            _buf[Cy] = BlankLine();
        }
    }

    public void DeleteLines(int n)
    {
        if (Cy < _top || Cy > _bot) return;
        n = Math.Clamp(n, 1, _bot - Cy + 1);
        for (int k = 0; k < n; k++)
        {
            for (int r = Cy; r < _bot; r++) _buf[r] = _buf[r + 1];
            _buf[_bot] = BlankLine();
        }
    }

    // ---- scroll region / cursor save --------------------------------------

    public void SetScrollRegion(int top, int bottom)
    {
        if (top <= 0) top = 1;
        if (bottom <= 0 || bottom > Rows) bottom = Rows;
        if (top >= bottom) { top = 1; bottom = Rows; }
        _top = top - 1;
        _bot = bottom - 1;
        CursorTo(0, 0);
    }

    public void SaveCursor() => _saved = (Cx, Cy, _fg, _bg, _flags, OriginMode);

    public void RestoreCursor()
    {
        Cx = Math.Min(_saved.cx, Cols - 1);
        Cy = Math.Min(_saved.cy, Rows - 1);
        _fg = _saved.fg; _bg = _saved.bg; _flags = _saved.fl; OriginMode = _saved.origin;
        _wrapPending = false;
    }

    public void FullReset()
    {
        _fg = Theme.Default; _bg = Theme.Default; _flags = CellFlags.None;
        AutoWrap = true; OriginMode = false; CursorVisible = true;
        AppCursorKeys = false; AppKeypad = false; BracketedPaste = false; InsertMode = false;
        Mouse = MouseTracking.None; MouseSgr = false;
        _top = 0; _bot = Rows - 1;
        Cx = Cy = 0; _wrapPending = false;
        if (_inAlt) SetAltScreen(false);
        EraseInDisplay(2);
        _scrollback.Clear();
    }

    // ---- alternate screen --------------------------------------------------

    public void SetAltScreen(bool on)
    {
        if (on == _inAlt) return;
        if (on)
        {
            SaveCursor();
            _inAlt = true;
            _buf = _alt;
            EraseInDisplay(2);
            Cx = Cy = 0; _wrapPending = false;
        }
        else
        {
            _inAlt = false;
            _buf = _primary;
            RestoreCursor();
        }
    }

    public bool InAlt => _inAlt;

    // ---- SGR ---------------------------------------------------------------

    public void SetGraphics(IReadOnlyList<int> p, IReadOnlyList<bool>? colon = null)
    {
        if (p.Count == 0) { ResetPen(); return; }
        for (int i = 0; i < p.Count; i++)
        {
            // Skip ':' sub-parameters: they belong to the preceding code (the "3"
            // in 4:3 curly-underline, or the channels of 38:2:r:g:b) and must not
            // be read as standalone SGR codes — that was leaking underline on.
            if (colon is not null && i < colon.Count && colon[i]) continue;

            int n = p[i];
            switch (n)
            {
                case 0: ResetPen(); break;
                case 1: _flags |= CellFlags.Bold; break;
                case 2: _flags |= CellFlags.Faint; break;
                case 3: _flags |= CellFlags.Italic; break;
                case 4:
                    // 4 = underline on; 4:x = styled underline (x=0 off, else on).
                    if (colon is not null && i + 1 < p.Count && i + 1 < colon.Count && colon[i + 1] && p[i + 1] == 0)
                        _flags &= ~CellFlags.Underline;
                    else
                        _flags |= CellFlags.Underline;
                    break;
                case 5: case 6: _flags |= CellFlags.Blink; break;
                case 7: _flags |= CellFlags.Reverse; break;
                case 8: _flags |= CellFlags.Hidden; break;
                case 9: _flags |= CellFlags.Strike; break;
                case 21: case 22: _flags &= ~(CellFlags.Bold | CellFlags.Faint); break;
                case 23: _flags &= ~CellFlags.Italic; break;
                case 24: _flags &= ~CellFlags.Underline; break;
                case 25: _flags &= ~CellFlags.Blink; break;
                case 27: _flags &= ~CellFlags.Reverse; break;
                case 28: _flags &= ~CellFlags.Hidden; break;
                case 29: _flags &= ~CellFlags.Strike; break;
                case 39: _fg = Theme.Default; break;
                case 49: _bg = Theme.Default; break;
                case 38: i = ExtColor(p, i, ref _fg); break;
                case 48: i = ExtColor(p, i, ref _bg); break;
                default:
                    if (n >= 30 && n <= 37) _fg = n - 30;
                    else if (n >= 40 && n <= 47) _bg = n - 40;
                    else if (n >= 90 && n <= 97) _fg = (n - 90) + 8;
                    else if (n >= 100 && n <= 107) _bg = (n - 100) + 8;
                    break;
            }
        }
    }

    private static int ExtColor(IReadOnlyList<int> p, int i, ref int slot)
    {
        if (i + 1 >= p.Count) return i;
        int kind = p[i + 1];
        if (kind == 5 && i + 2 < p.Count) { slot = p[i + 2] & 0xFF; return i + 2; }
        if (kind == 2 && i + 4 < p.Count)
        {
            slot = Theme.Rgb((byte)p[i + 2], (byte)p[i + 3], (byte)p[i + 4]);
            return i + 4;
        }
        return i + 1;
    }

    private void ResetPen() { _fg = Theme.Default; _bg = Theme.Default; _flags = CellFlags.None; }

    // ---- snapshot for rendering -------------------------------------------

    /// <summary>Copy the visible viewport (scrolled back by <paramref name="off"/> lines)
    /// into <paramref name="dest"/> (Rows rows × Cols cells).</summary>
    public void CopyViewport(int off, Cell[][] dest)
    {
        off = Math.Clamp(off, 0, _scrollback.Count);
        int topAbs = _scrollback.Count - off;     // absolute index of first visible line
        for (int r = 0; r < Rows; r++)
        {
            int abs = topAbs + r;
            Cell[] src = abs < _scrollback.Count ? _scrollback[abs] : _buf[abs - _scrollback.Count];
            var d = dest[r];
            int n = Math.Min(d.Length, src.Length);
            Array.Copy(src, d, n);
            for (int c = n; c < d.Length; c++) d[c] = default;
        }
    }

    // ---- text extraction (selection / copy) -------------------------------

    /// <summary>Total addressable rows = scrollback + visible screen.</summary>
    public int TotalRows => _scrollback.Count + Rows;

    private Cell[]? AbsLine(int abs)
    {
        if (abs < 0) return null;
        if (abs < _scrollback.Count) return _scrollback[abs];
        int i = abs - _scrollback.Count;
        return i < Rows ? _buf[i] : null;
    }

    /// <summary>Extract the text in the (inclusive) range, trimming trailing
    /// blanks per line and joining rows with CRLF.</summary>
    public string GetText(int r1, int c1, int r2, int c2)
    {
        if (r1 > r2 || (r1 == r2 && c1 > c2)) { (r1, c1, r2, c2) = (r2, c2, r1, c1); }
        var outSb = new System.Text.StringBuilder();
        for (int r = r1; r <= r2; r++)
        {
            Cell[]? line = AbsLine(r);
            int start = r == r1 ? Math.Max(0, c1) : 0;
            int end = r == r2 ? Math.Min(Cols - 1, c2) : Cols - 1;
            var row = new System.Text.StringBuilder();
            if (line is not null)
                for (int c = start; c <= end; c++)
                {
                    Cell cell = line[c];
                    if (cell.Width == 0) continue;                 // wide-char continuation
                    row.Append(cell.Rune == 0 ? " " : char.ConvertFromUtf32(cell.Rune));
                    if (cell.Combo is not null) row.Append(cell.Combo);
                }
            outSb.Append(row.ToString().TrimEnd(' '));
            if (r < r2) outSb.Append("\r\n");
        }
        return outSb.ToString();
    }

    /// <summary>Word boundaries at (absRow, col) for double-click selection.</summary>
    public bool WordSpan(int absRow, int col, out int s, out int e)
    {
        s = e = col;
        Cell[]? line = AbsLine(absRow);
        if (line is null || col < 0 || col >= Cols) return false;
        if (!IsWordChar(line[col].Rune)) return true;             // single non-word cell
        while (s > 0 && IsWordChar(line[s - 1].Rune)) s--;
        while (e < Cols - 1 && IsWordChar(line[e + 1].Rune)) e++;
        return true;
    }

    private static bool IsWordChar(int cp) =>
        cp > 0x7f || (cp > ' ' && (char.IsLetterOrDigit((char)cp) || "._-/~+@:".IndexOf((char)cp) >= 0));
}

public enum MouseTracking { None, X10, Normal, ButtonEvent, AnyEvent }

/// <summary>Fixed-capacity ring of scrollback lines: O(1) push (no array shifting),
/// indexed 0 = oldest. Push returns the evicted line array for recycling.</summary>
internal sealed class Scrollback
{
    private readonly Cell[]?[] _ring;
    private int _start, _count;

    public Scrollback(int capacity) => _ring = new Cell[Math.Max(1, capacity)][];

    public int Count => _count;

    public Cell[] this[int i] => _ring[(_start + i) % _ring.Length]!;

    public Cell[]? Push(Cell[] line)
    {
        int cap = _ring.Length;
        if (_count < cap) { _ring[(_start + _count) % cap] = line; _count++; return null; }
        Cell[]? evicted = _ring[_start];
        _ring[_start] = line;
        _start = (_start + 1) % cap;
        return evicted;     // recycle this array as the next blank line
    }

    public void Clear() { _start = _count = 0; Array.Clear(_ring, 0, _ring.Length); }
}
