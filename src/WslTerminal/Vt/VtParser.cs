using System.Text;

namespace WslTerminal.Vt;

/// <summary>
/// Incremental VT/ANSI parser. Bytes are decoded as UTF-8 and dispatched to
/// the <see cref="Screen"/>. Handles C0 controls, CSI, OSC, a few ESC singles,
/// the DEC special-graphics (line-drawing) charset, and device queries.
/// State persists across <see cref="Parse"/> calls so split sequences work.
/// </summary>
public sealed class VtParser
{
    private enum S { Ground, Esc, EscInt, Csi, CsiInt, Osc, OscEsc, Str }

    private readonly Screen _s;
    private readonly Action<byte[]> _respond;
    private readonly Action<string> _setTitle;
    private readonly Action<string> _setCwd;

    private S _state = S.Ground;

    // UTF-8 decode
    private int _cp, _need;

    // CSI collection
    private readonly List<int> _params = new();
    private readonly List<bool> _paramsColon = new();   // true => param is a ':' sub-parameter of the previous
    private bool _pendingColon;                          // separator before the next param was ':' ?
    private int _cur;
    private bool _curHas;
    private char _priv;            // private marker: ? < = >
    private char _inter;           // intermediate byte

    // ESC intermediates ( ) etc.
    private char _escInter;

    // OSC
    private readonly StringBuilder _osc = new(256);

    // charset (DEC special graphics / line drawing)
    private bool _g0Special, _g1Special, _shiftOut;

    public VtParser(Screen screen, Action<byte[]> respond, Action<string> setTitle, Action<string> setCwd)
    {
        _s = screen;
        _respond = respond;
        _setTitle = setTitle;
        _setCwd = setCwd;
    }

    public void Parse(ReadOnlySpan<byte> data)
    {
        int n = data.Length;
        for (int i = 0; i < n; i++)
        {
            // Fast path: in the Ground state with no pending UTF-8 byte, blast a
            // run of plain printable ASCII straight to the screen, skipping the
            // per-byte state-machine dispatch. (DEC special-graphics mapping still
            // applies via Print, which is cheap.)
            if (_state == S.Ground && _need == 0)
            {
                byte b = data[i];
                if (b >= 0x20 && b < 0x7f)
                {
                    int start = i;
                    do { i++; } while (i < n && data[i] >= 0x20 && data[i] < 0x7f);
                    for (int k = start; k < i; k++) Print(data[k]);
                    i--;            // for-loop re-increments
                    continue;
                }
            }
            Step(data[i]);
        }
    }

    private void Step(byte b)
    {
        switch (_state)
        {
            case S.Ground: Ground(b); break;
            case S.Esc: Escape(b); break;
            case S.EscInt: EscapeIntermediate(b); break;
            case S.Csi: Csi(b); break;
            case S.CsiInt: CsiIntermediate(b); break;
            case S.Osc: Osc(b); break;
            case S.OscEsc: OscEsc(b); break;
            case S.Str: StrConsume(b); break;
        }
    }

    // ---- ground / text -----------------------------------------------------

    private void Ground(byte b)
    {
        if (_need > 0)
        {
            if ((b & 0xC0) == 0x80) { _cp = (_cp << 6) | (b & 0x3F); if (--_need == 0) Print(_cp); return; }
            _need = 0;             // malformed; fall through to reinterpret b
        }

        if (b < 0x80)
        {
            if (b < 0x20 || b == 0x7f) { C0(b); return; }
            Print(b);
        }
        else if ((b & 0xE0) == 0xC0) { _cp = b & 0x1F; _need = 1; }
        else if ((b & 0xF0) == 0xE0) { _cp = b & 0x0F; _need = 2; }
        else if ((b & 0xF8) == 0xF0) { _cp = b & 0x07; _need = 3; }
        else { Print(0xFFFD); }    // invalid lead byte
    }

    private void C0(byte b)
    {
        switch (b)
        {
            case 0x07: /* BEL */ break;
            case 0x08: _s.Backspace(); break;
            case 0x09: _s.Tab(); break;
            case 0x0A: case 0x0B: case 0x0C: _s.Index(); break;
            case 0x0D: _s.CarriageReturn(); break;
            case 0x0E: _shiftOut = true; break;   // SO -> G1
            case 0x0F: _shiftOut = false; break;  // SI -> G0
            case 0x1B: BeginEsc(); break;
        }
    }

    private void Print(int cp)
    {
        bool special = _shiftOut ? _g1Special : _g0Special;
        if (special && cp >= 0x60 && cp <= 0x7e) cp = DecSpecial[cp - 0x60];
        _s.PutRune(cp);
    }

    // ---- escape ------------------------------------------------------------

    private void BeginEsc() { _state = S.Esc; }

