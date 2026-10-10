//! Name search: parse a query, scan the index in parallel, rank, top-k.

use crate::index::{char_bit, start_bit};
use crate::live::Live;
use crate::pathwin;
use crate::walk::{FLAG_HIDDEN, KIND_DIR, KIND_FILE, KIND_LINK};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Fuzzy,
    Exact,
    Prefix,
    Suffix,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub text: Vec<u8>,
    pub mask: u64,
    pub mode: Mode,
    pub negate: bool,
    pub loose: u64,
    pub start: u64,
}

impl Token {
    #[inline(always)]
    fn fits(&self, m: u64) -> bool {
        let miss = self.mask & !m;
        (miss == 0) | ((((miss & !self.loose) | (miss & miss.wrapping_sub(1))) == 0) & (m & self.start != 0))
    }
}

const TYPO_MIN_LEN: usize = 5;
const TYPO_COST: i32 = 60;

fn takes_typos(text: &[u8], mode: Mode) -> bool {
    mode == Mode::Fuzzy && text.len() >= TYPO_MIN_LEN
}

#[derive(Clone, Default)]
pub struct Query {
    pub tokens: Vec<Token>,
    pub kind: Option<u8>,
    pub exts: Vec<Vec<u8>>,
    pub scope: Option<Vec<u8>>,
    pub size: (u64, u64),
    pub mtime: (u32, u32),
    pub name_re: Option<regex::bytes::Regex>,
    pub path_re: Option<regex::bytes::Regex>,
    pub limit: usize,
    pub grep: Option<String>,
    pub grep_mode: GrepMode,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub enum GrepMode {
    #[default]
    Literal,
    Regex,
    Symbol,
}

pub struct Hit {
    pub score: i32,
    pub idx: u32,
    pub over: Option<Vec<u8>>,
}

#[rustfmt::skip]
const TYPES: &[(&str, &[&str])] = &[
    ("image", &["png", "jpg", "jpeg", "gif", "heic", "heif", "webp", "tiff", "tif", "bmp", "svg", "raw", "cr2", "cr3", "nef", "arw", "dng", "psd", "ico", "icns", "avif", "jxl"]),
    ("video", &["mp4", "mov", "m4v", "mkv", "avi", "webm", "wmv", "flv", "mpg", "mpeg", "3gp", "hevc"]),
    ("audio", &["mp3", "m4a", "aac", "wav", "flac", "aiff", "aif", "ogg", "opus", "alac", "caf", "mid", "midi", "m4r"]),
    ("doc", &["pdf", "doc", "docx", "pages", "txt", "md", "rtf", "odt", "key", "ppt", "pptx", "numbers", "xls", "xlsx", "csv", "epub", "tex"]),
    ("code", &["rs", "c", "h", "cc", "cpp", "hpp", "m", "mm", "swift", "go", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "java", "kt", "rb", "php", "cs", "sh", "zsh", "bash", "fish", "lua", "sql", "html", "css", "scss", "json", "yaml", "yml", "toml", "xml", "vue", "svelte", "zig", "nim", "hs", "ml", "ex", "exs", "erl", "clj", "dart", "r", "jl", "metal", "glsl", "wgsl", "proto", "graphql", "nix", "ps1", "bat", "cmd"]),
    ("archive", &["zip", "tar", "gz", "tgz", "bz2", "xz", "7z", "rar", "dmg", "pkg", "iso", "zst", "lz4", "xip", "cab", "msi"]),
    ("font", &["ttf", "otf", "woff", "woff2", "ttc", "dfont"]),
    ("exe", &["exe", "dll", "sys", "com", "scr"]),
];

impl Query {
    pub fn parse(s: &str, home: &str) -> Result<Query, String> {
        let mut q = Query { size: (0, u64::MAX), mtime: (0, u32::MAX), limit: 50, ..Default::default() };
        for word in split_words(s) {
            if let Some((k, v)) = word.split_once(':') {
                if q.filter(k, v, home)? {
                    continue;
                }
            }
            for piece in word.split(|c| c == '/' || c == '\\').filter(|p| !p.is_empty()) {
                q.push_token(piece);
            }
        }
        Ok(q)
    }

    pub fn push_token(&mut self, w: &str) {
        let (mut t, mut negate, mut mode) = (w, false, Mode::Fuzzy);
        if let Some(r) = t.strip_prefix('!') {
            (t, negate, mode) = (r, true, Mode::Exact);
        }
        if let Some(r) = t.strip_prefix('\'') {
            (t, mode) = (r, Mode::Exact);
        } else if let Some(r) = t.strip_prefix('^') {
            (t, mode) = (r, Mode::Prefix);
        } else if let Some(r) = t.strip_suffix('$') {
            (t, mode) = (r, Mode::Suffix);
        }
        if t.is_empty() || (!negate && self.tokens.iter().filter(|t| !t.negate).count() >= 8) {
            return;
        }
        let text: Vec<u8> = t.bytes().map(|b| b.to_ascii_lowercase()).collect();
        let mask = text.iter().fold(0, |m, &b| m | char_bit(b));
        let (loose, start) = if takes_typos(&text, mode) { (mask & !char_bit(text[0]), start_bit(text[0])) } else { (0, 0) };
        self.tokens.push(Token { text, mask, mode, negate, loose, start });
    }

