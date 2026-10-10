//! Trigram content index over the user's text files. Candidates are read
//! fresh from disk so hits are never stale.

use crate::index::{HDR, as_bytes, fields, header, layout, sec};
use crate::live::Live;
use crate::pathwin;
use crate::query::{GrepMode, Query, fold};
use crate::walk::KIND_FILE;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const MAX_FILE: u64 = 1 << 20;
const SEG_BYTES: u64 = 64 << 20;
const MERGE_CAP: usize = 96 << 20;
const MAGIC: &[u8; 8] = b"WSCSEG01";
const BITSET: u32 = 1 << 31;

#[rustfmt::skip]
const SKIP_DIRS: &[&[u8]] = &[
    b"node_modules", b".git", b"target", b"__pycache__", b".venv", b"venv", b"site-packages",
    b".next", b".turbo", b".cache", b"dist", b"build", b".build", b".rustup", b".cargo", b".npm",
    b".bun", b"vendor", b"coverage", b".svn", b".hg", b".gradle", b".m2", b"WinSxS", b"Windows",
];

#[rustfmt::skip]
const SKIP_UNDER_HOME: &[&[u8]] = &[
    b"AppData\\Local\\Temp", b"AppData\\Local\\Microsoft", b"AppData\\Roaming\\Microsoft",
    b"AppData\\Local\\Packages", b".cargo\\registry", b".rustup",
];

#[rustfmt::skip]
const TEXT_EXTS: &[&[u8]] = &[
    b"rs", b"c", b"h", b"cc", b"cpp", b"cxx", b"hpp", b"hh", b"cs", b"go", b"py", b"pyi", b"js", b"mjs",
    b"cjs", b"ts", b"tsx", b"jsx", b"java", b"kt", b"rb", b"php", b"sh", b"ps1", b"bat", b"cmd", b"lua",
    b"sql", b"html", b"htm", b"css", b"scss", b"json", b"jsonc", b"yaml", b"yml", b"toml", b"xml", b"md",
    b"txt", b"csv", b"tsv", b"ini", b"cfg", b"conf", b"env", b"proto", b"graphql", b"cmake", b"mk",
    b"log", b"diff", b"patch", b"svg", b"pl", b"vim", b"el", b"asm", b"gitignore", b"editorconfig",
    b"lock", b"sum", b"dockerfile",
];

const DEFINES: &str = "fn|func|function|def|class|struct|enum|trait|interface|type|impl|let|const|var|val|static|module|mod|macro_rules!|define|typedef|union|namespace";

pub struct Segment {
    map: Mmap,
    pub id: u64,
    pub ndocs: usize,
    ndocs1: usize,
    ntri: usize,
    ntri1: usize,
    plen: usize,
    paths_len: usize,
    off: [usize; NS],
    dead: Vec<u64>,
    pub live_docs: usize,
}

#[derive(Clone, Copy)]
enum S {
    TriKey,
    TriOff,
    Post,
    PathOff,
    Paths,
    Size,
    Mtime,
    ByPath,
    Rank,
}
const NS: usize = 9;

fn lens(ndocs: usize, ntri: usize, plen: usize, paths_len: usize) -> [usize; NS] {
    [ntri * 4, (ntri + 1) * 4, plen, (ndocs + 1) * 4, paths_len, ndocs * 8, ndocs * 4, ndocs * 4, ndocs]
}

impl Segment {
    sec!(tri_key, S::TriKey, u32, ntri);
    sec!(tri_off, S::TriOff, u32, ntri1);
    sec!(post, S::Post, u8, plen);
    sec!(path_off, S::PathOff, u32, ndocs1);
    sec!(paths, S::Paths, u8, paths_len);
    sec!(size, S::Size, u64, ndocs);
    sec!(mtime, S::Mtime, u32, ndocs);
    sec!(by_path, S::ByPath, u32, ndocs);
    sec!(rank, S::Rank, i8, ndocs);

    pub fn path(&self, d: u32) -> &[u8] {
        let o = self.path_off();
        &self.paths()[o[d as usize] as usize..o[d as usize + 1] as usize]
    }

    #[inline]
    pub fn is_dead(&self, d: u32) -> bool {
        self.dead[d as usize >> 6] & (1 << (d & 63)) != 0
    }

