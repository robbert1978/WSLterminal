using System.Buffers.Binary;
using System.IO;

namespace WslTerminal;

/// <summary>
/// A single PTY session over its own wsl.exe + <see cref="WslProcess"/>, using
/// the single-session helper (wslpty) framing on stdin. Used by the console
/// diagnostic modes; the GUI uses the multiplexed <see cref="WslMux"/> instead.
/// </summary>
public sealed class WslSession : IDisposable
{
    private readonly WslProcess _p;
    private readonly object _writeLock = new();
    private bool _disposed;

    /// <summary>Raw PTY output stream from the helper. Read on a worker thread.</summary>
    public Stream Output => _p.StdOut;

    private WslSession(WslProcess p) => _p = p;

    public static WslSession Launch(string distribution, string command, bool useCwd = false)
        => new(WslProcess.Launch(distribution, command));

    // ---- framed control channel (host -> helper) ---------------------------

    /// <summary>Forward terminal input bytes to the PTY master (frame type 0x00).</summary>
    public void SendData(ReadOnlySpan<byte> data)
    {
        Span<byte> hdr = stackalloc byte[5];
        hdr[0] = 0x00;
        BinaryPrimitives.WriteUInt32LittleEndian(hdr[1..], (uint)data.Length);
        lock (_writeLock)
        {
            _p.StdIn.Write(hdr);
            if (!data.IsEmpty) _p.StdIn.Write(data);
            _p.StdIn.Flush();
        }
    }

    /// <summary>Resize the PTY window (frame type 0x01 -> TIOCSWINSZ + SIGWINCH).</summary>
    public void SendResize(int cols, int rows)
    {
        if (cols <= 0 || rows <= 0) return;
        Span<byte> f = stackalloc byte[5];
        f[0] = 0x01;
        BinaryPrimitives.WriteUInt16LittleEndian(f[1..], (ushort)Math.Min(cols, ushort.MaxValue));
        BinaryPrimitives.WriteUInt16LittleEndian(f[3..], (ushort)Math.Min(rows, ushort.MaxValue));
        lock (_writeLock) { _p.StdIn.Write(f); _p.StdIn.Flush(); }
    }

    /// <summary>Deliver a signal to the shell (frame type 0x02), e.g. SIGHUP=1.</summary>
    public void SendSignal(int signo)
    {
        Span<byte> f = stackalloc byte[2] { 0x02, (byte)signo };
        lock (_writeLock) { _p.StdIn.Write(f); _p.StdIn.Flush(); }
    }

    /// <summary>Closes the input channel (EOF to the helper's control stream).</summary>
    public void CloseInput()
    {
        lock (_writeLock) { try { _p.StdIn.Dispose(); } catch { } }
    }

    public bool WaitForExit(int milliseconds, out uint exitCode) => _p.WaitForExit(milliseconds, out exitCode);

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _p.Dispose();
    }
}