    pub fn filter(&mut self, k: &str, v: &str, home: &str) -> Result<bool, String> {
        match k {
            "ext" => self.exts.extend(v.split(',').map(|e| e.trim_start_matches('.').to_ascii_lowercase().into_bytes())),
            "type" => {
                for t in v.split(',') {
                    let (_, exts) = TYPES.iter().find(|(n, _)| *n == t).ok_or(format!("unknown type {t}"))?;
                    self.exts.extend(exts.iter().map(|e| e.as_bytes().to_vec()));
                }
            }
            "kind" => {
                self.kind = Some(match v {
                    "file" | "f" => KIND_FILE,
                    "dir" | "folder" | "d" => KIND_DIR,
                    "link" | "symlink" | "l" => KIND_LINK,
                    _ => return Err(format!("unknown kind {v}")),
                })
            }
            "in" => {
                let p = if let Some(r) = v.strip_prefix('~') { format!("{home}{r}") } else { v.to_string() };
                let p = p.replace('/', "\\");
                let p = std::fs::canonicalize(&p).map(|c| pathwin::strip_nt_prefix(c.to_string_lossy().as_bytes().to_vec())).unwrap_or_else(|_| pathwin::normalize(p.as_bytes()));
                self.scope = Some(p);
            }
            "drive" => {
                let d = v.chars().next().ok_or("bad drive")?.to_ascii_uppercase();
                if !d.is_ascii_alphabetic() {
                    return Err("bad drive".into());
                }
                self.scope = Some(vec![d as u8, b':']);
            }
            "size" => self.size = range(v, parse_size)?,
            "mtime" | "modified" => {
                let now = now_secs();
                let (lo, hi) = range(v, parse_age)?;
                self.mtime = (now.saturating_sub(hi.min(now as u64) as u32), now.saturating_sub(lo as u32));
                if hi == u64::MAX {
                    self.mtime.0 = 0;
                }
            }
            "re" => self.name_re = Some(regex::bytes::Regex::new(&format!("(?i){v}")).map_err(|e| e.to_string())?),
            "path" => self.path_re = Some(regex::bytes::Regex::new(&format!("(?i){v}")).map_err(|e| e.to_string())?),
            "limit" => self.limit = v.parse().map_err(|_| "bad limit")?,
            "grep" | "content" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Literal),
            "regex" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Regex),
            "sym" | "symbol" => (self.grep, self.grep_mode) = (Some(v.to_string()), GrepMode::Symbol),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn clone_for_scan(&self) -> Query {
        Query { grep: None, ..self.clone() }
    }

    pub fn match_path(&self, path: &[u8], kind: u8, size: u64, mtime: u32) -> Option<i32> {
        self.match_path_with(path, kind, size, mtime, |dirs| self.dir_match(dirs))
    }

    pub fn match_path_with(&self, path: &[u8], kind: u8, size: u64, mtime: u32, dirs: impl FnOnce(&[u8]) -> DirMatch) -> Option<i32> {
        if let Some(s) = &self.scope {
            if !scope_ok(path, s) {
                return None;
            }
        }
        let name = pathwin::name(path);
        if name.is_empty()
            || self.kind.is_some_and(|k| kind & 3 != k)
            || (!self.exts.is_empty() && !ext_ok(name, &self.exts))
            || size < self.size.0
            || size > self.size.1
            || mtime < self.mtime.0
            || mtime > self.mtime.1
        {
            return None;
        }
        let mut pos = self.tokens.iter().filter(|t| !t.negate).peekable();
        let m = crate::index::name_mask(name);
        if pos.peek().is_some() && !pos.any(|t| t.fits(m) && token_score(name, m, t).is_some()) {
            return None;
        }
        if self.tokens.iter().any(|t| t.negate && token_matches(name, t)) {
            return None;
        }
        let folder = pathwin::parent(path);
        let d = dirs(folder);
        if d.negated {
            return None;
        }
        let pos: Vec<&Token> = self.tokens.iter().filter(|t| !t.negate).collect();
        let all = (1u32 << pos.len()) - 1;
        let (mut got, mut inherited, mut score) = (0u32, 0u32, 0i32);
        for (t, tok) in pos.iter().enumerate() {
            if let Some(s) = token_score(name, m, tok) {
                got |= 1 << t;
                score += s;
            } else if let Some(s) = d.best[t] {
                inherited |= 1 << t;
                score += s * 3 / 4;
            }
        }
        if !pos.is_empty() && (got == 0 || (got | inherited) != all) {
            return None;
        }
        if self.name_re.as_ref().is_some_and(|re| !re.is_match(name)) || self.path_re.as_ref().is_some_and(|re| !re.is_match(path)) {
            return None;
        }
        Some(score)
    }

