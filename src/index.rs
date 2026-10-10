//! Flat mmap'd name index. Entries laid out directory-block by directory-block
//! in DFS order so `in:` is a range. Distinct names interned and scored once.

use crate::pathwin;
use crate::walk::{KIND_DIR, Listing, NONE};
use memmap2::{Mmap, MmapMut};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

const MAGIC: &[u8; 8] = b"WSIDX001";

#[derive(Clone, Copy)]
enum Sec {
    NameMask,
    NameOff,
    Names,
    EntName,
    Kind,
    Parent,
    Size,
    Mtime,
    DirEntry,
    DirStart,
    DirLen,
    DirEnd,
    DirPrior,
    DirParent,
    NameEntsOff,
    NameEnts,
}
const NSEC: usize = 16;

pub struct Index {
    map: Mmap,
    pub n: usize,
    pub d: usize,
    pub u: usize,
    names_len: usize,
    u1: usize,
    pub event_id: u64,
    pub synced_at: u32,
    off: [usize; NSEC],
    plan: std::sync::OnceLock<MemoPlan>,
}

pub struct MemoPlan {
    pub upper: Vec<u32>,
    pub chunks: Vec<(u32, std::ops::Range<u32>)>,
}

macro_rules! sec {
    ($name:ident, $s:expr, $t:ty, $len:ident) => {
        pub fn $name(&self) -> &[$t] {
            unsafe { std::slice::from_raw_parts(self.map.as_ptr().add(self.off[$s as usize]) as *const $t, self.$len) }
        }
    };
}
pub(crate) use sec;

impl Index {
    sec!(name_mask, Sec::NameMask, u64, u);
    sec!(name_off, Sec::NameOff, u32, u1);
    sec!(names, Sec::Names, u8, names_len);
    sec!(ent_name, Sec::EntName, u32, n);
    sec!(kind, Sec::Kind, u8, n);
    sec!(parent, Sec::Parent, u32, n);
    sec!(size_raw, Sec::Size, u32, n);
    sec!(mtime, Sec::Mtime, u32, n);
    sec!(dir_entry, Sec::DirEntry, u32, d);
    sec!(dir_start, Sec::DirStart, u32, d);
    sec!(dir_len, Sec::DirLen, u32, d);
    sec!(dir_end, Sec::DirEnd, u32, d);
    sec!(dir_prior, Sec::DirPrior, i8, d);
    sec!(dir_parent, Sec::DirParent, u32, d);
    sec!(name_ents_off, Sec::NameEntsOff, u32, u1);
    sec!(name_ents, Sec::NameEnts, u32, n);

    pub fn uname(&self, id: u32) -> &[u8] {
        let o = self.name_off();
        &self.names()[o[id as usize] as usize..o[id as usize + 1] as usize]
    }

    pub fn name(&self, i: usize) -> &[u8] {
        self.uname(self.ent_name()[i])
    }

    pub fn size_of(&self, i: usize) -> u64 {
        dec_size(self.size_raw()[i])
    }

    pub fn dir_of(&self, entry: u32) -> Option<u32> {
        self.dir_entry().binary_search(&entry).ok().map(|d| d as u32)
    }

    pub fn children(&self, d: u32) -> std::ops::Range<usize> {
        let s = self.dir_start()[d as usize] as usize;
        s..s + self.dir_len()[d as usize] as usize
    }

    pub fn descendants(&self, k: u32) -> std::ops::Range<u32> {
        let de = self.dir_entry();
        let (s, e) = (self.dir_start()[k as usize], self.dir_end()[k as usize]);
        de.partition_point(|&x| x < s) as u32..de.partition_point(|&x| x < e) as u32
    }

    pub fn memo_plan(&self) -> &MemoPlan {
        self.plan.get_or_init(|| {
            const CHUNK: u32 = 4096;
            let de = self.dir_entry();
            let mut p = MemoPlan { upper: Vec::new(), chunks: Vec::new() };
            let mut big = vec![0u32];
            while let Some(k) = big.pop() {
                let (s, l) = (self.dir_start()[k as usize], self.dir_len()[k as usize]);
                let kids = de.partition_point(|&x| x < s) as u32..de.partition_point(|&x| x < s + l) as u32;
                for c in kids {
                    p.upper.push(c);
                    let r = self.descendants(c);
                    if r.end - r.start > CHUNK {
                        big.push(c);
                    } else if r.start < r.end {
                        p.chunks.push((c, r));
                    }
                }
            }
            p.chunks.sort_by_key(|c| c.1.start);
            p
        })
    }

