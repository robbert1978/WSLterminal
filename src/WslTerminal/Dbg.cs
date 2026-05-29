using System.Runtime.CompilerServices;
using System.Text;

namespace WslTerminal;

/// <summary>Tiny ad-hoc trace buffer used while diagnosing the render path.</summary>
internal static class Dbg
{
    public static readonly StringBuilder Trace = new();
    public static int Id(object? o) => o is null ? 0 : RuntimeHelpers.GetHashCode(o);
    public static void Log(string s) => Trace.AppendLine(s);
}
