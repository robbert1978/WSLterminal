using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace WslTerminal;

/// <summary>
/// P/Invoke surface. The interesting one is <see cref="WslLaunch"/> from
/// wslapi.dll: it starts a process inside a WSL distribution and hands it
/// three Windows handles as fd 0/1/2 — letting us drive WSL with our own
/// raw pipes, without conhost/ConPTY and without the wsl.exe console binary.
/// </summary>
internal static partial class Native
{
    [StructLayout(LayoutKind.Sequential)]
    public struct SECURITY_ATTRIBUTES
    {
        public int nLength;
        public IntPtr lpSecurityDescriptor;
        public int bInheritHandle;
    }

    public const int HANDLE_FLAG_INHERIT = 0x1;
    public const uint INFINITE = 0xFFFFFFFF;
    public const uint WAIT_OBJECT_0 = 0x0;
    public const uint WAIT_TIMEOUT = 0x102;

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool CreatePipe(
        out SafeFileHandle hReadPipe,
        out SafeFileHandle hWritePipe,
        ref SECURITY_ATTRIBUTES lpPipeAttributes,
        uint nSize);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool SetHandleInformation(SafeFileHandle hObject, int dwMask, int dwFlags);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    public static partial uint WaitForSingleObject(IntPtr hHandle, uint dwMilliseconds);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool CloseHandle(IntPtr hObject);

    [LibraryImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool TerminateProcess(IntPtr hProcess, uint uExitCode);

    // --- headless process launch (wsl.exe with no console window) -----------

    public const uint CREATE_NO_WINDOW = 0x08000000;
    public const uint STARTF_USESTDHANDLES = 0x00000100;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct STARTUPINFO
    {
        public int cb;
        public string? lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute, dwFlags;
        public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROCESS_INFORMATION
    {
        public IntPtr hProcess, hThread;
        public int dwProcessId, dwThreadId;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool CreateProcess(
        string? lpApplicationName, System.Text.StringBuilder lpCommandLine,
        IntPtr lpProcessAttributes, IntPtr lpThreadAttributes,
        [MarshalAs(UnmanagedType.Bool)] bool bInheritHandles, uint dwCreationFlags,
        IntPtr lpEnvironment, string? lpCurrentDirectory,
        ref STARTUPINFO lpStartupInfo, out PROCESS_INFORMATION lpProcessInformation);

    // --- console attach (so diagnostic console modes can still print) -------

    public const int ATTACH_PARENT_PROCESS = -1;
    public const int STD_OUTPUT_HANDLE = -11;
    public static readonly IntPtr INVALID_HANDLE_VALUE = new(-1);
    public const uint GENERIC_READ = 0x80000000, GENERIC_WRITE = 0x40000000;
    public const uint FILE_SHARE_READ = 1, FILE_SHARE_WRITE = 2, OPEN_EXISTING = 3;

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool AttachConsole(int dwProcessId);

    [DllImport("kernel32.dll")]
    public static extern IntPtr GetStdHandle(int nStdHandle);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr CreateFileW(string lpFileName, uint dwDesiredAccess, uint dwShareMode,
        IntPtr lpSecurityAttributes, uint dwCreationDisposition, uint dwFlagsAndAttributes, IntPtr hTemplateFile);

    // --- DWM composition: cheap window translucency without AllowsTransparency --
    // A transparent WPF render surface composited by DWM (honoring per-pixel
    // alpha across the extended frame), instead of WPF's expensive layered
    // window. The acrylic system backdrop is Windows 11 22621+.

    [StructLayout(LayoutKind.Sequential)]
    public struct MARGINS { public int cxLeftWidth, cxRightWidth, cyTopHeight, cyBottomHeight; }

    public const int DWMWA_SYSTEMBACKDROP_TYPE = 38;
    public const int DWMSBT_TRANSIENTWINDOW = 3;   // acrylic

    [LibraryImport("dwmapi.dll")]
    public static partial int DwmExtendFrameIntoClientArea(IntPtr hwnd, in MARGINS margins);

    [LibraryImport("dwmapi.dll")]
    public static partial int DwmSetWindowAttribute(IntPtr hwnd, int attribute, in int value, int size);

    // --- wslapi.dll ---------------------------------------------------------

    // BOOL WslIsDistributionRegistered(PCWSTR distributionName);
    [LibraryImport("wslapi.dll", StringMarshalling = StringMarshalling.Utf16)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static partial bool WslIsDistributionRegistered(string distributionName);
}
