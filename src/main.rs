use wsearch::{index, live, query};

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

const USAGE: &str = "usage:
  wsearch                        OneCommander-style browser (Mica, dual-pane)
  wsearch <query...> [--json]    search (starts the daemon if needed)
  wsearch stdio                  JSON lines on stdin/stdout
  wsearch serve [--root DIR]     run the daemon in the foreground
  wsearch status
  wsearch install [--startup]    copy to %LOCALAPPDATA%\\wsearch; --startup runs at logon
  wsearch uninstall              remove the logon entry (keeps the index)
  wsearch bench <query...>       time a query in-process against the saved index
  wsearch index [--root DIR]     build the name index in-process and exit
  wsearch gui                    same as launching with no args
  wsearch win32                  classic Win32 listview UI

Queries: fuzzy words, 'exact, ^prefix, suffix$, !exclude
Filters: ext: type: kind: in: drive: size: mtime: re: path: limit: grep: regex: sym:";

fn home() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into())
}

fn data_dir() -> PathBuf {
    let d = wsearch::default_dir();
    std::fs::create_dir_all(&d).ok();
    d
}

fn parse_root(args: &[String]) -> Option<PathBuf> {
    args.windows(2).find(|w| w[0] == "--root").map(|w| PathBuf::from(&w[1])).or_else(|| std::env::var("WSEARCH_ROOT").ok().map(PathBuf::from))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => eprintln!("{USAGE}"),
        None | Some("gui") => wsearch::ui::run(),
        Some("win32") => wsearch::ui::run_win32(),
        Some("serve") => wsearch::serve(data_dir(), home(), parse_root(&args)),
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
        Some("bench") => bench(&args[1..].iter().filter(|a| *a != "--root").cloned().collect::<Vec<_>>().join(" ")),
        Some("index") => build_index(parse_root(&args)),
        Some("install") => install(args.iter().any(|a| a == "--startup")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
            print_one(&serde_json::json!({"q": q.join(" ")}), json);
        }
    }
}

fn print_one(req: &serde_json::Value, raw: bool) {
    let mut s = wsearch::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    writeln!(s, "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).unwrap();
    if raw {
        print!("{line}");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        die(v["error"].as_str().unwrap_or("error"));
    }
    let mut out = std::io::stdout().lock();
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
        }
    }
}

fn stdio() {
    let s = wsearch::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut up = s.try_clone().unwrap();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if writeln!(up, "{line}").is_err() {
                break;
            }
        }
    });
    let mut out = std::io::stdout().lock();
    for line in BufReader::new(s).lines() {
        let Ok(line) = line else { break };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn bench(qs: &str) {
    let idx = index::Index::load(&data_dir().join("index.bin")).unwrap_or_else(|| die("no index yet; run wsearch serve or wsearch index"));
    let live = live::Live::new(idx);
    let q = query::Query::parse(qs, &home()).unwrap_or_else(|e| die(&e));
    let s = query::Searcher { live: &live };
    let mut times = Vec::new();
    let mut hits = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        hits = s.search(&q);
        times.push(t.elapsed());
    }
    let mut p = Vec::new();
    for h in hits.iter().take(10) {
        live.base.path(h.idx as usize, &mut p);
        println!("{:5} {}", h.score, String::from_utf8_lossy(&p));
    }
    times.sort();
    eprintln!("first {:.2?}  median {:.2?}  min {:.2?}", times[0].max(times[times.len() - 1]), times[times.len() / 2], times[0]);
}

fn build_index(root: Option<PathBuf>) {
    let t = Instant::now();
    let eng = wsearch::Engine::start(wsearch::Options { dir: data_dir(), home: home(), root, skip: None }).unwrap_or_else(|e| die(&e));
    loop {
        let s = eng.status();
        if s.ready {
            eprintln!("indexed {} entries in {:.2?} mft={} bytes={}", s.entries, t.elapsed(), s.mft, s.index_bytes);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn install(startup: bool) {
    let dest = data_dir().join("wsearch.exe");
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(std::env::current_exe().unwrap(), &dest).unwrap_or_else(|e| die(&format!("copy: {e}")));
    if let Some(dir) = dest.parent() {
        wsearch::ui::copy_ui_next_to(dir);
    }
    if !startup {
        println!("installed {}; daemon starts on first search", dest.display());
        return;
    }
    set_run_key(Some(&dest));
    println!("installed {} (HKCU Run wsearch)", dest.display());
}

fn uninstall() {
    set_run_key(None);
    println!("removed logon entry; index kept in {}", data_dir().display());
}

fn set_run_key(exe: Option<&std::path::Path>) {
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
    };
    let sub = wsearch::pathwin::to_wide(b"Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let mut hk = std::ptr::null_mut();
    let ok = unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, std::ptr::null(), 0, KEY_SET_VALUE, std::ptr::null(), &mut hk, std::ptr::null_mut())
    };
    if ok != 0 {
        die("registry");
    }
    let value = wsearch::pathwin::to_wide(b"wsearch");
    match exe {
        Some(p) => {
            let cmd = format!("\"{}\" serve", p.display());
            let w = wsearch::pathwin::to_wide(cmd.as_bytes());
            unsafe {
                RegSetValueExW(hk, value.as_ptr(), 0, REG_SZ, w.as_ptr() as *const u8, (w.len() * 2) as u32);
            }
        }
        None => unsafe {
            RegDeleteValueW(hk, value.as_ptr());
        },
    }
    unsafe { RegCloseKey(hk) };
}

fn die(msg: &str) -> ! {
    eprintln!("wsearch: {msg}");
    std::process::exit(1)
}
