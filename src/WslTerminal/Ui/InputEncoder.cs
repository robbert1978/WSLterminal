using System.Text;
using System.Windows.Input;

namespace WslTerminal.Ui;

/// <summary>
/// Translates special keys / modified keys into terminal byte sequences.
/// Returns null for plain printable input so OnTextInput handles it (which
/// respects the keyboard layout, Shift, and IME composition).
/// </summary>
internal static class InputEncoder
{
    public static byte[]? Encode(Key key, ModifierKeys mods, bool appCursor)
    {
        bool ctrl = mods.HasFlag(ModifierKeys.Control);
        bool alt = mods.HasFlag(ModifierKeys.Alt);
        bool shift = mods.HasFlag(ModifierKeys.Shift);
        int mod = 1 + (shift ? 1 : 0) + (alt ? 2 : 0) + (ctrl ? 4 : 0);

        switch (key)
        {
            case Key.Up: return Cursor('A', mod, appCursor);
            case Key.Down: return Cursor('B', mod, appCursor);
            case Key.Right: return Cursor('C', mod, appCursor);
            case Key.Left: return Cursor('D', mod, appCursor);
            case Key.Home: return Cursor('H', mod, appCursor);
            case Key.End: return Cursor('F', mod, appCursor);

            case Key.Insert: return Tilde(2, mod);
            case Key.Delete: return Tilde(3, mod);
            case Key.PageUp: return Tilde(5, mod);
            case Key.PageDown: return Tilde(6, mod);

            case Key.F1: return Fn('P', 11, mod);
            case Key.F2: return Fn('Q', 12, mod);
            case Key.F3: return Fn('R', 13, mod);
            case Key.F4: return Fn('S', 14, mod);
            case Key.F5: return Tilde(15, mod);
            case Key.F6: return Tilde(17, mod);
            case Key.F7: return Tilde(18, mod);
            case Key.F8: return Tilde(19, mod);
            case Key.F9: return Tilde(20, mod);
            case Key.F10: return Tilde(21, mod);
            case Key.F11: return Tilde(23, mod);
            case Key.F12: return Tilde(24, mod);

            case Key.Enter: return alt ? Esc("\r") : Ascii("\r");
            case Key.Tab: return shift ? Ascii("\x1b[Z") : Ascii("\t");
            case Key.Escape: return Ascii("\x1b");
            case Key.Back: return alt ? Esc("\x7f") : Ascii(ctrl ? "\b" : "\x7f");
            case Key.Space when ctrl: return new byte[] { 0 };
        }

        // Ctrl/Alt + letter -> control byte or meta-prefixed letter.
        if (key >= Key.A && key <= Key.Z)
        {
            char letter = (char)('a' + (key - Key.A));
            if (ctrl)
            {
                byte ctl = (byte)((key - Key.A) + 1);   // ^A=1 .. ^Z=26
                return alt ? new byte[] { 0x1b, ctl } : new[] { ctl };
            }
            if (alt) return Esc((shift ? char.ToUpperInvariant(letter) : letter).ToString());
        }

        // A few common control punctuation combos.
        if (ctrl)
        {
            switch (key)
            {
                case Key.OemOpenBrackets: return new byte[] { 0x1b }; // Ctrl+[
                case Key.Oem6: return new byte[] { 0x1d };            // Ctrl+]
                case Key.Oem5: return new byte[] { 0x1c };            // Ctrl+\
                case Key.OemMinus: return new byte[] { 0x1f };        // Ctrl+_
            }
        }

        return null; // let OnTextInput produce the character
    }

    private static byte[] Cursor(char fin, int mod, bool app) =>
        mod > 1 ? Ascii($"\x1b[1;{mod}{fin}")
                : Ascii(app ? $"\x1bO{fin}" : $"\x1b[{fin}");

    private static byte[] Tilde(int n, int mod) =>
        mod > 1 ? Ascii($"\x1b[{n};{mod}~") : Ascii($"\x1b[{n}~");

    private static byte[] Fn(char ss3, int n, int mod) =>
        mod > 1 ? Ascii($"\x1b[{n};{mod}~") : Ascii($"\x1bO{ss3}");

    private static byte[] Ascii(string s) => Encoding.ASCII.GetBytes(s);
    private static byte[] Esc(string s) => Encoding.UTF8.GetBytes("\x1b" + s);
}