    fn kill(&mut self, d: u32) -> bool {
        let w = &mut self.dead[d as usize >> 6];
        if *w & (1 << (d & 63)) != 0 {
            return false;
        }
        *w |= 1 << (d & 63);
        self.live_docs -= 1;
        true
    }

    fn with_prefix<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = u32> + 'a {
        let bp = self.by_path();
        let start = bp.partition_point(|&d| self.path(d) < prefix);
        bp[start..].iter().copied().take_while(move |&d| self.path(d).starts_with(prefix)).filter(move |&d| !self.is_dead(d))
    }

    fn direct_children(&self, prefix: &[u8]) -> Vec<u32> {
        let bp = self.by_path();
        let mut out = Vec::new();
        let mut i = bp.partition_point(|&d| self.path(d) < prefix);
        while i < bp.len() {
            let p = self.path(bp[i]);
            if !p.starts_with(prefix) {
                break;
            }
            match p[prefix.len()..].iter().position(|&b| pathwin::is_sep(b)) {
                None => {
                    if !self.is_dead(bp[i]) {
                        out.push(bp[i]);
                    }
                    i += 1;
                }
                Some(k) => {
                    let mut hi = p[..prefix.len() + k + 1].to_vec();
                    *hi.last_mut().unwrap() = pathwin::SEP + 1;
                    i += bp[i..].partition_point(|&d| self.path(d) < hi.as_slice());
                }
            }
        }
        out
    }

    fn postings(&self, tri: u32) -> Vec<u32> {
        let mut out = Vec::new();
        if let Ok(k) = self.tri_key().binary_search(&tri) {
            self.list_into(k, &mut out);
        }
        out
    }

    fn list_into(&self, k: usize, out: &mut Vec<u32>) {
        let o = self.tri_off();
        let mut bytes = &self.post()[(o[k] & !BITSET) as usize..(o[k + 1] & !BITSET) as usize];
        if o[k] & BITSET != 0 {
            for (w, &b) in bytes.iter().enumerate() {
                let mut b = b;
                while b != 0 {
                    out.push(w as u32 * 8 + b.trailing_zeros());
                    b &= b - 1;
                }
            }
            return;
        }
        let mut last = 0u32;
        while !bytes.is_empty() {
            let (v, n) = varint(bytes);
            bytes = &bytes[n..];
            last += v;
            out.push(last);
        }
    }

    fn load(dir: &Path, id: u64) -> Option<Segment> {
        let f = std::fs::File::open(seg_path(dir, id)).ok()?;
        let map = unsafe { Mmap::map(&f) }.ok()?;
        let [ndocs, ntri, plen, paths_len] = fields(&map, MAGIC)?.map(|v| v as usize);
        let (off, total) = layout(&lens(ndocs, ntri, plen, paths_len));
        if map.len() < total {
            return None;
        }
        let mut dead = vec![0u64; ndocs.div_ceil(64)];
        if let Ok(b) = std::fs::read(dead_path(dir, id)) {
            for (w, c) in dead.iter_mut().zip(b.chunks_exact(8)) {
                *w = u64::from_le_bytes(c.try_into().unwrap());
            }
        }
        let live_docs = ndocs - dead.iter().map(|w| w.count_ones() as usize).sum::<usize>();
        Some(Segment { map, id, ndocs, ndocs1: ndocs + 1, ntri, ntri1: ntri + 1, plen, paths_len, off, dead, live_docs })
    }

    fn save_dead(&self, dir: &Path) {
        let bytes: Vec<u8> = self.dead.iter().flat_map(|w| w.to_le_bytes()).collect();
        let tmp = dead_path(dir, self.id).with_extension("tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(tmp, dead_path(dir, self.id));
        }
    }
}

fn seg_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:06}.fsc"))
}
fn dead_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("seg-{id:06}.dead"))
}

