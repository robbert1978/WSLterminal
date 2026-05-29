using System.IO;
using System.Text;

namespace WslTerminal;

/// <summary>
/// Locates the compiled Linux helper and builds the WslLaunch command line
/// that stages it into the distro's /tmp and execs it. Staging into /tmp
/// avoids drvfs exec quirks (the artifact lives on the Windows filesystem).
/// </summary>
public static class WslBootstrap
{
    /// <summary>Translate a Windows path to its /mnt/&lt;drive&gt;/… WSL form.</summary>
    public static string WindowsToWslPath(string winPath)
    {
        string full = Path.GetFullPath(winPath);
        if (full.Length >= 2 && full[1] == ':')
        {
            char drive = char.ToLowerInvariant(full[0]);
            string rest = full[2..].Replace('\\', '/');
            if (!rest.StartsWith('/')) rest = "/" + rest;
            return $"/mnt/{drive}{rest}";
        }
        return full.Replace('\\', '/');
    }

    /// <summary>Find artifacts/wslpty (single-session helper) by walking up from the exe.</summary>
    public static string ResolveHelper() => ResolveArtifact("wslpty", "WSLPTY_BIN");

    /// <summary>Find artifacts/wslptyd (the multiplexed PTY server).</summary>
    public static string ResolveServer() => ResolveArtifact("wslptyd", "WSLPTYD_BIN");

    private static string ResolveArtifact(string name, string envVar)
    {
        string? env = Environment.GetEnvironmentVariable(envVar);
        if (!string.IsNullOrWhiteSpace(env) && File.Exists(env)) return env;

        var dir = new DirectoryInfo(AppContext.BaseDirectory);
        for (int i = 0; i < 8 && dir is not null; i++, dir = dir.Parent)
        {
            string candidate = Path.Combine(dir.FullName, "artifacts", name);
            if (File.Exists(candidate)) return candidate;
        }
        throw new FileNotFoundException(
            $"Could not locate artifacts/{name}. Build it first (native/build.sh inside WSL) " +
            $"or set {envVar} to its Windows path.");
    }

    /// <summary>
    /// Shell snippet for the launch: stage the helper into /tmp, make it
    /// executable, optionally cd into <paramref name="startDir"/>, then exec it
    /// (replacing the shell so signals/exit pass through). Run via the login
    /// shell, so the cd happens before the helper forks its own login shell.
    /// </summary>
    public static string BuildLaunchCommand(string helperWinPath, string? startDir = null)
    {
        string src = WindowsToWslPath(helperWinPath);
        var sb = new StringBuilder();
        sb.Append("d=/tmp/wslpty.$$; cp '").Append(src).Append("' \"$d\" 2>/dev/null; chmod +x \"$d\"; ");
        if (!string.IsNullOrEmpty(startDir))
            sb.Append("cd -- '").Append(startDir.Replace("'", "'\\''")).Append("' 2>/dev/null; ");
        sb.Append("exec \"$d\"");
        return sb.ToString();
    }

    /// <summary>Stage the multiplexed PTY server into /tmp and exec it. Per-session
    /// directories are sent in each OPEN frame, so no cwd is baked in here.</summary>
    public static string BuildServerCommand(string serverWinPath)
    {
        string src = WindowsToWslPath(serverWinPath);
        return $"d=/tmp/wslptyd.$$; cp '{src}' \"$d\" 2>/dev/null; chmod +x \"$d\"; exec \"$d\"";
    }
}
