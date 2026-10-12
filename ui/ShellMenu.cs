using System.IO;
using System.Runtime.InteropServices;
using System.Windows.Interop;

namespace WSearch.Ui;

static class ShellMenu
{
    const uint CmfExplore = 0x4, CmfCanRename = 0x10, CmfItemMenu = 0x80, CmfExtendedVerbs = 0x100;
    const uint TpmReturnCmd = 0x100, TpmRightButton = 0x2;
    const uint CmicMaskUnicode = 0x4000, CmicMaskPtInvoke = 0x20000000;
    const int SwShownormal = 1;
    const int WmInitMenuPopup = 0x0117, WmDrawItem = 0x002B, WmMeasureItem = 0x002C, WmMenuChar = 0x0120;

    static readonly Guid IidShellFolder = new("000214E6-0000-0000-C000-000000000046");
    static readonly Guid IidContextMenu = new("000214E4-0000-0000-C000-000000000046");
    public static void Show(IntPtr hwnd, IReadOnlyList<string> paths, int screenX, int screenY, bool background = false)
    {
        if (paths.Count == 0) return;
        var existing = paths.Where(p => File.Exists(p) || Directory.Exists(p)).ToList();
        if (existing.Count == 0) return;

        IContextMenu? cm = null;
        IntPtr pidlAbs = IntPtr.Zero;
        var childPidls = new List<IntPtr>();
        try
        {
            if (background && existing.Count == 1 && Directory.Exists(existing[0]))
                cm = MenuForFolder(existing[0], hwnd, out pidlAbs);
            else
                cm = MenuForItems(existing, hwnd, out pidlAbs, childPidls);
            if (cm == null) return;
            Popup(cm, hwnd, screenX, screenY);
        }
        finally
        {
            if (pidlAbs != IntPtr.Zero) CoTaskMemFree(pidlAbs);
            if (cm != null) Marshal.ReleaseComObject(cm);
        }
    }

    static IContextMenu? MenuForItems(List<string> paths, IntPtr hwnd, out IntPtr pidlAbs, List<IntPtr> children)
    {
        pidlAbs = IntPtr.Zero;
        var dir = Path.GetDirectoryName(paths[0]);
        if (string.IsNullOrEmpty(dir)) return null;
        var same = paths.Where(p => string.Equals(Path.GetDirectoryName(p), dir, StringComparison.OrdinalIgnoreCase)).ToList();
        if (same.Count == 0) return null;

        if (SHParseDisplayName(same[0], IntPtr.Zero, out pidlAbs, 0, out _) != 0 || pidlAbs == IntPtr.Zero)
            return null;
        var iid = IidShellFolder;
        if (SHBindToParent(pidlAbs, ref iid, out var folder, out var firstChild) != 0 || folder == null)
            return null;
        children.Add(firstChild);
        var apidl = new IntPtr[same.Count];
        apidl[0] = firstChild;
        var extra = new List<IntPtr>();
        try
        {
            for (var i = 1; i < same.Count; i++)
            {
                if (SHParseDisplayName(same[i], IntPtr.Zero, out var abs, 0, out _) != 0) continue;
                extra.Add(abs);
                SHBindToParent(abs, ref iid, out _, out var child);
                apidl[i] = child;
            }
            var n = (uint)apidl.Count(p => p != IntPtr.Zero);
            if (n == 0) return null;
            var packed = apidl.Where(p => p != IntPtr.Zero).ToArray();
            var iidCm = IidContextMenu;
            folder.GetUIObjectOf(hwnd, (uint)packed.Length, packed, ref iidCm, IntPtr.Zero, out var cm);
            return cm;
        }
        finally
        {
            foreach (var p in extra) if (p != IntPtr.Zero) CoTaskMemFree(p);
            Marshal.ReleaseComObject(folder);
        }
    }

