using System.Windows;
using ICSharpCode.AvalonEdit;

namespace WslTerminal.Ui;

/// <summary>
/// A file opened in a viewer/editor tab. Wraps the hosted element (an editable
/// <see cref="TextEditor"/> for text, or an image/info element otherwise) and
/// tracks dirty state + saving back to the WSL file. Images, binaries, and files
/// that were truncated at the read cap are not editable.
/// </summary>
internal sealed class FileDocument
{
    public FrameworkElement Element { get; }
    public TextEditor? Editor { get; }
    public string Distro { get; }
    public string LinuxPath { get; }
    public string Name { get; }

    private string _saved;                 // last-saved text, for the dirty check
    public event Action? DirtyChanged;

    public bool IsEditable => Editor is not null;
    public bool IsDirty => Editor is not null && Editor.Text != _saved;

    public FileDocument(FrameworkElement element, TextEditor? editor, string distro, string linuxPath, string name)
    {
        Element = element;
        Editor = editor;
        Distro = distro;
        LinuxPath = linuxPath;
        Name = name;
        _saved = editor?.Text ?? "";
        if (editor is not null)
            editor.TextChanged += (_, _) => DirtyChanged?.Invoke();
    }

    /// <summary>Write the editor's text back to the WSL file. Returns false on I/O
    /// error or when the document isn't editable.</summary>
    public bool Save()
    {
        if (Editor is null) return false;
        string text = Editor.Text;
        if (!WslFiles.WriteText(Distro, LinuxPath, text)) return false;
        _saved = text;
        DirtyChanged?.Invoke();
        return true;
    }
}
