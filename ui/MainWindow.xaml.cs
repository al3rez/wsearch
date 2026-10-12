using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;

namespace WSearch.Ui;

public partial class MainWindow : Window
{
    readonly ObservableCollection<Place> _places = new();
    readonly ObservableCollection<FileRow> _a = new();
    readonly ObservableCollection<FileRow> _b = new();
    readonly List<string> _histA = new();
    readonly List<string> _histB = new();
    int _hiA, _hiB;
    string _pathA = "", _pathB = "";
    bool _scopePc = true;
    bool _activeA = true;
    readonly DispatcherTimer _debounce = new() { Interval = TimeSpan.FromMilliseconds(50) };
    readonly DispatcherTimer _statusTimer = new() { Interval = TimeSpan.FromSeconds(2) };
    ulong _took;
    int _indexed;
    bool _indexing;
    int _previewGen, _loadA, _loadB;
    string? _sortA, _sortB;
    bool _ascA = true, _ascB = true;
    readonly DispatcherTimer _previewWait = new() { Interval = TimeSpan.FromMilliseconds(80) };
    FileRow? _previewRow;

    public MainWindow()
    {
        InitializeComponent();
        ListA.ItemsSource = _a;
        ListB.ItemsSource = _b;
        Sidebar.ItemsSource = _places;
        _debounce.Tick += (_, _) => { _debounce.Stop(); RunSearch(); };
        _statusTimer.Tick += (_, _) => RefreshStatus();
        _previewWait.Tick += (_, _) => { _previewWait.Stop(); if (_previewRow != null) RunPreview(_previewRow); };
    }

    void OnLoaded(object sender, RoutedEventArgs e)
    {
        Theme.ApplyMica(this);
        try { BuildPlaces(); } catch { }
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        try { NavigateA(home, true); } catch { }
        try { NavigateB(Path.Combine(home, "Downloads"), true); } catch { }
        EngineClient.EnsureDaemon();
        _statusTimer.Start();
        RefreshStatus();
        SearchBox.Focus();
    }

    void OnStateChanged(object sender, EventArgs e)
    {
        MaxBtn.Content = WindowState == WindowState.Maximized ? "\uE923" : "\uE922";
        MaxBtn.ToolTip = WindowState == WindowState.Maximized ? "Restore" : "Maximize";
    }
    void MinClick(object sender, RoutedEventArgs e) => WindowState = WindowState.Minimized;
    void MaxClick(object sender, RoutedEventArgs e) => WindowState = WindowState == WindowState.Maximized ? WindowState.Normal : WindowState.Maximized;
    void CloseClick(object sender, RoutedEventArgs e) => Close();