    pub fn dir_match(&self, dirs: &[u8]) -> DirMatch {
        let comps = pathwin::split(dirs);
        let mut d = DirMatch { negated: false, best: [None; 8] };
        d.negated = self.tokens.iter().any(|t| t.negate && comps.iter().any(|c| token_matches(c, t)));
        for (t, tok) in self.tokens.iter().filter(|t| !t.negate).enumerate() {
            d.best[t] = comps.iter().filter_map(|c| token_score(c, !0, tok)).max();
        }
        d
    }
}

#[derive(Clone, Copy)]
pub struct DirMatch {
    negated: bool,
    best: [Option<i32>; 8],
}

pub fn scope_ok(path: &[u8], scope: &[u8]) -> bool {
    if path.len() < scope.len() {
        return false;
    }
    if !path[..scope.len()].eq_ignore_ascii_case(scope) {
        return false;
    }
    path.len() == scope.len() || pathwin::is_sep(path[scope.len()])
}

fn split_words(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn range(v: &str, p: fn(&str) -> Option<u64>) -> Result<(u64, u64), String> {
    let bad = || format!("bad range {v}");
    if let Some(r) = v.strip_prefix(">=").or_else(|| v.strip_prefix('>')) {
        return Ok((p(r).ok_or_else(bad)?, u64::MAX));
    }
    if let Some(r) = v.strip_prefix("<=").or_else(|| v.strip_prefix('<')) {
        return Ok((0, p(r).ok_or_else(bad)?));
    }
    if let Some((a, b)) = v.split_once("..") {
        return Ok((p(a).ok_or_else(bad)?, p(b).ok_or_else(bad)?));
    }
    let x = p(v).ok_or_else(bad)?;
    Ok((x, x))
}

fn split_unit(s: &str) -> (f64, String) {
    let i = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    (s[..i].parse().unwrap_or(f64::NAN), s[i..].to_ascii_lowercase())
}

fn parse_size(s: &str) -> Option<u64> {
    let (n, u) = split_unit(s);
    let m = match u.as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        _ => return None,
    };
    (!n.is_nan()).then_some((n * m) as u64)
}

fn parse_age(s: &str) -> Option<u64> {
    let (n, u) = split_unit(s);
    let m = match u.as_str() {
        "s" => 1.0,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        "" | "d" => 86400.0,
        "w" => 604800.0,
        "mo" => 2592000.0,
        "y" => 31536000.0,
        _ => return None,
    };
    (!n.is_nan()).then_some((n * m) as u64)
}

pub fn now_secs() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32)
}

#[inline(always)]
pub(crate) fn fold(b: u8) -> u8 {
    b | (((b.wrapping_sub(b'A') < 26) as u8) << 5)
}

#[inline(always)]
fn is_subseq(name: &[u8], q: &[u8]) -> bool {
    let mut j = 0;
    for &b in name {
        if fold(b) == q[j] {
            j += 1;
            if j == q.len() {
                return true;
            }
        }
    }
    false
}

fn find_ci(name: &[u8], q: &[u8]) -> Option<usize> {
    if q.len() > name.len() {
        return None;
    }
    (0..=name.len() - q.len()).find(|&i| name[i..i + q.len()].iter().zip(q).all(|(&a, &b)| fold(a) == b))
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Lower,
    Upper,
    Digit,
    Delim,
    Other,
}

#[inline(always)]
fn class(b: u8) -> Class {
    match b {
        b'a'..=b'z' => Class::Lower,
        b'A'..=b'Z' => Class::Upper,
        b'0'..=b'9' => Class::Digit,
        b' ' | b'_' | b'-' | b'.' | b'/' | b'\\' | b'(' | b')' | b'[' | b']' | b',' | b'+' | b'@' => Class::Delim,
        _ => Class::Other,
    }
}

const SCORE_MATCH: i32 = 16;
const GAP_START: i32 = -3;
const GAP_EXT: i32 = -1;
const BONUS_BOUNDARY: i32 = 8;
const BONUS_CAMEL: i32 = 7;
const BONUS_CONSEC: i32 = 4;

#[inline(always)]
fn bonus(prev: Class, cur: Class) -> i32 {
    match (prev, cur) {
        (Class::Delim, c) if c != Class::Delim => BONUS_BOUNDARY,
        (Class::Lower, Class::Upper) | (Class::Lower | Class::Upper, Class::Digit) => BONUS_CAMEL,
        _ => 0,
    }
}

pub fn fuzzy_score(name: &[u8], q: &[u8]) -> Option<i32> {
    fuzzy_score_capped(name, q, 100)
}

fn fuzzy_score_capped(name: &[u8], q: &[u8], cap: i32) -> Option<i32> {
    let mut end = 0;
    let mut from = 0;
    for &c in q {
        end = from + find_folded(&name[from..], c)?;
        from = end + 1;
    }
    if q.len() == 1 {
        return Some(single_score(name, end, cap));
    }
    let mut start = end + 1;
    for &c in q.iter().rev() {
        start = rfind_folded(&name[..start], c)?;
    }
    let mut score = 0;
    let (mut at, mut first_bonus) = (start, 0);
    for (k, &c) in q.iter().enumerate() {
        let mut run = false;
        if k > 0 {
            let last = at;
            at = last + 1 + find_folded(&name[last + 1..=end], c)?;
            run = at == last + 1;
            if !run {
                score += GAP_START + (at - last - 2) as i32 * GAP_EXT;
            }
        }
        let prev = if at == 0 { Class::Delim } else { class(name[at - 1]) };
        let mut b = bonus(prev, class(name[at]));
        if run {
            if b >= BONUS_BOUNDARY && b > first_bonus {
                first_bonus = b;
            }
            b = b.max(first_bonus).max(BONUS_CONSEC);
        } else {
            first_bonus = b;
        }
        score += SCORE_MATCH + if k == 0 { b * 2 } else { b };
    }
    let off = (name.len() > 1 && name[0] == b'.') as usize;
    let stem = name.iter().rposition(|&b| b == b'.').filter(|&p| p > off).unwrap_or(name.len());
    let contiguous = end + 1 - start == q.len();
    let placed = if start == off && contiguous && end + 1 == name.len() {
        100
    } else if start == off && contiguous && end + 1 == stem {
        80
    } else if start == off && contiguous {
        30
    } else {
        0
    };
    Some(score + placed.min(cap) - (name.len() as i32).min(80) / 3)
}

