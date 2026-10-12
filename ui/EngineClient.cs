using System.IO;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;

namespace WSearch.Ui;

sealed class Hit
{
    public string Path { get; set; } = "";
    public string Kind { get; set; } = "file";
    public ulong Size { get; set; }
    public uint Mtime { get; set; }
    public int Score { get; set; }
}

sealed class SearchReply
{
    public List<Hit> Hits { get; } = new();
    public ulong TookUs { get; set; }
    public string? Error { get; set; }
    public bool Indexing { get; set; }
    public int Entries { get; set; }
}

static class EngineClient
{
    const string Pipe = "wsearch-ipc";

    static JsonDocument Roundtrip(string json)
    {
        using var pipe = new NamedPipeClientStream(".", Pipe, PipeDirection.InOut, PipeOptions.None);
        pipe.Connect(4000);
        var bytes = Encoding.UTF8.GetBytes(json + "\n");
        pipe.Write(bytes, 0, bytes.Length);
        pipe.Flush();
        using var r = new StreamReader(pipe, Encoding.UTF8, false, 4096, true);
        var line = r.ReadLine() ?? "{}";
        return JsonDocument.Parse(line);
    }

    public static SearchReply Search(string q, int limit = 500)
    {
        try
        {
            using var doc = Roundtrip($"{{\"q\":{JsonSerializer.Serialize(q)},\"limit\":{limit}}}");
            var root = doc.RootElement;
            var reply = new SearchReply
            {
                TookUs = root.TryGetProperty("took_us", out var t) ? t.GetUInt64() : 0,
                Error = root.TryGetProperty("ok", out var ok) && ok.GetBoolean() ? null : (root.TryGetProperty("error", out var er) ? er.GetString() : "error"),
            };
            if (root.TryGetProperty("hits", out var hits) && hits.ValueKind == JsonValueKind.Array)
            {
                foreach (var h in hits.EnumerateArray())
                {
                    reply.Hits.Add(new Hit
                    {
                        Path = h.GetProperty("path").GetString() ?? "",
                        Kind = h.TryGetProperty("kind", out var k) ? k.GetString() ?? "file" : "file",
                        Size = h.TryGetProperty("size", out var s) ? s.GetUInt64() : 0,
                        Mtime = h.TryGetProperty("mtime", out var m) && m.TryGetUInt64(out var mv) ? (uint)mv : 0,
                        Score = h.TryGetProperty("score", out var sc) ? sc.GetInt32() : 0,
                    });
                }
            }
            return reply;
        }
        catch (Exception ex)
        {
            return new SearchReply { Error = ex.Message, Indexing = true };
        }
    }

    public static SearchReply Status()
    {
        try
        {
            using var doc = Roundtrip("{\"op\":\"status\"}");
            var root = doc.RootElement;
            var ok = root.TryGetProperty("ok", out var o) && o.GetBoolean();
            return new SearchReply
            {
                Indexing = !ok,
                Entries = root.TryGetProperty("entries", out var e) ? e.GetInt32() : 0,
                Error = ok ? null : root.TryGetProperty("error", out var er) ? er.GetString() : "indexing",
            };
        }
        catch (Exception ex)
        {
            return new SearchReply { Error = ex.Message, Indexing = true };
        }
    }

    public static void EnsureDaemon()
    {
        try
        {
            using var p = new NamedPipeClientStream(".", Pipe, PipeDirection.InOut);
            p.Connect(200);
            return;
        }
        catch { }

        var exe = FindWsearch();
        if (exe == null) return;
        try
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo
            {
                FileName = exe,
                Arguments = "serve",
                UseShellExecute = true,
                Verb = "runas",
                WindowStyle = System.Diagnostics.ProcessWindowStyle.Hidden,
            });
        }
        catch { }
    }

    static string? FindWsearch()
    {
        var dir = AppContext.BaseDirectory;
        foreach (var p in new[]
        {
            Path.Combine(dir, "wsearch.exe"),
            Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "wsearch", "wsearch.exe"),
            Path.GetFullPath(Path.Combine(dir, @"..\..\..\..\target\release\wsearch.exe")),
            Path.GetFullPath(Path.Combine(dir, @"..\..\..\..\target\debug\wsearch.exe")),
        })
            if (File.Exists(p)) return p;
        return null;
    }
}
