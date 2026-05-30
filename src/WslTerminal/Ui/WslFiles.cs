using System.IO;

namespace WslTerminal.Ui;

/// <summary>
/// Bridges WSL (Linux) paths to Windows so the file sidebar can list and read
/// them without spawning a process: WSL exposes its filesystem to Windows at
/// <c>\\wsl.localhost\&lt;distro&gt;\…</c> (older builds: <c>\\wsl$\&lt;distro&gt;</c>),
/// so a Linux path like <c>/home/u/p</c> maps to <c>\\wsl.localhost\Ubuntu\home\u\p</c>.
/// </summary>
internal static class WslFiles
{
    /// <summary>Map a Linux path in <paramref name="distro"/> to its Windows UNC form.</summary>
    public static string ToUnc(string distro, string linuxPath)
    {
        string rel = linuxPath.Replace('/', '\\').TrimStart('\\');
        return $@"\\wsl.localhost\{distro}\{rel}";
    }

    /// <summary>One entry in a directory listing.</summary>
    public sealed record Entry(string Name, string LinuxPath, bool IsDir, long Size);

    /// <summary>
    /// List <paramref name="linuxDir"/>'s immediate children (dirs first, then files,
    /// each alphabetical, case-insensitive). Returns empty on any I/O error so the
    /// UI never throws. Dotfiles are included.
    /// </summary>
    public static List<Entry> List(string distro, string linuxDir)
    {
        var result = new List<Entry>();
        try
        {
            string unc = ToUnc(distro, linuxDir);
            var di = new DirectoryInfo(unc);
            if (!di.Exists) return result;

            var dirs = new List<Entry>();
            var files = new List<Entry>();
            string baseLinux = linuxDir.TrimEnd('/');
            foreach (var fsi in di.EnumerateFileSystemInfos())
            {
                bool isDir = (fsi.Attributes & FileAttributes.Directory) != 0;
                string childLinux = baseLinux + "/" + fsi.Name;
                long size = isDir ? 0 : SafeLen(fsi);
                var e = new Entry(fsi.Name, childLinux, isDir, size);
                (isDir ? dirs : files).Add(e);
            }
            dirs.Sort((a, b) => string.Compare(a.Name, b.Name, StringComparison.OrdinalIgnoreCase));
            files.Sort((a, b) => string.Compare(a.Name, b.Name, StringComparison.OrdinalIgnoreCase));
            result.AddRange(dirs);
            result.AddRange(files);
        }
        catch { /* permission / not-a-dir / disconnected: empty list */ }
        return result;
    }

    private static long SafeLen(FileSystemInfo fsi)
    {
        try { return fsi is FileInfo fi ? fi.Length : 0; }
        catch { return 0; }
    }

    /// <summary>Read a file's bytes (capped) for preview. Null on error.</summary>
    public static byte[]? ReadBytes(string distro, string linuxPath, int maxBytes = 2 * 1024 * 1024)
    {
        try
        {
            string unc = ToUnc(distro, linuxPath);
            using var fs = new FileStream(unc, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
            int n = (int)Math.Min(fs.Length, maxBytes);
            var buf = new byte[n];
            int read = 0;
            while (read < n)
            {
                int r = fs.Read(buf, read, n - read);
                if (r <= 0) break;
                read += r;
            }
            return read == n ? buf : buf[..read];
        }
        catch { return null; }
    }

    /// <summary>The file's full size in bytes (-1 on error). Used to detect when a
    /// preview was truncated at the read cap, so we don't save a partial file.</summary>
    public static long FileLength(string distro, string linuxPath)
    {
        try { return new FileInfo(ToUnc(distro, linuxPath)).Length; }
        catch { return -1; }
    }

    /// <summary>Write UTF-8 text back to a WSL file (no BOM, overwriting). Returns
    /// false on any I/O error. The write goes through the \\wsl.localhost share, an
    /// in-place overwrite that keeps the existing inode/permissions.</summary>
    public static bool WriteText(string distro, string linuxPath, string text)
    {
        try
        {
            var bytes = new System.Text.UTF8Encoding(false).GetBytes(text);
            using var fs = new FileStream(ToUnc(distro, linuxPath), FileMode.Create, FileAccess.Write, FileShare.Read);
            fs.Write(bytes, 0, bytes.Length);
            return true;
        }
        catch { return false; }
    }

    /// <summary>True if the first chunk of bytes looks like binary (has a NUL).</summary>
    public static bool LooksBinary(ReadOnlySpan<byte> bytes)
    {
        int n = Math.Min(bytes.Length, 8000);
        for (int i = 0; i < n; i++) if (bytes[i] == 0) return true;
        return false;
    }

    private static readonly HashSet<string> ImageExt = new(StringComparer.OrdinalIgnoreCase)
        { ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".webp" };

    public static bool IsImage(string name) => ImageExt.Contains(Path.GetExtension(name));

    public static string HumanSize(long bytes)
    {
        string[] u = { "B", "KB", "MB", "GB", "TB" };
        double s = bytes; int i = 0;
        while (s >= 1024 && i < u.Length - 1) { s /= 1024; i++; }
        return i == 0 ? $"{bytes} B" : $"{s:0.#} {u[i]}";
    }
}
