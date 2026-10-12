using System.IO;
using System.IO.Compression;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Media.Imaging;

namespace WSearch.Ui;

sealed class PreviewItem
{
    public string Name { get; init; } = "";
    public string Detail { get; init; } = "";
    public ImageSource? Icon { get; init; }
}

static class Previewer
{
    static readonly HashSet<string> Images = new(StringComparer.OrdinalIgnoreCase)
    {
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".ico", ".jfif", ".tif", ".tiff", ".webp", ".heic", ".heif", ".avif",
    };
    static readonly HashSet<string> Text = new(StringComparer.OrdinalIgnoreCase)
    {
        ".txt", ".md", ".log", ".ini", ".cfg", ".csv", ".json", ".xml", ".yml", ".yaml", ".toml",
        ".cs", ".rs", ".py", ".js", ".ts", ".tsx", ".jsx", ".css", ".html", ".htm", ".svg",
        ".c", ".h", ".cpp", ".hpp", ".cc", ".go", ".java", ".kt", ".ps1", ".psm1", ".bat", ".cmd",
        ".sh", ".sql", ".gitignore", ".editorconfig", ".properties", ".conf", ".env",
    };
    static readonly HashSet<string> ZipLike = new(StringComparer.OrdinalIgnoreCase)
    {
        ".zip", ".cbz", ".jar", ".nupkg", ".apk", ".whl", ".docx", ".xlsx", ".pptx",
    };

    public static bool IsImage(string ext) => Images.Contains(ext);
    public static bool IsText(string ext) => Text.Contains(ext);
    public static bool IsZip(string ext) => ZipLike.Contains(ext);

    public static ImageSource? Bitmap(string path, int decodeW)
    {
        try
        {
            var bi = new BitmapImage();
            using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
            bi.BeginInit();
            bi.CacheOption = BitmapCacheOption.OnLoad;
            bi.DecodePixelWidth = decodeW;
            bi.StreamSource = fs;
            bi.EndInit();
            bi.Freeze();
            return bi;
        }
        catch { return null; }
    }

    public static ImageSource? ShellThumb(string path, int px)
    {
        try
        {
            var iid = typeof(IShellItemImageFactory).GUID;
            var hr = SHCreateItemFromParsingName(path, IntPtr.Zero, ref iid, out var factory);
            if (hr != 0 || factory == null) return null;
            var sz = new SIZE { cx = px, cy = px };
            const int bigger = 0x1, thumbOnly = 0x8;
            hr = factory.GetImage(sz, bigger | thumbOnly, out var hbm);
            if (hr != 0 || hbm == IntPtr.Zero)
                hr = factory.GetImage(sz, bigger, out hbm);
            if (hr != 0 || hbm == IntPtr.Zero) return null;
            try
            {
                var src = Imaging.CreateBitmapSourceFromHBitmap(hbm, IntPtr.Zero, Int32Rect.Empty, BitmapSizeOptions.FromEmptyOptions());
                src.Freeze();
                return src;
            }
            finally { DeleteObject(hbm); }
        }
        catch { return null; }
    }

    public static string? TextHead(string path, int maxChars = 8000)
    {
        try
        {
            using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
            if (fs.Length == 0) return "(empty)";
            var n = (int)Math.Min(fs.Length, maxChars);
            var buf = new byte[n];
            var got = fs.Read(buf, 0, n);
            if (got >= 3 && buf[0] == 0xEF && buf[1] == 0xBB && buf[2] == 0xBF)
                return System.Text.Encoding.UTF8.GetString(buf, 3, got - 3);
            if (LooksBinary(buf, got)) return null;
            return System.Text.Encoding.UTF8.GetString(buf, 0, got);
        }
        catch { return null; }
    }

    static bool LooksBinary(byte[] b, int n)
    {
        var zeros = 0;
        for (var i = 0; i < n; i++)
            if (b[i] == 0) zeros++;
        return zeros > n / 20;
    }

    public static List<PreviewItem> ZipList(string path, int limit = 60)
    {
        var items = new List<PreviewItem>();
        try
        {
            using var z = ZipFile.OpenRead(path);
            foreach (var e in z.Entries.Take(limit))
            {
                var dir = e.FullName.EndsWith('/');
                items.Add(new PreviewItem
                {
                    Name = e.FullName.TrimEnd('/'),
                    Detail = dir ? "folder" : FileRow.FormatSize((ulong)e.Length),
                    Icon = Icons.Get(e.Name, dir),
                });
            }
            if (z.Entries.Count > limit)
                items.Add(new PreviewItem { Name = $"… {z.Entries.Count - limit} more", Detail = "" });
        }
        catch (Exception ex)
        {
            items.Add(new PreviewItem { Name = ex.Message, Detail = "" });
        }
        return items;
    }

    public static List<PreviewItem> FolderList(string path, int limit = 40)
    {
        var items = new List<PreviewItem>();
        try
        {
            var di = new DirectoryInfo(path);
            var kids = di.EnumerateFileSystemInfos()
                .Where(i => (i.Attributes & FileAttributes.Hidden) == 0)
                .OrderBy(i => (i.Attributes & FileAttributes.Directory) == 0)
                .ThenBy(i => i.Name, StringComparer.OrdinalIgnoreCase)
                .Take(limit + 1)
                .ToList();
            foreach (var i in kids.Take(limit))
            {
                var dir = (i.Attributes & FileAttributes.Directory) != 0;
                var size = dir ? 0UL : (ulong)((i as FileInfo)?.Length ?? 0);
                items.Add(new PreviewItem
                {
                    Name = i.Name,
                    Detail = dir ? "folder" : FileRow.FormatSize(size),
                    Icon = Icons.Get(i.FullName, dir),
                });
            }
            if (kids.Count > limit)
                items.Add(new PreviewItem { Name = "…", Detail = "" });
        }
        catch (Exception ex)
        {
            items.Add(new PreviewItem { Name = ex.Message, Detail = "" });
        }
        return items;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct SIZE { public int cx, cy; }

    [ComImport]
    [Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellItemImageFactory
    {
        [PreserveSig]
        int GetImage(SIZE size, int flags, out IntPtr phbm);
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHCreateItemFromParsingName(string pszPath, IntPtr pbc, ref Guid riid, out IShellItemImageFactory ppv);

    [DllImport("gdi32.dll")]
    static extern bool DeleteObject(IntPtr ho);
}
