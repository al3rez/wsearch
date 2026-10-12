//! Native Win32 file browser with wsearch in the toolbar (FinderSearch for Windows).

mod search;
mod shell;

use crate::pathwin;
use parking_lot::Mutex;
use search::{Hit, SearchResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontW, FW_NORMAL, GetSysColorBrush, UpdateWindow};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, ICC_TREEVIEW_CLASSES,
    INITCOMMONCONTROLSEX, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_SETIMAGELIST, LVM_SETITEMCOUNT, LVN_COLUMNCLICK,
    LVN_GETDISPINFOW, LVN_ITEMACTIVATE, LVIF_TEXT, LVNI_SELECTED, LVS_EDITLABELS, LVS_ICON, LVS_OWNERDATA, LVS_REPORT,
    LVS_SHAREIMAGELISTS, LVS_SHOWSELALWAYS, LVSIL_NORMAL, LVSIL_SMALL, NMLISTVIEW, NMLVDISPINFOW, NMHDR, TVM_INSERTITEMW,
    TVIF_PARAM, TVIF_TEXT, TVITEMW, TVN_SELCHANGEDW, NMTREEVIEWW,
};
use windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, SetFocus};
use windows_sys::Win32::UI::Shell::{SHGetImageList, SHIL_LARGE, SHIL_SMALL};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, KillTimer, LoadCursorW, MoveWindow, PostQuitMessage,
    RegisterClassExW, SendMessageW, SetTimer, SetWindowLongPtrW, ShowWindow, TrackPopupMenu, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, HMENU, IDC_ARROW, MF_SEPARATOR, MF_STRING, MINMAXINFO, MSG,
    SW_SHOW, TPM_RETURNCMD, WM_COMMAND, WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_GETMINMAXINFO, WM_NOTIFY, WM_SETFONT,
    WM_SIZE, WM_TIMER, WNDCLASSEXW, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const WM_APP: u32 = 0x8000;
const WM_APP_SEARCH: u32 = WM_APP + 1;
const WM_APP_STATUS: u32 = WM_APP + 2;
const TIMER_SEARCH: usize = 1;
const TIMER_STATUS: usize = 2;

const IDC_LIST: i32 = 100;
const IDC_TREE: i32 = 101;
const IDC_SEARCH: i32 = 102;
const IDC_STATUS: i32 = 103;
const IDC_PATH: i32 = 104;
const IDC_BACK: i32 = 110;
const IDC_FWD: i32 = 111;
const IDC_UP: i32 = 112;
const IDC_ICONS: i32 = 113;
const IDC_LISTBTN: i32 = 114;
const IDC_SCOPE_PC: i32 = 115;
const IDC_SCOPE_FOLDER: i32 = 116;

const IDM_OPEN: usize = 200;
const IDM_REVEAL: usize = 201;
const IDM_CUT: usize = 202;
const IDM_COPY: usize = 203;
const IDM_PASTE: usize = 204;
const IDM_DELETE: usize = 205;
const IDM_RENAME: usize = 206;
const IDM_PROPS: usize = 207;
const IDM_NEWFOLDER: usize = 208;
const IDM_REFRESH: usize = 209;
const IDM_COPYPATH: usize = 210;

