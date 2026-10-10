//! Owns the live name index, follows USN / directory changes, keeps the
//! content index current, answers searches.

use crate::content::{self, Content, Grep, GrepResult};
use crate::index::Index;
use crate::live::Live;
use crate::nt::{self, Handle, UsnErr, VolumeCrawl};
use crate::pathwin;
use crate::query::{Query, Searcher};
use crate::walk;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
use windows_sys::Win32::System::IO::OVERLAPPED;

const COMPACT_PENDING: usize = 50_000;
const COMPACT_EVERY: Duration = Duration::from_secs(12 * 3600);
const SCAN_THREADS: usize = 32;
const CONTENT_QUIET: Duration = Duration::from_secs(2);
const CONTENT_MAX_WAIT: Duration = Duration::from_secs(300);
const INDEXING: &str = "indexing";

pub struct Options {
    pub dir: PathBuf,
    pub home: String,
    /// If set, only this folder (or drive) is indexed. `None` = all local NTFS volumes.
    pub root: Option<PathBuf>,
    pub skip: Option<Vec<PathBuf>>,
}

pub struct Found {
    pub path: PathBuf,
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    pub score: i32,
}

#[derive(serde::Serialize)]
pub struct Status {
    #[serde(skip)]
    pub ready: bool,
    pub entries: usize,
    pub dirs: usize,
    pub overlay: usize,
    pub removed: usize,
    pub event_id: u64,
    pub index_bytes: usize,
    pub content_docs: usize,
    pub content_segments: usize,
    pub content_bytes: usize,
    pub content_pending: usize,
    pub mft: bool,
    pub owner: bool,
}

#[derive(Clone)]
pub struct Engine {
    s: Arc<Shared>,
}

struct Shared {
    live: RwLock<Option<Live>>,
    content: RwLock<Content>,
    home: String,
    dir: PathBuf,
    root: Option<PathBuf>,
    wake: Sender<Vec<DirEv>>,
    save_requested: AtomicBool,
    content_tx: Sender<(Vec<Vec<u8>>, Vec<Vec<u8>>)>,
    content_rx: Mutex<Option<Receiver<(Vec<Vec<u8>>, Vec<Vec<u8>>)>>>,
    content_pending: AtomicUsize,
    owner: AtomicBool,
    #[allow(dead_code)]
    lock: std::fs::File,
    mft: AtomicBool,
    volumes: Mutex<Vec<(u8, Handle, u64, i64)>>,
}

#[derive(Clone)]
pub struct DirEv {
    pub path: Vec<u8>,
    pub recursive: bool,
}

fn try_lock(f: &std::fs::File) -> bool {
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    unsafe { LockFileEx(f.as_raw_handle() as HANDLE, LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &mut ov) != 0 }
}

fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{} {}", crate::query::now_secs(), msg.as_ref());
    let _ = err.flush();
}

fn search_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::LazyLock<rayon::ThreadPool> = std::sync::LazyLock::new(|| {
        rayon::ThreadPoolBuilder::new().thread_name(|i| format!("wsearch-search-{i}")).build().unwrap()
    });
    &POOL
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().name(name.into()).spawn(f).expect("spawn");
}

impl Engine {
    pub fn start(opts: Options) -> Result<Engine, String> {
        std::fs::create_dir_all(&opts.dir).map_err(|e| e.to_string())?;
        nt::enable_privileges();
        let lock = std::fs::File::create(opts.dir.join("daemon.lock")).map_err(|e| e.to_string())?;
        let owner = try_lock(&lock);
        if owner && crate::winpriv::is_elevated() && opts.root.is_none() && !opts.dir.join("mft.ok").exists() {
            let can_mft = nt::drive_letters().iter().copied().any(|l| nt::is_ntfs(l) && nt::open_volume(l).is_some());
            if can_mft {
                log("elevated: rebuilding name index from $MFT");
                let _ = std::fs::remove_file(opts.dir.join("index.bin"));
            }
        }
        if let Some(skip) = opts.skip {
            let _ = walk::SKIP.set(skip.into_iter().map(|p| pathwin::normalize(p.to_string_lossy().as_bytes())).collect());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctx, crx) = std::sync::mpsc::channel();
        let content = if owner { Content::open(opts.dir.join("content")) } else { Content::open_shared(opts.dir.join("content")) };
        let shared = Arc::new(Shared {
            live: RwLock::new(None),
            content: RwLock::new(content),
            home: opts.home,
            dir: opts.dir,
            root: opts.root,
            wake: tx,
            save_requested: AtomicBool::new(false),
            content_tx: ctx,
            content_rx: Mutex::new(Some(crx)),
            content_pending: AtomicUsize::new(0),
            owner: AtomicBool::new(owner),
            lock,
            mft: AtomicBool::new(false),
            volumes: Mutex::new(Vec::new()),
        });
        let base = Index::load(&shared.dir.join("index.bin"));
        let s = shared.clone();
        spawn("wsearch-apply", move || {
            let base = match base {
                Some(b) => {
                    log(format!("loaded {} entries", b.n));
                    b
                }
                None if owner => full_build(&s),
                None => wait_for_index(&s.dir),
            };
            *s.live.write() = Some(Live::new(base));
            if owner {
                start_content(&s);
                start_watchers(&s);
            }
            apply_loop(&s, rx);
        });
        Ok(Engine { s: shared })
    }