#[inline]
fn varint(b: &[u8]) -> (u32, usize) {
    let mut v = 0u32;
    for (i, &x) in b.iter().enumerate().take(5) {
        v |= ((x & 0x7f) as u32) << (7 * i);
        if x & 0x80 == 0 {
            return (v, i + 1);
        }
    }
    (v, b.len().min(5))
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn trigrams(buf: &[u8], seen: &mut [u64], out: &mut Vec<u32>) {
    let start = out.len();
    if buf.len() < 3 {
        return;
    }
    let mut t = (fold(buf[0]) as u32) << 8 | fold(buf[1]) as u32;
    for &b in &buf[2..] {
        t = ((t << 8) | fold(b) as u32) & 0xFF_FFFF;
        let (w, bit) = ((t >> 6) as usize, 1u64 << (t & 63));
        if seen[w] & bit == 0 {
            seen[w] |= bit;
            out.push(t);
        }
    }
    for &t in &out[start..] {
        seen[(t >> 6) as usize] = 0;
    }
    out[start..].sort_unstable();
}

fn trigrams_small(s: &[u8]) -> Vec<u32> {
    let mut t: Vec<u32> = s.windows(3).map(|w| (fold(w[0]) as u32) << 16 | (fold(w[1]) as u32) << 8 | fold(w[2]) as u32).collect();
    t.sort_unstable();
    t.dedup();
    t
}

fn symbol_key(name: &[u8]) -> u32 {
    let h = name.iter().fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    h | 0x8000_0000
}

fn plain_identifier(name: &[u8]) -> bool {
    name.first().is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_') && name.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

fn symbols(buf: &[u8], out: &mut Vec<u32>) {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        RegexBuilder::new(&format!(r"(?-u:\b)(?:{DEFINES})(?:<[^>\n]*>)?[ \t*&]+(?:mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*")).unicode(false).build().unwrap()
    });
    let mut keys = Vec::new();
    let mut at = 0;
    while let Some(m) = RE.find_at(buf, at) {
        let end = m.end();
        let begin = buf[..end].iter().rposition(|&b| !(b.is_ascii_alphanumeric() || b == b'_')).map_or(0, |p| p + 1);
        keys.push(symbol_key(&buf[begin..end]));
        at = begin + 1;
    }
    keys.sort_unstable();
    keys.dedup();
    out.extend(keys);
}

#[derive(Default)]
pub struct Docs {
    buf: Vec<u8>,
    items: Vec<(u32, u32, u64, u32)>,
}

impl Docs {
    fn push(&mut self, path: &[u8], size: u64, mtime: u32) {
        self.items.push((self.buf.len() as u32, path.len() as u32, size, mtime));
        self.buf.extend_from_slice(path);
    }
    fn path(&self, i: usize) -> &[u8] {
        let (o, l, _, _) = self.items[i];
        &self.buf[o as usize..(o + l) as usize]
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    fn sort(&mut self) {
        let buf = &self.buf;
        self.items.sort_by(|a, b| buf[a.0 as usize..(a.0 + a.1) as usize].cmp(&buf[b.0 as usize..(b.0 + b.1) as usize]));
        self.items.dedup_by(|a, b| buf[a.0 as usize..(a.0 + a.1) as usize] == buf[b.0 as usize..(b.0 + b.1) as usize]);
    }
    fn find(&self, path: &[u8]) -> Option<usize> {
        let i = self.items.partition_point(|&(o, l, _, _)| &self.buf[o as usize..(o + l) as usize] < path);
        (i < self.items.len() && self.path(i) == path).then_some(i)
    }
    pub fn batches(&self) -> Vec<std::ops::Range<usize>> {
        let (mut out, mut start, mut bytes) = (Vec::new(), 0, 0u64);
        for (i, it) in self.items.iter().enumerate() {
            if bytes >= SEG_BYTES {
                out.push(start..i);
                (start, bytes) = (i, 0);
            }
            bytes += it.2;
        }
        if start < self.items.len() {
            out.push(start..self.items.len());
        }
        out
    }
}

const NOT_TEXT: i8 = i8::MIN;

struct DocMeta<'a> {
    path: &'a [u8],
    size: u64,
    mtime: u32,
    rank: i8,
}

struct Split {
    seen: Vec<u64>,
    buf: Vec<u8>,
    flat: Vec<u32>,
    syms: Vec<u32>,
    docs: Vec<(usize, u32, u32, bool, u32, u32)>,
}