const LVM_FIRST: u32 = 0x1000;
const LVM_GETITEMCOUNT: u32 = LVM_FIRST + 4;
const LVM_GETITEMSTATE: u32 = LVM_FIRST + 44;
const LVM_EDITLABEL: u32 = LVM_FIRST + 23;
const LVM_SETCOLUMNWIDTH: u32 = LVM_FIRST + 30;
const LVS_EX_FULLROWSELECT: u32 = 0x20;
const LVS_EX_DOUBLEBUFFER: u32 = 0x10000;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = LVM_FIRST + 54;
const LVIS_SELECTED: u32 = 2;
const LVCF_TEXT: u32 = 4;
const LVCF_WIDTH: u32 = 2;
const LVCF_SUBITEM: u32 = 8;
const LVN_FIRST: i32 = -100;
const TVM_GETNEXTITEM: u32 = 0x110A;
const TVGN_CARET: u32 = 9;
const TVN_FIRST: i32 = -400;
const EN_CHANGE: u16 = 0x0300;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: usize = 1;
const BST_UNCHECKED: usize = 0;
const BS_PUSHBUTTON: u32 = 0;
const BS_AUTORADIOBUTTON: u32 = 9;
const BS_NOTIFY: u32 = 0x4000;
const SS_LEFT: u32 = 0;
const ES_AUTOHSCROLL: u32 = 0x80;
const WS_TABSTOP: u32 = 0x00010000;
const WS_GROUP: u32 = 0x00020000;
const LVS_SINGLESEL: u32 = 0x4;
const LVS_AUTOARRANGE: u32 = 0x100;
const GWL_STYLE: i32 = -16;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const COLOR_WINDOW: i32 = 5;
const COLOR_BTNFACE: i32 = 15;
const COLOR_WINDOWTEXT: i32 = 8;
const WM_CTLCOLOREDIT: u32 = 0x0133;
const WM_KEYDOWN: u32 = 0x0100;
const VK_F5: u16 = 0x74;
const VK_F2: u16 = 0x71;
const VK_DELETE: u16 = 0x2E;
const VK_BACK: u16 = 0x08;
const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_F: u16 = 0x46;
const VK_L: u16 = 0x4C;
const VK_C: u16 = 0x43;
const VK_X: u16 = 0x58;
const VK_V: u16 = 0x56;
const VK_A: u16 = 0x41;
const VK_N: u16 = 0x4E;
const VK_W: u16 = 0x57;
const VK_1: u16 = 0x31;
const VK_2: u16 = 0x32;
const LVM_GETHEADER: u32 = LVM_FIRST + 31;
const HDM_SETITEM: u32 = 0x1200 + 12;

static PENDING: Mutex<Option<(u64, SearchResult)>> = Mutex::new(None);
static GEN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct Row {
    path: PathBuf,
    name: String,
    kind: String,
    size: u64,
    mtime: u32,
    is_dir: bool,
    icon: i32,
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Pc,
    Folder,
}

struct App {
    hwnd: HWND,
    tree: HWND,
    list: HWND,
    search: HWND,
    status: HWND,
    pathbar: HWND,
    back: HWND,
    fwd: HWND,
    up: HWND,
    btn_icons: HWND,
    btn_list: HWND,
    scope_pc: HWND,
    scope_folder: HWND,
    font: isize,
    current: PathBuf,
    history: Vec<PathBuf>,
    hist_i: usize,
    rows: Vec<Row>,
    query: String,
    scope: Scope,
    icons: bool,
    hidden: bool,
    took_us: u64,
    indexing: bool,
    entries: usize,
    cut: bool,
    sort_col: i32,
    sort_asc: bool,
    imagelist_small: isize,
    imagelist_large: isize,
}

pub fn run() {
    if let Some(ui) = find_wpf() {
        match std::process::Command::new(&ui).status() {
            Ok(_) => return,
            Err(_) => {}
        }
    }
    run_win32();
}

fn find_wpf() -> Option<PathBuf> {
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("WSearch.Ui.exe"));
            cands.push(dir.join("../../../ui/bin/Release/net8.0-windows/WSearch.Ui.exe"));
            cands.push(dir.join("../../ui/bin/Release/net8.0-windows/WSearch.Ui.exe"));
        }
    }
    cands.push(crate::default_dir().join("WSearch.Ui.exe"));
    cands.into_iter().find(|p| p.is_file())
}

pub fn copy_ui_next_to(dest_dir: &Path) {
    let Some(src) = find_wpf() else { return };
    let Some(dir) = src.parent() else { return };
    for name in ["WSearch.Ui.exe", "WSearch.Ui.dll", "WSearch.Ui.runtimeconfig.json", "WSearch.Ui.deps.json"] {
        let s = dir.join(name);
        if s.is_file() {
            let _ = std::fs::copy(&s, dest_dir.join(name));
        }
    }
}