#[inline(always)]
fn find_folded(s: &[u8], c: u8) -> Option<usize> {
    if c.is_ascii_lowercase() { memchr::memchr2(c, c - 32, s) } else { memchr::memchr(c, s) }
}

#[inline(always)]
fn rfind_folded(s: &[u8], c: u8) -> Option<usize> {
    if c.is_ascii_lowercase() { memchr::memrchr2(c, c - 32, s) } else { memchr::memrchr(c, s) }
}

#[inline]
fn single_score(name: &[u8], i: usize, cap: i32) -> i32 {
    let prev = if i == 0 { Class::Delim } else { class(name[i - 1]) };
    let mut score = SCORE_MATCH + bonus(prev, class(name[i])) * 2;
    let off = (name.len() > 1 && name[0] == b'.') as usize;
    if i == off {
        let stem = name.iter().rposition(|&b| b == b'.').filter(|&p| p > off).unwrap_or(name.len());
        score += cap.min(if i + 1 == name.len() { 100 } else if i + 1 == stem { 80 } else { 30 });
    }
    score - (name.len() as i32).min(80) / 3
}

fn typo_score(name: &[u8], m: u64, q: &[u8]) -> Option<i32> {
    let mut best = typo_at(name, (name.len() > 1 && name[0] == b'.') as usize, q);
    if m & char_bit(b' ') != 0 {
        for sp in memchr::memchr_iter(b' ', name) {
            best = best.max(typo_at(name, sp + 1, q));
        }
    }
    best
}

fn typo_at(name: &[u8], s: usize, q: &[u8]) -> Option<i32> {
    if name.get(s).is_none_or(|&b| fold(b) != q[0]) {
        return None;
    }
    let mut fixed = [0u8; 128];
    let n = one_edit_prefix(&name[s..], q)?;
    let fixed = fixed.get_mut(..n)?;
    for (f, &b) in fixed.iter_mut().zip(&name[s..]) {
        *f = fold(b);
    }
    Some(fuzzy_score_capped(name, fixed, 30)? - TYPO_COST)
}

fn one_edit_prefix(w: &[u8], q: &[u8]) -> Option<usize> {
    let starts = |w: &[u8], q: &[u8]| w.len() >= q.len() && w.iter().zip(q).all(|(&a, &b)| fold(a) == b);
    let i = (0..q.len()).find(|&i| i >= w.len() || fold(w[i]) != q[i])?;
    if q[i].is_ascii_digit() || w.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let rest = &q[i + 1..];
    let after = w.get(i + 1..).unwrap_or_default();
    if i + 1 < q.len() && i + 1 < w.len() && fold(w[i]) == q[i + 1] && fold(w[i + 1]) == q[i] && starts(&w[i + 2..], &q[i + 2..]) {
        return Some(q.len());
    }
    if i < w.len() && starts(after, rest) {
        return Some(q.len());
    }
    if starts(&w[i..], rest) {
        return Some(q.len() - 1);
    }
    (i < w.len() && starts(after, &q[i..])).then_some(q.len() + 1)
}

#[inline]
fn token_score(name: &[u8], m: u64, t: &Token) -> Option<i32> {
    match t.mode {
        Mode::Fuzzy if takes_typos(&t.text, t.mode) => {
            let clean = if t.mask & !m == 0 { fuzzy_score(name, &t.text) } else { None };
            clean.max(if m & t.start != 0 { typo_score(name, m, &t.text) } else { None })
        }
        Mode::Fuzzy => fuzzy_score(name, &t.text),
        Mode::Exact => find_ci(name, &t.text).map(|p| 40 + if p == 0 { 30 } else { 0 } - (name.len() as i32).min(80) / 3),
        Mode::Prefix => {
            (name.len() >= t.text.len() && name.iter().zip(&t.text).all(|(&a, &b)| fold(a) == b)).then(|| 60 - (name.len() as i32).min(80) / 3)
        }
        Mode::Suffix => (name.len() >= t.text.len() && name[name.len() - t.text.len()..].iter().zip(&t.text).all(|(&a, &b)| fold(a) == b))
            .then(|| 50 - (name.len() as i32).min(80) / 3),
    }
}

#[inline]
fn token_matches(name: &[u8], t: &Token) -> bool {
    match t.mode {
        Mode::Fuzzy => is_subseq(name, &t.text),
        _ => token_score(name, !0, t).is_some(),
    }
}

struct TopK {
    k: usize,
    buf: Vec<u64>,
    floor: u64,
}

#[inline(always)]
fn key(score: i32, i: u32) -> u64 {
    (((score as i64 - i32::MIN as i64) as u64) << 32) | (!i) as u64
}

