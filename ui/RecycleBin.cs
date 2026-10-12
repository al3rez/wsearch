using System.IO;

namespace WSearch.Ui;

static class RecycleBin
{
    const int SsBitBucket = 10;

    public static List<FileRow> List()
    {
        var rows = new List<FileRow>();
        try
        {
            var t = Type.GetTypeFromProgID("Shell.Application");
            if (t == null) return rows;
            dynamic shell = Activator.CreateInstance(t)!;
            dynamic ns = shell.NameSpace(SsBitBucket);
            if (ns == null) return rows;
            foreach (dynamic item in ns.Items())
            {
                string name;
                string path;
                try { name = (string)item.Name; } catch { continue; }
                try { path = (string)item.Path; } catch { path = ""; }
                if (string.IsNullOrEmpty(name)) continue;
                var folder = false;
                try { folder = (bool)item.IsFolder; } catch { }
                ulong size = 0;
                try { size = Convert.ToUInt64(item.Size); } catch { }
                uint mtime = 0;
                try
                {
                    DateTime dt = item.ModifyDate;
                    mtime = (uint)new DateTimeOffset(DateTime.SpecifyKind(dt, DateTimeKind.Local)).ToUnixTimeSeconds();
                }
                catch { }
                string loc = "";
                try { loc = (string)ns.GetDetailsOf(item, 1); } catch { }
                rows.Add(new FileRow
                {
                    Path = path,
                    Name = name,
                    IsDir = folder,
                    Size = folder ? 0 : size,
                    Mtime = mtime,
                    Kind = folder ? "Folder" : (Path.GetExtension(name).Trim('.') is { Length: > 0 } ext ? ext.ToUpperInvariant() : "Deleted"),
                    Location = loc,
                });
            }
        }
        catch { }
        return rows;
    }
}
