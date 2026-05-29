using System.IO;
using Microsoft.Win32.SafeHandles;

namespace WslTerminal;

/// <summary>
/// The app is a GUI-subsystem (WinExe) binary so launching it never allocates a
/// console window. The diagnostic console modes still want stdout, so: if output
/// is already redirected (pipe/file) we leave it as-is; otherwise we attach to the
/// parent console if there is one. We never AllocConsole — that would pop a window.
/// </summary>
internal static class ConsoleHelper
{
    public static void Ensure()
    {
        IntPtr h = Native.GetStdHandle(Native.STD_OUTPUT_HANDLE);
        if (h != IntPtr.Zero && h != Native.INVALID_HANDLE_VALUE)
            return; // stdout already valid (redirected) — Console will use it

        if (!Native.AttachConsole(Native.ATTACH_PARENT_PROCESS))
            return; // launched without a parent console; run silently

        IntPtr outH = Native.CreateFileW("CONOUT$", Native.GENERIC_READ | Native.GENERIC_WRITE,
            Native.FILE_SHARE_READ | Native.FILE_SHARE_WRITE, IntPtr.Zero, Native.OPEN_EXISTING, 0, IntPtr.Zero);
        if (outH != IntPtr.Zero && outH != Native.INVALID_HANDLE_VALUE)
        {
            var sw = new StreamWriter(new FileStream(new SafeFileHandle(outH, true), FileAccess.Write)) { AutoFlush = true };
            Console.SetOut(sw);
            Console.SetError(sw);
        }

        IntPtr inH = Native.CreateFileW("CONIN$", Native.GENERIC_READ | Native.GENERIC_WRITE,
            Native.FILE_SHARE_READ | Native.FILE_SHARE_WRITE, IntPtr.Zero, Native.OPEN_EXISTING, 0, IntPtr.Zero);
        if (inH != IntPtr.Zero && inH != Native.INVALID_HANDLE_VALUE)
            Console.SetIn(new StreamReader(new FileStream(new SafeFileHandle(inH, true), FileAccess.Read)));
    }
}
