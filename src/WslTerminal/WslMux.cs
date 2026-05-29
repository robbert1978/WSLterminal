using System.Buffers.Binary;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;
using System.Threading;

namespace WslTerminal;

/// <summary>
/// Multiplexes many PTY sessions over one wsl.exe + one wslptyd server process.
/// Opening N terminal windows therefore costs 1 wsl.exe + 1 server (+ N shells),
/// instead of N×(wsl.exe + helper). One reader thread demuxes server output and
/// dispatches per-session; writes are framed under a lock.
/// </summary>
public sealed class WslMux : IDisposable
{
    private readonly WslProcess _p;
    private readonly object _writeLock = new();
    private readonly object _lock = new();
    private readonly Dictionary<uint, MuxSession> _sessions = new();
    private int _nextId;
    private volatile bool _dead;
    private readonly Thread _reader;

    public bool IsDead => _dead;

    private WslMux(WslProcess p)
    {
        _p = p;
        _reader = new Thread(ReaderLoop) { IsBackground = true, Name = "wsl-mux-reader" };
        _reader.Start();
    }

    public static WslMux Start(string distribution, string serverWinPath)
        => new(WslProcess.Launch(distribution, WslBootstrap.BuildServerCommand(serverWinPath)));

    /// <summary>Open a new PTY session (forkpty + login shell) on the server.</summary>
    public MuxSession Open(int cols, int rows, string? cwd)
    {
        uint id = (uint)Interlocked.Increment(ref _nextId);
        var s = new MuxSession(this, id);
        lock (_lock) _sessions[id] = s;

        byte[] cwdB = Encoding.UTF8.GetBytes(cwd ?? "");
        var payload = new byte[2 + 2 + 4 + cwdB.Length + 4];   // cols,rows,cwdLen,cwd,shellLen(0)
        BinaryPrimitives.WriteUInt16LittleEndian(payload.AsSpan(0), (ushort)Math.Clamp(cols, 1, ushort.MaxValue));
        BinaryPrimitives.WriteUInt16LittleEndian(payload.AsSpan(2), (ushort)Math.Clamp(rows, 1, ushort.MaxValue));
        BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(4), (uint)cwdB.Length);
        cwdB.CopyTo(payload, 8);
        BinaryPrimitives.WriteUInt32LittleEndian(payload.AsSpan(8 + cwdB.Length), 0u);  // shell len 0 => $SHELL
        WriteFrame(id, 1, payload);
        return s;
    }

    internal void SendData(uint id, ReadOnlySpan<byte> data) => WriteFrame(id, 2, data);

    internal void SendResize(uint id, int cols, int rows)
    {
        if (cols <= 0 || rows <= 0) return;
        Span<byte> p = stackalloc byte[4];
        BinaryPrimitives.WriteUInt16LittleEndian(p, (ushort)Math.Min(cols, ushort.MaxValue));
        BinaryPrimitives.WriteUInt16LittleEndian(p[2..], (ushort)Math.Min(rows, ushort.MaxValue));
        WriteFrame(id, 3, p);
    }

    internal void SendSignal(uint id, int signo)
    {
        Span<byte> p = stackalloc byte[1] { (byte)signo };
        WriteFrame(id, 4, p);
    }

    internal void Close(uint id)
    {
        WriteFrame(id, 5, ReadOnlySpan<byte>.Empty);
        lock (_lock) _sessions.Remove(id);
    }

    private void WriteFrame(uint id, byte type, ReadOnlySpan<byte> payload)
    {
        Span<byte> hdr = stackalloc byte[9];
        BinaryPrimitives.WriteUInt32LittleEndian(hdr, id);
        hdr[4] = type;
        BinaryPrimitives.WriteUInt32LittleEndian(hdr[5..], (uint)payload.Length);
        lock (_writeLock)
        {
            if (_dead) return;
            try
            {
                _p.StdIn.Write(hdr);
                if (!payload.IsEmpty) _p.StdIn.Write(payload);
                _p.StdIn.Flush();
            }
            catch { _dead = true; }
        }
    }

    private void ReaderLoop()
    {
        var hdr = new byte[9];
        try
        {
            while (ReadFull(_p.StdOut, hdr, 9))
            {
                uint id = BinaryPrimitives.ReadUInt32LittleEndian(hdr);
                byte type = hdr[4];
                uint len = BinaryPrimitives.ReadUInt32LittleEndian(hdr.AsSpan(5));
                if (len > 64u * 1024 * 1024) break;             // sanity bound

                byte[] payload = len > 0 ? new byte[len] : Array.Empty<byte>();
                if (len > 0 && !ReadFull(_p.StdOut, payload, (int)len)) break;

                MuxSession? s;
                lock (_lock) _sessions.TryGetValue(id, out s);
                if (type == 2) s?.RaiseData(payload);
                else if (type == 6)
                {
                    int code = len >= 4 ? (int)BinaryPrimitives.ReadUInt32LittleEndian(payload) : 0;
                    s?.RaiseExit(code);
                    lock (_lock) _sessions.Remove(id);
                }
            }
        }
        catch { /* server gone */ }

        _dead = true;
        MuxSession[] all;
        lock (_lock) { all = _sessions.Values.ToArray(); _sessions.Clear(); }
        foreach (var s in all) s.RaiseExit(-1);
    }

    private static bool ReadFull(Stream s, byte[] buf, int n)
    {
        int got = 0;
        while (got < n)
        {
            int k = s.Read(buf, got, n - got);
            if (k <= 0) return false;
            got += k;
        }
        return true;
    }

    public void Dispose()
    {
        _dead = true;
        // Close stdin first: the server sees EOF, tears down its sessions and
        // exits, which closes its stdout end so the reader's blocking Read
        // returns EOF (a synchronous pipe Read can't be cancelled by just
        // closing the handle, so we must unblock it this way).
        try { _p.StdIn.Dispose(); } catch { }
        _reader.Join(1500);
        try { _p.Dispose(); } catch { }
    }
}

/// <summary>One PTY session within a <see cref="WslMux"/>.</summary>
public sealed class MuxSession
{
    private readonly WslMux _mux;
    public uint Id { get; }

    public event Action<byte[]>? DataReceived;
    public event Action<int>? Exited;

    internal MuxSession(WslMux mux, uint id) { _mux = mux; Id = id; }

    public void SendData(ReadOnlySpan<byte> data) => _mux.SendData(Id, data);
    public void SendResize(int cols, int rows) => _mux.SendResize(Id, cols, rows);
    public void SendSignal(int signo) => _mux.SendSignal(Id, signo);
    public void Close() => _mux.Close(Id);

    internal void RaiseData(byte[] data) => DataReceived?.Invoke(data);
    internal void RaiseExit(int code) => Exited?.Invoke(code);
}

/// <summary>Keeps one <see cref="WslMux"/> per distribution, recreated if it dies.</summary>
public static class WslMuxManager
{
    private static readonly object Lock = new();
    private static readonly Dictionary<string, WslMux> Muxes = new();

    public static WslMux Get(string distribution, string serverWinPath)
    {
        lock (Lock)
        {
            if (!Muxes.TryGetValue(distribution, out WslMux? mux) || mux.IsDead)
            {
                mux = WslMux.Start(distribution, serverWinPath);
                Muxes[distribution] = mux;
            }
            return mux;
        }
    }

    public static void DisposeAll()
    {
        lock (Lock)
        {
            foreach (var m in Muxes.Values) m.Dispose();
            Muxes.Clear();
        }
    }
}