impl TopK {
    fn new(k: usize) -> TopK {
        TopK { k, buf: Vec::new(), floor: if k == 0 { u64::MAX } else { 0 } }
    }
    #[inline]
    fn push(&mut self, key: u64) {
        if key > self.floor {
            self.buf.push(key);
            if self.buf.len() >= (2 * self.k).max(64) {
                self.cut();
            }
        }
    }
    fn cut(&mut self) {
        if self.buf.len() > self.k {
            self.buf.select_nth_unstable_by(self.k - 1, |a, b| b.cmp(a));
            self.buf.truncate(self.k);
            self.floor = self.buf[self.k - 1];
        }
    }
}

fn top_k(n: usize, k: usize, visit: impl Fn(std::ops::Range<usize>, &mut TopK) + Sync) -> Vec<Hit> {
    let pieces = (rayon::current_num_threads() * 4).min(n.max(1));
    let step = n.div_ceil(pieces).max(1);
    let tops: Vec<TopK> = (0..pieces)
        .into_par_iter()
        .map(|p| {
            let mut top = TopK::new(k);
            visit((p * step).min(n)..((p + 1) * step).min(n), &mut top);
            top.cut();
            top
        })
        .collect();
    let floor = tops.iter().filter(|t| t.buf.len() == k).map(|t| t.floor).max().unwrap_or(0);
    let mut keys: Vec<u64> = tops.into_iter().flat_map(|t| t.buf).filter(|&x| x >= floor).collect();
    if keys.len() > k {
        if k == 0 {
            return Vec::new();
        }
        keys.select_nth_unstable_by(k - 1, |a, b| b.cmp(a));
        keys.truncate(k);
    }
    keys.into_iter().map(|x| Hit { score: ((x >> 32) as i64 + i32::MIN as i64) as i32, idx: !(x as u32), over: None }).collect()
}

fn ext_ok(name: &[u8], exts: &[Vec<u8>]) -> bool {
    let Some(dot) = name.iter().rposition(|&b| b == b'.') else { return false };
    let e = &name[dot + 1..];
    exts.iter().any(|x| x.len() == e.len() && x.iter().zip(e).all(|(&a, &b)| a == fold(b)))
}

const SELECTIVE: usize = 60_000;
const NAME_CHUNK: usize = 1 << 15;

pub struct Searcher<'a> {
    pub live: &'a Live,
}