pub fn build_segment(dir: &Path, id: u64, docs: &Docs, range: std::ops::Range<usize>) -> Option<Segment> {
    use std::io::Read;
    let splits: Vec<Split> = range
        .clone()
        .into_par_iter()
        .with_min_len(256)
        .fold(
            || Split { seen: vec![0u64; (1 << 24) / 64], buf: Vec::new(), flat: Vec::new(), syms: Vec::new(), docs: Vec::new() },
            |mut sp, i| {
                sp.buf.clear();
                let p = String::from_utf8_lossy(docs.path(i));
                let ok = std::fs::File::open(p.as_ref())
                    .ok()
                    .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut sp.buf).ok())
                    .is_some_and(|n| n as u64 <= MAX_FILE && memchr::memchr(0, &sp.buf[..n.min(8192)]).is_none());
                let (start, sym) = (sp.flat.len() as u32, sp.syms.len() as u32);
                if ok {
                    trigrams(&sp.buf, &mut sp.seen, &mut sp.flat);
                    symbols(&sp.buf, &mut sp.syms);
                }
                sp.docs.push((i, start, sp.flat.len() as u32 - start, ok, sym, sp.syms.len() as u32 - sym));
                sp
            },
        )
        .map(|mut sp| {
            sp.seen = Vec::new();
            sp.buf = Vec::new();
            sp
        })
        .collect();
    let order: Vec<(usize, usize)> = splits.iter().enumerate().flat_map(|(si, sp)| (0..sp.docs.len()).map(move |k| (si, k))).collect();
    let meta: Vec<DocMeta> = order
        .iter()
        .map(|&(si, k)| {
            let (i, _, _, text, _, _) = splits[si].docs[k];
            let (_, _, size, mtime) = docs.items[i];
            DocMeta { path: docs.path(i), size, mtime, rank: if text { doc_rank(docs.path(i)) } else { NOT_TEXT } }
        })
        .collect();
    let tris = |d: usize| {
        let (si, k) = order[d];
        let (_, st, len, _, _, _) = splits[si].docs[k];
        &splits[si].flat[st as usize..(st + len) as usize]
    };
    let syms = |d: usize| {
        let (si, k) = order[d];
        let (_, _, _, _, st, len) = splits[si].docs[k];
        &splits[si].syms[st as usize..(st + len) as usize]
    };
    let mut v: Vec<u64> = Vec::new();
    for d in 0..order.len() {
        v.extend(tris(d).iter().map(|&t| (t as u64) << 32 | d as u64));
        v.extend(syms(d).iter().map(|&t| (t as u64) << 32 | d as u64));
    }
    v.sort_unstable();
    let mut i = 0;
    write_segment(dir, id, &meta, |list| {
        let t = (*v.get(i)? >> 32) as u32;
        while i < v.len() && (v[i] >> 32) as u32 == t {
            list.push(v[i] as u32);
            i += 1;
        }
        Some(t)
    })
}

pub fn merge(dir: &Path, id: u64, segs: &[&Segment]) -> Option<Segment> {
    let mut meta = Vec::new();
    let mut remap: Vec<Vec<u32>> = Vec::with_capacity(segs.len());
    for s in segs {
        let mut r = vec![u32::MAX; s.ndocs];
        for d in 0..s.ndocs as u32 {
            if !s.is_dead(d) {
                r[d as usize] = meta.len() as u32;
                meta.push(DocMeta { path: s.path(d), size: s.size()[d as usize], mtime: s.mtime()[d as usize], rank: s.rank()[d as usize] });
            }
        }
        remap.push(r);
    }
    let mut pos = vec![0usize; segs.len()];
    let mut part = Vec::new();
    write_segment(dir, id, &meta, |list| {
        loop {
            let t = segs.iter().zip(&pos).filter_map(|(s, &p)| s.tri_key().get(p).copied()).min()?;
            for (si, s) in segs.iter().enumerate() {
                if s.tri_key().get(pos[si]) == Some(&t) {
                    part.clear();
                    s.list_into(pos[si], &mut part);
                    list.extend(part.iter().map(|&d| remap[si][d as usize]).filter(|&d| d != u32::MAX));
                    pos[si] += 1;
                }
            }
            if !list.is_empty() {
                return Some(t);
            }
        }
    })
}