pub fn run_win32() {
    unsafe {
        let _ = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let _ = SetProcessDpiAwarenessContext(-4isize as _); // PER_MONITOR_AWARE_V2
        let mut icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_TREEVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES };
        InitCommonControlsEx(&mut icc);

        let class: Vec<u16> = "WSearchBrowser\0".encode_utf16().collect();
        let inst = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: inst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        let title: Vec<u16> = "WSearch\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1050,
            680,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        let mut msg = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let app = Box::new(create_app(hwnd));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
            let app = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App);
            populate_tree(app);
            go(app, user_home(), true);
            SetTimer(hwnd, TIMER_STATUS, 2000, None);
            let hwnd_i = hwnd as isize;
            std::thread::spawn(move || {
                let s = search::status();
                *PENDING.lock() = Some((0, s));
                PostMessage(hwnd_i, WM_APP_STATUS);
            });
            0
        }
        WM_SIZE => {
            if let Some(app) = app_mut(hwnd) {
                layout(app);
            }
            0
        }
        WM_GETMINMAXINFO => {
            let m = l as *mut MINMAXINFO;
            if !m.is_null() {
                (*m).ptMinTrackSize.x = 850;
                (*m).ptMinTrackSize.y = 470;
            }
            0
        }
        WM_COMMAND => {
            if let Some(app) = app_mut(hwnd) {
                let id = (w & 0xffff) as i32;
                let code = ((w >> 16) & 0xffff) as u16;
                if id == IDC_SEARCH && code == EN_CHANGE {
                    KillTimer(hwnd, TIMER_SEARCH);
                    SetTimer(hwnd, TIMER_SEARCH, 50, None);
                } else {
                    on_command(app, id as usize);
                }
            }
            0
        }
        WM_TIMER => {
            if let Some(app) = app_mut(hwnd) {
                if w == TIMER_SEARCH {
                    KillTimer(hwnd, TIMER_SEARCH);
                    kick_search(app);
                } else if w == TIMER_STATUS {
                    let hwnd_i = app.hwnd as isize;
                    std::thread::spawn(move || {
                        let s = search::status();
                        *PENDING.lock() = Some((0, s));
                        PostMessage(hwnd_i, WM_APP_STATUS);
                    });
                }
            }
            0
        }
        WM_NOTIFY => {
            if let Some(app) = app_mut(hwnd) {
                on_notify(app, l);
            }
            0
        }
        WM_CONTEXTMENU => {
            if let Some(app) = app_mut(hwnd) {
                context_menu(app, l);
            }
            0
        }
        WM_KEYDOWN => {
            if let Some(app) = app_mut(hwnd) {
                on_key(app, w as u16);
            }
            0
        }
        WM_APP_SEARCH => {
            if let Some(app) = app_mut(hwnd) {
                apply_search(app);
            }
            0
        }
        WM_APP_STATUS => {
            if let Some(app) = app_mut(hwnd) {
                if let Some((_, s)) = PENDING.lock().take() {
                    app.indexing = s.indexing;
                    if s.entries > 0 {
                        app.entries = s.entries;
                    }
                    update_status(app);
                }
            }
            0
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                drop(Box::from_raw(p as *mut App));
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

fn app_mut<'a>(hwnd: HWND) -> Option<&'a mut App> {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if p == 0 {
        None
    } else {
        Some(unsafe { &mut *(p as *mut App) })
    }
}

fn create_app(hwnd: HWND) -> App {
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let font = CreateFontW(-15, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, 1, 0, 0, 5, 0, w("Segoe UI").as_ptr());
        let ex = WS_EX_CLIENTEDGE;
        let child = WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS;
        let tree = CreateWindowExW(ex, w("SysTreeView32").as_ptr(), std::ptr::null(), child | WS_TABSTOP, 0, 0, 0, 0, hwnd, IDC_TREE as HMENU, inst, std::ptr::null());
        let list = CreateWindowExW(
            ex,
            w("SysListView32").as_ptr(),
            std::ptr::null(),
            child | WS_TABSTOP | LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS | LVS_EDITLABELS | LVS_AUTOARRANGE,
            0,
            0,
            0,
            0,
            hwnd,
            IDC_LIST as HMENU,
            inst,
            std::ptr::null(),
        );
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as usize, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize);
        insert_col(list, 0, "Name", 280);
        insert_col(list, 1, "Date modified", 140);
        insert_col(list, 2, "Size", 90);
        insert_col(list, 3, "Type", 120);
        insert_col(list, 4, "Location", 320);

        let mut small = std::ptr::null_mut();
        let mut large = std::ptr::null_mut();
        let iid = windows_sys::core::GUID { data1: 0x46EB5926, data2: 0x582E, data3: 0x4017, data4: [0x9F, 0xDF, 0xE8, 0x99, 0x8D, 0xAA, 0x09, 0x50] };
        SHGetImageList(SHIL_SMALL as i32, &iid, &mut small);
        SHGetImageList(SHIL_LARGE as i32, &iid, &mut large);
        if !small.is_null() {
            SendMessageW(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, small as isize);
        }
        if !large.is_null() {
            SendMessageW(list, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, large as isize);
        }
        SendMessageW(list, LVM_FIRST + 11, 2, 0); // LVM_SETCALLBACKMASK LVIF_IMAGE

        let search = CreateWindowExW(ex, w("EDIT").as_ptr(), std::ptr::null(), child | WS_TABSTOP | ES_AUTOHSCROLL, 0, 0, 0, 0, hwnd, IDC_SEARCH as HMENU, inst, std::ptr::null());
        let cue: Vec<u16> = "Search\0".encode_utf16().collect();
        SendMessageW(search, 0x1501, 1, cue.as_ptr() as isize); // EM_SETCUEBANNER
        let pathbar = CreateWindowExW(0, w("STATIC").as_ptr(), std::ptr::null(), child | SS_LEFT, 0, 0, 0, 0, hwnd, IDC_PATH as HMENU, inst, std::ptr::null());
        let status = CreateWindowExW(0, w("STATIC").as_ptr(), w("Starting search index…").as_ptr(), child | SS_LEFT, 0, 0, 0, 0, hwnd, IDC_STATUS as HMENU, inst, std::ptr::null());
        let mkbtn = |title: &str, id: i32, style: u32| {
            CreateWindowExW(0, w("BUTTON").as_ptr(), w(title).as_ptr(), child | WS_TABSTOP | BS_PUSHBUTTON | BS_NOTIFY | style, 0, 0, 0, 0, hwnd, id as HMENU, inst, std::ptr::null())
        };
        let back = mkbtn("<", IDC_BACK, 0);
        let fwd = mkbtn(">", IDC_FWD, 0);
        let up = mkbtn("Up", IDC_UP, 0);
        let btn_list = mkbtn("List", IDC_LISTBTN, 0);
        let btn_icons = mkbtn("Icons", IDC_ICONS, 0);
        let scope_pc = CreateWindowExW(0, w("BUTTON").as_ptr(), w("This PC").as_ptr(), child | WS_TABSTOP | WS_GROUP | BS_AUTORADIOBUTTON, 0, 0, 0, 0, hwnd, IDC_SCOPE_PC as HMENU, inst, std::ptr::null());
        let scope_folder = CreateWindowExW(0, w("BUTTON").as_ptr(), w("Folder").as_ptr(), child | WS_TABSTOP | BS_AUTORADIOBUTTON, 0, 0, 0, 0, hwnd, IDC_SCOPE_FOLDER as HMENU, inst, std::ptr::null());
        SendMessageW(scope_pc, BM_SETCHECK, BST_CHECKED, 0);
        for h in [tree, list, search, pathbar, status, back, fwd, up, btn_list, btn_icons, scope_pc, scope_folder] {
            SendMessageW(h, WM_SETFONT, font as usize, 1);
        }
        App {
            hwnd,
            tree,
            list,
            search,
            status,
            pathbar,
            back,
            fwd,
            up,
            btn_icons,
            btn_list,
            scope_pc,
            scope_folder,
            font: font as isize,
            current: user_home(),
            history: vec![user_home()],
            hist_i: 0,
            rows: Vec::new(),
            query: String::new(),
            scope: Scope::Pc,
            icons: false,
            hidden: false,
            took_us: 0,
            indexing: true,
            entries: 0,
            cut: false,
            sort_col: 0,
            sort_asc: true,
            imagelist_small: small as isize,
            imagelist_large: large as isize,
        }
    }
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn insert_col(list: HWND, i: i32, title: &str, width: i32) {
    #[repr(C)]
    struct LvCol {
        mask: u32,
        fmt: i32,
        cx: i32,
        psz: *mut u16,
        cch: i32,
        sub: i32,
        iimage: i32,
        iorder: i32,
        cxmin: i32,
        cxdefault: i32,
        cxideal: i32,
    }
    let mut title = w(title);
    let col = LvCol { mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM, fmt: 0, cx: width, psz: title.as_mut_ptr(), cch: 0, sub: i, iimage: 0, iorder: 0, cxmin: 0, cxdefault: 0, cxideal: 0 };
    unsafe { SendMessageW(list, LVM_INSERTCOLUMNW, i as usize, &col as *const _ as isize) };
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".into()))
}

