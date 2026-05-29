using System.IO;
using System.Text;

namespace WslTerminal;

/// <summary>Headless console entry points used to validate the WSL PTY path
/// without a GUI (probe / selftest / a basic interactive relay).</summary>
internal static class ConsoleModes
{
    // Confirm the headless launch + pipes work. No PTY helper here, so `tty` is
    // expected to report "not a tty" — the contrast case.
    public static int Probe(string distro)
    {
        const string cmd =
            "echo ARG0=$0; echo SHELL=$SHELL; " +
            "echo PARENT=$(cat /proc/$PPID/comm 2>/dev/null); " +
            "printf 'TTY='; tty; echo PROBE_DONE";
        Console.WriteLine($"[probe] headless wsl.exe('{distro}', <inline cmd>, pipes) — no PTY helper");
        using var s = WslSession.Launch(distro, cmd);
        Console.Write(ReadToEnd(s.Output, 8000));
        s.WaitForExit(3000, out uint code);
        Console.WriteLine($"[probe] exit={code}");
        return 0;
    }

    // THE acceptance test: spawn the PTY helper via WslLaunch and drive a real
    // login shell with `tty; exit`. Success = the shell reports /dev/pts/N.
    public static int SelfTest(string distro)
    {
        string helper = WslBootstrap.ResolveHelper();
        string cmd = WslBootstrap.BuildLaunchCommand(helper);
        Console.WriteLine($"[selftest] helper = {helper}");
        Console.WriteLine($"[selftest] launch = {cmd}");

        using var s = WslSession.Launch(distro, cmd);

        var sb = new StringBuilder();
        var reader = new Thread(() =>
        {
            var buf = new byte[8192];
            int n;
            try { while ((n = s.Output.Read(buf, 0, buf.Length)) > 0) sb.Append(Encoding.UTF8.GetString(buf, 0, n)); }
            catch { }
        }) { IsBackground = true };
        reader.Start();

        s.SendResize(120, 30);
        Thread.Sleep(150);
        s.SendData(Encoding.UTF8.GetBytes("tty\n"));
        Thread.Sleep(150);
        s.SendData(Encoding.UTF8.GetBytes("exit\n"));

        reader.Join(6000);
        s.WaitForExit(2000, out uint code);

        string captured = sb.ToString();
        Console.WriteLine("---- captured PTY output (escaped) ----");
        Console.WriteLine(Visible(captured));
        Console.WriteLine("---------------------------------------");

        bool ok = captured.Contains("/dev/pts/");
        Console.WriteLine($"[selftest] shell exit={code}");
        Console.WriteLine(ok
            ? "[selftest] PASS — real WSL PTY (/dev/pts/N) via headless wsl.exe (CREATE_NO_WINDOW) + forkpty; no terminal window"
            : "[selftest] FAIL — did not observe /dev/pts/N");
        return ok ? 0 : 1;
    }

    public static int Interactive(string distro)
    {
        string helper = WslBootstrap.ResolveHelper();
        using var s = WslSession.Launch(distro, WslBootstrap.BuildLaunchCommand(helper));

        var outThread = new Thread(() =>
        {
            var stdout = Console.OpenStandardOutput();
            var buf = new byte[8192];
            int n;
            try { while ((n = s.Output.Read(buf, 0, buf.Length)) > 0) { stdout.Write(buf, 0, n); stdout.Flush(); } }
            catch { }
        }) { IsBackground = true };
        outThread.Start();

        s.SendResize(Math.Max(Console.WindowWidth, 80), Math.Max(Console.WindowHeight, 24));

        var stdin = Console.OpenStandardInput();
        var inbuf = new byte[4096];
        int r;
        try { while ((r = stdin.Read(inbuf, 0, inbuf.Length)) > 0) s.SendData(inbuf.AsSpan(0, r)); }
        catch { }
        s.CloseInput();
        s.WaitForExit(2000, out _);
        return 0;
    }

    private static string ReadToEnd(Stream stream, int timeoutMs)
    {
        var sb = new StringBuilder();
        var done = new ManualResetEventSlim(false);
        var t = new Thread(() =>
        {
            var buf = new byte[8192];
            int n;
            try { while ((n = stream.Read(buf, 0, buf.Length)) > 0) sb.Append(Encoding.UTF8.GetString(buf, 0, n)); }
            catch { }
            finally { done.Set(); }
        }) { IsBackground = true };
        t.Start();
        done.Wait(timeoutMs);
        return sb.ToString();
    }

    private static string Visible(string s)
    {
        var sb = new StringBuilder(s.Length);
        foreach (char c in s)
        {
            if (c == 0x1b) sb.Append("\\e");
            else if (c == '\n') sb.Append("\\n\n");
            else if (c == '\r') sb.Append("\\r");
            else if (c < 0x20) sb.Append($"\\x{(int)c:x2}");
            else sb.Append(c);
        }
        return sb.ToString();
    }
}
