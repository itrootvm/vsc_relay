use crate::fsutil;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

struct Counts {
    day: String,
    map: HashMap<String, u64>,
}

static STATE: OnceLock<Mutex<Counts>> = OnceLock::new();

fn state() -> &'static Mutex<Counts> {
    STATE.get_or_init(|| {
        Mutex::new(Counts {
            day: String::new(),
            map: HashMap::new(),
        })
    })
}

fn stats_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("stats.json")
}

pub fn record(tag: &str) {
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut s = state().lock().unwrap();
    if s.day != day {
        s.day = day;
        s.map.clear();
    }
    *s.map.entry(tag.to_string()).or_insert(0) += 1;
    let obj = json!({
        "day": s.day,
        "sessions": s.map.get("session_started").copied().unwrap_or(0),
        "turns": s.map.get("turn_complete").copied().unwrap_or(0),
        "questions": s.map.get("question_asked").copied().unwrap_or(0),
        "errors": s.map.get("error").copied().unwrap_or(0),
    });
    let _ = fsutil::secure_write(&stats_path(), obj.to_string().as_bytes());
}