impl Searcher<'_> {
    pub fn scope_range(&self, q: &Query) -> Option<(usize, usize)> {
        let idx = &self.live.base;
        let Some(scope) = &q.scope else { return Some((1, idx.n)) };
        let e = idx.lookup(scope)?;
        let d = idx.dir_of(e)? as usize;
        Some((idx.dir_start()[d] as usize, idx.dir_end()[d] as usize))
    }

    pub fn search(&self, q: &Query) -> Vec<Hit> {
        let mut hits = self.search_base(q);
        hits.extend(self.search_overlay(q));
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.idx.cmp(&b.idx)));
        hits.truncate(q.limit);
        hits
    }

    fn search_base(&self, q: &Query) -> Vec<Hit> {
        let Some((lo, hi)) = self.scope_range(q) else { return Vec::new() };
        let pos: Vec<&Token> = q.tokens.iter().filter(|t| !t.negate).collect();
        let neg: Vec<&Token> = q.tokens.iter().filter(|t| t.negate).collect();
        let scored = self.names(q, &pos, &neg);
        let s = Scan { q, live: self.live, names: &scored.names, npos: pos.len(), need_dirs: pos.len() > 1 || !neg.is_empty(), now: now_secs() };
        if scored.names.ok_entries <= SELECTIVE {
            return s.selective(lo, hi);
        }
        let memo = if s.need_dirs { Some(scored.memo.get_or_init(|| self.dir_tokens(&scored.names))) } else { None };
        s.full(lo, hi, memo.map(|m| m.as_slice()))
    }

    fn names(&self, q: &Query, pos: &[&Token], neg: &[&Token]) -> Arc<Scored> {
        let key = NameKey::of(q);
        {
            let g = self.live.names_cache.0.lock();
            if let Some(p) = g.as_ref() {
                if p.key == key {
                    return p.clone();
                }
            }
        }
        let scored = Arc::new(Scored { at: std::time::Instant::now(), key, names: self.score_names(q, pos, neg), memo: OnceLock::new() });
        *self.live.names_cache.0.lock() = Some(scored.clone());
        scored
    }

    fn score_names(&self, q: &Query, pos: &[&Token], neg: &[&Token]) -> NameTable {
        let idx = &self.live.base;
        let mask = idx.name_mask();
        let ne_off = idx.name_ents_off();
        let score_one = |k: usize, ok_entries: &mut usize| -> Option<NameHit> {
            let m = mask[k];
            let fits = |t: &&Token| t.fits(m);
            if !pos.is_empty() && !pos.iter().any(fits) && !neg.iter().any(fits) {
                return None;
            }
            let name = idx.uname(k as u32);
            let mut h = NameHit { score: 0, bits: 0, flags: name_flags(name), best: [0; 4] };
            for (t, tok) in pos.iter().enumerate() {
                if tok.fits(m) {
                    if let Some(s) = token_score(name, m, tok) {
                        h.bits |= 1 << t;
                        let s16 = s.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        h.score = h.score.saturating_add(s16);
                        if t < 4 {
                            h.best[t] = s16.max(0);
                        }
                    }
                }
            }
            if neg.iter().any(|t| token_matches(name, t)) {
                h.flags |= NF_NEG;
            }
            let ok = (pos.is_empty() || h.bits != 0)
                && h.flags & NF_NEG == 0
                && (q.exts.is_empty() || ext_ok(name, &q.exts))
                && q.name_re.as_ref().is_none_or(|re| re.is_match(name));
            if ok {
                h.flags |= NF_OK;
                *ok_entries += (ne_off[k + 1] - ne_off[k]) as usize;
            }
            (ok || h.bits != 0 || h.flags & NF_NEG != 0).then_some(h)
        };
        let mut bits = vec![0u64; idx.u.div_ceil(64)];
        let mut dense = DENSE_POOL.lock().pop().filter(|d| d.len() == idx.u).unwrap_or_else(|| vec![NameHit::NONE; idx.u]);
        let counts: Vec<(usize, usize)> = dense
            .par_chunks_mut(NAME_CHUNK)
            .zip(bits.par_chunks_mut(NAME_CHUNK / 64))
            .enumerate()
            .map(|(c, (slots, words))| {
                let (a, b) = (c * NAME_CHUNK, ((c + 1) * NAME_CHUNK).min(idx.u));
                let (mut n, mut ok) = (0usize, 0usize);
                for k in a..b {
                    if let Some(h) = score_one(k, &mut ok) {
                        slots[k - a] = h;
                        words[(k - a) >> 6] |= 1 << (k & 63);
                        n += 1;
                    }
                }
                (n, ok)
            })
            .collect();
        let ok_entries = counts.iter().map(|c| c.1).sum();
        let total: usize = counts.iter().map(|c| c.0).sum();
        let mut t = NameTable { bits, sparse: Vec::new(), dense: Some(dense), ok_entries };
        if total <= 1 << 16 {
            t.sparse = t.iter().collect();
            if let Some(d) = t.dense.take() {
                recycle_dense(d);
            }
        }
        t
    }

    fn search_overlay(&self, q: &Query) -> Vec<Hit> {
        let now = now_secs();
        let pos: Vec<&Token> = q.tokens.iter().filter(|t| !t.negate).collect();
        let cands: Vec<(&Vec<u8>, &crate::live::OEnt)> = self.live.over.iter().filter(|(_, o)| pos.is_empty() || pos.iter().any(|t| t.fits(o.mask))).collect();
        let mut hits: Vec<Hit> = cands
            .par_iter()
            .fold(
                || (Vec::new(), HashMap::<&[u8], DirMatch, crate::index::Fx>::default()),
                |(mut out, mut memo), &(path, o)| {
                    let dir = pathwin::parent(path);
                    if let Some(score) = q.match_path_with(path, o.kind, o.size, o.mtime, |_| *memo.entry(dir).or_insert_with(|| q.dir_match(dir))) {
                        let score = score + o.prior as i32 + rank_tweaks(name_flags(pathwin::name(path)), o.kind, o.mtime, now);
                        out.push(Hit { score, idx: u32::MAX, over: Some(path.clone()) });
                    }
                    (out, memo)
                },
            )
            .map(|(out, _)| out)
            .flatten_iter()
            .collect();
        if hits.len() > q.limit {
            hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.over.cmp(&b.over)));
            hits.truncate(q.limit);
        }
        hits
    }

    fn dir_tokens(&self, names: &NameTable) -> Vec<DirMemo> {
        let idx = &self.live.base;
        let de = idx.dir_entry();
        let en = idx.ent_name();
        let mut out = vec![DirMemo::default(); idx.d];
        out.par_iter_mut().enumerate().with_min_len(1 << 12).for_each(|(k, slot)| {
            if k > 0 {
                *slot = DirMemo::own(names, en[de[k] as usize]);
            }
        });
        let dp = idx.dir_parent();
        let plan = idx.memo_plan();
        for &k in &plan.upper {
            out[k as usize] = out[k as usize].under(out[dp[k as usize] as usize]);
        }
        let roots: Vec<(u32, DirMemo)> = plan.chunks.iter().map(|(c, _)| (*c, out[*c as usize])).collect();
        let mut slices = Vec::with_capacity(plan.chunks.len());
        let mut rest: &mut [DirMemo] = &mut out;
        let mut at = 0usize;
        for (_, r) in &plan.chunks {
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(r.start as usize - at);
            let (mine, tail) = tail.split_at_mut((r.end - r.start) as usize);
            slices.push(mine);
            rest = tail;
            at = r.end as usize;
        }
        slices.into_par_iter().zip(&plan.chunks).zip(roots).for_each(|((slice, (_, r)), (c, root))| {
            let a = r.start as usize;
            for k in a..r.end as usize {
                let p = dp[k];
                let pm = if p == c { root } else { slice[p as usize - a] };
                slice[k - a] = slice[k - a].under(pm);
            }
        });
        out
    }
}

struct Scan<'a> {
    q: &'a Query,
    live: &'a Live,
    names: &'a NameTable,
    npos: usize,
    need_dirs: bool,
    now: u32,
}

