using System.Collections.Generic;
using System.Windows.Controls;

namespace WslTerminal.Ui;

/// <summary>One tab: a tree of split panes (each pane is its own session on the
/// shared wslptyd server) plus the chip shown in the tab strip. The tab's title
/// follows its active pane.</summary>
internal sealed class TerminalTab
{
    public PaneNode Root = null!;          // root of the pane tree
    public Pane Active = null!;            // focused leaf pane
    public readonly List<Pane> Panes = new();

    public Border Chip { get; set; } = null!;
    public TextBlock Label { get; set; } = null!;
    public string Title { get; set; } = "shell";
}