    pub fn home(&self) -> &str {
        &self.s.home
    }

    pub fn search(&self, q: &Query) -> Result<Vec<Found>, String> {
        let g = self.s.live.read();
        let Some(live) = g.as_ref() else { return Err(INDEXING.into()) };
        let mut p = Vec::new();
        Ok(search_pool()
            .install(|| Searcher { live }.search(q))
            .into_iter()
            .map(|h| {
                let (kind, size, mtime) = match &h.over {
                    Some(path) => {
                        let o = live.over[path];
                        p = path.clone();
                        (o.kind, o.size, o.mtime)
                    }
                    None => {
                        let i = h.idx as usize;
                        live.base.path(i, &mut p);
                        (live.base.kind()[i], live.base.size_of(i), live.base.mtime()[i])
                    }
                };
                Found { path: PathBuf::from(String::from_utf8_lossy(&p).as_ref()), kind, size, mtime, score: h.score }
            })
            .collect())
    }

    pub fn grep(&self, q: &Query, g: &Grep) -> Result<(GrepResult, bool), String> {
        let home = self.s.home.as_bytes();
        let indexed = q.scope.as_ref().is_none_or(|s| content::in_scope(s, home));
        if indexed {
            return Ok((search_pool().install(|| self.s.content.read().search(g, q)), true));
        }
        let paths = {
            let l = self.s.live.read();
            let Some(live) = l.as_ref() else { return Err(INDEXING.into()) };
            search_pool().install(|| content::scan_paths(live, q.clone_for_scan()))
        };
        Ok((content::verify(g, &paths, q.limit), false))
    }

    pub fn status(&self) -> Status {
        let l = self.s.live.read();
        let c = self.s.content.read();
        Status {
            ready: l.is_some(),
            entries: l.as_ref().map_or(0, |l| l.base.n),
            dirs: l.as_ref().map_or(0, |l| l.base.d),
            overlay: l.as_ref().map_or(0, |l| l.over.len()),
            removed: l.as_ref().map_or(0, |l| l.dead_count),
            event_id: l.as_ref().map_or(0, |l| l.event_id),
            index_bytes: l.as_ref().map_or(0, |l| l.base.bytes()),
            content_docs: c.docs(),
            content_segments: c.segs.len(),
            content_bytes: c.bytes(),
            content_pending: self.s.content_pending.load(Ordering::Relaxed),
            mft: self.s.mft.load(Ordering::Relaxed),
            owner: self.s.owner.load(Ordering::Relaxed),
        }
    }

    pub fn save(&self) {
        self.s.save_requested.store(true, Ordering::Relaxed);
        let _ = self.s.wake.send(Vec::new());
    }
}