fn write_segment(dir: &Path, id: u64, docs: &[DocMeta<'_>], mut next: impl FnMut(&mut Vec<u32>) -> Option<u32>) -> Option<Segment> {
    if docs.is_empty() {
        return None;
    }
    let ndocs = docs.len();
    let (mut keys, mut tri_off, mut post) = (Vec::new(), Vec::new(), Vec::new());
    let mut list = Vec::new();
    loop {
        list.clear();
        let Some(t) = next(&mut list) else { break };
        keys.push(t);
        if list.len() * 8 > ndocs {
            tri_off.push(post.len() as u32 | BITSET);
            let at = post.len();
            post.resize(at + ndocs.div_ceil(8), 0);
            for &d in &list {
                post[at + d as usize / 8] |= 1 << (d % 8);
            }
        } else {
            tri_off.push(post.len() as u32);
            let mut last = 0u32;
            for &d in &list {
                put_varint(&mut post, d - last);
                last = d;
            }
        }
    }
    tri_off.push(post.len() as u32);
    let mut paths = Vec::new();
    let mut path_off = vec![0u32];
    for d in docs {
        paths.extend_from_slice(d.path);
        path_off.push(paths.len() as u32);
    }
    let size: Vec<u64> = docs.iter().map(|d| d.size).collect();
    let mtime: Vec<u32> = docs.iter().map(|d| d.mtime).collect();
    let rank: Vec<i8> = docs.iter().map(|d| d.rank).collect();
    let mut by_path: Vec<u32> = (0..ndocs as u32).collect();
    by_path.sort_by(|&a, &b| docs[a as usize].path.cmp(&docs[b as usize].path));
    let (off, _) = layout(&lens(ndocs, keys.len(), post.len(), paths.len()));
    let hdr = header(MAGIC, &[ndocs as u64, keys.len() as u64, post.len() as u64, paths.len() as u64]);
    let sections: [&[u8]; NS] = [as_bytes(&keys), as_bytes(&tri_off), &post, as_bytes(&path_off), &paths, as_bytes(&size), as_bytes(&mtime), as_bytes(&by_path), as_bytes(&rank)];
    let p = seg_path(dir, id);
    let tmp = p.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(&hdr)?;
        let mut at = HDR;
        for (k, sec) in sections.iter().enumerate() {
            f.write_all(&vec![0u8; off[k] - at])?;
            f.write_all(sec)?;
            at = off[k] + sec.len();
        }
        f.write_all(&vec![0u8; ((at + 63) & !63) - at])?;
        f.flush()
    };
    write().ok()?;
    std::fs::rename(&tmp, &p).ok()?;
    Segment::load(dir, id)
}

pub fn eligible(path: &[u8], size: u64, home: &[u8]) -> bool {
    name_ok(pathwin::name(path), size) && in_scope(path, home)
}

fn name_ok(name: &[u8], size: u64) -> bool {
    if size > MAX_FILE {
        return false;
    }
    match name.iter().rposition(|&b| b == b'.').filter(|&p| p > 0) {
        Some(dot) => {
            let ext = &name[dot + 1..];
            TEXT_EXTS.iter().any(|x| x.eq_ignore_ascii_case(ext)) && !name.ends_with(b".min.js")
        }
        None => size <= 256 << 10,
    }
}

pub fn in_scope(path: &[u8], home: &[u8]) -> bool {
    if path.len() < home.len() || !path[..home.len()].eq_ignore_ascii_case(home) {
        return false;
    }
    if path.len() > home.len() && !pathwin::is_sep(path[home.len()]) {
        return false;
    }
    let rel = if path.len() > home.len() { &path[home.len() + 1..] } else { b"" };
    if SKIP_UNDER_HOME.iter().any(|p| rel.len() >= p.len() && rel[..p.len()].eq_ignore_ascii_case(p) && rel.get(p.len()).is_none_or(|&b| pathwin::is_sep(b))) {
        return false;
    }
    !pathwin::split(rel).iter().any(|c| SKIP_DIRS.iter().any(|s| c.eq_ignore_ascii_case(s)))
}

fn doc_rank(path: &[u8]) -> i8 {
    let mut r = 0i8;
    if pathwin::split(path).iter().any(|c| c.first() == Some(&b'.')) {
        r -= 2;
    }
    let name = pathwin::name(path);
    if [b".log".as_slice(), b".lock", b".sum"].iter().any(|x| name.ends_with(x)) {
        r -= 1;
    }
    r
}

pub struct Content {
    pub dir: PathBuf,
    pub segs: Vec<Segment>,
    next_id: u64,
}