fn populate_tree(app: &App) {
    fn add(tree: HWND, parent: isize, text: &str, path: &str) -> isize {
        let mut name = w(text);
        let item = TVITEMW {
            mask: TVIF_TEXT | TVIF_PARAM,
            hItem: 0,
            state: 0,
            stateMask: 0,
            pszText: name.as_mut_ptr(),
            cchTextMax: 0,
            iImage: 0,
            iSelectedImage: 0,
            cChildren: 0,
            lParam: Box::into_raw(Box::new(path.to_string())) as isize,
        };
        // TVINSERTSTRUCT layout: hParent, hInsertAfter, item
        #[repr(C)]
        struct Ins {
            parent: isize,
            after: isize,
            item: TVITEMW,
        }
        let ins = Ins { parent, after: -65534, item }; // TVI_LAST
        unsafe { SendMessageW(tree, TVM_INSERTITEMW, 0, &ins as *const _ as isize) }
    }
    let root = add(app.tree, 0, "Recents", ":recents");
    let _ = root;
    add(app.tree, 0, "Desktop", &user_home().join("Desktop").to_string_lossy());
    add(app.tree, 0, "Documents", &user_home().join("Documents").to_string_lossy());
    add(app.tree, 0, "Downloads", &user_home().join("Downloads").to_string_lossy());
    add(app.tree, 0, "Pictures", &user_home().join("Pictures").to_string_lossy());
    add(app.tree, 0, "Music", &user_home().join("Music").to_string_lossy());
    add(app.tree, 0, "Videos", &user_home().join("Videos").to_string_lossy());
    let pc = add(app.tree, 0, "This PC", ":pc");
    for d in shell::drives() {
        let s = d.to_string_lossy().to_string();
        add(app.tree, pc, &s, &s);
    }
    let _ = pc;
}