    pub fn path(&self, i: usize, out: &mut Vec<u8>) {
        out.clear();
        let mut chain = [0u32; 256];
        let mut k = 0;
        let mut e = i as u32;
        while e != 0 && k < chain.len() {
            chain[k] = e;
            k += 1;
            e = self.dir_entry()[self.parent()[e as usize] as usize];
        }
        for j in (0..k).rev() {
            let nm = self.name(chain[j] as usize);
            if out.is_empty() {
                out.extend_from_slice(nm);
            } else {
                if out.last() != Some(&b':') {
                    out.push(pathwin::SEP);
                } else {
                    out.push(pathwin::SEP);
                }
                out.extend_from_slice(nm);
            }
        }
    }

    pub fn lookup(&self, path: &[u8]) -> Option<u32> {
        let p = pathwin::normalize(path);
        let mut e = 0u32;
        for comp in pathwin::split(&p) {
            let r = self.children(self.dir_of(e)?);
            let (mut lo, mut hi) = (r.start, r.end);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if self.name(mid) < comp {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            e = if lo < r.end && self.name(lo) == comp {
                lo as u32
            } else {
                r.clone().find(|&c| self.name(c).eq_ignore_ascii_case(comp))? as u32
            };
        }
        Some(e)
    }

    pub fn build(mut ls: Vec<Listing>, event_id: u64, synced_at: u32, home: &[u8]) -> Index {
        ls.par_iter_mut().for_each(|l| {
            let names = &l.names;
            l.ents.sort_unstable_by(|a, b| names[a.name_off as usize..][..a.name_len as usize].cmp(&names[b.name_off as usize..][..b.name_len as usize]));
        });
        let max_id = ls.iter().map(|l| l.id).max().unwrap_or(0) as usize;
        let mut by_id = vec![NONE; max_id + 1];
        for (i, l) in ls.iter().enumerate() {
            by_id[l.id as usize] = i as u32;
        }
        let n_max = 1 + ls.iter().map(|l| l.ents.len()).sum::<usize>();
        let names_max: usize = ls.iter().map(|l| l.names.len()).sum();

        let mut name_off = vec![0u32; n_max];
        let mut name_len = vec![0u16; n_max];
        let mut kind = vec![0u8; n_max];
        let mut parent = vec![0u32; n_max];
        let mut size = vec![0u64; n_max];
        let mut mtime = vec![0u32; n_max];
        let mut names = Vec::with_capacity(names_max);
        kind[0] = KIND_DIR;
        let mut pos = 1usize;
        let mut blocks: Vec<(u32, u32, u32)> = Vec::with_capacity(ls.len());
        let mut stack: Vec<(u32, u32)> = vec![(0, by_id[0])];
        let mut kids: Vec<(u32, u32)> = Vec::new();
        while let Some((e, li)) = stack.pop() {
            if li == NONE {
                continue;
            }
            let l = &ls[li as usize];
            let start = pos;
            kids.clear();
            for r in &l.ents {
                name_off[pos] = names.len() as u32;
                name_len[pos] = r.name_len;
                names.extend_from_slice(&l.names[r.name_off as usize..][..r.name_len as usize]);
                kind[pos] = r.kind;
                size[pos] = r.size;
                mtime[pos] = r.mtime;
                parent[pos] = e;
                if let Some(&cli) = by_id.get(r.child as usize).filter(|&&cli| cli != NONE) {
                    kids.push((pos as u32, cli));
                }
                pos += 1;
            }
            blocks.push((e, start as u32, (pos - start) as u32));
            stack.extend(kids.iter().rev());
        }
        drop(ls);
        let n = pos;
        blocks.sort_unstable();
        let d = blocks.len();
        let mut dir_of = vec![NONE; n];
        for (k, b) in blocks.iter().enumerate() {
            dir_of[b.0 as usize] = k as u32;
        }
        parent[..n].par_iter_mut().for_each(|p| *p = dir_of[*p as usize]);
        parent[0] = 0;
        drop(dir_of);

        let mut ids: HashMap<&[u8], u32, Fx> = HashMap::with_capacity_and_hasher(n / 3, Fx);
        let mut ent_name = vec![0u32; n];
        let mut uoff = vec![0u32];
        let mut unames: Vec<u8> = Vec::with_capacity(names.len() / 2);
        for i in 0..n {
            let nm = &names[name_off[i] as usize..][..name_len[i] as usize];
            let next = ids.len() as u32;
            let id = *ids.entry(nm).or_insert_with(|| {
                unames.extend_from_slice(nm);
                uoff.push(unames.len() as u32);
                next
            });
            ent_name[i] = id;
        }
        let u = ids.len();
        drop(ids);
        let mut umask = vec![0u64; u];
        umask.par_iter_mut().enumerate().for_each(|(k, m)| *m = name_mask(&unames[uoff[k] as usize..uoff[k + 1] as usize]));
        let enc: Vec<u32> = size[..n].iter().map(|&s| enc_size(s)).collect();
        let mut ne_off = vec![0u32; u + 1];
        for &id in &ent_name {
            ne_off[id as usize + 1] += 1;
        }
        for k in 0..u {
            ne_off[k + 1] += ne_off[k];
        }
        let mut fill = ne_off[..u].to_vec();
        let mut ne = vec![0u32; n];
        for (i, &id) in ent_name.iter().enumerate() {
            ne[fill[id as usize] as usize] = i as u32;
            fill[id as usize] += 1;
        }
        drop(fill);

        let dir_entry: Vec<u32> = blocks.iter().map(|b| b.0).collect();
        let mut end: Vec<u32> = blocks.iter().map(|b| b.1 + b.2).collect();
        for k in (1..d).rev() {
            let p = parent[dir_entry[k] as usize] as usize;
            end[p] = end[p].max(end[k]);
        }
        let comps: Vec<&[u8]> = pathwin::split(home);
        let mut prior = vec![0i8; d];
        let mut depth = vec![0u8; d];
        let mut hm = vec![0u8; d];
        for k in 1..d {
            let e = dir_entry[k] as usize;
            let p = parent[e] as usize;
            let nm = &names[name_off[e] as usize..][..name_len[e] as usize];
            depth[k] = depth[p].saturating_add(1);
            hm[k] = match hm[p] {
                u8::MAX => u8::MAX,
                h if h as usize >= comps.len() => h,
                h if comps[h as usize].eq_ignore_ascii_case(nm) => h + 1,
                _ => u8::MAX,
            };
            let entered_home = hm[k] as usize == comps.len() && (hm[p] as usize) < comps.len();
            let adj = prior_adjust(nm, depth[k]) + if entered_home { 15 } else { 0 };
            prior[k] = (prior[p] as i32 + adj).clamp(-100, 60) as i8;
        }

        let (off, total) = layout(&section_lens(n, d, u, unames.len()));
        let mut m = MmapMut::map_anon(total).expect("anon map");
        let mut put = |s: Sec, v: &[u8]| m[off[s as usize]..][..v.len()].copy_from_slice(v);
        put(Sec::NameMask, as_bytes(&umask));
        put(Sec::NameOff, as_bytes(&uoff));
        put(Sec::Names, &unames);
        put(Sec::EntName, as_bytes(&ent_name));
        put(Sec::Kind, &kind[..n]);
        put(Sec::Parent, as_bytes(&parent[..n]));
        put(Sec::Size, as_bytes(&enc));
        put(Sec::Mtime, as_bytes(&mtime[..n]));
        put(Sec::DirEntry, as_bytes(&dir_entry));
        put(Sec::DirStart, as_bytes(&blocks.iter().map(|b| b.1).collect::<Vec<_>>()));
        put(Sec::DirLen, as_bytes(&blocks.iter().map(|b| b.2).collect::<Vec<_>>()));
        put(Sec::DirEnd, as_bytes(&end));
        put(Sec::DirPrior, as_bytes(&prior));
        put(Sec::DirParent, as_bytes(&dir_entry.iter().map(|&e| parent[e as usize]).collect::<Vec<_>>()));
        put(Sec::NameEntsOff, as_bytes(&ne_off));
        put(Sec::NameEnts, as_bytes(&ne));
        m[..HDR].copy_from_slice(&header(MAGIC, &[n as u64, d as u64, u as u64, unames.len() as u64, event_id, synced_at as u64]));
        Index::from_map(m.make_read_only().unwrap()).unwrap()
    }

    fn from_map(map: Mmap) -> Option<Index> {
        let [n, d, u, names_len, event_id, synced_at] = fields(&map, MAGIC)?.map(|v| v as usize);
        let (off, total) = layout(&section_lens(n, d, u, names_len));
        if map.len() < total {
            return None;
        }
        Some(Index { n, d, u, u1: u + 1, names_len, event_id: event_id as u64, synced_at: synced_at as u32, off, map, plan: std::sync::OnceLock::new() })
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&header(MAGIC, &[self.n as u64, self.d as u64, self.u as u64, self.names_len as u64, self.event_id, self.synced_at as u64]))?;
        f.write_all(&self.map[HDR..])?;
        f.sync_data()?;
        std::fs::rename(tmp, path)
    }