impl Content {
    pub fn open_shared(dir: PathBuf) -> Content {
        let manifest: Vec<u64> = std::fs::read_to_string(dir.join("manifest")).unwrap_or_default().split_whitespace().filter_map(|s| s.parse().ok()).collect();
        let segs: Vec<Segment> = manifest.iter().filter_map(|&id| Segment::load(&dir, id)).collect();
        let next_id = segs.iter().map(|s| s.id + 1).max().unwrap_or(1);
        Content { dir, segs, next_id }
    }

    pub fn open(dir: PathBuf) -> Content {
        std::fs::create_dir_all(&dir).ok();
        let Content { dir, segs, next_id } = Content::open_shared(dir);
        let keep: Vec<String> = segs.iter().flat_map(|s| [format!("seg-{:06}.fsc", s.id), format!("seg-{:06}.dead", s.id)]).collect();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("seg-") && !keep.contains(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Content { dir, segs, next_id }
    }

    fn save_manifest(&self) {
        let s: String = self.segs.iter().map(|s| format!("{}\n", s.id)).collect();
        let tmp = self.dir.join("manifest.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(tmp, self.dir.join("manifest"));
        }
    }

    pub fn docs(&self) -> usize {
        self.segs.iter().map(|s| s.live_docs).sum()
    }
    pub fn bytes(&self) -> usize {
        self.segs.iter().map(|s| s.map.len()).sum()
    }

    pub fn diff(&mut self, wants: Vec<(Vec<u8>, bool, Docs)>) -> Docs {
        let mut todo = Docs::default();
        let mut touched = vec![false; self.segs.len()];
        for (dir, recursive, want) in wants {
            let mut held = vec![false; want.len()];
            let lo = pathwin::join(&dir, b"");
            for (si, s) in self.segs.iter_mut().enumerate() {
                let ids = if recursive { s.with_prefix(&lo).collect::<Vec<_>>() } else { s.direct_children(&lo) };
                for d in ids {
                    match want.find(s.path(d)) {
                        Some(i) if want.items[i].2 == s.size()[d as usize] && want.items[i].3 == s.mtime()[d as usize] => held[i] = true,
                        _ => touched[si] |= s.kill(d),
                    }
                }
            }
            for (i, h) in held.iter().enumerate() {
                if !h {
                    let (_, _, size, mtime) = want.items[i];
                    todo.push(want.path(i), size, mtime);
                }
            }
        }
        for (s, t) in self.segs.iter().zip(&touched) {
            if *t {
                s.save_dead(&self.dir);
            }
        }
        todo.sort();
        todo
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id - 1
    }
    pub fn push(&mut self, seg: Segment) {
        self.segs.push(seg);
        self.save_manifest();
    }
    pub fn merge_plan(&self) -> Option<Vec<u64>> {
        let tier = |s: &Segment| (s.plen.max(1) as f64).log(4.0) as u32;
        let mut by_tier: HashMap<u32, Vec<&Segment>> = HashMap::new();
        for s in &self.segs {
            by_tier.entry(tier(s)).or_default().push(s);
        }
        let mut tiers: Vec<_> = by_tier.into_iter().filter(|(_, v)| v.len() >= 8).collect();
        tiers.sort_by_key(|(t, _)| *t);
        for (_, v) in tiers {
            let group: Vec<u64> = v.iter().take(8).map(|s| s.id).collect();
            let bytes: usize = v.iter().take(8).map(|s| s.plen).sum();
            if bytes <= MERGE_CAP {
                return Some(group);
            }
        }
        None
    }
    pub fn segments(&self, ids: &[u64]) -> Vec<&Segment> {
        self.segs.iter().filter(|s| ids.contains(&s.id)).collect()
    }
    pub fn replace(&mut self, ids: &[u64], seg: Segment) {
        self.segs.retain(|s| !ids.contains(&s.id));
        for id in ids {
            let _ = std::fs::remove_file(seg_path(&self.dir, *id));
            let _ = std::fs::remove_file(dead_path(&self.dir, *id));
        }
        self.segs.push(seg);
        self.save_manifest();
    }

    fn candidates(&self, plan: &TQ, filt: &Query) -> Vec<(usize, u32)> {
        let mut out: Vec<(usize, u32)> = self
            .segs
            .par_iter()
            .enumerate()
            .flat_map_iter(|(si, s)| {
                let ids = eval(s, plan).unwrap_or_else(|| (0..s.ndocs as u32).collect());
                ids.into_iter()
                    .filter(move |&d| !s.is_dead(d) && s.rank()[d as usize] != NOT_TEXT)
                    .filter(move |&d| filt.match_path(s.path(d), KIND_FILE, s.size()[d as usize], s.mtime()[d as usize]).is_some())
                    .map(move |d| (si, d))
                    .collect::<Vec<_>>()
            })
            .collect();
        out.sort_by_key(|&(si, d)| {
            let s = &self.segs[si];
            std::cmp::Reverse((s.rank()[d as usize], s.mtime()[d as usize]))
        });
        out
    }

    pub fn search(&self, g: &Grep, filt: &Query) -> GrepResult {
        let plan = g.plan();
        let cands = self.candidates(&plan, filt);
        let paths: Vec<&[u8]> = cands.iter().map(|&(si, d)| self.segs[si].path(d)).collect();
        verify(g, &paths, filt.limit)
    }
}

pub fn wanted(live: &Live, home: &[u8], dir: &[u8], recursive: bool) -> Docs {
    let mut want = Docs::default();
    if !in_scope(dir, home) {
        return want;
    }
    let idx = &live.base;
    let mut p = Vec::new();
    if let Some(d) = idx.lookup(dir).filter(|&e| !live.is_dead(e)).and_then(|e| idx.dir_of(e)) {
        let range = if recursive { idx.dir_start()[d as usize] as usize..idx.dir_end()[d as usize] as usize } else { idx.children(d) };
        for i in range {
            if idx.kind()[i] & 3 != KIND_FILE || live.is_dead(i as u32) || !name_ok(idx.name(i), idx.size_of(i)) {
                continue;
            }
            if recursive {
                idx.path(i, &mut p);
            } else {
                p = pathwin::join(dir, idx.name(i));
            }
            if in_scope(&p, home) {
                want.push(&p, idx.size_of(i), idx.mtime()[i]);
            }
        }
    }
    let lo = pathwin::join(dir, b"");
    for (k, o) in live.over.range(lo.clone()..).take_while(|(k, _)| k.starts_with(&lo)) {
        if o.kind & 3 == KIND_FILE && (recursive || !k[lo.len()..].iter().any(|&b| pathwin::is_sep(b))) && eligible(k, o.size, home) {
            want.push(k, o.size, o.mtime);
        }
    }
    want
}

pub fn wants(live: &Live, home: &[u8], dirs: &[Vec<u8>], trees: &[Vec<u8>]) -> Vec<(Vec<u8>, bool, Docs)> {
    let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
    for (d, r) in dirs.iter().map(|d| (d, false)).chain(trees.iter().map(|d| (d, true))) {
        if in_scope(d, home) {
            out.push((d.clone(), r));
        } else if r && home.starts_with(d.as_slice()) {
            out.push((home.to_vec(), true));
        }
    }
    out.sort();
    out.dedup();
    out.into_iter()
        .map(|(d, r)| {
            let mut w = wanted(live, home, &d, r);
            w.sort();
            (d, r, w)
        })
        .collect()
}

pub fn scan_paths(live: &Live, q: Query) -> Vec<Vec<u8>> {
    let s = crate::query::Searcher { live };
    s.search(&q)
        .into_iter()
        .filter_map(|h| {
            let mut p = Vec::new();
            match h.over {
                Some(path) => Some(path),
                None => {
                    live.base.path(h.idx as usize, &mut p);
                    Some(p)
                }
            }
        })
        .collect()
}

pub struct Grep {
    pub pattern: String,
    pub mode: GrepMode,
    pub max_per_file: usize,
    pub budget: Option<std::time::Duration>,
    re: Regex,
}

impl Grep {
    pub fn new(pattern: &str, mode: GrepMode) -> Result<Grep, String> {
        let smart_ci = !pattern.chars().any(|c| c.is_uppercase());
        let src = match mode {
            GrepMode::Literal => regex::escape(pattern),
            GrepMode::Regex => pattern.to_string(),
            GrepMode::Symbol => format!(r"(?-u:\b)(?:{DEFINES})(?:<[^>\n]*>)?[ \t*&]+(?:mut[ \t]+)?{}(?-u:\b)", regex::escape(pattern)),
        };
        let re = RegexBuilder::new(&src).case_insensitive(smart_ci && mode != GrepMode::Symbol).multi_line(true).size_limit(1 << 26).build().map_err(|e| e.to_string())?;
        Ok(Grep { pattern: pattern.to_string(), mode, max_per_file: 5, budget: Some(std::time::Duration::from_millis(250)), re })
    }

