using System.Collections.Generic;
using System.Numerics;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Vortice.Direct2D1;
using Vortice.DirectWrite;
using Vortice.DCommon;
using Vortice.Mathematics;
using Vortice.WIC;
using DWriteFactoryType = Vortice.DirectWrite.FactoryType;
using D2DFactoryType = Vortice.Direct2D1.FactoryType;

namespace WslTerminal.Ui;

/// <summary>
/// Rasterizes emoji graphemes to color bitmaps with Direct2D + DirectWrite
/// (the only Windows path that renders color fonts — WPF can't). Results are
/// cached per (grapheme, pixel size) as frozen WPF BitmapSources the terminal
/// surface draws like any image. All calls happen on the UI thread.
/// </summary>
public sealed class EmojiRenderer : IDisposable
{
    private readonly ID2D1Factory _d2d;
    private readonly IDWriteFactory _dw;
    private readonly IWICImagingFactory _wic;
    private readonly Dictionary<string, BitmapSource?> _cache = new();

    public EmojiRenderer()
    {
        _d2d = D2D1.D2D1CreateFactory<ID2D1Factory>(D2DFactoryType.SingleThreaded);
        _dw = DWrite.DWriteCreateFactory<IDWriteFactory>(DWriteFactoryType.Shared);
        _wic = new IWICImagingFactory();
    }

    /// <summary>Returns a cached color bitmap for the grapheme at the given pixel
    /// size, or null if it couldn't be rendered.</summary>
    public BitmapSource? Get(string grapheme, int pxW, int pxH, float emPx)
    {
        string key = $"{grapheme}{pxW}x{pxH}";
        if (_cache.TryGetValue(key, out BitmapSource? cached)) return cached;
        BitmapSource? bmp = null;
        try { bmp = Render(grapheme, pxW, pxH, emPx); }
        catch { bmp = null; }
        _cache[key] = bmp;
        return bmp;
    }

    private BitmapSource Render(string grapheme, int pxW, int pxH, float emPx)
    {
        using IWICBitmap wicBitmap = _wic.CreateBitmap(
            (uint)pxW, (uint)pxH, Vortice.WIC.PixelFormat.Format32bppPBGRA, BitmapCreateCacheOption.CacheOnLoad);

        var rtProps = new RenderTargetProperties(
            new Vortice.DCommon.PixelFormat(Vortice.DXGI.Format.B8G8R8A8_UNorm, Vortice.DCommon.AlphaMode.Premultiplied));
        using ID2D1RenderTarget rt = _d2d.CreateWicBitmapRenderTarget(wicBitmap, rtProps);

        using IDWriteTextFormat fmt = _dw.CreateTextFormat(
            "Segoe UI Emoji", null, FontWeight.Normal, FontStyle.Normal, FontStretch.Normal, emPx, "");
        fmt.TextAlignment = TextAlignment.Center;
        fmt.ParagraphAlignment = ParagraphAlignment.Center;

        using IDWriteTextLayout layout = _dw.CreateTextLayout(grapheme, fmt, pxW, pxH);
        using ID2D1SolidColorBrush brush = rt.CreateSolidColorBrush(new Color4(1f, 1f, 1f, 1f));

        rt.BeginDraw();
        rt.Clear(new Color4(0f, 0f, 0f, 0f));
        rt.DrawTextLayout(Vector2.Zero, layout, brush, DrawTextOptions.EnableColorFont);
        rt.EndDraw();

        int stride = pxW * 4;
        var buffer = new byte[stride * pxH];
        wicBitmap.CopyPixels((uint)stride, buffer);
        var src = BitmapSource.Create(pxW, pxH, 96, 96, PixelFormats.Pbgra32, null, buffer, stride);
        src.Freeze();
        return src;
    }

    public void Dispose()
    {
        _cache.Clear();
        _d2d.Dispose();
        _dw.Dispose();
        _wic.Dispose();
    }
}