    pub fn saved_event_id(path: &Path) -> Option<u64> {
        use std::io::Read;
        let mut h = [0u8; 56];
        std::fs::File::open(path).ok()?.read_exact(&mut h).ok()?;
        fields::<6>(&h, MAGIC).map(|f| f[4])
    }

    pub fn load(path: &Path) -> Option<Index> {
        let f = std::fs::File::open(path).ok()?;
        Index::from_map(unsafe { Mmap::map(&f) }.ok()?)
    }

    pub fn bytes(&self) -> usize {
        self.map.len()
    }

    pub fn prefault(&self) {
        unsafe {
            use windows_sys::Win32::System::Memory::{PrefetchVirtualMemory, WIN32_MEMORY_RANGE_ENTRY};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut range = WIN32_MEMORY_RANGE_ENTRY { VirtualAddress: self.map.as_ptr() as *mut _, NumberOfBytes: self.map.len() };
            let _ = PrefetchVirtualMemory(GetCurrentProcess(), 1, &mut range, 0);
        }
        let mut sum = 0u8;
        let p = self.map.as_ptr();
        for i in (0..self.map.len()).step_by(4096) {
            sum = sum.wrapping_add(unsafe { std::ptr::read_volatile(p.add(i)) });
        }
        std::hint::black_box(sum);
    }
}

