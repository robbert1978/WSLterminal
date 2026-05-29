using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace WslTerminal;

/// <summary>
/// Launches WSL headlessly with redirected pipes and exposes the raw stdio
/// streams + process handle. Prefers <c>wslg.exe</c> (the GUI-subsystem
/// launcher) so Windows never allocates a console — no conhost/Windows Terminal
/// window appears. Used by both the single-session <see cref="WslSession"/> and
/// the multiplexed <see cref="WslMux"/>.
/// </summary>
public sealed class WslProcess : IDisposable
{
    public Stream StdIn { get; }
    public Stream StdOut { get; }
    private IntPtr _process;
    private bool _disposed;

    private WslProcess(Stream stdin, Stream stdout, IntPtr process)
    {
        StdIn = stdin;
        StdOut = stdout;
        _process = process;
    }

    // Prefer wslg.exe over wsl.exe. wsl.exe is a console-subsystem binary, so
    // launching it makes Windows allocate a console (a brief flash) even with
    // CREATE_NO_WINDOW; wslg.exe is the GUI-subsystem launcher Microsoft ships
    // for exactly this, so no console is ever created. It takes the same
    // arguments and relays redirected stdio identically. Resolve order:
    // $WSL_LAUNCHER override, then %ProgramFiles%\WSL\wslg.exe, then wsl.exe.
    private static readonly string Launcher = ResolveLauncher();

    /// <summary>The WSL launcher chosen at startup (wslg.exe when present).</summary>
    public static string LauncherPath => Launcher;

    private static string ResolveLauncher()
    {
        string? overridePath = Environment.GetEnvironmentVariable("WSL_LAUNCHER");
        if (!string.IsNullOrWhiteSpace(overridePath) && File.Exists(overridePath))
            return overridePath;

        string programFiles = Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles);
        string wslg = Path.Combine(programFiles, "WSL", "wslg.exe");
        if (File.Exists(wslg)) return wslg;

        return Path.Combine(Environment.SystemDirectory, "wsl.exe");
    }

    public static WslProcess Launch(string distribution, string command)
    {
        var sa = new Native.SECURITY_ATTRIBUTES
        {
            nLength = Marshal.SizeOf<Native.SECURITY_ATTRIBUTES>(),
            lpSecurityDescriptor = IntPtr.Zero,
            bInheritHandle = 1,
        };

        if (!Native.CreatePipe(out SafeFileHandle inRead, out SafeFileHandle inWrite, ref sa, 0))
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "CreatePipe(stdin) failed");
        if (!Native.CreatePipe(out SafeFileHandle outRead, out SafeFileHandle outWrite, ref sa, 0))
            throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), "CreatePipe(stdout) failed");

        Native.SetHandleInformation(inWrite, Native.HANDLE_FLAG_INHERIT, 0);
        Native.SetHandleInformation(outRead, Native.HANDLE_FLAG_INHERIT, 0);

        // The launcher parses its command line raw, so the distro name must NOT
        // be quoted (it would keep the quotes -> WSL_E_DISTRO_NOT_FOUND). `~`
        // starts in the Linux home dir.
        var cmd = new StringBuilder();
        cmd.Append('"').Append(Launcher).Append('"')
           .Append(" ~ --distribution ").Append(distribution)
           .Append(' ').Append(command);

        var si = new Native.STARTUPINFO
        {
            cb = Marshal.SizeOf<Native.STARTUPINFO>(),
            dwFlags = (int)Native.STARTF_USESTDHANDLES,
            hStdInput = inRead.DangerousGetHandle(),
            hStdOutput = outWrite.DangerousGetHandle(),
            hStdError = outWrite.DangerousGetHandle(),
        };

        // CREATE_NO_WINDOW is a no-op for wslg.exe (GUI subsystem) but keeps the
        // wsl.exe fallback windowless too.
        bool ok = Native.CreateProcess(null, cmd, IntPtr.Zero, IntPtr.Zero,
            bInheritHandles: true, Native.CREATE_NO_WINDOW, IntPtr.Zero, null,
            ref si, out Native.PROCESS_INFORMATION pi);
        GC.KeepAlive(inRead);
        GC.KeepAlive(outWrite);

        if (!ok)
        {
            int err = Marshal.GetLastWin32Error();
            inRead.Dispose(); inWrite.Dispose(); outRead.Dispose(); outWrite.Dispose();
            throw new System.ComponentModel.Win32Exception(err,
                $"CreateProcess({Path.GetFileName(Launcher)}) failed for distribution '{distribution}'.");
        }
        Native.CloseHandle(pi.hThread);

        inRead.Dispose();
        outWrite.Dispose();

        // Read buffer batches pipe syscalls (big win for throughput; FileStream
        // still returns available bytes immediately, so no added latency). Writes
        // are flushed explicitly so a tiny buffer is fine.
        var stdin = new FileStream(inWrite, FileAccess.Write, bufferSize: 1, isAsync: false);
        var stdout = new FileStream(outRead, FileAccess.Read, bufferSize: 65536, isAsync: false);
        return new WslProcess(stdin, stdout, pi.hProcess);
    }

    public bool WaitForExit(int milliseconds, out uint exitCode)
    {
        exitCode = 0;
        uint ms = milliseconds < 0 ? Native.INFINITE : (uint)milliseconds;
        if (Native.WaitForSingleObject(_process, ms) != Native.WAIT_OBJECT_0) return false;
        Native.GetExitCodeProcess(_process, out exitCode);
        return true;
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        try { StdIn.Dispose(); } catch { }
        try { StdOut.Dispose(); } catch { }
        if (_process != IntPtr.Zero)
        {
            Native.CloseHandle(_process);
            _process = IntPtr.Zero;
        }
    }
}
