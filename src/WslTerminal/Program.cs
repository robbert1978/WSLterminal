using System.IO;
using System.Text;
using System.Windows;
using WslTerminal;
using WslTerminal.Ui;
using WslTerminal.Vt;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        string distro = "Ubuntu";
        string mode = "--gui";
        string? startDir = null;
        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--gui":
                case "--probe":
                case "--selftest":
                case "--interactive":
                case "--vttest":
                case "--rendertest":
                case "--shottest":
                case "--tabtest":
                case "--splittest":
                case "--settingstest":
                case "--muxtest":
                case "--emojitest":
                case "--opacitytest":
                case "--benchtest":
                case "--pipetest":
                    mode = args[i];
                    break;
                case "--distro":
                    if (i + 1 < args.Length) distro = args[++i];
                    break;
                case "--cd":
                    if (i + 1 < args.Length) startDir = args[++i];
                    break;
            }
        }

        if (mode != "--gui") ConsoleHelper.Ensure();          // attach console for diagnostics

        if (mode == "--vttest") return VtSelfTest.Run();      // no WSL needed
        if (mode == "--rendertest") return RenderProbe.Run(); // no WSL needed
        if (mode == "--settingstest") return SettingsTest();  // no WSL needed
        if (mode == "--emojitest") return EmojiTest();        // no WSL needed
        if (mode == "--opacitytest") return OpacityTest();    // no WSL needed
        if (mode == "--benchtest") return BenchTest();        // no WSL needed

        try { Console.OutputEncoding = Encoding.UTF8; } catch { }

        if (!Native.WslIsDistributionRegistered(distro))
        {
            Console.Error.WriteLine($"Distribution '{distro}' is not registered.");
            return 2;
        }

        return mode switch
        {
            "--gui" => Gui(distro, startDir),
            "--shottest" => ShotTest(distro),
            "--tabtest" => TabTest(distro),
            "--splittest" => SplitTest(distro),
            "--muxtest" => MuxTest(distro),
            "--pipetest" => PipeTest(distro),
            "--probe" => ConsoleModes.Probe(distro),
            "--selftest" => ConsoleModes.SelfTest(distro),
            "--interactive" => ConsoleModes.Interactive(distro),
            _ => 64,
        };
    }

    // Prove the multiplexed server: two sessions over ONE wsl.exe + ONE server,
    // each getting its own /dev/pts.
    private static int MuxTest(string distro)
    {
        var mux = WslMux.Start(distro, WslBootstrap.ResolveServer());
        var sb1 = new StringBuilder();
        var sb2 = new StringBuilder();
        using var done1 = new ManualResetEventSlim();
        using var done2 = new ManualResetEventSlim();

        var s1 = mux.Open(80, 24, null);
        s1.DataReceived += d => { lock (sb1) sb1.Append(Encoding.UTF8.GetString(d)); };
        s1.Exited += _ => done1.Set();
        var s2 = mux.Open(80, 24, null);
        s2.DataReceived += d => { lock (sb2) sb2.Append(Encoding.UTF8.GetString(d)); };
        s2.Exited += _ => done2.Set();

        Thread.Sleep(250);
        s1.SendData(Encoding.UTF8.GetBytes("tty\n"));
        s2.SendData(Encoding.UTF8.GetBytes("tty\n"));
        Thread.Sleep(200);
        s1.SendData(Encoding.UTF8.GetBytes("exit\n"));
        s2.SendData(Encoding.UTF8.GetBytes("exit\n"));
        done1.Wait(6000);
        done2.Wait(6000);

        var m1 = System.Text.RegularExpressions.Regex.Match(sb1.ToString(), @"/dev/pts/\d+");
        var m2 = System.Text.RegularExpressions.Regex.Match(sb2.ToString(), @"/dev/pts/\d+");
        Console.WriteLine($"[muxtest] session1 -> {m1.Value}   session2 -> {m2.Value}");
        bool ok = m1.Success && m2.Success && m1.Value != m2.Value;
        Console.WriteLine(ok
            ? $"[muxtest] PASS — two real PTYs over one {Path.GetFileName(WslProcess.LauncherPath)} + one wslptyd server, distinct /dev/pts"
            : "[muxtest] FAIL — expected two distinct /dev/pts/N");
        mux.Dispose();
        return ok ? 0 : 1;
    }

    // Launch the real window, let the shell render, then capture the live
    // terminal surface to a PNG and assert it painted shell output.
    private static int ShotTest(string distro)
    {
        string server = WslBootstrap.ResolveServer();
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        var win = new MainWindow(distro, server, Settings.Load());
        int result = 1;

        var timer = new System.Windows.Threading.DispatcherTimer
        { Interval = TimeSpan.FromSeconds(2.0) };
        timer.Tick += (_, _) =>
        {
            timer.Stop();
            string png = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal_window.png");
            var (nonBg, colored, w, h) = win.CaptureStats(png);
            Console.WriteLine($"[shottest] window={w}x{h} nonBackgroundPx={nonBg} coloredPx={colored}");
            Console.WriteLine($"[shottest] saved {png}");
            result = nonBg > 500 && colored > 0 ? 0 : 1;
            Console.WriteLine(result == 0
                ? "[shottest] PASS — live on-screen window renders shell output (prompt + color)"
                : "[shottest] FAIL — window did not paint expected shell content");
            win.Close();
            app.Shutdown();
        };

        win.Show();
        timer.Start();
        app.Run();
        WslMuxManager.DisposeAll();
        return result;
    }

    // Drive the tab API: open the window (1 tab), add 2 more, capture the window
    // (tab strip visible), then close down to 1. Asserts the tab count tracks.
    private static int TabTest(string distro)
    {
        string server = WslBootstrap.ResolveServer();
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        var win = new MainWindow(distro, server, Settings.Load());
        int step = 0, result = 1;
        win.Show();
        var timer = new System.Windows.Threading.DispatcherTimer { Interval = TimeSpan.FromSeconds(1.3) };
        timer.Tick += (_, _) =>
        {
            step++;
            if (step == 1) { win.TestNewTab(); win.TestNewTab(); }     // -> 3 tabs
            else if (step == 2)
            {
                string png = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal_tabs.png");
                win.CaptureWindow(png);
                Console.WriteLine($"[tabtest] tabs after +2 = {win.TestTabCount} (expect 3); saved {png}");
                win.TestSwitch(-1);
                win.TestCloseActive();                                 // -> 2 tabs
            }
            else
            {
                int n = win.TestTabCount;
                result = n == 2 ? 0 : 1;
                Console.WriteLine($"[tabtest] tabs after close = {n} (expect 2)");
                Console.WriteLine(result == 0
                    ? "[tabtest] PASS — multiple tabs over one wslptyd server"
                    : "[tabtest] FAIL");
                timer.Stop();
                app.Shutdown();
            }
        };
        timer.Start();
        app.Run();
        WslMuxManager.DisposeAll();
        return result;
    }

    // Split the active pane right then down (3 panes), capture the layout, close
    // a pane back to 2. Asserts the pane count tracks.
    private static int SplitTest(string distro)
    {
        string server = WslBootstrap.ResolveServer();
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        var win = new MainWindow(distro, server, Settings.Load());
        int step = 0, result = 1;
        win.Show();
        var timer = new System.Windows.Threading.DispatcherTimer { Interval = TimeSpan.FromSeconds(1.3) };
        timer.Tick += (_, _) =>
        {
            step++;
            if (step == 1) win.TestSplit(true);         // split right -> 2 panes
            else if (step == 2) win.TestSplit(false);   // split down  -> 3 panes
            else if (step == 3)
            {
                string png = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal_split.png");
                win.CaptureWindow(png);
                Console.WriteLine($"[splittest] panes after right+down = {win.TestPaneCount} (expect 3); saved {png}");
                win.TestClosePane();                     // -> 2 panes
            }
            else
            {
                int n = win.TestPaneCount;
                result = n == 2 ? 0 : 1;
                Console.WriteLine($"[splittest] panes after close = {n} (expect 2)");
                Console.WriteLine(result == 0
                    ? "[splittest] PASS — split right/down + close, all on one server"
                    : "[splittest] FAIL");
                timer.Stop();
                app.Shutdown();
            }
        };
        timer.Start();
        app.Run();
        WslMuxManager.DisposeAll();
        return result;
    }

    // Verify the appearance dialog constructs + lays out without throwing, and
    // render it to a PNG for inspection.
    private static int SettingsTest()
    {
        var app = new Application { ShutdownMode = ShutdownMode.OnExplicitShutdown };
        int result = 1;
        app.Dispatcher.BeginInvoke(() =>
        {
            try
            {
                // Catch InvariantGlobalization misconfig: the live dialog uses the
                // UI culture (e.g. en-US/1033) for text input/rendering, which
                // throws if globalization-invariant mode is on.
                _ = System.Globalization.CultureInfo.GetCultureInfo("en-US");

                // Round-trip check: the dialog must preserve EVERY field (a missing
                // control would silently reset Opacity/Selection to defaults on OK).
                var probe = new Settings { Opacity = 80, Selection = "#123456", FontSize = 13, Background = "#010203" };
                var snap = new SettingsWindow(probe, _ => { }).SnapshotForTest();
                if (snap.Opacity != 80 || snap.Selection != "#123456" || (int)snap.FontSize != 13 || snap.Background != "#010203")
                    throw new Exception($"settings round-trip lost a field: op={snap.Opacity} sel={snap.Selection} size={snap.FontSize} bg={snap.Background}");

                var dlg = new SettingsWindow(Settings.Load(), _ => { });
                dlg.Show();
                dlg.UpdateLayout();
                int w = (int)dlg.ActualWidth, h = (int)dlg.ActualHeight;
                if (w > 0 && h > 0 && dlg.Content is System.Windows.Media.Visual v)
                {
                    var rtb = new System.Windows.Media.Imaging.RenderTargetBitmap(
                        w, h, 96, 96, System.Windows.Media.PixelFormats.Pbgra32);
                    rtb.Render(v);
                    var enc = new System.Windows.Media.Imaging.PngBitmapEncoder();
                    enc.Frames.Add(System.Windows.Media.Imaging.BitmapFrame.Create(rtb));
                    string png = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal_settings.png");
                    using (var fs = System.IO.File.Create(png)) enc.Save(fs);
                    Console.WriteLine($"[settingstest] dialog {w}x{h}, saved {png}");
                }
                result = 0;
                Console.WriteLine("[settingstest] PASS — settings dialog opens");
                dlg.Close();
            }
            catch (Exception ex)
            {
                Console.WriteLine("[settingstest] FAIL — " + ex.Message);
                result = 1;
            }
            app.Shutdown();
        });
        app.Run();
        return result;
    }

    // Verify the actual terminal renders fallback glyphs + combining marks: feed
    // emoji/kaomoji/CJK to a Terminal+TerminalView, render, and check the cells
    // and pixels. (Emoji are monochrome — WPF has no color-font support.)
    private static int EmojiTest()
    {
        const int W = 620, H = 80;
        var term = new Terminal(80, 24);
        var view = new TerminalView(term) { Width = W, Height = H };
        view.EnsureGrid(W, H);
        term.Feed(Encoding.UTF8.GetBytes("🐍 (๑˃̵ᴗ˂̵)و café 日本語 ★✓←"));
        view.Measure(new System.Windows.Size(W, H));
        view.Arrange(new System.Windows.Rect(0, 0, W, H));

        var rtb = new System.Windows.Media.Imaging.RenderTargetBitmap(W, H, 96, 96, System.Windows.Media.PixelFormats.Pbgra32);
        rtb.Render(view);
        int stride = W * 4;
        var px = new byte[H * stride];
        rtb.CopyPixels(px, stride, 0);
        long nonBg = 0, colored = 0;
        for (int i = 0; i < px.Length; i += 4)
        {
            byte b = px[i], g = px[i + 1], r = px[i + 2];
            if (Math.Abs(r - 0x0C) > 12 || Math.Abs(g - 0x0C) > 12 || Math.Abs(b - 0x0C) > 12) nonBg++;
            if (Math.Max(r, Math.Max(g, b)) - Math.Min(r, Math.Min(g, b)) > 40) colored++;  // non-gray => color emoji
        }

        // inspect the grid: snake stored as one rune, combining marks attached
        var grid = new Cell[view.GridRows][];
        for (int r = 0; r < view.GridRows; r++) grid[r] = new Cell[view.GridCols];
        term.CaptureViewport(0, grid);
        bool snake = false, combo = false, cjk = false;
        foreach (var c in grid[0])
        {
            if (c.Rune == 0x1F40D) snake = true;
            if (c.Combo != null) combo = true;
            if (c.Rune >= 0x4E00 && c.Rune <= 0x9FFF) cjk = true;
        }

        string png = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal_emoji.png");
        var enc = new System.Windows.Media.Imaging.PngBitmapEncoder();
        enc.Frames.Add(System.Windows.Media.Imaging.BitmapFrame.Create(rtb));
        using (var fs = System.IO.File.Create(png)) enc.Save(fs);

        Console.WriteLine($"[emojitest] nonBackgroundPx={nonBg}  coloredPx={colored}  snakeCell={snake}  combiningAttached={combo}  cjkCell={cjk}");
        Console.WriteLine($"[emojitest] saved {png}");
        bool ok = nonBg > 600 && colored > 0 && snake && combo && cjk;
        Console.WriteLine(ok
            ? "[emojitest] PASS — color emoji (Direct2D) + kaomoji/CJK (font fallback) render"
            : "[emojitest] FAIL");
        return ok ? 0 : 1;
    }

    // Verify the background renders translucent: at 90% opacity the empty
    // background pixels must have alpha ~= 229 (not 255). DWM composites that
    // alpha through the window (no AllowsTransparency layered window needed).
    private static int OpacityTest()
    {
        const int W = 320, H = 160;
        var term = new Terminal(80, 24);
        var view = new TerminalView(term);
        view.EnsureGrid(W, H);
        view.SetBackgroundOpacity(0.90);
        view.Measure(new System.Windows.Size(W, H));
        view.Arrange(new System.Windows.Rect(0, 0, W, H));

        var rtb = new System.Windows.Media.Imaging.RenderTargetBitmap(W, H, 96, 96, System.Windows.Media.PixelFormats.Pbgra32);
        rtb.Render(view);
        var px = new byte[W * H * 4];
        rtb.CopyPixels(px, W * 4, 0);
        int sx = W - 10, sy = H - 10;                    // empty bg, away from the cursor at (0,0)
        int a = px[(sy * W + sx) * 4 + 3];               // alpha channel
        Console.WriteLine($"[opacitytest] background pixel alpha = {a}/255 (expect ~229 for 90%)");
        bool ok = a is > 215 and < 245;
        Console.WriteLine(ok
            ? "[opacitytest] PASS — background is translucent (desktop shows through via DWM composition)"
            : "[opacitytest] FAIL — background not translucent");
        return ok ? 0 : 1;
    }

    // Measure VT parse + grid-update throughput (no pipe, no render) to locate
    // the bottleneck, mirroring termbench's workloads.
    private static int BenchTest()
    {
        static void Bench(string name, byte[] data, int reps)
        {
            var term = new Terminal(120, 30);
            var sw = System.Diagnostics.Stopwatch.StartNew();
            for (int i = 0; i < reps; i++) term.Feed(data);
            sw.Stop();
            double gb = (double)data.Length * reps / 1e9;
            Console.WriteLine($"[bench] {name,-12} {sw.Elapsed.TotalSeconds,8:0.000}s  {gb * 1000:0}MB  {gb / sw.Elapsed.TotalSeconds:0.0000} GB/s");
        }

        var many = Encoding.ASCII.GetBytes(string.Concat(System.Linq.Enumerable.Repeat("the quick brown fox jumps\r\n", 40000)));
        Bench("ManyLine", many, 80);

        var longl = Encoding.ASCII.GetBytes(new string('x', 1_000_000));
        Bench("LongLine", longl, 200);

        var sb = new StringBuilder();
        for (int i = 0; i < 60000; i++) sb.Append("\x1b[3").Append(i % 8).Append(";4").Append((i + 1) % 8).Append('m').Append('X');
        Bench("FGBGPerChar", Encoding.ASCII.GetBytes(sb.ToString()), 80);

        var utf = Encoding.UTF8.GetBytes(string.Concat(System.Linq.Enumerable.Repeat("ascii 日本語 mixed text here\r\n", 40000)));
        Bench("Utf8Mixed", utf, 60);
        return 0;
    }

    // End-to-end input throughput: flood ~100MB of text from WSL through the real
    // pty -> wslptyd -> mux pipe -> VT parse + grid (no render), like termbench.
    private static int PipeTest(string distro)
    {
        var mux = WslMux.Start(distro, WslBootstrap.ResolveServer());
        var term = new Terminal(120, 30);
        long total = 0;
        using var done = new ManualResetEventSlim();
        var s = mux.Open(120, 30, null);
        s.DataReceived += d => { System.Threading.Interlocked.Add(ref total, d.Length); term.Feed(d); };
        s.Exited += _ => done.Set();

        Thread.Sleep(400);                               // shell startup
        var sw = System.Diagnostics.Stopwatch.StartNew();
        s.SendData(Encoding.UTF8.GetBytes(
            "yes 'the quick brown fox jumps over the lazy dog' | head -n 2000000; exit\n"));
        done.Wait(120000);
        sw.Stop();
        Console.WriteLine($"[pipetest] received {total / 1e6:0}MB in {sw.Elapsed.TotalSeconds:0.000}s = "
            + $"{total / 1e9 / sw.Elapsed.TotalSeconds:0.0000} GB/s (pty + wslptyd + mux + parse, no render)");
        mux.Dispose();
        return 0;
    }

    private static int Gui(string distro, string? startDir)
    {
        // If a host instance is already running, hand it this launch and exit, so
        // all windows share one process + one wslptyd per distro.
        if (SingleInstance.TryForward(distro, startDir)) return 0;

        string server;
        try { server = WslBootstrap.ResolveServer(); }
        catch (Exception ex)
        {
            MessageBox.Show(ex.Message, "WSL Terminal", MessageBoxButton.OK, MessageBoxImage.Error);
            return 3;
        }

        // OnLastWindowClose so Ctrl+Shift+N windows (same process, shared server)
        // keep the app alive until the last one closes.
        var app = new Application { ShutdownMode = ShutdownMode.OnLastWindowClose };

        // Become the host: later launches forward here and we open their window.
        SingleInstance.StartHost((d, cwd) => app.Dispatcher.BeginInvoke(new Action(() =>
        {
            try { new MainWindow(d, server, Settings.Load(), cwd).Show(); }
            catch (Exception ex)
            {
                try
                {
                    System.IO.File.AppendAllText(
                        System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal.log"),
                        $"{DateTime.Now:HH:mm:ss} forward-open failed {ex.Message}{Environment.NewLine}");
                }
                catch { }
            }
        })));
        app.DispatcherUnhandledException += (_, e) =>
        {
            try
            {
                System.IO.File.AppendAllText(
                    System.IO.Path.Combine(System.IO.Path.GetTempPath(), "wslterminal.log"),
                    $"{DateTime.Now:HH:mm:ss} UNHANDLED {e.Exception}{Environment.NewLine}");
            }
            catch { }
            MessageBox.Show(e.Exception.Message, "WSL Terminal error");
            e.Handled = true;
        };

        var win = new MainWindow(distro, server, Settings.Load(), startDir);
        int code = app.Run(win);
        WslMuxManager.DisposeAll();    // closes wsl.exe -> server exits
        return code;
    }
}
