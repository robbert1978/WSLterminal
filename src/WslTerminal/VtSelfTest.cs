using System.Text;
using WslTerminal.Vt;

namespace WslTerminal;

/// <summary>Headless checks for the VT emulator (no WSL involved): feed known
/// byte sequences into a <see cref="Terminal"/> and assert the resulting grid.</summary>
internal static class VtSelfTest
{
    private static int _pass, _fail;

    public static int Run()
    {
        _pass = _fail = 0;

        Case("plain text", 20, 6, "hello", (t, g) =>
            g[0][0].Rune == 'h' && g[0][1].Rune == 'e' && g[0][4].Rune == 'o');

        Case("SGR fg + reset", 20, 6, "\x1b[31mR\x1b[0mG", (t, g) =>
            g[0][0].Rune == 'R' && g[0][0].Fg == 1 &&
            g[0][1].Rune == 'G' && g[0][1].Fg == Theme.Default);

        Case("truecolor SGR", 20, 6, "\x1b[38;2;10;20;30mX", (t, g) =>
            g[0][0].Rune == 'X' && g[0][0].Fg == Theme.Rgb(10, 20, 30));

        // Colon-form styled underline (4:3 = curly): underline ON, and the "3"
        // sub-parameter must NOT leak as italic. This was the underline-bleed bug.
        Case("SGR 4:3 styled underline", 20, 6, "\x1b[4:3mU", (t, g) =>
            (g[0][0].Flags & CellFlags.Underline) != 0 && (g[0][0].Flags & CellFlags.Italic) == 0);

        // 4:0 turns underline OFF (apps pair 4:3 / 4:0).
        Case("SGR 4:0 underline off", 20, 6, "\x1b[4:3mA\x1b[4:0mB", (t, g) =>
            (g[0][0].Flags & CellFlags.Underline) != 0 && (g[0][1].Flags & CellFlags.Underline) == 0);

        // Plain 24 still turns underline off.
        Case("SGR 24 underline off", 20, 6, "\x1b[4mA\x1b[24mB", (t, g) =>
            (g[0][0].Flags & CellFlags.Underline) != 0 && (g[0][1].Flags & CellFlags.Underline) == 0);

        // Colon-form truecolor (38:2:r:g:b): fg set, no stray flags leak.
        Case("SGR colon truecolor", 20, 6, "\x1b[38:2:10:20:30mX", (t, g) =>
            g[0][0].Rune == 'X' && g[0][0].Fg == Theme.Rgb(10, 20, 30) && g[0][0].Flags == CellFlags.None);

        // \e[>4m is XTMODKEYS (private '>' CSI), NOT SGR. It must NOT be read as
        // SGR 4 = underline (that bug made Claude Code's whole screen underlined).
        Case("CSI >4m is not underline", 20, 6, "\x1b[>4mX", (t, g) =>
            g[0][0].Rune == 'X' && (g[0][0].Flags & CellFlags.Underline) == 0);

        Case("CR + erase-line + write", 20, 6, "ABC\r\x1b[KZ", (t, g) =>
            g[0][0].Rune == 'Z' && g[0][1].Rune == 0 && g[0][2].Rune == 0);

        Case("CUP row/col", 20, 6, "\x1b[2;3HX", (t, g) => g[1][2].Rune == 'X');

        Case("autowrap", 4, 4, "ABCDE", (t, g) =>
            g[0][0].Rune == 'A' && g[0][3].Rune == 'D' && g[1][0].Rune == 'E');

        Case("UTF-8 wide char", 20, 6, "世a", (t, g) =>
            g[0][0].Rune == 0x4E16 && g[0][0].Width == 2 && g[0][1].Width == 0 && g[0][2].Rune == 'a');

        Case("DEC line drawing", 20, 6, "\x1b(0q\x1b(B", (t, g) => g[0][0].Rune == 0x2500);

        Case("scroll + scrollback", 10, 3, "1\r\n2\r\n3\r\n4", (t, g) =>
            g[0][0].Rune == '2' && g[1][0].Rune == '3' && g[2][0].Rune == '4' && t.ScrollbackCount >= 1);

        Case("insert/delete chars", 10, 3, "ABCD\x1b[1G\x1b[2@", (t, g) =>
            g[0][0].Rune == 0 && g[0][1].Rune == 0 && g[0][2].Rune == 'A' && g[0][3].Rune == 'B');

        // device status report flows back through the Respond event
        {
            var t = new Terminal(20, 6);
            string resp = "";
            t.Respond += b => resp += Encoding.ASCII.GetString(b);
            t.Feed(Encoding.UTF8.GetBytes("\x1b[6n"));
            Check("DSR cursor-pos reply", resp == "\x1b[1;1R");
        }

        Case("alt screen isolation", 10, 4, "main\x1b[?1049h\x1b[2J\x1b[?1049l", (t, g) =>
            g[0][0].Rune == 'm' && g[0][3].Rune == 'n');   // primary survives alt round-trip

        Case("OSC 7 working directory", 20, 4, "\x1b]7;file://Agartha/home/robbert/proj\x1b\\", (t, g) =>
            t.CurrentDirectory == "/home/robbert/proj");

        // selection / copy text extraction
        Case("selection word", 20, 4, "hello world", (t, g) => t.GetText(0, 0, 0, 4) == "hello");
        Case("selection trims trailing", 20, 4, "hello world", (t, g) => t.GetText(0, 6, 0, 19) == "world");
        Case("selection multiline CRLF", 20, 4, "ab\r\ncd", (t, g) => t.GetText(0, 0, 1, 1) == "ab\r\ncd");
        Case("double-click word span", 20, 2, "foo bar", (t, g) =>
            t.WordSpan(0, 1, out int s, out int e) && s == 0 && e == 2);

        Console.WriteLine($"[vttest] {_pass} passed, {_fail} failed");
        return _fail == 0 ? 0 : 1;
    }

    private static void Case(string name, int cols, int rows, string feed, Func<Terminal, Cell[][], bool> assert)
    {
        var t = new Terminal(cols, rows);
        t.Feed(Encoding.UTF8.GetBytes(feed));
        var g = Alloc(rows, cols);
        t.CaptureViewport(0, g);
        Check(name, assert(t, g));
    }

    private static void Check(string name, bool ok)
    {
        if (ok) { _pass++; Console.WriteLine($"  PASS  {name}"); }
        else { _fail++; Console.WriteLine($"  FAIL  {name}"); }
    }

    private static Cell[][] Alloc(int rows, int cols)
    {
        var g = new Cell[rows][];
        for (int r = 0; r < rows; r++) g[r] = new Cell[cols];
        return g;
    }
}