fn layout(app: &App) {
    let mut rc = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { GetClientRect(app.hwnd, &mut rc) };
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    let sb = 208;
    let tool = 32;
    let path_h = 24;
    let status_h = 28;
    let pad = 6;
    unsafe {
        MoveWindow(app.back, pad, pad, 32, 24, 1);
        MoveWindow(app.fwd, pad + 36, pad, 32, 24, 1);
        MoveWindow(app.up, pad + 72, pad, 40, 24, 1);
        MoveWindow(app.btn_list, pad + 120, pad, 48, 24, 1);
        MoveWindow(app.btn_icons, pad + 172, pad, 48, 24, 1);
        MoveWindow(app.scope_pc, pad + 228, pad, 80, 24, 1);
        MoveWindow(app.scope_folder, pad + 312, pad, 70, 24, 1);
        MoveWindow(app.search, pad + 390, pad, (w - 390 - pad * 2).max(160), 24, 1);
        MoveWindow(app.tree, 0, tool, sb, h - tool - status_h, 1);
        MoveWindow(app.pathbar, sb + 8, tool, w - sb - 16, path_h, 1);
        MoveWindow(app.list, sb, tool + path_h, w - sb, h - tool - path_h - status_h, 1);
        MoveWindow(app.status, 8, h - status_h + 4, w - 16, 20, 1);
    }
}

fn go(app: &mut App, path: PathBuf, push: bool) {
    app.query.clear();
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.search, w("").as_ptr()) };
    if push {
        app.history.truncate(app.hist_i + 1);
        app.history.push(path.clone());
        app.hist_i = app.history.len() - 1;
    }
    app.current = path;
    load_dir(app);
}

fn load_dir(app: &mut App) {
    app.rows.clear();
    if let Ok(rd) = std::fs::read_dir(&app.current) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name == "." || name == ".." {
                continue;
            }
            let (mt, sz, is_dir, hidden) = shell::path_mtime_size(&p);
            if hidden && !app.hidden {
                continue;
            }
            let kind = shell::kind_label(&name, is_dir);
            let icon = shell::file_icon_index(&p.to_string_lossy(), is_dir);
            app.rows.push(Row { path: p, name, kind, size: sz, mtime: mt, is_dir, icon });
        }
    }
    sort_rows(app);
    refresh_list(app);
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.pathbar, w(&app.current.to_string_lossy()).as_ptr()) };
    update_status(app);
}

fn sort_rows(app: &mut App) {
    let col = app.sort_col;
    let asc = app.sort_asc;
    app.rows.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            return if a.is_dir { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }
        let o = match col {
            1 => a.mtime.cmp(&b.mtime),
            2 => a.size.cmp(&b.size),
            3 => a.kind.to_ascii_lowercase().cmp(&b.kind.to_ascii_lowercase()),
            4 => a.path.to_string_lossy().to_ascii_lowercase().cmp(&b.path.to_string_lossy().to_ascii_lowercase()),
            _ => a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()),
        };
        if asc { o } else { o.reverse() }
    });
}

