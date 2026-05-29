using System.Windows;
using System.Windows.Controls;
using WslTerminal.Vt;

namespace WslTerminal.Ui;

/// <summary>A node in a tab's pane tree: either a leaf <see cref="Pane"/> (one
/// terminal) or a <see cref="SplitNode"/> (two children split left/right or
/// top/bottom). <see cref="Element"/> is the WPF element placed in the layout.</summary>
internal abstract class PaneNode
{
    public SplitNode? Parent;
    public abstract FrameworkElement Element { get; }
}

/// <summary>A leaf pane: a terminal emulator + its view + its multiplexed session.
/// The view is wrapped in <see cref="Host"/> (a Border) for the active highlight.</summary>
internal sealed class Pane : PaneNode
{
    public Terminal Term { get; }
    public TerminalView View { get; }
    public MuxSession? Session { get; }
    public Border Host { get; }
    public string Title { get; set; } = "shell";

    public Pane(Terminal term, TerminalView view, MuxSession? session, Border host)
    {
        Term = term; View = view; Session = session; Host = host;
    }

    public override FrameworkElement Element => Host;
}

/// <summary>Two child nodes side by side (Columns) or stacked (rows), with a
/// draggable GridSplitter between them.</summary>
internal sealed class SplitNode : PaneNode
{
    public bool Columns;        // true = side-by-side (split right); false = stacked (split down)
    public PaneNode A, B;
    public Grid Grid { get; }

    public SplitNode(bool columns, PaneNode a, PaneNode b, Grid grid)
    {
        Columns = columns; A = a; B = b; Grid = grid;
    }

    public override FrameworkElement Element => Grid;
}
