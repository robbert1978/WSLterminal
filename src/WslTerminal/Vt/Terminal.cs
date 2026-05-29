namespace WslTerminal.Vt;

/// <summary>Thread-safe front end over <see cref="Screen"/> + <see cref="VtParser"/>.
/// The reader thread calls <see cref="Feed"/>; the UI thread calls
/// <see cref="CaptureViewport"/> for rendering. Both take <see cref="Sync"/>.</summary>
public sealed class Terminal
{
    public readonly object Sync = new();
    private readonly Screen _screen;
    private readonly VtParser _parser;

    /// <summary>Bytes the emulator must send back to the PTY (DSR/DA replies).</summary>
    public event Action<byte[]>? Respond;
    public event Action<string>? TitleChanged;

    private string? _cwd;
    /// <summary>Latest working directory reported by the shell via OSC 7 (or null).</summary>
    public string? CurrentDirectory { get { lock (Sync) return _cwd; } }

    public Terminal(int cols, int rows)
    {
        _screen = new Screen(cols, rows);
        _parser = new VtParser(_screen,
            bytes => Respond?.Invoke(bytes),
            title => TitleChanged?.Invoke(title),
            cwd => { lock (Sync) _cwd = cwd; });
    }

    public int Cols { get { lock (Sync) return _screen.Cols; } }
    public int Rows { get { lock (Sync) return _screen.Rows; } }
    public bool AppCursorKeys { get { lock (Sync) return _screen.AppCursorKeys; } }
    public bool BracketedPaste { get { lock (Sync) return _screen.BracketedPaste; } }

    public void Feed(ReadOnlySpan<byte> data) { lock (Sync) _parser.Parse(data); }

    public void Resize(int cols, int rows) { lock (Sync) _screen.Resize(cols, rows); }

    public ViewportInfo CaptureViewport(int scrollOffset, Cell[][] dest)
    {
        lock (Sync)
        {
            _screen.CopyViewport(scrollOffset, dest);
            return new ViewportInfo(
                _screen.Cx, _screen.Cy, _screen.CursorVisible,
                _screen.ScrollbackCount, _screen.InAlt);
        }
    }

    public int ScrollbackCount { get { lock (Sync) return _screen.ScrollbackCount; } }

    public int TotalRows { get { lock (Sync) return _screen.TotalRows; } }

    public string GetText(int r1, int c1, int r2, int c2) { lock (Sync) return _screen.GetText(r1, c1, r2, c2); }

    public bool WordSpan(int absRow, int col, out int s, out int e) { lock (Sync) return _screen.WordSpan(absRow, col, out s, out e); }
}

public readonly record struct ViewportInfo(int CursorX, int CursorY, bool CursorVisible, int ScrollbackCount, bool InAlt);
