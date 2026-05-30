using System.Collections.Generic;
using System.Windows;
using System.Windows.Controls;

namespace WslTerminal.Ui;

/// <summary>One tab. Normally a tree of split panes (each pane is its own session
/// on the shared wslptyd server), with the title following the active pane. A tab
/// can instead be a <b>document</b> tab — a file viewer (<see cref="Document"/> is
/// the hosted preview element) with no panes or sessions.</summary>
internal sealed class TerminalTab
{
    public PaneNode Root = null!;          // root of the pane tree (null for a document tab)
    public Pane Active = null!;            // focused leaf pane (null for a document tab)
    public readonly List<Pane> Panes = new();

    /// <summary>Non-null => this is a file-viewer tab; holds the preview element.</summary>
    public FrameworkElement? Document;
    public bool IsDocument => Document is not null;

    /// <summary>For a file-viewer tab: the open document (editable text / save / dirty).</summary>
    public FileDocument? Doc;

    public Border Chip { get; set; } = null!;
    public TextBlock Label { get; set; } = null!;
    public string Title { get; set; } = "shell";
}