    private void Escape(byte b)
    {
        switch ((char)b)
        {
            case '[': ResetCsi(); _state = S.Csi; return;
            case ']': _osc.Clear(); _state = S.Osc; return;
            case 'P': case 'X': case '^': case '_': _state = S.Str; return; // DCS/SOS/PM/APC
            case '(': case ')': case '*': case '+': _escInter = (char)b; _state = S.EscInt; return;
            case 'M': _s.ReverseIndex(); _state = S.Ground; return;
            case 'D': _s.Index(); _state = S.Ground; return;
            case 'E': _s.NextLine(); _state = S.Ground; return;
            case '7': _s.SaveCursor(); _state = S.Ground; return;
            case '8': _s.RestoreCursor(); _state = S.Ground; return;
            case 'c': _s.FullReset(); _state = S.Ground; return;
            case '=': _s.AppKeypad = true; _state = S.Ground; return;
            case '>': _s.AppKeypad = false; _state = S.Ground; return;
            case 'H': _s.SetTabStop(); _state = S.Ground; return;
            default: _state = S.Ground; return;
        }
    }

    private void EscapeIntermediate(byte b)
    {
        bool special = b == '0';                  // DEC special graphics
        bool ascii = b == 'B' || b == 'A';
        if (_escInter == '(') { if (special) _g0Special = true; else if (ascii) _g0Special = false; }
        else if (_escInter == ')') { if (special) _g1Special = true; else if (ascii) _g1Special = false; }
        _state = S.Ground;
    }

    // ---- CSI ---------------------------------------------------------------

    private void ResetCsi() { _params.Clear(); _paramsColon.Clear(); _pendingColon = false; _cur = 0; _curHas = false; _priv = '\0'; _inter = '\0'; }

    // Flush the accumulating number as a parameter, recording whether the separator
    // *before* it was a colon (i.e. it's a sub-parameter). sepAfterIsColon is the
    // separator we just hit, which precedes the next parameter.
    private void FlushParam(bool sepAfterIsColon)
    {
        _params.Add(_curHas ? _cur : 0);
        _paramsColon.Add(_pendingColon);
        _pendingColon = sepAfterIsColon;
        _cur = 0; _curHas = false;
    }

    private void Csi(byte b)
    {
        char c = (char)b;
        if (b < 0x20) { C0(b); return; }                       // embedded control
        if (c is '?' or '<' or '=' or '>') { _priv = c; return; }
        if (c >= '0' && c <= '9') { _cur = _cur * 10 + (c - '0'); _curHas = true; return; }
        if (c == ';') { FlushParam(false); return; }   // parameter separator
        if (c == ':') { FlushParam(true); return; }    // sub-parameter separator (e.g. 4:3, 38:2:r:g:b)
        if (b >= 0x20 && b <= 0x2f) { _inter = c; _state = S.CsiInt; return; }
        FlushParam(false);
        DispatchCsi(c);
        _state = S.Ground;
    }

    private void CsiIntermediate(byte b)
    {
        char c = (char)b;
        if (b >= 0x20 && b <= 0x2f) { _inter = c; return; }
        FlushParam(false);
        DispatchCsi(c);
        _state = S.Ground;
    }

    private int P(int i, int def = 0)
    {
        if (i >= _params.Count) return def;
        return _params[i] == 0 && def != 0 ? def : _params[i];
    }

    private void DispatchCsi(char f)
    {
        if (_priv == '?') { DecMode(f); return; }

        // Private CSI with a '>' '<' or '=' prefix (e.g. \e[>4m XTMODKEYS, \e[>c
        // secondary-DA) is NOT a standard sequence — only secondary DA is handled.
        // Critically, \e[>4m must NOT fall through to SGR 'm' (it was being read as
        // SGR 4 = underline, which then bled into all following text).
        if (_priv is '>' or '<' or '=')
        {
            if (f == 'c') PrimaryDa();   // secondary device attributes (_priv '>')
            return;
        }

        switch (f)
        {
            case 'A': _s.CursorUp(P(0, 1)); break;
            case 'B': case 'e': _s.CursorDown(P(0, 1)); break;
            case 'C': case 'a': _s.CursorFwd(P(0, 1)); break;
            case 'D': _s.CursorBack(P(0, 1)); break;
            case 'E': _s.CarriageReturn(); _s.CursorDown(P(0, 1)); break;
            case 'F': _s.CarriageReturn(); _s.CursorUp(P(0, 1)); break;
            case 'G': case '`': _s.CursorCol(P(0, 1) - 1); break;
            case 'd': _s.CursorRow(P(0, 1) - 1); break;
            case 'H': case 'f': _s.CursorTo(P(0, 1) - 1, P(1, 1) - 1); break;
            case 'J': _s.EraseInDisplay(P(0)); break;
            case 'K': _s.EraseInLine(P(0)); break;
            case 'L': _s.InsertLines(P(0, 1)); break;
            case 'M': _s.DeleteLines(P(0, 1)); break;
            case 'P': _s.DeleteChars(P(0, 1)); break;
            case '@': _s.InsertChars(P(0, 1)); break;
            case 'X': _s.EraseChars(P(0, 1)); break;
            case 'S': _s.ScrollUp(P(0, 1)); break;
            case 'T': _s.ScrollDown(P(0, 1)); break;
            case 'r': _s.SetScrollRegion(P(0), _params.Count > 1 ? P(1) : 0); break;
            case 'm': _s.SetGraphics(_params, _paramsColon); break;
            case 'h': StdMode(true); break;
            case 'l': StdMode(false); break;
            case 'n': DeviceStatus(P(0)); break;
            case 'c': PrimaryDa(); break;
            case 'g': if (P(0) == 3) _s.ClearAllTabs(); else _s.ClearTabStop(); break;
            case 's': _s.SaveCursor(); break;
            case 'u': _s.RestoreCursor(); break;
        }
    }

