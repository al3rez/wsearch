//! JSON-lines client for the wsearch daemon.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub kind: String,
    pub size: u64,
    pub mtime: u32,
    pub score: i32,
}

pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub took_us: u64,
    pub error: Option<String>,
    pub indexing: bool,
    pub entries: usize,
}

fn roundtrip(req: &Value) -> Result<Value, String> {
    let mut s = crate::connect(&crate::default_dir()).map_err(|e| e.to_string())?;
    writeln!(s, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| e.to_string())
}

pub fn search(q: &str, limit: usize) -> SearchResult {
    match roundtrip(&json!({"q": q, "limit": limit})) {
        Ok(v) => {
            let hits = v["hits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|h| {
                    Some(Hit {
                        path: h["path"].as_str()?.to_string(),
                        kind: h["kind"].as_str().unwrap_or("file").to_string(),
                        size: h["size"].as_u64().unwrap_or(0),
                        mtime: h["mtime"].as_u64().unwrap_or(0) as u32,
                        score: h["score"].as_i64().unwrap_or(0) as i32,
                    })
                })
                .collect();
            SearchResult {
                hits,
                took_us: v["took_us"].as_u64().unwrap_or(0),
                error: if v["ok"] == true { None } else { Some(v["error"].as_str().unwrap_or("error").to_string()) },
                indexing: false,
                entries: 0,
            }
        }
        Err(e) => SearchResult { hits: Vec::new(), took_us: 0, error: Some(e), indexing: true, entries: 0 },
    }
}

pub fn status() -> SearchResult {
    match roundtrip(&json!({"op": "status"})) {
        Ok(v) => SearchResult {
            hits: Vec::new(),
            took_us: 0,
            error: if v["ok"] == true { None } else { Some(v["error"].as_str().unwrap_or("indexing").to_string()) },
            indexing: v["ok"] != true,
            entries: v["entries"].as_u64().unwrap_or(0) as usize,
        },
        Err(e) => SearchResult { hits: Vec::new(), took_us: 0, error: Some(e), indexing: true, entries: 0 },
    }
}
