//! JSON-lines over a named pipe. `wsearch stdio` and the CLI are thin clients.

use crate::walk::{KIND_DIR, KIND_FILE, KIND_LINK};
use crate::{Engine, GrepMode, Options, Query};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, WriteFile};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};

pub fn pipe_name() -> Vec<u16> {
    r"\\.\pipe\wsearch-ipc".encode_utf16().chain(std::iter::once(0)).collect()
}

fn pipe_security() -> Option<(*mut core::ffi::c_void, windows_sys::Win32::Security::SECURITY_ATTRIBUTES)> {
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let sddl: Vec<u16> = r"D:(A;;GA;;;AU)(A;;GA;;;BA)".encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd = std::ptr::null_mut();
    let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), 1, &mut sd, std::ptr::null_mut()) };
    if ok == 0 {
        return None;
    }
    let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
    Some((sd, sa))
}

pub fn serve(dir: PathBuf, home: String, root: Option<PathBuf>) {
    if !crate::winpriv::is_elevated() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if crate::winpriv::relaunch_elevated(&args) {
            return;
        }
        eprintln!("UAC declined; crawling without $MFT");
    }
    std::fs::create_dir_all(&dir).ok();
    let engine = match Engine::start(Options { dir: dir.clone(), home, root, skip: None }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{} {e}", crate::query::now_secs());
            return;
        }
    };
    let name = pipe_name();
    let sec = pipe_security();
    let sa_ptr = sec.as_ref().map(|(_, sa)| sa as *const _ as *const _).unwrap_or(std::ptr::null());
    loop {
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                sa_ptr,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let err = unsafe { GetLastError() };
            eprintln!("{} CreateNamedPipe failed err={err}", crate::query::now_secs());
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(if err == ERROR_PIPE_BUSY { 50 } else { 500 }));
            continue;
        }
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(h) };
                continue;
            }
        }
        let e = engine.clone();
        let raw = h as isize;
        std::thread::spawn(move || handle(raw as HANDLE, &e));
    }
}

fn handle(h: HANDLE, engine: &Engine) {
    let file = unsafe { std::fs::File::from_raw_handle(h as _) };
    let mut r = BufReader::new(file.try_clone().unwrap());
    let mut w = file;
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let resp = respond(line.trim_end(), engine);
        let mut s = resp.to_string();
        s.push('\n');
        if w.write_all(s.as_bytes()).is_err() {
            break;
        }
        let _ = w.flush();
        let _ = WriteFile;
    }
    unsafe {
        DisconnectNamedPipe(h);
    }
    drop(w);
}

fn respond(line: &str, engine: &Engine) -> Value {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "error": format!("bad json: {e}")}),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let mut out = match run(&v, engine) {
        Ok(r) => r,
        Err(e) => json!({"ok": false, "error": e}),
    };
    out["id"] = id;
    out
}

fn run(v: &Value, engine: &Engine) -> Result<Value, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
    let is_grep = op == "grep"
        || (op == "search"
            && v.get("q").and_then(Value::as_str).is_some_and(|q| ["grep:", "regex:", "sym:", "content:", "symbol:"].iter().any(|k| q.contains(k))));
    match op {
        "ping" => Ok(json!({"ok": true})),
        "save" => {
            engine.save();
            Ok(json!({"ok": true, "scheduled": true}))
        }
        _ if is_grep => grep(v, engine),
        "status" => {
            let s = engine.status();
            if !s.ready {
                return Err("indexing (first run crawls NTFS $MFT, seconds — not a tree walk)".into());
            }
            let mut v = serde_json::to_value(s).map_err(|e| e.to_string())?;
            v["ok"] = true.into();
            Ok(v)
        }
        "search" => {
            let q = parse_request(v, engine.home())?;
            let t = Instant::now();
            let found = engine.search(&q)?;
            let took = t.elapsed().as_micros() as u64;
            let hits: Vec<Value> = found
                .iter()
                .map(|f| {
                    json!({
                        "path": f.path.to_string_lossy(),
                        "kind": kind_name(f.kind),
                        "size": f.size,
                        "mtime": f.mtime,
                        "score": f.score,
                    })
                })
                .collect();
            Ok(json!({"ok": true, "took_us": took, "hits": hits}))
        }
        _ => Err(format!("unknown op {op}")),
    }
}

fn grep(v: &Value, engine: &Engine) -> Result<Value, String> {
    let mut q = parse_request(v, engine.home())?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("regex") => GrepMode::Regex,
        Some("symbol") => GrepMode::Symbol,
        Some("literal") => GrepMode::Literal,
        Some(m) => return Err(format!("unknown mode {m}")),
        None => q.grep_mode,
    };
    let pattern = v.get("pattern").and_then(Value::as_str).map(str::to_string).or_else(|| q.grep.take()).ok_or("grep needs a pattern")?;
    let mut g = crate::Grep::new(&pattern, mode)?;
    if let Some(n) = v.get("per_file").and_then(Value::as_u64) {
        g.max_per_file = n as usize;
    }
    if let Some(ms) = v.get("budget_ms").and_then(Value::as_u64) {
        g.budget = (ms > 0).then(|| Duration::from_millis(ms));
    }
    let t = Instant::now();
    let (r, indexed) = engine.grep(&q, &g)?;
    let files: Vec<Value> = r
        .files
        .iter()
        .map(|f| {
            json!({
                "path": String::from_utf8_lossy(&f.path),
                "matches": f.lines.iter().map(|(n, t)| json!({"line": n, "text": t})).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "took_us": t.elapsed().as_micros() as u64,
        "source": if indexed { "index" } else { "scan" },
        "candidates": r.candidates,
        "read": r.read,
        "complete": r.complete,
        "indexing": engine.status().content_pending,
        "files": files,
    }))
}

fn parse_request(v: &Value, home: &str) -> Result<Query, String> {
    let mut q = Query::parse(v.get("q").and_then(Value::as_str).unwrap_or(""), home)?;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "limit" {
                q.limit = val.as_u64().ok_or("limit must be a number")? as usize;
            } else {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                q.filter(k, &s, home)?;
            }
        }
    }
    Ok(q)
}

fn kind_name(k: u8) -> &'static str {
    match k & 3 {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        _ => "other",
    }
}

pub fn connect(dir: &Path) -> std::io::Result<std::fs::File> {
    let name = pipe_name();
    if let Some(f) = try_connect(&name) {
        return Ok(f);
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log"))?;
    const DETACHED: u32 = 0x00000008;
    const NEW_GROUP: u32 = 0x00000200;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("serve").creation_flags(DETACHED | NEW_GROUP).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    for _ in 0..120 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(f) = try_connect(&name) {
            return Ok(f);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "cannot reach daemon"))
}

fn try_connect(name: &[u16]) -> Option<std::fs::File> {
    for _ in 0..20 {
        let h = unsafe { CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut()) };
        if h != INVALID_HANDLE_VALUE {
            return Some(unsafe { std::fs::File::from_raw_handle(h as _) });
        }
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if err == ERROR_PIPE_BUSY {
            unsafe { WaitNamedPipeW(name.as_ptr(), 200) };
            continue;
        }
        return None;
    }
    None
}

#[allow(dead_code)]
fn _owned(h: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(h as _) }
}
#[allow(dead_code)]
fn _into(h: OwnedHandle) -> HANDLE {
    h.into_raw_handle() as HANDLE
}
