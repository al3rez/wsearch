using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using Microsoft.Win32;

namespace WSearch.Ui;

static class Theme
{
    const int DwmwaUseImmersiveDarkMode = 20;
    const int DwmwaSystemBackdropType = 38;
    const int DwmsbtMainwindow = 2;

    [DllImport("dwmapi.dll")]
    static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int value, int size);

    [DllImport("dwmapi.dll")]
    static extern int DwmExtendFrameIntoClientArea(IntPtr hwnd, ref Margins margins);

    [StructLayout(LayoutKind.Sequential)]
    struct Margins { public int cxLeftWidth, cxRightWidth, cyTopHeight, cyBottomHeight; }

    public static bool IsDark
    {
        get
        {
            try
            {
                var v = Registry.GetValue(@"HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme", 1);
                return v is int i && i == 0;
            }
            catch { return false; }
        }
    }

    public static void Apply(Application app)
    {
        if (IsDark)
        {
            // Win11 dark: SolidBackgroundFillColorBase / Secondary
            Set(app, "Fg", Color.FromRgb(0xFF, 0xFF, 0xFF));
            Set(app, "FgMuted", Color.FromRgb(0xC5, 0xC5, 0xC5));
            Set(app, "Hover", Color.FromArgb(0x15, 0xFF, 0xFF, 0xFF));
            Set(app, "Select", Color.FromRgb(0x4C, 0x4C, 0x4C));
            Set(app, "NavSelect", Color.FromRgb(0x33, 0x33, 0x33));
            Set(app, "Line", Color.FromRgb(0x3F, 0x3F, 0x3F));
            Set(app, "Sidebar", Color.FromRgb(0x19, 0x19, 0x19));
            Set(app, "Content", Color.FromRgb(0x20, 0x20, 0x20));
            Set(app, "Pane", Color.FromRgb(0x2C, 0x2C, 0x2C));
            Set(app, "Accent", Color.FromRgb(0x60, 0xCD, 0xFF));
            Set(app, "ScrollThumb", Color.FromArgb(0x61, 0xFF, 0xFF, 0xFF));
            Set(app, "ScrollThumbHover", Color.FromArgb(0x8A, 0xFF, 0xFF, 0xFF));
        }
        else
        {
            Set(app, "Fg", Color.FromRgb(0x1C, 0x1C, 0x1C));
            Set(app, "FgMuted", Color.FromRgb(0x5A, 0x5A, 0x5A));
            Set(app, "Hover", Color.FromArgb(0x12, 0x00, 0x00, 0x00));
            Set(app, "Select", Color.FromRgb(0xCD, 0xE8, 0xF6));
            Set(app, "NavSelect", Color.FromRgb(0xE6, 0xE6, 0xE6));
            Set(app, "Line", Color.FromRgb(0xE0, 0xE0, 0xE0));
            Set(app, "Sidebar", Color.FromRgb(0xF3, 0xF3, 0xF3));
            Set(app, "Content", Color.FromRgb(0xFF, 0xFF, 0xFF));
            Set(app, "Pane", Color.FromRgb(0xF6, 0xF6, 0xF6));
            Set(app, "Accent", Color.FromRgb(0x00, 0x78, 0xD4));
            Set(app, "ScrollThumb", Color.FromArgb(0x61, 0x00, 0x00, 0x00));
            Set(app, "ScrollThumbHover", Color.FromArgb(0x8A, 0x00, 0x00, 0x00));
        }
    }

    public static void Watch(Application app)
    {
        SystemEvents.UserPreferenceChanged += (_, e) =>
        {
            if (e.Category is not (UserPreferenceCategory.General or UserPreferenceCategory.VisualStyle or UserPreferenceCategory.Color))
                return;
            app.Dispatcher.BeginInvoke(() =>
            {
                Apply(app);
                foreach (Window w in app.Windows)
                    ApplyMica(w);
            });
        };
    }

    static void Set(Application app, string key, Color c)
    {
        var b = new SolidColorBrush(c);
        b.Freeze();
        app.Resources[key] = b;
    }

    public static void ApplyMica(Window window)
    {
        var hwnd = new WindowInteropHelper(window).EnsureHandle();
        var dark = IsDark ? 1 : 0;
        DwmSetWindowAttribute(hwnd, DwmwaUseImmersiveDarkMode, ref dark, 4);
        var mica = DwmsbtMainwindow;
        DwmSetWindowAttribute(hwnd, DwmwaSystemBackdropType, ref mica, 4);
        var m = new Margins { cxLeftWidth = 0, cxRightWidth = 0, cyTopHeight = 1, cyBottomHeight = 0 };
        DwmExtendFrameIntoClientArea(hwnd, ref m);
    }
}