    static IContextMenu? MenuForFolder(string path, IntPtr hwnd, out IntPtr pidlAbs)
    {
        pidlAbs = IntPtr.Zero;
        if (SHGetDesktopFolder(out var desktop) != 0 || desktop == null) return null;
        try
        {
            uint eaten = 0, attr = 0;
            desktop.ParseDisplayName(hwnd, IntPtr.Zero, path, out eaten, out pidlAbs, ref attr);
            if (pidlAbs == IntPtr.Zero) return null;
            var iid = IidShellFolder;
            desktop.BindToObject(pidlAbs, IntPtr.Zero, ref iid, out var folderPtr);
            if (folderPtr == IntPtr.Zero) return null;
            var folder = (IShellFolder)Marshal.GetObjectForIUnknown(folderPtr);
            Marshal.Release(folderPtr);
            try
            {
                var iidCm = IidContextMenu;
                folder.CreateViewObject(hwnd, ref iidCm, out var cmPtr);
                if (cmPtr == IntPtr.Zero) return null;
                var cm = (IContextMenu)Marshal.GetObjectForIUnknown(cmPtr);
                Marshal.Release(cmPtr);
                return cm;
            }
            finally { Marshal.ReleaseComObject(folder); }
        }
        finally { Marshal.ReleaseComObject(desktop); }
    }

