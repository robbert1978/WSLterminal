using System.Text;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using WslTerminal.Ui;
using WslTerminal.Vt;

namespace WslTerminal;

/// <summary>Headless render check: lay out a TerminalView, feed colored text,
/// render to a bitmap, and confirm glyphs and colors actually painted. Verifies
/// the GlyphRun path without needing a visible window.</summary>
internal static class RenderProbe
{
    public static int Run()
    {
        const int W = 480, H = 240;
        var term = new Terminal(80, 24);
        var view = new TerminalView(term) { Width = W, Height = H };

        // Size the grid and feed content BEFORE any layout/render pass: an
        // unconnected visual realizes OnRender only once, so the content must be
        // present first. (The live window re-renders on MarkDirty via the Dispatcher.)
        view.EnsureGrid(W, H);
        term.Feed(Encoding.UTF8.GetBytes(
            "hello world\r\n\x1b[31mREDTEXT\x1b[0m\r\n\x1b[42mGREEN-BACKGROUND\x1b[0m\r\n"));
        view.SelectForTest(0, 0, 0, 10);   // highlight "hello world" on row 0

        view.Measure(new Size(W, H));
        view.Arrange(new Rect(0, 0, W, H));

        var rtb = new RenderTargetBitmap(W, H, 96, 96, PixelFormats.Pbgra32);
        rtb.Render(view);

        int stride = W * 4;
        var px = new byte[H * stride];
        rtb.CopyPixels(px, stride, 0);

        long nonBg = 0, redText = 0, greenBg = 0, selBlue = 0;
        for (int i = 0; i < px.Length; i += 4)
        {
            byte b = px[i], g = px[i + 1], r = px[i + 2];
            if (!(Math.Abs(r - 0x0C) < 8 && Math.Abs(g - 0x0C) < 8 && Math.Abs(b - 0x0C) < 8)) nonBg++;
            if (r > 120 && g < 70 && b < 70) redText++;                                  // red foreground text
            if (g > 100 && r < 70 && b < 70) greenBg++;                                  // green background block
            if (Math.Abs(r - 0x26) < 22 && Math.Abs(g - 0x4F) < 22 && Math.Abs(b - 0x78) < 26) selBlue++; // selection
        }

        var (cw, ch) = view.CellSize;
        Console.WriteLine($"[rendertest] grid={view.GridCols}x{view.GridRows} cell={cw:0.0}x{ch:0.0}");
        Console.WriteLine($"[rendertest] nonBackgroundPixels={nonBg} redText={redText} greenBg={greenBg} selectionHighlight={selBlue}");
        bool ok = nonBg > 200 && redText > 0 && greenBg > 0 && selBlue > 0;
        Console.WriteLine(ok
            ? "[rendertest] PASS — glyphs, colors, and selection highlight rendered"
            : "[rendertest] FAIL — expected text + red fg + green bg + selection pixels");
        return ok ? 0 : 1;
    }
}
