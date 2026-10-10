//! Immutable base + dead bitset + overlay of entries added since last compact.
//! Directory events are applied by listing that folder and diffing.

use crate::index::{Index, enc_size};
use crate::pathwin;
use crate::walk::{self, FLAG_MOUNT, KIND_DIR, Listing, NONE, RawEnt};
use std::collections::{BTreeMap, HashMap};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesExW, GetFileExInfoStandard,
    WIN32_FILE_ATTRIBUTE_DATA,
};

#[derive(Clone, Copy)]
pub struct OEnt {
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    pub mask: u64,
    pub prior: i8,
}

impl OEnt {
    pub fn new(path: &[u8], kind: u8, size: u64, mtime: u32) -> OEnt {
        OEnt { kind, size, mtime, mask: crate::index::name_mask(pathwin::name(path)), prior: 0 }
    }
}

pub struct Live {
    pub base: Index,
    dead: Vec<u64>,
    pub dead_count: usize,
    pub over: BTreeMap<Vec<u8>, OEnt>,
    pub event_id: u64,
    pub trees: Vec<Vec<u8>>,
    pub names_cache: crate::query::NameCache,
    pub synced_at: u32,
    priors: HashMap<Vec<u8>, i8>,
}

pub struct Fetched {
    path: Vec<u8>,
    recursive: bool,
    blocked: bool,
    attrs: Option<OEnt>,
    listing: Option<Listing>,
    scans: HashMap<Vec<u8>, Vec<Listing>>,
}

pub enum Applied {
    Done,
    Rebuild,
}

impl Live {
    pub fn new(base: Index) -> Live {
        base.prefault();
        let words = base.n.div_ceil(64);
        let event_id = base.event_id;
        let synced_at = base.synced_at;
        Live {
            base,
            dead: vec![0; words],
            dead_count: 0,
            over: BTreeMap::new(),
            event_id,
            trees: Vec::new(),
            names_cache: Default::default(),
            synced_at,
            priors: HashMap::new(),
        }
    }

    fn put(&mut self, path: Vec<u8>, mut e: OEnt) {
        let par = pathwin::parent(&path).to_vec();
        e.prior = match self.priors.get(&par) {
            Some(&p) => p,
            None => {
                let mut up = par.as_slice();
                let mut prior = 0;
                while !up.is_empty() {
                    if let Some(d) = self.base.lookup(up).and_then(|e| self.base.dir_of(e)) {
                        prior = self.base.dir_prior()[d as usize];
                        break;
                    }
                    let next = pathwin::parent(up);
                    if next == up {
                        break;
                    }
                    up = next;
                }
                self.priors.insert(par, prior);
                prior
            }
        };
        self.over.insert(path, e);
    }

    #[inline]
    pub fn is_dead(&self, i: u32) -> bool {
        self.dead[i as usize >> 6] & (1 << (i & 63)) != 0
    }

    fn kill(&mut self, i: u32) {
        let w = &mut self.dead[i as usize >> 6];
        if *w & (1 << (i & 63)) == 0 {
            *w |= 1 << (i & 63);
            self.dead_count += 1;
        }
    }

    fn kill_subtree(&mut self, e: u32) {
        self.kill(e);
        if let Some(d) = self.base.dir_of(e) {
            let (a, b) = (self.base.dir_start()[d as usize], self.base.dir_end()[d as usize]);
            for i in a..b {
                self.kill(i);
            }
        }
    }

    fn drop_over_subtree(&mut self, path: &[u8]) {
        self.over.remove(path);
        let (lo, hi) = subtree_bounds(path);
        let keys: Vec<Vec<u8>> = self.over.range(lo..hi).map(|(k, _)| k.clone()).collect();
        for k in keys {
            self.over.remove(&k);
        }
    }

    fn base_alive(&self, path: &[u8]) -> Option<u32> {
        self.base.lookup(path).filter(|&e| !self.is_dead(e))
    }

    fn remove_path(&mut self, path: &[u8]) {
        self.trees.push(path.to_vec());
        if let Some(e) = self.base_alive(path) {
            self.kill_subtree(e);
        }
        self.drop_over_subtree(path);
    }

    pub fn apply_dir(&mut self, path: &[u8], recursive: bool) -> Applied {
        let f = self.fetch(path, recursive);
        self.apply(f)
    }