fn refresh_list(app: &App) {
    unsafe {
        SendMessageW(app.list, LVM_SETITEMCOUNT, app.rows.len(), 0);
        windows_sys::Win32::Graphics::Gdi::InvalidateRect(app.list, std::ptr::null(), 1);
    }
}

fn update_status(app: &App) {
    let sel = selected(app).len();
    let mut s = format!("{} items", app.rows.len());
    if sel > 0 {
        s.push_str(&format!("  |  {sel} selected"));
    }
    if !app.query.is_empty() {
        s.push_str(&format!("  |  {:.2} ms", app.took_us as f64 / 1000.0));
    }
    if app.indexing {
        s.push_str("  |  Indexing…");
    } else if app.entries > 0 {
        s.push_str(&format!("  |  {} files indexed", app.entries));
    }
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.status, w(&s).as_ptr()) };
}

fn search_text(app: &App) -> String {
    unsafe {
        let n = GetWindowTextLengthW(app.search) as usize;
        if n == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; n + 1];
        GetWindowTextW(app.search, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..n])
    }
}

fn kick_search(app: &mut App) {
    let q = search_text(app);
    app.query = q.clone();
    if q.is_empty() {
        load_dir(app);
        return;
    }
    let mut full = q;
    if app.scope == Scope::Folder {
        full = format!("{} in:{}", full, app.current.display());
    }
    let gen = GEN.fetch_add(1, Ordering::Relaxed) + 1;
    GEN.store(gen, Ordering::Relaxed);
    let hwnd_i = app.hwnd as isize;
    std::thread::spawn(move || {
        let r = search::search(&full, 500);
        *PENDING.lock() = Some((gen, r));
        unsafe { PostMessage(hwnd_i, WM_APP_SEARCH) };
    });
}

fn apply_search(app: &mut App) {
    let Some((gen, r)) = PENDING.lock().take() else { return };
    if gen != GEN.load(Ordering::Relaxed) && gen != 0 {
        return;
    }
    if let Some(e) = r.error {
        app.indexing = true;
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.status, w(&e).as_ptr()) };
        return;
    }
    app.took_us = r.took_us;
    app.indexing = r.indexing;
    app.rows = r
        .hits
        .into_iter()
        .map(|h| {
            let is_dir = h.kind == "dir";
            let name = Path::new(&h.path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| h.path.clone());
            let icon = shell::file_icon_index(&h.path, is_dir);
            Row { path: PathBuf::from(&h.path), name, kind: shell::kind_label(&h.path, is_dir), size: h.size, mtime: h.mtime, is_dir, icon }
        })
        .collect();
    sort_rows(app);
    refresh_list(app);
    update_status(app);
}

fn selected(app: &App) -> Vec<usize> {
    let mut out = Vec::new();
    unsafe {
        let mut i = -1i32;
        loop {
            i = SendMessageW(app.list, LVM_GETNEXTITEM, (i as usize) as _, LVNI_SELECTED as isize) as i32;
            if i < 0 {
                break;
            }
            out.push(i as usize);
        }
    }
    out
}

fn selected_paths(app: &App) -> Vec<PathBuf> {
    selected(app).into_iter().filter_map(|i| app.rows.get(i).map(|r| r.path.clone())).collect()
}