    private void StdMode(bool on)
    {
        for (int i = 0; i < Math.Max(1, _params.Count); i++)
        {
            if (P(i) == 4) _s.InsertMode = on;   // IRM
        }
    }

    private void DecMode(char f)
    {
        bool on = f == 'h';
        if (f != 'h' && f != 'l') return;
        for (int i = 0; i < Math.Max(1, _params.Count); i++)
        {
            switch (P(i))
            {
                case 1: _s.AppCursorKeys = on; break;
                case 6: _s.OriginMode = on; _s.CursorTo(0, 0); break;
                case 7: _s.AutoWrap = on; break;
                case 25: _s.CursorVisible = on; break;
                case 9: _s.Mouse = on ? MouseTracking.X10 : MouseTracking.None; break;
                case 1000: _s.Mouse = on ? MouseTracking.Normal : MouseTracking.None; break;
                case 1002: _s.Mouse = on ? MouseTracking.ButtonEvent : MouseTracking.None; break;
                case 1003: _s.Mouse = on ? MouseTracking.AnyEvent : MouseTracking.None; break;
                case 1006: _s.MouseSgr = on; break;
                case 47: case 1047: case 1049: _s.SetAltScreen(on); break;
                case 2004: _s.BracketedPaste = on; break;
            }
        }
    }

    private void DeviceStatus(int n)
    {
        if (n == 5) _respond(Encoding.ASCII.GetBytes("\x1b[0n"));
        else if (n == 6) _respond(Encoding.ASCII.GetBytes($"\x1b[{_s.Cy + 1};{_s.Cx + 1}R"));
    }

    private void PrimaryDa()
    {
        if (_priv == '>') _respond(Encoding.ASCII.GetBytes("\x1b[>0;276;0c"));
        else _respond(Encoding.ASCII.GetBytes("\x1b[?1;2c"));
    }

    // ---- OSC ---------------------------------------------------------------

    private void Osc(byte b)
    {
        if (b == 0x07) { OscDone(); return; }                  // BEL terminator
        if (b == 0x1b) { _state = S.OscEsc; return; }          // maybe ST (ESC \)
        _osc.Append((char)b);
    }

    private void OscEsc(byte b)
    {
        if (b == (byte)'\\') { OscDone(); return; }            // ST
        // not ST: treat as a new escape
        _state = S.Ground;
        Step(0x1b);
        Step(b);
    }

    private void OscDone()
    {
        string s = _osc.ToString();
        _state = S.Ground;
        int semi = s.IndexOf(';');
        string num = semi < 0 ? s : s[..semi];
        string text = semi < 0 ? "" : s[(semi + 1)..];
        if (num is "0" or "1" or "2") _setTitle(text);
        else if (num == "7") { string? cwd = ParseFileUri(text); if (cwd is not null) _setCwd(cwd); }
    }

    // OSC 7 reports the working directory as file://HOST/PATH (percent-encoded).
    private static string? ParseFileUri(string s)
    {
        const string pfx = "file://";
        if (!s.StartsWith(pfx, StringComparison.Ordinal)) return null;
        int slash = s.IndexOf('/', pfx.Length);   // skip the host, start of path
        if (slash < 0) return null;
        try { return Uri.UnescapeDataString(s[slash..]); }
        catch { return s[slash..]; }
    }

    private void StrConsume(byte b)
    {
        // DCS/SOS/PM/APC: consume until ST (ESC \) or BEL.
        if (b == 0x07) { _state = S.Ground; return; }
        if (b == 0x1b) { _state = S.OscEsc; _osc.Clear(); return; }
    }

    // DEC special graphics: maps 0x60..0x7e to box-drawing/line glyphs.
    private static readonly int[] DecSpecial =
    {
        0x25C6, 0x2592, 0x2409, 0x240C, 0x240D, 0x240A, 0x00B0, 0x00B1, // ` a b c d e f g
        0x2424, 0x240B, 0x2518, 0x2510, 0x250C, 0x2514, 0x253C, 0x23BA, // h i j k l m n o
        0x23BB, 0x2500, 0x23BC, 0x23BD, 0x251C, 0x2524, 0x2534, 0x252C, // p q r s t u v w
        0x2502, 0x2264, 0x2265, 0x03C0, 0x2260, 0x00A3, 0x00B7,         // x y z { | } ~
    };
}