    pub fn fetch(&self, path: &[u8], recursive: bool) -> Fetched {
        let p = pathwin::normalize(path);
        let mut f = Fetched { path: p, recursive, blocked: false, attrs: None, listing: None, scans: HashMap::new() };
        if walk::blocked(&f.path) {
            f.blocked = true;
            return f;
        }
        if recursive {
            f.attrs = lstat(&f.path);
            if f.attrs.is_some_and(|a| a.kind & 3 == KIND_DIR && a.kind & FLAG_MOUNT == 0) {
                f.scans.insert(f.path.clone(), walk::scan(&f.path, 4));
            }
            return f;
        }
        let Some(listing) = walk::list_one(&f.path) else {
            f.attrs = lstat(&f.path);
            return f;
        };
        let cur = self.current_children(&f.path);
        for r in &listing.ents {
            if r.kind & 3 != KIND_DIR || r.kind & FLAG_MOUNT != 0 {
                continue;
            }
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            let was_dir = match cur.get(name) {
                Some(Some(c)) => self.base.kind()[*c as usize] & 3 == KIND_DIR,
                Some(None) => self.over.get(&pathwin::join(&f.path, name)).is_some_and(|o| o.kind & 3 == KIND_DIR),
                None => false,
            };
            if !was_dir {
                let child = pathwin::join(&f.path, name);
                f.scans.insert(child.clone(), walk::scan(&child, 4));
            }
        }
        f.listing = Some(listing);
        f
    }

    fn current_children(&self, p: &[u8]) -> HashMap<Vec<u8>, Option<u32>> {
        let mut cur: HashMap<Vec<u8>, Option<u32>> = HashMap::new();
        if let Some(d) = self.base_alive(p).and_then(|e| self.base.dir_of(e)) {
            for c in self.base.children(d) {
                if !self.is_dead(c as u32) {
                    cur.insert(self.base.name(c).to_vec(), Some(c as u32));
                }
            }
        }
        let (lo, hi) = subtree_bounds(p);
        let plen = lo.len();
        for k in self.over.range(lo..hi).map(|(k, _)| k) {
            if !k[plen..].iter().any(|&b| pathwin::is_sep(b)) {
                cur.insert(k[plen..].to_vec(), None);
            }
        }
        cur
    }