    fn plan(&self) -> TQ {
        match self.mode {
            GrepMode::Symbol if plain_identifier(self.pattern.as_bytes()) => TQ::Tri(symbol_key(self.pattern.as_bytes())),
            GrepMode::Literal | GrepMode::Symbol => literal_plan(self.pattern.as_bytes()),
            GrepMode::Regex => regex_syntax::Parser::new().parse(&self.pattern).ok().and_then(|h| longest_lit(&h)).map(|s| literal_plan(&s)).unwrap_or(TQ::All),
        }
    }
}

enum TQ {
    All,
    Tri(u32),
    And(Vec<TQ>),
}

fn literal_plan(s: &[u8]) -> TQ {
    let t = trigrams_small(s);
    if t.is_empty() { TQ::All } else { TQ::And(t.into_iter().map(TQ::Tri).collect()) }
}

fn longest_lit(h: &regex_syntax::hir::Hir) -> Option<Vec<u8>> {
    use regex_syntax::hir::HirKind;
    match h.kind() {
        HirKind::Literal(l) => Some(l.0.to_vec()),
        HirKind::Concat(v) => v.iter().filter_map(longest_lit).max_by_key(|s| s.len()),
        HirKind::Capture(c) => longest_lit(&c.sub),
        _ => None,
    }
}

fn eval(s: &Segment, plan: &TQ) -> Option<Vec<u32>> {
    match plan {
        TQ::All => None,
        TQ::Tri(t) => Some(s.postings(*t)),
        TQ::And(v) => {
            let mut lists: Vec<Vec<u32>> = v.iter().filter_map(|p| eval(s, p)).collect();
            if lists.is_empty() {
                return None;
            }
            lists.sort_by_key(|l| l.len());
            let mut acc = lists.remove(0);
            for l in lists {
                acc = intersect(&acc, &l);
            }
            Some(acc)
        }
    }
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[derive(Default)]
pub struct GrepResult {
    pub files: Vec<FileMatches>,
    pub candidates: usize,
    pub read: usize,
    pub complete: bool,
}

pub struct FileMatches {
    pub path: Vec<u8>,
    pub lines: Vec<(usize, String)>,
}

fn read_pool() -> &'static rayon::ThreadPool {
    static POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| rayon::ThreadPoolBuilder::new().num_threads(4).thread_name(|i| format!("wsearch-read-{i}")).build().unwrap());
    &POOL
}