impl Scan<'_> {
    #[inline(always)]
    fn score(&self, i: usize, nh: NameHit, memo: DirMemo, floor: u64, pbuf: &mut Vec<u8>) -> Option<u64> {
        let idx = &self.live.base;
        let q = self.q;
        let k = idx.kind()[i];
        if q.kind.is_some_and(|want| k & 3 != want) {
            return None;
        }
        if q.size != (0, u64::MAX) {
            let sz = crate::index::dec_size(idx.size_raw()[i]);
            if sz < q.size.0 || sz > q.size.1 {
                return None;
            }
        }
        let mtime = idx.mtime()[i];
        if q.mtime != (0, u32::MAX) && (mtime < q.mtime.0 || mtime > q.mtime.1) {
            return None;
        }
        let mut score = nh.score as i32;
        if self.need_dirs {
            let all = (1u32 << self.npos) - 1;
            if memo.bits == u32::MAX || (nh.bits as u32 | memo.bits) & all != all {
                return None;
            }
            for t in 0..self.npos {
                if nh.bits & (1 << t) == 0 {
                    score += memo.best.get(t).map_or(6, |&b| b as i32 * 3 / 4);
                }
            }
        }
        if self.live.is_dead(i as u32) {
            return None;
        }
        let p = idx.parent()[i] as usize;
        score += idx.dir_prior()[p] as i32 + rank_tweaks(nh.flags, k, mtime, self.now);
        let key = key(score, i as u32);
        if key <= floor {
            return None;
        }
        if let Some(re) = &q.path_re {
            idx.path(i, pbuf);
            if !re.is_match(pbuf) {
                return None;
            }
        }
        Some(key)
    }

    fn full(&self, lo: usize, hi: usize, memo: Option<&[DirMemo]>) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ent_name, parent) = (idx.ent_name(), idx.parent());
        top_k(hi - lo, self.q.limit, |r, top| {
            let mut pbuf = Vec::new();
            for i in lo + r.start..lo + r.end {
                let Some(nh) = self.names.get(ent_name[i]).filter(|h| h.flags & NF_OK != 0) else { continue };
                let m = memo.map_or(DirMemo::default(), |m| m[parent[i] as usize]);
                if let Some(k) = self.score(i, nh, m, top.floor, &mut pbuf) {
                    top.push(k);
                }
            }
        })
    }

    fn selective(&self, lo: usize, hi: usize) -> Vec<Hit> {
        let idx = &self.live.base;
        let (ne_off, ne, parent) = (idx.name_ents_off(), idx.name_ents(), idx.parent());
        let ok: Vec<(u32, NameHit)> = self.names.iter().filter(|(_, h)| h.flags & NF_OK != 0).collect();
        top_k(ok.len(), self.q.limit, |r, top| {
            let (mut pbuf, mut memo) = (Vec::new(), HashMap::<u32, DirMemo, crate::index::Fx>::default());
            for &(id, nh) in &ok[r] {
                for &e in &ne[ne_off[id as usize] as usize..ne_off[id as usize + 1] as usize] {
                    let i = e as usize;
                    if i < lo || i >= hi {
                        continue;
                    }
                    let m = if self.need_dirs { self.memo_of(parent[i], &mut memo) } else { DirMemo::default() };
                    if let Some(k) = self.score(i, nh, m, top.floor, &mut pbuf) {
                        top.push(k);
                    }
                }
            }
        })
    }

    fn memo_of(&self, d: u32, cache: &mut HashMap<u32, DirMemo, crate::index::Fx>) -> DirMemo {
        let idx = &self.live.base;
        let (de, en, dp) = (idx.dir_entry(), idx.ent_name(), idx.dir_parent());
        let mut chain = Vec::new();
        let mut k = d;
        let mut acc = loop {
            if k == 0 {
                break DirMemo::default();
            }
            if let Some(&m) = cache.get(&k) {
                break m;
            }
            chain.push(k);
            k = dp[k as usize];
        };
        for &k in chain.iter().rev() {
            acc = DirMemo::own(self.names, en[de[k as usize] as usize]).under(acc);
            cache.insert(k, acc);
        }
        acc
    }
}

#[derive(PartialEq)]
struct NameKey {
    tokens: Vec<(Vec<u8>, Mode, bool)>,
    exts: Vec<Vec<u8>>,
    name_re: Option<String>,
}

impl NameKey {
    fn of(q: &Query) -> NameKey {
        NameKey {
            tokens: q.tokens.iter().map(|t| (t.text.clone(), t.mode, t.negate)).collect(),
            exts: q.exts.clone(),
            name_re: q.name_re.as_ref().map(|r| r.as_str().to_string()),
        }
    }
}

pub struct Scored {
    at: std::time::Instant,
    key: NameKey,
    names: NameTable,
    memo: OnceLock<Vec<DirMemo>>,
}

#[derive(Default)]
pub struct NameCache(pub Mutex<Option<Arc<Scored>>>);

