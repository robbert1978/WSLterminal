using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Threading;

namespace WslTerminal;

/// <summary>
/// Single-instance coordination over a per-user named pipe. The first process to
/// run becomes the host; later launches forward their (distro, cwd) request to
/// the host and exit. That way every window lives in ONE process and shares ONE
/// wslptyd server per distro, no matter how often the app is launched.
/// </summary>
public static class SingleInstance
{
    private static string PipeName => $"WslTerminal.{Environment.UserName}";

    /// <summary>Try to hand this launch to an already-running host. Returns true
    /// if forwarded (this process should exit); false if no host exists yet (this
    /// process should become the host and call <see cref="StartHost"/>).</summary>
    public static bool TryForward(string distro, string? cwd)
    {
        try
        {
            using var client = new NamedPipeClientStream(".", PipeName, PipeDirection.Out);
            client.Connect(600);                       // throws quickly if no host
            byte[] payload = Encoding.UTF8.GetBytes($"{distro}\t{cwd}");
            client.Write(payload, 0, payload.Length);
            client.Flush();
            return true;
        }
        catch { return false; }
    }

    /// <summary>Accept forwarded launches; <paramref name="onOpen"/> is called with
    /// (distro, cwd) for each (on the pipe thread — marshal to the UI yourself).</summary>
    public static void StartHost(Action<string, string?> onOpen)
    {
        var t = new Thread(() =>
        {
            while (true)
            {
                try
                {
                    using var server = new NamedPipeServerStream(
                        PipeName, PipeDirection.In,
                        NamedPipeServerStream.MaxAllowedServerInstances, PipeTransmissionMode.Byte);
                    server.WaitForConnection();
                    using var ms = new MemoryStream();
                    server.CopyTo(ms);
                    string s = Encoding.UTF8.GetString(ms.ToArray());
                    int tab = s.IndexOf('\t');
                    string distro = tab < 0 ? s : s[..tab];
                    string? cwd = tab < 0 ? null : s[(tab + 1)..];
                    if (string.IsNullOrEmpty(cwd)) cwd = null;
                    onOpen(distro, cwd);
                }
                catch { Thread.Sleep(100); }
            }
        })
        { IsBackground = true, Name = "wsl-single-instance" };
        t.Start();
    }
}