pub fn verify(g: &Grep, paths: &[impl AsRef<[u8]> + Sync], limit: usize) -> GrepResult {
    let t = std::time::Instant::now();
    let mut r = GrepResult { candidates: paths.len(), ..Default::default() };
    let mut at = 0;
    let mut batch = 64;
    while at < paths.len() && r.files.len() < limit {
        if g.budget.is_some_and(|b| t.elapsed() > b) {
            break;
        }
        let end = (at + batch).min(paths.len());
        let found: Vec<Option<FileMatches>> = read_pool().install(|| paths[at..end].par_iter().map(|p| match_file(g, p.as_ref())).collect());
        r.read += end - at;
        r.files.extend(found.into_iter().flatten());
        at = end;
        batch = (batch * 2).min(1024);
    }
    r.complete = at >= paths.len() || r.files.len() >= limit;
    r.files.truncate(limit);
    r
}

fn match_file(g: &Grep, path: &[u8]) -> Option<FileMatches> {
    use std::io::Read;
    let p = String::from_utf8_lossy(path);
    let mut buf = Vec::new();
    std::fs::File::open(p.as_ref()).ok()?.take(MAX_FILE).read_to_end(&mut buf).ok()?;
    let mut lines = Vec::new();
    for (i, line) in buf.split(|&b| b == b'\n').enumerate() {
        if g.re.is_match(line) {
            lines.push((i + 1, String::from_utf8_lossy(line).trim_end_matches('\r').to_string()));
            if lines.len() >= g.max_per_file {
                break;
            }
        }
    }
    (!lines.is_empty()).then_some(FileMatches { path: path.to_vec(), lines })
}