    pub fn apply(&mut self, mut f: Fetched) -> Applied {
        if f.blocked {
            return Applied::Done;
        }
        let p = std::mem::take(&mut f.path);
        if f.recursive {
            self.remove_path(&p);
            if let Some(attrs) = f.attrs {
                let scan = f.scans.remove(&p);
                self.add_new(p, attrs, scan);
            }
            return Applied::Done;
        }
        let Some(listing) = f.listing else {
            if f.attrs.is_none() {
                self.remove_path(&p);
            }
            return Applied::Done;
        };
        let mut cur = self.current_children(&p);
        for r in &listing.ents {
            let name = &listing.names[r.name_off as usize..][..r.name_len as usize];
            let child = pathwin::join(&p, name);
            let now = OEnt::new(name, r.kind, r.size, r.mtime);
            match cur.remove(name) {
                None => {
                    let scan = f.scans.remove(&child);
                    self.add_new(child, now, scan);
                }
                Some(Some(c)) => {
                    let c = c as usize;
                    let was_kind = self.base.kind()[c];
                    if was_kind & 3 != now.kind & 3 {
                        self.kill_subtree(c as u32);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if now.kind & 3 != KIND_DIR && (self.base.size_raw()[c] != enc_size(now.size) || self.base.mtime()[c] != now.mtime) {
                        self.kill(c as u32);
                        self.put(child, now);
                    }
                }
                Some(None) => {
                    let old = self.over[&child];
                    if old.kind & 3 != now.kind & 3 {
                        self.drop_over_subtree(&child);
                        let scan = f.scans.remove(&child);
                        self.add_new(child, now, scan);
                    } else if (old.kind, old.size, old.mtime) != (now.kind, now.size, now.mtime) {
                        self.put(child, now);
                    }
                }
            }
        }
        for (name, c) in cur {
            let child = pathwin::join(&p, &name);
            match c {
                Some(c) => {
                    if self.base.kind()[c as usize] & 3 == KIND_DIR {
                        self.trees.push(child.clone());
                    }
                    self.kill_subtree(c);
                }
                None => {
                    if self.over.get(&child).is_some_and(|o| o.kind & 3 == KIND_DIR) {
                        self.trees.push(child.clone());
                    }
                    self.drop_over_subtree(&child);
                }
            }
        }
        Applied::Done
    }

    fn add_new(&mut self, path: Vec<u8>, e: OEnt, scan: Option<Vec<Listing>>) {
        let is_dir = e.kind & 3 == KIND_DIR && e.kind & FLAG_MOUNT == 0;
        self.put(path.clone(), e);
        if !is_dir {
            return;
        }
        self.trees.push(path.clone());
        let ls = scan.unwrap_or_else(|| walk::scan(&path, 4));
        for_each_path(&ls, &path, |p, r| {
            let e = OEnt::new(&p, r.kind, r.size, r.mtime);
            self.put(p, e);
        });
    }

    pub fn to_listings(&self) -> Vec<Listing> {
        let mut by_parent: HashMap<&[u8], Vec<(&[u8], OEnt)>> = HashMap::new();
        for (k, v) in &self.over {
            let parent = pathwin::parent(k);
            by_parent.entry(parent).or_default().push((pathwin::name(k), *v));
        }
        let mut out = Vec::new();
        let mut stack: Vec<(Option<u32>, Vec<u8>, u32)> = vec![(Some(0), Vec::new(), 0)];
        let mut next = 1u32;
        while let Some((bd, path, id)) = stack.pop() {
            let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
            let mut push = |l: &mut Listing, name: &[u8], kind: u8, size: u64, mtime: u32, child_path: Option<Vec<u8>>, bdir: Option<u32>| {
                let child = match child_path {
                    Some(cp) => {
                        let c = next;
                        next += 1;
                        stack.push((bdir, cp, c));
                        c
                    }
                    None => NONE,
                };
                l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child });
                l.names.extend_from_slice(name);
            };
            if let Some(d) = bd {
                for c in self.base.children(d) {
                    if self.is_dead(c as u32) {
                        continue;
                    }
                    let name = self.base.name(c);
                    let k = self.base.kind()[c];
                    let sub = self.base.dir_of(c as u32);
                    let cp = sub.map(|_| pathwin::join(&path, name));
                    push(&mut l, name, k, self.base.size_of(c), self.base.mtime()[c], cp, sub);
                }
            }
            if let Some(kids) = by_parent.get(path.as_slice()) {
                for &(name, o) in kids {
                    let descend = o.kind & 3 == KIND_DIR && o.kind & FLAG_MOUNT == 0;
                    push(&mut l, name, o.kind, o.size, o.mtime, descend.then(|| pathwin::join(&path, name)), None);
                }
            }
            out.push(l);
        }
        out
    }
}

pub fn for_each_path(ls: &[Listing], root: &[u8], mut f: impl FnMut(Vec<u8>, &RawEnt)) {
    let mut by_id: HashMap<u32, &Listing> = HashMap::with_capacity(ls.len());
    for l in ls {
        by_id.insert(l.id, l);
    }
    let mut stack = vec![(0u32, root.to_vec())];
    while let Some((id, path)) = stack.pop() {
        let Some(l) = by_id.get(&id) else { continue };
        for r in &l.ents {
            let p = pathwin::join(&path, &l.names[r.name_off as usize..][..r.name_len as usize]);
            if r.child != NONE {
                stack.push((r.child, p.clone()));
            }
            f(p, r);
        }
    }
}

fn subtree_bounds(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lo = pathwin::join(path, b"");
    let mut hi = lo.clone();
    if let Some(last) = hi.last_mut() {
        *last = last.saturating_add(1);
    }
    (lo, hi)
}

pub fn lstat(path: &[u8]) -> Option<OEnt> {
    let w = pathwin::long_wide(path);
    let mut info: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    if unsafe { GetFileAttributesExW(w.as_ptr(), GetFileExInfoStandard, &mut info as *mut _ as *mut _) } == 0 {
        return None;
    }
    let attr = info.dwFileAttributes;
    let mut kind = if attr & FILE_ATTRIBUTE_DIRECTORY != 0 { KIND_DIR } else { crate::walk::KIND_FILE };
    if attr & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        kind = if attr & FILE_ATTRIBUTE_DIRECTORY != 0 { KIND_DIR | FLAG_MOUNT } else { crate::walk::KIND_LINK };
    }
    if attr & FILE_ATTRIBUTE_HIDDEN != 0 {
        kind |= crate::walk::FLAG_HIDDEN;
    }
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    let mtime = walk::filetime_unix(info.ftLastWriteTime.dwLowDateTime, info.ftLastWriteTime.dwHighDateTime);
    Some(OEnt::new(path, kind, size, mtime))
}