fn wait_for_index(dir: &Path) -> Index {
    loop {
        if let Some(b) = Index::load(&dir.join("index.bin")) {
            return b;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn start_content(s: &Arc<Shared>) {
    let Some(rx) = s.content_rx.lock().take() else { return };
    let _ = s.content_tx.send((Vec::new(), vec![s.home.as_bytes().to_vec()]));
    let s = s.clone();
    spawn("wsearch-content", move || content_loop(&s, rx));
}

fn start_watchers(s: &Arc<Shared>) {
    let vols: Vec<(u8, u64, i64)> = s.volumes.lock().iter().map(|(l, _, j, u)| (*l, *j, *u)).collect();
    if vols.is_empty() {
        if let Some(root) = &s.root {
            watch_rdc(s.clone(), root.clone());
        }
        return;
    }
    for (letter, jid, usn) in vols {
        let s = s.clone();
        spawn(&format!("wsearch-usn-{letter}"), move || usn_loop(s, letter, jid, usn));
    }
}

fn usn_loop(s: Arc<Shared>, letter: u8, mut jid: u64, mut usn: i64) {
    let Some(h) = nt::open_volume(letter) else { return };
    loop {
        match nt::read_usn(&h, jid, usn) {
            Ok((next, events)) => {
                usn = next;
                if !events.is_empty() {
                    let mut dirs: HashMap<Vec<u8>, bool> = HashMap::new();
                    for e in events {
                        if let Some(p) = nt::path_from_frn(&h, e.parent_frn) {
                            if let Some(root) = &s.root {
                                let r = pathwin::normalize(root.to_string_lossy().as_bytes());
                                if !crate::query::scope_ok(&p, &r) {
                                    continue;
                                }
                            }
                            dirs.entry(p).or_default();
                        }
                    }
                    let batch: Vec<DirEv> = dirs.into_iter().map(|(path, recursive)| DirEv { path, recursive }).collect();
                    if !batch.is_empty() {
                        let _ = s.wake.send(batch);
                    }
                }
            }
            Err(UsnErr::Wrap) => {
                log(format!("{letter}: USN journal wrapped, rebuilding"));
                let idx = full_build(&s);
                *s.live.write() = Some(Live::new(idx));
                if let Some((nj, nu)) = nt::query_usn(&h) {
                    jid = nj;
                    usn = nu;
                }
            }
            Err(UsnErr::Io) => std::thread::sleep(Duration::from_millis(250)),
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn watch_rdc(s: Arc<Shared>, root: PathBuf) {
    spawn("wsearch-rdc", move || {
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
            FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            ReadDirectoryChangesW,
        };
        const FILE_LIST_DIRECTORY: u32 = 1;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        let w = pathwin::long_wide(root.to_string_lossy().as_bytes());
        let h = unsafe {
            CreateFileW(w.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, std::ptr::null_mut())
        };
        if h == INVALID_HANDLE_VALUE {
            return;
        }
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut ret = 0u32;
            let ok = unsafe {
                ReadDirectoryChangesW(h, buf.as_mut_ptr() as *mut _, buf.len() as u32, 1, FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE, &mut ret, std::ptr::null_mut(), None)
            };
            if ok == 0 {
                break;
            }
            let mut off = 0usize;
            let mut dirs = Vec::new();
            while off + 12 <= ret as usize {
                let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                let nlen = u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()) as usize;
                let mut wname = Vec::new();
                if off + 12 + nlen <= ret as usize {
                    for c in buf[off + 12..off + 12 + nlen].chunks_exact(2) {
                        wname.push(u16::from_le_bytes(c.try_into().unwrap()));
                    }
                }
                let rel = pathwin::from_wide(&wname);
                let full = pathwin::join(root.to_string_lossy().as_bytes(), &rel);
                dirs.push(pathwin::parent(&full).to_vec());
                if next == 0 {
                    break;
                }
                off += next;
            }
            dirs.sort();
            dirs.dedup();
            let batch: Vec<DirEv> = dirs.into_iter().map(|path| DirEv { path, recursive: false }).collect();
            let _ = s.wake.send(batch);
        }
        unsafe { CloseHandle(h) };
    });
}

fn content_loop(shared: &Shared, rx: Receiver<(Vec<Vec<u8>>, Vec<Vec<u8>>)>) {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let home = shared.home.as_bytes().to_vec();
    let mut pending: HashMap<(Vec<u8>, bool), (Instant, Instant)> = HashMap::new();
    loop {
        let wait = if pending.is_empty() { Duration::from_secs(3600) } else { Duration::from_millis(250) };
        match rx.recv_timeout(wait) {
            Ok(first) => {
                let now = Instant::now();
                for (d, t) in std::iter::once(first).chain(rx.try_iter()) {
                    for key in d.into_iter().map(|p| (p, false)).chain(t.into_iter().map(|p| (p, true))) {
                        if content::in_scope(&key.0, &home) || (key.1 && home.starts_with(&key.0)) {
                            pending.entry(key).and_modify(|e| e.1 = now).or_insert((now, now));
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let ripe: Vec<(Vec<u8>, bool)> = pending
            .iter()
            .filter(|(_, (first, last))| last.elapsed() >= CONTENT_QUIET || first.elapsed() >= CONTENT_MAX_WAIT)
            .map(|(k, _)| k.clone())
            .collect();
        if ripe.is_empty() {
            continue;
        }
        let (mut dirs, mut trees) = (Vec::<Vec<u8>>::new(), Vec::<Vec<u8>>::new());
        for k in ripe {
            pending.remove(&k);
            if k.1 { trees.push(k.0) } else { dirs.push(k.0) }
        }
        let t = Instant::now();
        let wants = {
            let g = shared.live.read();
            let Some(live) = g.as_ref() else { continue };
            content::wants(live, &home, &dirs, &trees)
        };
        let todo = shared.content.write().diff(wants);
        if todo.len() == 0 {
            continue;
        }
        let n = todo.len();
        shared.content_pending.store(n, Ordering::Relaxed);
        for batch in todo.batches() {
            let (dir, id) = {
                let mut c = shared.content.write();
                (c.dir.clone(), c.alloc_id())
            };
            let len = batch.len();
            if let Some(seg) = pool.install(|| content::build_segment(&dir, id, &todo, batch)) {
                shared.content.write().push(seg);
            }
            shared.content_pending.fetch_sub(len, Ordering::Relaxed);
        }
        drop(todo);
        loop {
            let plan = shared.content.read().merge_plan();
            let Some(ids) = plan else { break };
            let (dir, id) = {
                let mut c = shared.content.write();
                (c.dir.clone(), c.alloc_id())
            };
            let merged = {
                let c = shared.content.read();
                pool.install(|| content::merge(&dir, id, &c.segments(&ids)))
            };
            match merged {
                Some(seg) => shared.content.write().replace(&ids, seg),
                None => break,
            }
        }
        if n > 100 {
            log(format!("content: indexed {n} files in {:.2?}", t.elapsed()));
        }
    }
}

fn full_build(shared: &Shared) -> Index {
    let t = Instant::now();
    let started = crate::query::now_secs();
    let threads = std::thread::available_parallelism().map(|n| (n.get() * 2).clamp(8, 64)).unwrap_or(SCAN_THREADS);
    let t_crawl = Instant::now();
    let ls = crawl(shared, threads);
    log(format!("crawl {} listings in {:.2?}", ls.len(), t_crawl.elapsed()));
    let t_build = Instant::now();
    let eid = {
        let vols = shared.volumes.lock();
        vols.first().map(|v| v.3 as u64).unwrap_or(0)
    };
    let idx = Index::build(ls, eid, started, shared.home.as_bytes());
    log(format!("build {} entries in {:.2?}", idx.n, t_build.elapsed()));
    let path = shared.dir.join("index.bin");
    let t_save = Instant::now();
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    log(format!("save {:.2?}  total {:.2?} mft={}", t_save.elapsed(), t.elapsed(), shared.mft.load(Ordering::Relaxed)));
    if shared.mft.load(Ordering::Relaxed) {
        let _ = std::fs::write(shared.dir.join("mft.ok"), b"1");
    } else {
        let _ = std::fs::remove_file(shared.dir.join("mft.ok"));
    }
    Index::load(&path).unwrap_or(idx)
}

fn crawl(shared: &Shared, threads: usize) -> Vec<walk::Listing> {
    if let Some(root) = &shared.root {
        let p = pathwin::normalize(root.to_string_lossy().as_bytes());
        if p.len() == 2 && p[1] == b':' && nt::is_ntfs(p[0]) {
            if let Some(vol) = nt::crawl_volume(p[0]) {
                remember_vol(shared, &vol);
                shared.mft.store(true, Ordering::Relaxed);
                return nt::listings_from_mft(&[vol]);
            }
        }
        log(format!("walking {}", String::from_utf8_lossy(&p)));
        let ls = walk::scan(&p, threads);
        return walk::wrap_path(&p, ls);
    }
    let letters = nt::drive_letters();
    let mut mft_vols = Vec::new();
    let mut walked = Vec::new();
    for letter in letters {
        if !nt::is_ntfs(letter) {
            let root = vec![letter, b':', b'\\'];
            walked.push((vec![letter, b':'], walk::scan(&root, threads.max(4))));
            continue;
        }
        match nt::crawl_volume(letter) {
            Some(vol) => {
                remember_vol(shared, &vol);
                mft_vols.push(vol);
            }
            None => {
                let root = vec![letter, b':', b'\\'];
                log(format!("no volume handle for {letter}: (admin for MFT); walking"));
                walked.push((vec![letter, b':'], walk::scan(&root, threads.max(4))));
            }
        }
    }
    let walked_ls = fold_walked(walked);
    if !mft_vols.is_empty() {
        shared.mft.store(true, Ordering::Relaxed);
        let mut ls = nt::listings_from_mft(&mft_vols);
        if !walked_ls.is_empty() {
            merge_computer(&mut ls, walked_ls);
        }
        return ls;
    }
    walked_ls
}

fn fold_walked(walked: Vec<(Vec<u8>, Vec<walk::Listing>)>) -> Vec<walk::Listing> {
    let mut acc: Option<Vec<walk::Listing>> = None;
    for (name, ls) in walked {
        let wrapped = walk::wrap_path(&name, ls);
        match &mut acc {
            None => acc = Some(wrapped),
            Some(base) => merge_computer(base, wrapped),
        }
    }
    acc.unwrap_or_default()
}

fn remember_vol(shared: &Shared, vol: &VolumeCrawl) {
    if let Some(h) = nt::open_volume(vol.letter) {
        shared.volumes.lock().push((vol.letter, h, vol.journal_id, vol.next_usn));
    }
}

fn merge_computer(base: &mut Vec<walk::Listing>, extra: Vec<walk::Listing>) {
    if extra.is_empty() || base.is_empty() {
        return;
    }
    let mut extra = extra;
    let root = extra.remove(0);
    for e in root.ents {
        let name = root.names[e.name_off as usize..][..e.name_len as usize].to_vec();
        let name_off = base[0].names.len() as u32;
        base[0].names.extend_from_slice(&name);
        base[0].ents.push(walk::RawEnt {
            name_off,
            name_len: name.len() as u16,
            kind: e.kind,
            size: e.size,
            mtime: e.mtime,
            child: e.child,
        });
    }
    base.extend(extra);
}

fn compact(shared: &Shared) {
    let t = Instant::now();
    let (ls, eid, synced) = {
        let g = shared.live.read();
        let live = g.as_ref().unwrap();
        (live.to_listings(), live.event_id, live.synced_at)
    };
    let idx = Index::build(ls, eid, synced, shared.home.as_bytes());
    let path = shared.dir.join("index.bin");
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    let idx = Index::load(&path).unwrap_or(idx);
    let n = idx.n;
    *shared.live.write() = Some(Live::new(idx));
    log(format!("compacted to {n} entries in {:.2?}", t.elapsed()));
}

fn apply_loop(shared: &Arc<Shared>, rx: Receiver<Vec<DirEv>>) {
    let mut last_save = Instant::now();
    loop {
        let mut events = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        events.extend(rx.try_iter().flatten());
        if !events.is_empty() {
            let mut dirs: HashMap<Vec<u8>, bool> = HashMap::new();
            for e in events {
                *dirs.entry(e.path).or_default() |= e.recursive;
            }
            let fetched: Vec<_> = {
                let g = shared.live.read();
                let live = g.as_ref().unwrap();
                dirs.iter().map(|(p, rec)| live.fetch(p, *rec)).collect()
            };
            let mut trees = Vec::new();
            {
                let mut g = shared.live.write();
                let live = g.as_mut().unwrap();
                for f in fetched {
                    let _ = live.apply(f);
                }
                trees.append(&mut live.trees);
                live.synced_at = crate::query::now_secs();
            }
            let (rec, flat): (Vec<_>, Vec<_>) = dirs.into_iter().partition(|(_, r)| *r);
            trees.extend(rec.into_iter().map(|(p, _)| p));
            let _ = shared.content_tx.send((flat.into_iter().map(|(p, _)| p).collect(), trees));
        }
        if let Some(l) = shared.live.read().as_ref() {
            l.names_cache.trim_if_idle(Duration::from_secs(60));
        }
        if !shared.owner.load(Ordering::Relaxed) {
            continue;
        }
        let (pending, stale) = {
            let g = shared.live.read();
            let live = g.as_ref().unwrap();
            (live.over.len() + live.dead_count, live.event_id != live.base.event_id)
        };
        let asked = shared.save_requested.swap(false, Ordering::Relaxed);
        if asked || pending > COMPACT_PENDING || (stale && last_save.elapsed() > COMPACT_EVERY) {
            compact(shared);
            last_save = Instant::now();
        }
    }
}

pub fn default_dir() -> PathBuf {
    if let Ok(d) = std::env::var("WSEARCH_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()));
    Path::new(&base).join("wsearch")
}