    void BuildPlaces()
    {
        _places.Clear();
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        AddPlace("This PC", ":thispc", Icons.Stock(Icons.SiidDesktopPc) ?? Icons.FromPath(@"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"));
        AddPlace("Desktop", Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory));
        AddPlace("Documents", Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments));
        AddPlace("Pictures", Environment.GetFolderPath(Environment.SpecialFolder.MyPictures));
        AddPlace("Videos", Environment.GetFolderPath(Environment.SpecialFolder.MyVideos));
        AddPlace("Music", Environment.GetFolderPath(Environment.SpecialFolder.MyMusic));
        AddPlace("Downloads", Path.Combine(home, "Downloads"));
        AddPlace("Programs", Environment.GetFolderPath(Environment.SpecialFolder.Programs), Icons.Stock(Icons.SiidSoftware));
        AddPlace("Recent Items", ":recents", Icons.FromPath(Environment.GetFolderPath(Environment.SpecialFolder.Recent)) ?? Icons.Stock(Icons.SiidFolder));
        AddPlace("Recycle Bin", ":recycle", Icons.Stock(Icons.SiidRecycler) ?? Icons.FromPath(@"::{645FF040-5081-101B-9F08-00AA002F954E}"));
        foreach (var d in DriveInfo.GetDrives().Where(d => d.IsReady))
            AddPlace(d.Name.TrimEnd('\\'), d.RootDirectory.FullName, Icons.FromPath(d.RootDirectory.FullName) ?? Icons.Stock(Icons.SiidDriveFixed));
        Sidebar.SelectedIndex = 1;
    }

    void AddPlace(string name, string path, ImageSource? icon = null)
    {
        icon ??= string.IsNullOrEmpty(path) || path.StartsWith(':') ? Icons.Stock(Icons.SiidFolder) : Icons.FromPath(path) ?? Icons.Get(path, true);
        _places.Add(new Place(name, path, icon));
    }

    void SidebarChanged(object sender, SelectionChangedEventArgs e)
    {
        if (Sidebar.SelectedItem is not Place p) return;
        if (p.Path == ":recents")
        {
            SearchBox.Text = "mtime:<30d";
            _scopePc = true;
            RunSearch();
            return;
        }
        if (p.Path == ":recycle")
        {
            ShowRecycle(_activeA, true);
            return;
        }
        if (p.Path == ":thispc")
        {
            LoadThisPc(_activeA ? _a : _b);
            if (_activeA) { _pathA = ""; PathA.Text = "This PC"; } else { _pathB = ""; PathB.Text = "This PC"; }
            UpdateStatus();
            return;
        }
        if (_activeA) NavigateA(p.Path, true); else NavigateB(p.Path, true);
    }

    void LoadThisPc(ObservableCollection<FileRow> dest)
    {
        dest.Clear();
        foreach (var d in DriveInfo.GetDrives().Where(x => x.IsReady))
        {
            ulong used = 0, total = 0;
            try { total = (ulong)d.TotalSize; used = total - (ulong)d.TotalFreeSpace; } catch { }
            dest.Add(new FileRow
            {
                Path = d.RootDirectory.FullName,
                Name = string.IsNullOrEmpty(d.VolumeLabel) ? d.Name.TrimEnd('\\') : $"{d.VolumeLabel} ({d.Name.TrimEnd('\\')})",
                IsDir = true,
                Size = total,
                Kind = total == 0 ? "Drive" : $"{FileRow.FormatSize(used)} / {FileRow.FormatSize(total)}",
                Location = d.RootDirectory.FullName,
            });
        }
    }

    void NavigateA(string path, bool push)
    {
        if (!Directory.Exists(path)) return;
        _pathA = path;
        if (push)
        {
            _histA.RemoveRange(_hiA + (_histA.Count > 0 ? 1 : 0), Math.Max(0, _histA.Count - _hiA - (_histA.Count > 0 ? 1 : 0)));
            if (_histA.Count == 0 || _histA[^1] != path) { _histA.Add(path); _hiA = _histA.Count - 1; }
        }
        PathA.Text = path;
        LoadDir(_pathA, true);
    }

    void NavigateB(string path, bool push)
    {
        if (!Directory.Exists(path)) return;
        _pathB = path;
        if (push)
        {
            if (_histB.Count == 0 || _histB[^1] != path) { _histB.Add(path); _hiB = _histB.Count - 1; }
        }
        PathB.Text = path;
        LoadDir(_pathB, false);
    }

    void LoadDir(string path, bool paneA)
    {
        var g = paneA ? ++_loadA : ++_loadB;
        Task.Run(() =>
        {
            var rows = BuildRows(path);
            Dispatcher.Invoke(() =>
            {
                if (paneA ? g != _loadA : g != _loadB) return;
                ApplyRows(paneA, rows);
            });
        });
    }

    void ShowRecycle(bool paneA, bool push)
    {
        if (paneA)
        {
            _pathA = ":recycle";
            PathA.Text = "Recycle Bin";
            if (push && (_histA.Count == 0 || _histA[^1] != ":recycle")) { _histA.Add(":recycle"); _hiA = _histA.Count - 1; }
        }
        else
        {
            _pathB = ":recycle";
            PathB.Text = "Recycle Bin";
            if (push && (_histB.Count == 0 || _histB[^1] != ":recycle")) { _histB.Add(":recycle"); _hiB = _histB.Count - 1; }
        }
        ApplyRows(paneA, RecycleBin.List());
    }

    void ApplyRows(bool paneA, List<FileRow> rows)
    {
        var dest = paneA ? _a : _b;
        var list = paneA ? ListA : ListB;
        list.ItemsSource = null;
        dest.Clear();
        foreach (var r in rows) dest.Add(r);
        list.ItemsSource = dest;
        ApplySort(dest, paneA ? _sortA : _sortB, paneA ? _ascA : _ascB);
        UpdateStatus();
    }


    static List<FileRow> BuildRows(string path)
    {
        var rows = new List<FileRow>(256);
        try
        {
            foreach (var i in new DirectoryInfo(path).EnumerateFileSystemInfos()
                .Where(x => (x.Attributes & FileAttributes.Hidden) == 0 && (x.Attributes & FileAttributes.System) == 0)
                .OrderBy(x => (x.Attributes & FileAttributes.Directory) == 0)
                .ThenBy(x => x.Name, StringComparer.OrdinalIgnoreCase)
                .Take(4000))
            {
                var dir = (i.Attributes & FileAttributes.Directory) != 0;
                rows.Add(new FileRow
                {
                    Path = i.FullName,
                    Name = i.Name,
                    IsDir = dir,
                    Size = dir ? 0 : (ulong)((i as FileInfo)?.Length ?? 0),
                    Mtime = (uint)new DateTimeOffset(i.LastWriteTimeUtc).ToUnixTimeSeconds(),
                    Kind = dir ? "Folder" : (i.Extension.Trim('.') is { Length: > 0 } ext ? ext.ToUpperInvariant() : "File"),
                    Location = path,
                });
            }
        }
        catch { }
        return rows;
    }


    void SearchChanged(object sender, TextChangedEventArgs e)
    {
        SearchHint.Visibility = string.IsNullOrEmpty(SearchBox.Text) ? Visibility.Visible : Visibility.Collapsed;
        _debounce.Stop();
        _debounce.Start();
    }

    void RunSearch()
    {
        var q = SearchBox.Text.Trim();
        if (q.Length == 0)
        {
            NavigateA(_pathA, false);
            return;
        }
        if (!_scopePc)
            q = $"{q} in:{_pathA}";
        var query = q;
        Task.Run(() =>
        {
            var r = EngineClient.Search(query, 400);
            Dispatcher.Invoke(() => ApplyHits(r));
        });
    }

    void ApplyHits(SearchReply r)
    {
        _took = r.TookUs;
        _indexing = r.Indexing;
        if (r.Error != null && r.Hits.Count == 0)
        {
            Status.Text = r.Error;
            return;
        }
        var dest = _activeA ? _a : _b;
        dest.Clear();
        foreach (var h in r.Hits)
        {
            var dir = h.Kind == "dir";
            dest.Add(new FileRow
            {
                Path = h.Path,
                Name = System.IO.Path.GetFileName(h.Path),
                IsDir = dir,
                Size = h.Size,
                Mtime = h.Mtime,
                Kind = dir ? "Folder" : (System.IO.Path.GetExtension(h.Path).Trim('.') is { Length: > 0 } ext ? ext.ToUpperInvariant() : "File"),
                Location = System.IO.Path.GetDirectoryName(h.Path) ?? "",
            });
        }
        if (_activeA) ApplySort(_a, _sortA, _ascA); else ApplySort(_b, _sortB, _ascB);
        UpdateStatus();
    }

    void RefreshStatus()
    {
        Task.Run(() =>
        {
            var s = EngineClient.Status();
            Dispatcher.Invoke(() =>
            {
                _indexing = s.Indexing;
                if (s.Entries > 0) _indexed = s.Entries;
                UpdateStatus();
            });
        });
    }

    void UpdateStatus()
    {
        var n = (_activeA ? _a : _b).Count;
        var s = $"{n} items";
        if (_took > 0 && !string.IsNullOrEmpty(SearchBox.Text))
            s += $"  ·  {_took / 1000.0:0.00} ms";
        if (_indexing) s += "  ·  Indexing";
        else if (_indexed > 0) s += $"  ·  {_indexed:N0} indexed";
        Status.Text = s;
    }

    void OpenRow(FileRow row, bool paneA)
    {
        if (row.IsDir)
        {
            if (row.Path == ":recycle") { ShowRecycle(paneA, true); return; }
            if (Directory.Exists(row.Path))
            {
                if (paneA) NavigateA(row.Path, true); else NavigateB(row.Path, true);
            }
            return;
        }
        try
        {
            Process.Start(new ProcessStartInfo(row.Path)
            {
                UseShellExecute = true,
                Verb = "open",
            });
        }
        catch { }
    }

    void ListADouble(object sender, MouseButtonEventArgs e)
    {
        _activeA = true;
        if (ListA.SelectedItem is FileRow r) OpenRow(r, true);
    }
    void ListBDouble(object sender, MouseButtonEventArgs e)
    {
        _activeA = false;
        if (ListB.SelectedItem is FileRow r) OpenRow(r, false);
    }
    void ListASelect(object sender, SelectionChangedEventArgs e)
    {
        _activeA = true;
        if (ListA.SelectedItem is FileRow r) ShowPreview(r);
    }
    void ListBSelect(object sender, SelectionChangedEventArgs e)
    {
        _activeA = false;
        if (ListB.SelectedItem is FileRow r) ShowPreview(r);
    }

    void ShowPreview(FileRow r)
    {
        _previewRow = r;
        PreviewMeta.Text = $"{r.Name}\n{r.Kind}  {r.SizeText}\n{r.DateText}\n{r.Path}";
        PreviewImage.Source = null;
        PreviewImage.Visibility = Visibility.Collapsed;
        PreviewText.Text = "";
        PreviewText.Visibility = Visibility.Collapsed;
        PreviewList.ItemsSource = null;
        PreviewList.Visibility = Visibility.Collapsed;
        _previewWait.Stop();
        _previewWait.Start();
    }

    void RunPreview(FileRow r)
    {
        var gen = ++_previewGen;
        var path = r.Path;
        var ext = System.IO.Path.GetExtension(path);
        var isDir = r.IsDir;
        Task.Run(() =>
        {
            ImageSource? img = null;
            string? text = null;
            List<PreviewItem>? list = null;
            try
            {
                if (isDir)
                    list = Previewer.FolderList(path);
                else if (!File.Exists(path)) { }
                else if (Previewer.IsZip(ext))
                    list = Previewer.ZipList(path);
                else if (Previewer.IsImage(ext))
                    img = Previewer.Bitmap(path, 480) ?? Previewer.ShellThumb(path, 384);
                else if (Previewer.IsText(ext))
                    text = Previewer.TextHead(path);
                else
                {
                    img = Previewer.ShellThumb(path, 384);
                    if (img == null) text = Previewer.TextHead(path);
                }
            }
            catch { }
            Dispatcher.Invoke(() =>
            {
                if (gen != _previewGen) return;
                if (img != null) { PreviewImage.Source = img; PreviewImage.Visibility = Visibility.Visible; }
                if (text != null) { PreviewText.Text = text; PreviewText.Visibility = Visibility.Visible; }
                if (list is { Count: > 0 }) { PreviewList.ItemsSource = list; PreviewList.Visibility = Visibility.Visible; }
            });
        });
    }

    void BackClick(object sender, RoutedEventArgs e)
    {
        if (_activeA && _hiA > 0) { _hiA--; Go(_histA[_hiA], true, false); }
        else if (!_activeA && _hiB > 0) { _hiB--; Go(_histB[_hiB], false, false); }
    }
    void FwdClick(object sender, RoutedEventArgs e)
    {
        if (_activeA && _hiA + 1 < _histA.Count) { _hiA++; Go(_histA[_hiA], true, false); }
        else if (!_activeA && _hiB + 1 < _histB.Count) { _hiB++; Go(_histB[_hiB], false, false); }
    }
    void UpAClick(object sender, RoutedEventArgs e) => UpPane(true);
    void UpBClick(object sender, RoutedEventArgs e) => UpPane(false);

    void Go(string path, bool paneA, bool push)
    {
        if (path == ":recycle") { ShowRecycle(paneA, push); return; }
        if (path is ":thispc" or "") { ShowThisPc(paneA); return; }
        if (paneA) NavigateA(path, push); else NavigateB(path, push);
    }

    void ShowThisPc(bool paneA)
    {
        if (paneA) { _pathA = ":thispc"; PathA.Text = "This PC"; LoadThisPc(_a); }
        else { _pathB = ":thispc"; PathB.Text = "This PC"; LoadThisPc(_b); }
        UpdateStatus();
    }

    void UpPane(bool paneA)
    {
        var cur = paneA ? _pathA : _pathB;
        if (cur is ":recycle" or ":thispc" or "") { ShowThisPc(paneA); return; }
        var p = Directory.GetParent(cur);
        if (p != null) { if (paneA) NavigateA(p.FullName, true); else NavigateB(p.FullName, true); }
        else ShowThisPc(paneA);
    }


    void SortAClick(object sender, RoutedEventArgs e) => SortPane(_a, ref _sortA, ref _ascA, e);
    void SortBClick(object sender, RoutedEventArgs e) => SortPane(_b, ref _sortB, ref _ascB, e);

    static void SortPane(ObservableCollection<FileRow> dest, ref string? col, ref bool asc, RoutedEventArgs e)
    {
        if (e.OriginalSource is not GridViewColumnHeader h) return;
        var key = h.Column?.Header as string;
        if (string.IsNullOrEmpty(key)) return;
        if (col == key) asc = !asc; else { col = key; asc = true; }
        ApplySort(dest, col, asc);
    }

    static void ApplySort(ObservableCollection<FileRow> dest, string? col, bool asc)
    {
        if (string.IsNullOrEmpty(col) || dest.Count == 0) return;
        IOrderedEnumerable<FileRow> q = dest.OrderBy(x => x.IsDir ? 0 : 1);
        q = (col, asc) switch
        {
            ("Name", true) => q.ThenBy(x => x.Name, StringComparer.OrdinalIgnoreCase),
            ("Name", false) => q.ThenByDescending(x => x.Name, StringComparer.OrdinalIgnoreCase),
            ("Date", true) => q.ThenBy(x => x.Mtime),
            ("Date", false) => q.ThenByDescending(x => x.Mtime),
            ("Size", true) => q.ThenBy(x => x.Size),
            ("Size", false) => q.ThenByDescending(x => x.Size),
            ("Type", true) => q.ThenBy(x => x.Kind, StringComparer.OrdinalIgnoreCase),
            ("Type", false) => q.ThenByDescending(x => x.Kind, StringComparer.OrdinalIgnoreCase),
            ("Location", true) => q.ThenBy(x => x.Location, StringComparer.OrdinalIgnoreCase),
            ("Location", false) => q.ThenByDescending(x => x.Location, StringComparer.OrdinalIgnoreCase),
            _ => q.ThenBy(x => x.Name, StringComparer.OrdinalIgnoreCase),
        };
        var list = q.ToList();
        dest.Clear();
        foreach (var x in list) dest.Add(x);
    }

    void ListARight(object sender, MouseButtonEventArgs e) => ShowShellMenu(ListA, _pathA, e);
    void ListBRight(object sender, MouseButtonEventArgs e) => ShowShellMenu(ListB, _pathB, e);

    void ShowShellMenu(ListView list, string folder, MouseButtonEventArgs e)
    {
        _activeA = list == ListA;
        DependencyObject? hit = e.OriginalSource as DependencyObject;
        while (hit != null && hit is not ListViewItem)
            hit = VisualTreeHelper.GetParent(hit);
        if (hit is ListViewItem lvi && lvi.DataContext is FileRow row && !list.SelectedItems.Contains(row))
            list.SelectedItem = row;

        var paths = list.SelectedItems.OfType<FileRow>().Select(r => r.Path).ToList();
        var background = paths.Count == 0;
        if (background)
        {
            if (folder == ":recycle")
                paths.Add(@"::{645FF040-5081-101B-9F08-00AA002F954E}");
            else if (string.IsNullOrEmpty(folder) || !Directory.Exists(folder)) return;
            else paths.Add(folder);
        }
        e.Handled = true;
        var pt = PointToScreen(e.GetPosition(this));
        ShellMenu.Show(new WindowInteropHelper(this).Handle, paths, (int)pt.X, (int)pt.Y, background);
    }


    void OnPreviewKeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.F && Keyboard.Modifiers == ModifierKeys.Control) { SearchBox.Focus(); e.Handled = true; }
        if (e.Key == Key.Escape) { SearchBox.Clear(); Go(_pathA, true, false); e.Handled = true; }
        if (e.Key == Key.Enter)
        {
            var list = _activeA ? ListA : ListB;
            if (list.SelectedItem is FileRow r) OpenRow(r, _activeA);
            e.Handled = true;
        }
        if (e.Key == Key.Back && Keyboard.Modifiers == ModifierKeys.Alt) { BackClick(sender, e); e.Handled = true; }
        if (e.Key == Key.Up && Keyboard.Modifiers == ModifierKeys.Alt)
        {
            if (_activeA) UpAClick(sender, e); else UpBClick(sender, e);
            e.Handled = true;
        }
        if (e.Key == Key.F5) { Go(_activeA ? _pathA : _pathB, _activeA, false); e.Handled = true; }
        if (e.Key == Key.Delete)
        {
            var list = _activeA ? ListA : ListB;
            foreach (var r in list.SelectedItems.OfType<FileRow>().ToList())
            {
                try
                {
                    if (r.IsDir) Directory.Delete(r.Path, true);
                    else File.Delete(r.Path);
                }
                catch { }
            }
            Go(_activeA ? _pathA : _pathB, _activeA, false);
            e.Handled = true;
        }
    }
}

sealed class Place
{
    public string Name { get; }
    public string Path { get; }
    public ImageSource? Icon { get; }
    public Place(string name, string path, ImageSource? icon) { Name = name; Path = path; Icon = icon; }
}
