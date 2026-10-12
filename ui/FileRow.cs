using System.Globalization;
using System.IO;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace WSearch.Ui;

sealed class FileRow
{
    public string Path { get; init; } = "";
    public string Name { get; init; } = "";
    public string Kind { get; init; } = "";
    public string Location { get; init; } = "";
    public ulong Size { get; init; }
    public uint Mtime { get; init; }
    public bool IsDir { get; init; }
    public ImageSource? Icon => Icons.Get(Path, IsDir);

    public string SizeText => IsDir ? "" : FormatSize(Size);
    public string DateText => Mtime == 0 ? "" : DateTimeOffset.FromUnixTimeSeconds(Mtime).ToLocalTime().ToString("yyyy-MM-dd HH:mm", CultureInfo.CurrentCulture);

    public static string FormatSize(ulong n)
    {
        if (n < 1024) return $"{n} B";
        if (n < 1024 * 1024) return $"{n / 1024.0:0.0} KB";
        if (n < 1024L * 1024 * 1024) return $"{n / (1024.0 * 1024):0.0} MB";
        return $"{n / (1024.0 * 1024 * 1024):0.0} GB";
    }
}

static class Icons
{
    const uint ShgfiIcon = 0x100, ShgfiSmallIcon = 0x1, ShgfiLargeIcon = 0x0, ShgfiUsefileattributes = 0x10;
    const uint ShgfiSysIconIndex = 0x4000, ShgfiAddOverlays = 0x20;
    const uint FileAttributeDirectory = 0x10;
    const uint ShgsiIcon = 0x100, ShgsiSmallIcon = 0x1;
    const uint IldTransparent = 0x1;
    public const uint SiidFolder = 3, SiidDriveFixed = 8, SiidMyNetwork = 17, SiidRecycler = 31, SiidSoftware = 82, SiidDesktopPc = 94;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct Shfileinfo
    {
        public IntPtr hIcon;
        public int iIcon;
        public uint dwAttributes;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szDisplayName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 80)] public string szTypeName;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct Shstockiconinfo
    {
        public uint cbSize;
        public IntPtr hIcon;
        public int iSysImageIndex;
        public int iIcon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 260)] public string szPath;
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern IntPtr SHGetFileInfo(string pszPath, uint attr, ref Shfileinfo psfi, uint cb, uint flags);

    [DllImport("shell32.dll")]
    static extern int SHGetStockIconInfo(uint siid, uint uFlags, ref Shstockiconinfo psii);

    [DllImport("comctl32.dll")]
    static extern IntPtr ImageList_GetIcon(IntPtr himl, int i, uint flags);

    [DllImport("user32.dll", SetLastError = true)]
    static extern bool DestroyIcon(IntPtr hIcon);

    static readonly Dictionary<string, ImageSource> Cache = new(StringComparer.OrdinalIgnoreCase);

    public static ImageSource? Get(string path, bool dir, bool large = false)
    {
        if (string.IsNullOrEmpty(path)) return dir ? Stock(SiidFolder) : null;
        var info = new Shfileinfo();
        var attr = dir ? FileAttributeDirectory : 0u;
        var flags = ShgfiSysIconIndex | (large ? ShgfiLargeIcon : ShgfiSmallIcon);
        if (!File.Exists(path) && !Directory.Exists(path))
            flags |= ShgfiUsefileattributes;
        var himl = SHGetFileInfo(path, attr, ref info, (uint)Marshal.SizeOf<Shfileinfo>(), flags);
        if (himl == IntPtr.Zero || info.iIcon < 0)
            return dir ? Stock(SiidFolder) : null;
        var key = (large ? "L:" : "S:") + info.iIcon;
        lock (Cache)
        {
            if (Cache.TryGetValue(key, out var hit)) return hit;
        }
        var src = FromHicon(ImageList_GetIcon(himl, info.iIcon, IldTransparent));
        if (src == null && dir) src = Stock(SiidFolder);
        if (src != null) lock (Cache) Cache[key] = src;
        return src;
    }

    public static ImageSource? FromPath(string path)
    {
        var dir = Directory.Exists(path);
        return Get(path, dir) ?? (dir ? Stock(SiidFolder) : Stock(SiidDriveFixed));
    }

    public static ImageSource? Stock(uint siid)
    {
        var key = "s:" + siid;
        lock (Cache)
        {
            if (Cache.TryGetValue(key, out var hit)) return hit;
        }
        var info = new Shstockiconinfo { cbSize = (uint)Marshal.SizeOf<Shstockiconinfo>() };
        if (SHGetStockIconInfo(siid, ShgsiIcon | ShgsiSmallIcon, ref info) != 0 || info.hIcon == IntPtr.Zero)
            return null;
        var src = FromHicon(info.hIcon);
        if (src != null) lock (Cache) Cache[key] = src;
        return src;
    }


    static ImageSource? FromHicon(IntPtr hIcon)
    {
        if (hIcon == IntPtr.Zero) return null;
        try
        {
            var src = Imaging.CreateBitmapSourceFromHIcon(hIcon, Int32Rect.Empty, BitmapSizeOptions.FromEmptyOptions());
            if (src.PixelWidth < 8 || src.PixelHeight < 8) return null;
            if (src.Format != PixelFormats.Bgra32 && src.Format != PixelFormats.Pbgra32)
            {
                var conv = new FormatConvertedBitmap(src, PixelFormats.Bgra32, null, 0);
                conv.Freeze();
                return conv;
            }
            src.Freeze();
            return src;
        }
        catch { return null; }
        finally { DestroyIcon(hIcon); }
    }
}