    static void Popup(IContextMenu cm, IntPtr hwnd, int x, int y)
    {
        var hmenu = CreatePopupMenu();
        if (hmenu == IntPtr.Zero) return;
        var cm2 = cm as IContextMenu2;
        var cm3 = cm as IContextMenu3;
        HwndSourceHook? hook = null;
        HwndSource? src = null;
        try
        {
            cm.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CmfExplore | CmfCanRename | CmfItemMenu | CmfExtendedVerbs);
            src = HwndSource.FromHwnd(hwnd);
            if (src != null)
            {
                IntPtr Hook(IntPtr _, int msg, IntPtr w, IntPtr l, ref bool handled)
                {
                    if (msg is not (WmInitMenuPopup or WmDrawItem or WmMeasureItem or WmMenuChar))
                        return IntPtr.Zero;
                    if (cm3 != null)
                    {
                        cm3.HandleMenuMsg2((uint)msg, w, l, out var lr);
                        handled = true;
                        return lr;
                    }
                    if (cm2 != null)
                    {
                        cm2.HandleMenuMsg((uint)msg, w, l);
                        handled = true;
                    }
                    return IntPtr.Zero;
                }
                hook = Hook;
                src.AddHook(hook);
            }
            var cmd = TrackPopupMenuEx(hmenu, TpmReturnCmd | TpmRightButton, x, y, hwnd, IntPtr.Zero);
            if (cmd == 0) return;
            var info = new Cminvokecommandinfoex
            {
                cbSize = (uint)Marshal.SizeOf<Cminvokecommandinfoex>(),
                fMask = CmicMaskUnicode | CmicMaskPtInvoke,
                hwnd = hwnd,
                lpVerb = (IntPtr)(cmd - 1),
                lpVerbW = (IntPtr)(cmd - 1),
                nShow = SwShownormal,
                ptInvoke = new Point { x = x, y = y },
            };
            var p = Marshal.AllocHGlobal(Marshal.SizeOf(info));
            try
            {
                Marshal.StructureToPtr(info, p, false);
                cm.InvokeCommand(p);
            }
            finally { Marshal.FreeHGlobal(p); }
        }
        finally
        {
            if (src != null && hook != null) src.RemoveHook(hook);
            DestroyMenu(hmenu);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    struct Point { public int x, y; }

    [StructLayout(LayoutKind.Sequential)]
    struct Cminvokecommandinfoex
    {
        public uint cbSize, fMask;
        public IntPtr hwnd, lpVerb, lpParameters, lpDirectory;
        public int nShow;
        public uint dwHotKey;
        public IntPtr hIcon, lpTitle, lpVerbW, lpParametersW, lpDirectoryW, lpTitleW;
        public Point ptInvoke;
    }

    [ComImport]
    [Guid("000214E6-0000-0000-C000-000000000046")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IShellFolder
    {
        void ParseDisplayName(IntPtr hwnd, IntPtr pbc, [MarshalAs(UnmanagedType.LPWStr)] string pszDisplayName, out uint pchEaten, out IntPtr ppidl, ref uint pdwAttributes);
        void EnumObjects(IntPtr hwnd, int grfFlags, out IntPtr ppenumIDList);
        void BindToObject(IntPtr pidl, IntPtr pbc, ref Guid riid, out IntPtr ppv);
        void BindToStorage(IntPtr pidl, IntPtr pbc, ref Guid riid, out IntPtr ppv);
        [PreserveSig] int CompareIDs(IntPtr lParam, IntPtr pidl1, IntPtr pidl2);
        void CreateViewObject(IntPtr hwndOwner, ref Guid riid, out IntPtr ppv);
        void GetAttributesOf(uint cidl, [MarshalAs(UnmanagedType.LPArray)] IntPtr[] apidl, ref uint rgfInOut);
        void GetUIObjectOf(IntPtr hwndOwner, uint cidl, [MarshalAs(UnmanagedType.LPArray)] IntPtr[] apidl, ref Guid riid, IntPtr rgfReserved, [MarshalAs(UnmanagedType.Interface)] out IContextMenu ppv);
        void GetDisplayNameOf(IntPtr pidl, uint uFlags, IntPtr pName);
        void SetNameOf(IntPtr hwnd, IntPtr pidl, [MarshalAs(UnmanagedType.LPWStr)] string pszName, uint uFlags, out IntPtr ppidlOut);
    }

    [ComImport]
    [Guid("000214E4-0000-0000-C000-000000000046")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IContextMenu
    {
        [PreserveSig] int QueryContextMenu(IntPtr hmenu, uint indexMenu, uint idCmdFirst, uint idCmdLast, uint uFlags);
        [PreserveSig] int InvokeCommand(IntPtr pici);
        [PreserveSig] int GetCommandString(UIntPtr idCmd, uint uType, IntPtr pReserved, IntPtr pszName, uint cchMax);
    }

    [ComImport]
    [Guid("000214F4-0000-0000-C000-000000000046")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IContextMenu2
    {
        [PreserveSig] int QueryContextMenu(IntPtr hmenu, uint indexMenu, uint idCmdFirst, uint idCmdLast, uint uFlags);
        [PreserveSig] int InvokeCommand(IntPtr pici);
        [PreserveSig] int GetCommandString(UIntPtr idCmd, uint uType, IntPtr pReserved, IntPtr pszName, uint cchMax);
        [PreserveSig] int HandleMenuMsg(uint uMsg, IntPtr wParam, IntPtr lParam);
    }

    [ComImport]
    [Guid("BCFCE0A0-EC17-11D0-8D10-00A0C90F2719")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IContextMenu3
    {
        [PreserveSig] int QueryContextMenu(IntPtr hmenu, uint indexMenu, uint idCmdFirst, uint idCmdLast, uint uFlags);
        [PreserveSig] int InvokeCommand(IntPtr pici);
        [PreserveSig] int GetCommandString(UIntPtr idCmd, uint uType, IntPtr pReserved, IntPtr pszName, uint cchMax);
        [PreserveSig] int HandleMenuMsg(uint uMsg, IntPtr wParam, IntPtr lParam);
        [PreserveSig] int HandleMenuMsg2(uint uMsg, IntPtr wParam, IntPtr lParam, out IntPtr plResult);
    }

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    static extern int SHParseDisplayName(string name, IntPtr bindCtx, out IntPtr pidl, uint sfgaoIn, out uint sfgaoOut);

    [DllImport("shell32.dll")]
    static extern int SHBindToParent(IntPtr pidl, ref Guid riid, out IShellFolder ppv, out IntPtr ppidlLast);

    [DllImport("shell32.dll")]
    static extern int SHGetDesktopFolder(out IShellFolder ppshf);

    [DllImport("ole32.dll")]
    static extern void CoTaskMemFree(IntPtr pv);

    [DllImport("user32.dll")]
    static extern IntPtr CreatePopupMenu();

    [DllImport("user32.dll")]
    static extern bool DestroyMenu(IntPtr hMenu);

    [DllImport("user32.dll")]
    static extern uint TrackPopupMenuEx(IntPtr hmenu, uint flags, int x, int y, IntPtr hwnd, IntPtr lptpm);
}