pub(crate) const HDR: usize = 4096;

pub(crate) fn header(magic: &[u8; 8], fields: &[u64]) -> Vec<u8> {
    let mut h = vec![0u8; HDR];
    h[..8].copy_from_slice(magic);
    for (k, v) in fields.iter().enumerate() {
        h[8 + k * 8..16 + k * 8].copy_from_slice(&v.to_le_bytes());
    }
    h
}

pub(crate) fn fields<const N: usize>(b: &[u8], magic: &[u8; 8]) -> Option<[u64; N]> {
    (b.len() >= 8 + N * 8 && &b[..8] == magic).then(|| std::array::from_fn(|k| u64::from_le_bytes(b[8 + k * 8..16 + k * 8].try_into().unwrap())))
}

pub(crate) fn layout<const N: usize>(lens: &[usize; N]) -> ([usize; N], usize) {
    let mut off = [0usize; N];
    let mut at = HDR;
    for (k, &l) in lens.iter().enumerate() {
        off[k] = at;
        at = (at + l + 63) & !63;
    }
    (off, at)
}

pub(crate) fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn section_lens(n: usize, d: usize, u: usize, names_len: usize) -> [usize; NSEC] {
    [u * 8, (u + 1) * 4, names_len, n * 4, n, n * 4, n * 4, n * 4, d * 4, d * 4, d * 4, d * 4, d, d * 4, (u + 1) * 4, n * 4]
}

pub fn enc_size(s: u64) -> u32 {
    if s < 1 << 31 { s as u32 } else { (1 << 31) | (s >> 21).min((1 << 31) - 1) as u32 }
}

pub fn dec_size(v: u32) -> u64 {
    if v & (1 << 31) == 0 { v as u64 } else { ((v & !(1 << 31)) as u64) << 21 }
}

#[derive(Clone, Copy, Default)]
pub struct Fx;
pub struct FxH(u64);
impl std::hash::BuildHasher for Fx {
    type Hasher = FxH;
    fn build_hasher(&self) -> FxH {
        FxH(0)
    }
}
impl std::hash::Hasher for FxH {
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(w)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

pub fn char_mask(s: &[u8]) -> u64 {
    s.iter().fold(0, |m, &b| m | char_bit(b))
}

pub fn name_mask(s: &[u8]) -> u64 {
    let off = (s.len() > 1 && s[0] == b'.') as usize;
    let starts = s.get(off).into_iter().chain(s.windows(2).filter(|w| w[0] == b' ').map(|w| &w[1]));
    starts.fold(char_mask(s), |m, &b| m | start_bit(b))
}

#[inline]
pub fn start_bit(b: u8) -> u64 {
    1 << (41 + b.to_ascii_lowercase() % 23)
}

#[inline]
pub fn char_bit(b: u8) -> u64 {
    match b {
        b'a'..=b'z' => 1 << (b - b'a'),
        b'A'..=b'Z' => 1 << (b - b'A'),
        b'0'..=b'9' => 1 << (26 + b - b'0'),
        b'.' => 1 << 36,
        b'-' | b'_' => 1 << 37,
        b' ' => 1 << 38,
        0x80.. => 1 << 39,
        _ => 1 << 40,
    }
}

fn prior_adjust(name: &[u8], depth: u8) -> i32 {
    if depth == 1 {
        return 0;
    }
    if name.first() == Some(&b'.') || name.first() == Some(&b'$') {
        return -25;
    }
    match name {
        b"Windows" | b"WinSxS" | b"System32" | b"SysWOW64" => -40,
        b"Program Files" | b"Program Files (x86)" | b"ProgramData" => -25,
        b"Users" => 8,
        b"Desktop" | b"Documents" | b"Downloads" | b"Projects" | b"src" | b"dev" | b"code" => 12,
        b"AppData" | b"Application Data" => -15,
        b"node_modules" | b"__pycache__" | b"site-packages" | b".git" | b".svn" => -30,
        b"target" | b"build" | b"dist" | b"out" | b"vendor" | b"tmp" | b"temp" | b"Cache" | b"cache" => -12,
        b"$Recycle.Bin" | b"System Volume Information" | b"Recovery" => -50,
        _ => 0,
    }
}