impl NameCache {
    pub fn trim_if_idle(&self, idle: std::time::Duration) {
        let mut g = self.0.lock();
        if g.as_ref().is_some_and(|s| s.at.elapsed() > idle) {
            *g = None;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct DirMemo {
    bits: u32,
    best: [i16; 4],
}

impl DirMemo {
    #[inline]
    fn own(names: &NameTable, name: u32) -> DirMemo {
        match names.get(name) {
            Some(h) if h.flags & NF_NEG != 0 => DirMemo { bits: u32::MAX, best: [0; 4] },
            Some(h) => DirMemo { bits: h.bits as u32, best: h.best },
            None => DirMemo::default(),
        }
    }
    #[inline]
    fn under(self, p: DirMemo) -> DirMemo {
        if p.bits == u32::MAX || self.bits == u32::MAX {
            return DirMemo { bits: u32::MAX, best: self.best };
        }
        let mut best = self.best;
        for t in 0..4 {
            best[t] = best[t].max(p.best[t]);
        }
        DirMemo { bits: self.bits | p.bits, best }
    }
}

static DENSE_POOL: Mutex<Vec<Vec<NameHit>>> = Mutex::new(Vec::new());

fn recycle_dense(d: Vec<NameHit>) {
    let mut p = DENSE_POOL.lock();
    if p.len() < 2 {
        p.push(d);
    }
}

struct NameTable {
    bits: Vec<u64>,
    sparse: Vec<(u32, NameHit)>,
    dense: Option<Vec<NameHit>>,
    ok_entries: usize,
}

impl Drop for NameTable {
    fn drop(&mut self) {
        if let Some(d) = self.dense.take() {
            recycle_dense(d);
        }
    }
}

impl NameTable {
    #[inline(always)]
    fn get(&self, id: u32) -> Option<NameHit> {
        if self.bits[id as usize >> 6] & (1 << (id & 63)) == 0 {
            return None;
        }
        match &self.dense {
            Some(d) => Some(d[id as usize]),
            None => self.sparse.binary_search_by_key(&id, |e| e.0).ok().map(|k| self.sparse[k].1),
        }
    }
    fn iter(&self) -> Box<dyn Iterator<Item = (u32, NameHit)> + '_> {
        match &self.dense {
            None => Box::new(self.sparse.iter().copied()),
            Some(d) => Box::new(self.bits.iter().enumerate().flat_map(move |(w, &b)| {
                let mut b = b;
                std::iter::from_fn(move || {
                    (b != 0).then(|| {
                        let id = w as u32 * 64 + b.trailing_zeros();
                        b &= b - 1;
                        (id, d[id as usize])
                    })
                })
            })),
        }
    }
}

#[derive(Clone, Copy)]
struct NameHit {
    score: i16,
    bits: u8,
    flags: u8,
    best: [i16; 4],
}

impl NameHit {
    const NONE: NameHit = NameHit { score: 0, bits: 0, flags: 0, best: [0; 4] };
}

const NF_OK: u8 = 1;
const NF_DOT: u8 = 2;
const NF_NEG: u8 = 8;

fn name_flags(name: &[u8]) -> u8 {
    let mut f = 0;
    if name.first() == Some(&b'.') || name.first() == Some(&b'$') {
        f |= NF_DOT;
    }
    f
}

#[inline]
fn rank_tweaks(flags: u8, kind: u8, mtime: u32, now: u32) -> i32 {
    let mut s = 0;
    if flags & NF_DOT != 0 {
        s -= 8;
    }
    if kind & FLAG_HIDDEN != 0 {
        s -= 8;
    }
    let age = now.saturating_sub(mtime);
    s += match age {
        0..=86_400 => 10,
        86_401..=604_800 => 7,
        604_801..=2_592_000 => 4,
        2_592_001..=31_536_000 => 1,
        _ => 0,
    };
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_fuzzy_and_filters() {
        let q = Query::parse("mian.rs ext:rs", r"C:\Users\me").unwrap();
        assert_eq!(q.tokens[0].text, b"mian.rs");
        assert_eq!(q.exts, vec![b"rs".to_vec()]);
    }

    #[test]
    fn fuzzy_finds_main() {
        assert!(fuzzy_score(b"main.rs", b"mian").is_none() || fuzzy_score(b"main.rs", b"main").is_some());
        assert!(fuzzy_score(b"main.rs", b"main").unwrap() > 50);
        assert!(token_score(b"main.rs", crate::index::name_mask(b"main.rs"), &Token {
            text: b"mian.rs".to_vec(),
            mask: b"mian.rs".iter().fold(0, |m, &b| m | char_bit(b)),
            mode: Mode::Fuzzy,
            negate: false,
            loose: !0,
            start: start_bit(b'm'),
        }).is_some());
    }

    #[test]
    fn typo_main() {
        let t = {
            let text = b"mian.rs".to_vec();
            let mask = text.iter().fold(0, |m, &b| m | char_bit(b));
            Token { text, mask, mode: Mode::Fuzzy, negate: false, loose: mask, start: start_bit(b'm') }
        };
        assert!(token_score(b"main.rs", crate::index::name_mask(b"main.rs"), &t).is_some());
    }

    #[test]
    fn index_and_search_tree() {
        let dir = std::env::temp_dir().join(format!("wsearch-smoke-{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.join("readme.md"), b"hello wsearch\n").unwrap();
        let p = dir.to_string_lossy().as_bytes().to_vec();
        let ls = crate::walk::wrap_path(&p, crate::walk::scan(&p, 2));
        assert!(ls.iter().any(|l| !l.ents.is_empty()));
        let idx = crate::index::Index::build(ls, 0, 0, &p);
        assert!(idx.n > 1);
        let live = crate::live::Live::new(idx);
        let q = Query::parse("main.rs", "").unwrap();
        let hits = Searcher { live: &live }.search(&q);
        assert!(!hits.is_empty(), "expected to find main.rs");
        let mut path = Vec::new();
        live.base.path(hits[0].idx as usize, &mut path);
        let s = String::from_utf8_lossy(&path);
        assert!(s.to_ascii_lowercase().contains("main.rs"), "hit path {s}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
