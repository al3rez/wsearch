//! Explorer-native icons, open, recycle, clipboard HDROP.

use crate::pathwin;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{GetLastError, HANDLE, HWND, MAX_PATH};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FO_COPY, FO_DELETE, FO_MOVE, SHFILEOPSTRUCTW, SHFileOperationW,
    SHGFI_SYSICONINDEX, SHGetFileInfoW, SHFILEINFOW, ShellExecuteW,
};

pub const CF_HDROP: u32 = 15;

pub fn file_icon_index(path: &str, is_dir: bool) -> i32 {
    let w = pathwin::to_wide(path.as_bytes());
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let attrs = if is_dir { 0x10u32 } else { 0 };
    let flags = SHGFI_SYSICONINDEX | 0x10; // SHGFI_USEFILEATTRIBUTES
    unsafe { SHGetFileInfoW(w.as_ptr(), attrs, &mut info, size_of::<SHFILEINFOW>() as u32, flags) };
    info.iIcon
}

pub fn open_path(hwnd: HWND, path: &str) {
    let file = pathwin::to_wide(path.as_bytes());
    let op = pathwin::to_wide(b"open");
    unsafe { ShellExecuteW(hwnd, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
}

pub fn reveal(path: &str) {
    let arg = format!("/select,\"{path}\"");
    let explorer = pathwin::to_wide(b"explorer.exe");
    let args = pathwin::to_wide(arg.as_bytes());
    unsafe { ShellExecuteW(0 as HWND, std::ptr::null(), explorer.as_ptr(), args.as_ptr(), std::ptr::null(), 1) };
}

pub fn properties(hwnd: HWND, path: &str) {
    let file = pathwin::to_wide(path.as_bytes());
    let op = pathwin::to_wide(b"properties");
    unsafe { ShellExecuteW(hwnd, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1) };
}

fn double_null_list(paths: &[PathBuf]) -> Vec<u16> {
    let mut w = Vec::new();
    for p in paths {
        w.extend(p.to_string_lossy().encode_utf16());
        w.push(0);
    }
    w.push(0);
    w
}

pub fn file_op(hwnd: HWND, op: u32, from: &[PathBuf], to: Option<&Path>) -> i32 {
    if from.is_empty() {
        return 0;
    }
    let mut from_w = double_null_list(from);
    let mut to_w = to.map(|p| {
        let mut v: Vec<u16> = p.to_string_lossy().encode_utf16().collect();
        v.push(0);
        v.push(0);
        v
    });
    let mut fos: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    fos.hwnd = hwnd;
    fos.wFunc = op;
    fos.pFrom = from_w.as_mut_ptr();
    fos.pTo = to_w.as_mut().map(|v| v.as_mut_ptr()).unwrap_or(std::ptr::null_mut());
    fos.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT) as u16;
    unsafe { SHFileOperationW(&mut fos) }
}

pub fn copy_files(hwnd: HWND, from: &[PathBuf], dest: &Path) -> i32 {
    file_op(hwnd, FO_COPY, from, Some(dest))
}
pub fn move_files(hwnd: HWND, from: &[PathBuf], dest: &Path) -> i32 {
    file_op(hwnd, FO_MOVE, from, Some(dest))
}
pub fn delete_files(hwnd: HWND, from: &[PathBuf]) -> i32 {
    file_op(hwnd, FO_DELETE, from, None)
}

#[repr(C)]
struct Dropfiles {
    p_files: u32,
    pt_x: i32,
    pt_y: i32,
    nc: i32,
    wide: i32,
}

pub fn clipboard_set_files(hwnd: HWND, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let names = double_null_list(paths);
    let header = size_of::<Dropfiles>();
    let bytes = header + names.len() * 2;
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            CloseClipboard();
            return false;
        }
        let df = p as *mut Dropfiles;
        (*df).p_files = header as u32;
        (*df).pt_x = 0;
        (*df).pt_y = 0;
        (*df).nc = 0;
        (*df).wide = 1;
        std::ptr::copy_nonoverlapping(names.as_ptr() as *const u8, p.add(header), names.len() * 2);
        GlobalUnlock(h);
        SetClipboardData(CF_HDROP, h as HANDLE);
        CloseClipboard();
    }
    true
}

pub fn clipboard_get_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    unsafe {
        if OpenClipboard(0 as HWND) == 0 {
            return out;
        }
        let h = GetClipboardData(CF_HDROP);
        if !h.is_null() {
            let p = GlobalLock(h as *mut _) as *const u8;
            if !p.is_null() {
                let df = &*(p as *const Dropfiles);
                if df.wide != 0 {
                    let mut s = p.add(df.p_files as usize) as *const u16;
                    loop {
                        if *s == 0 {
                            break;
                        }
                        let mut n = 0usize;
                        while *s.add(n) != 0 {
                            n += 1;
                        }
                        let slice = std::slice::from_raw_parts(s, n);
                        out.push(PathBuf::from(String::from_utf16_lossy(slice)));
                        s = s.add(n + 1);
                    }
                }
                GlobalUnlock(h as *mut _);
            }
        }
        CloseClipboard();
    }
    let _ = unsafe { GetLastError() };
    out
}

pub fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    unsafe {
        let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
        let hr = windows_sys::Win32::UI::Shell::SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p);
        if hr < 0 || p.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *p.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        CoTaskMemFree(p as _);
        Some(PathBuf::from(s))
    }
}

pub fn drives() -> Vec<PathBuf> {
    let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26).filter(|i| mask & (1 << i) != 0).map(|i| PathBuf::from(format!("{}:\\", (b'A' + i as u8) as char))).collect()
}

pub fn format_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} bytes")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn format_mtime(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    // Local via FileTime would be better; keep ISO-ish local via offset-naive UTC display.
    let s = d.as_secs();
    let days = s / 86400;
    let rem = s % 86400;
    let (y, m, day) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{day:02} {:02}:{:02}", rem / 3600, (rem / 60) % 60)
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

pub fn kind_label(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "File folder".into();
    }
    match Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "exe" => "Application".into(),
        "dll" => "Application extension".into(),
        "pdf" => "PDF".into(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => "Image".into(),
        "mp4" | "mkv" | "avi" | "mov" | "webm" => "Video".into(),
        "mp3" | "wav" | "flac" | "m4a" => "Audio".into(),
        "rs" | "c" | "h" | "cpp" | "py" | "js" | "ts" | "go" | "java" | "cs" => "Source code".into(),
        "txt" | "md" | "log" => "Text document".into(),
        "zip" | "7z" | "rar" | "tar" | "gz" => "Archive".into(),
        "docx" | "doc" => "Microsoft Word".into(),
        "xlsx" | "xls" => "Microsoft Excel".into(),
        "lnk" => "Shortcut".into(),
        ext if ext.is_empty() => "File".into(),
        ext => format!(".{ext} file"),
    }
}

pub fn path_mtime_size(path: &Path) -> (u32, u64, bool, bool) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let is_dir = m.is_dir();
            let hidden = {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    m.file_attributes() & 2 != 0
                }
                #[cfg(not(windows))]
                {
                    false
                }
            };
            let mt = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as u32).unwrap_or(0);
            (mt, m.len(), is_dir, hidden)
        }
        Err(_) => (0, 0, path.is_dir(), false),
    }
}