fn on_command(app: &mut App, id: usize) {
    match id as i32 {
        IDC_BACK => {
            if app.hist_i > 0 {
                app.hist_i -= 1;
                app.current = app.history[app.hist_i].clone();
                load_dir(app);
            }
        }
        IDC_FWD => {
            if app.hist_i + 1 < app.history.len() {
                app.hist_i += 1;
                app.current = app.history[app.hist_i].clone();
                load_dir(app);
            }
        }
        IDC_UP => {
            if let Some(p) = app.current.parent() {
                go(app, p.to_path_buf(), true);
            }
        }
        IDC_ICONS => set_icons(app, true),
        IDC_LISTBTN => set_icons(app, false),
        IDC_SCOPE_PC => {
            app.scope = Scope::Pc;
            if !app.query.is_empty() {
                kick_search(app);
            }
        }
        IDC_SCOPE_FOLDER => {
            app.scope = Scope::Folder;
            if !app.query.is_empty() {
                kick_search(app);
            }
        }
        x if x == IDM_OPEN as i32 => open_sel(app),
        x if x == IDM_REVEAL as i32 => {
            if let Some(p) = selected_paths(app).first() {
                shell::reveal(&p.to_string_lossy());
            }
        }
        x if x == IDM_COPY as i32 => {
            app.cut = false;
            shell::clipboard_set_files(app.hwnd, &selected_paths(app));
        }
        x if x == IDM_CUT as i32 => {
            app.cut = true;
            shell::clipboard_set_files(app.hwnd, &selected_paths(app));
        }
        x if x == IDM_PASTE as i32 => paste(app),
        x if x == IDM_DELETE as i32 => {
            let p = selected_paths(app);
            shell::delete_files(app.hwnd, &p);
            load_dir(app);
        }
        x if x == IDM_RENAME as i32 => {
            if let Some(&i) = selected(app).first() {
                unsafe { SendMessageW(app.list, LVM_EDITLABEL, i, 0) };
            }
        }
        x if x == IDM_PROPS as i32 => {
            if let Some(p) = selected_paths(app).first() {
                shell::properties(app.hwnd, &p.to_string_lossy());
            }
        }
        x if x == IDM_NEWFOLDER as i32 => new_folder(app),
        x if x == IDM_REFRESH as i32 => load_dir(app),
        x if x == IDM_COPYPATH as i32 => {
            if let Some(p) = selected_paths(app).first() {
                set_text_clip(&p.to_string_lossy());
            }
        }
        _ => {}
    }
}

fn set_icons(app: &mut App, icons: bool) {
    app.icons = icons;
    unsafe {
        let style = windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongW(app.list, GWL_STYLE) as u32;
        let mut style = style & !LVS_REPORT & !LVS_ICON;
        style |= if icons { LVS_ICON } else { LVS_REPORT };
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongW(app.list, GWL_STYLE, style as i32);
    }
    refresh_list(app);
}

fn open_sel(app: &mut App) {
    let paths = selected_paths(app);
    for p in paths {
        if p.is_dir() {
            go(app, p, true);
            return;
        }
        shell::open_path(app.hwnd, &p.to_string_lossy());
    }
}

fn paste(app: &mut App) {
    let files = shell::clipboard_get_files();
    if files.is_empty() {
        return;
    }
    if app.cut {
        shell::move_files(app.hwnd, &files, &app.current);
        app.cut = false;
    } else {
        shell::copy_files(app.hwnd, &files, &app.current);
    }
    load_dir(app);
}

fn new_folder(app: &mut App) {
    for i in 0..100 {
        let name = if i == 0 { "New folder".into() } else { format!("New folder ({i})") };
        let p = app.current.join(&name);
        if !p.exists() && std::fs::create_dir(&p).is_ok() {
            load_dir(app);
            if let Some(idx) = app.rows.iter().position(|r| r.path == p) {
                unsafe { SendMessageW(app.list, LVM_EDITLABEL, idx, 0) };
            }
            return;
        }
    }
}

fn on_key(app: &mut App, vk: u16) {
    let ctrl = unsafe { GetKeyState(0x11) } as u16 & 0x8000 != 0; // VK_CONTROL
    let alt = unsafe { GetKeyState(0x12) } as u16 & 0x8000 != 0;
    if ctrl && vk == VK_F {
        unsafe { SetFocus(app.search) };
        return;
    }
    if ctrl && vk == VK_L {
        unsafe { SetFocus(app.search) };
        return;
    }
    if ctrl && vk == VK_C {
        on_command(app, IDM_COPY);
        return;
    }
    if ctrl && vk == VK_X {
        on_command(app, IDM_CUT);
        return;
    }
    if ctrl && vk == VK_V {
        on_command(app, IDM_PASTE);
        return;
    }
    if ctrl && vk == VK_A {
        unsafe { SendMessageW(app.list, LVM_FIRST + 47, usize::MAX, LVIS_SELECTED as isize) }; // LVM_SETITEMSTATE all
        return;
    }
    if ctrl && vk == VK_N {
        new_folder(app);
        return;
    }
    if ctrl && vk == VK_1 {
        set_icons(app, false);
        return;
    }
    if ctrl && vk == VK_2 {
        set_icons(app, true);
        return;
    }
    if alt && vk == VK_BACK || (alt && vk == 0x25) {
        on_command(app, IDC_BACK as usize);
        return;
    }
    if alt && vk == 0x27 {
        on_command(app, IDC_FWD as usize);
        return;
    }
    if alt && vk == 0x26 {
        on_command(app, IDC_UP as usize);
        return;
    }
    match vk {
        VK_F5 => load_dir(app),
        VK_F2 => on_command(app, IDM_RENAME),
        VK_DELETE => on_command(app, IDM_DELETE),
        VK_RETURN => open_sel(app),
        VK_ESCAPE => {
            if !app.query.is_empty() {
                app.query.clear();
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.search, w("").as_ptr()) };
                load_dir(app);
            }
        }
        _ => {}
    }
}

fn context_menu(app: &mut App, lp: LPARAM) {
    let x = (lp & 0xffff) as i16 as i32;
    let y = ((lp >> 16) & 0xffff) as i16 as i32;
    unsafe {
        let m = CreatePopupMenu();
        AppendMenuW(m, MF_STRING, IDM_OPEN, w("Open").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_REVEAL, w("Show in Explorer").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, IDM_CUT, w("Cut\tCtrl+X").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_COPY, w("Copy\tCtrl+C").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_PASTE, w("Paste\tCtrl+V").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, IDM_DELETE, w("Delete\tDel").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_RENAME, w("Rename\tF2").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_COPYPATH, w("Copy path").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(m, MF_STRING, IDM_NEWFOLDER, w("New folder\tCtrl+N").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_REFRESH, w("Refresh\tF5").as_ptr());
        AppendMenuW(m, MF_STRING, IDM_PROPS, w("Properties").as_ptr());
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD, x, y, 0, app.hwnd, std::ptr::null());
        windows_sys::Win32::UI::WindowsAndMessaging::DestroyMenu(m);
        if cmd != 0 {
            on_command(app, cmd as usize);
        }
    }
}

fn on_notify(app: &mut App, l: LPARAM) {
    let hdr = unsafe { &*(l as *const NMHDR) };
    if hdr.hwndFrom == app.list {
        let code = hdr.code as i32;
        if code == LVN_GETDISPINFOW as i32 {
            let di = unsafe { &mut *(l as *mut NMLVDISPINFOW) };
            let i = di.item.iItem as usize;
            let sub = di.item.iSubItem;
            if i >= app.rows.len() {
                return;
            }
            let r = &app.rows[i];
            if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() {
                let text = match sub {
                    0 => r.name.clone(),
                    1 => shell::format_mtime(r.mtime),
                    2 => if r.is_dir { String::new() } else { shell::format_size(r.size) },
                    3 => r.kind.clone(),
                    4 => r.path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                    _ => String::new(),
                };
                let mut ww = w(&text);
                let n = (ww.len() - 1).min(di.item.cchTextMax as usize);
                unsafe { std::ptr::copy_nonoverlapping(ww.as_ptr(), di.item.pszText, n) };
                unsafe { *di.item.pszText.add(n) = 0 };
            }
            if di.item.mask & 2 != 0 {
                // LVIF_IMAGE
                di.item.iImage = r.icon;
            }
            return;
        }
        if code == LVN_ITEMACTIVATE as i32 {
            open_sel(app);
            return;
        }
        if code == LVN_COLUMNCLICK as i32 {
            let nv = unsafe { &*(l as *const NMLISTVIEW) };
            if app.sort_col == nv.iSubItem {
                app.sort_asc = !app.sort_asc;
            } else {
                app.sort_col = nv.iSubItem;
                app.sort_asc = true;
            }
            sort_rows(app);
            refresh_list(app);
        }
    }
    if hdr.hwndFrom == app.tree && hdr.code as i32 == TVN_SELCHANGEDW as i32 {
        let nv = unsafe { &*(l as *const NMTREEVIEWW) };
        let p = nv.itemNew.lParam;
        if p != 0 {
            let s = unsafe { &*(p as *const String) };
            if s == ":recents" {
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetWindowTextW(app.search, w("mtime:<30d").as_ptr()) };
                app.scope = Scope::Pc;
                kick_search(app);
            } else if s != ":pc" {
                go(app, PathBuf::from(s), true);
            }
        }
    }
}

fn set_text_clip(s: &str) {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    let mut w = s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    unsafe {
        if OpenClipboard(0 as HWND) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let p = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);
        SetClipboardData(13, h as _); // CF_UNICODETEXT
        CloseClipboard();
    }
}

unsafe fn PostMessage(hwnd: isize, msg: u32) {
    windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(hwnd as HWND, msg, 0, 0);
}

fn wparam_hi(w: WPARAM) -> u16 {
    ((w >> 16) & 0xffff) as u16
}
