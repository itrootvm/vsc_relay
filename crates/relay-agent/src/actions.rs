use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

const CAP: usize = 4096;
const MAX_INLINE: usize = 60;

struct Store {
    map: HashMap<u32, String>,
    order: VecDeque<u32>,
    next: u32,
}

impl Store {
    fn new() -> Self {
        Store {
            map: HashMap::new(),
            order: VecDeque::new(),
            next: 1,
        }
    }

    fn put(&mut self, data: String) -> u32 {
        let id = self.next;
        self.next = if self.next == u32::MAX {
            1
        } else {
            self.next + 1
        };
        self.map.insert(id, data);
        self.order.push_back(id);
        while self.order.len() > CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        id
    }

    fn get(&self, id: u32) -> Option<String> {
        self.map.get(&id).cloned()
    }
}

static TEXT_STORE: OnceLock<Mutex<Store>> = OnceLock::new();
static CB_STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn text_store() -> &'static Mutex<Store> {
    TEXT_STORE.get_or_init(|| Mutex::new(Store::new()))
}

fn cb_store() -> &'static Mutex<Store> {
    CB_STORE.get_or_init(|| Mutex::new(Store::new()))
}

pub fn put(data: String) -> u32 {
    text_store().lock().unwrap().put(data)
}

pub fn get(id: u32) -> Option<String> {
    text_store().lock().unwrap().get(id)
}

pub fn encode(data: String) -> String {
    if data.len() <= MAX_INLINE && !data.starts_with("x:") {
        return data;
    }
    format!("x:{}", cb_store().lock().unwrap().put(data))
}

pub fn decode(data: &str) -> Option<String> {
    let id = data.strip_prefix("x:")?.parse::<u32>().ok()?;
    cb_store().lock().unwrap().get(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_output_never_exceeds_64_bytes() {
        let long = "act:sayid:".to_string() + &"超長い日本語のエイリアス".repeat(20);
        assert!(long.len() > 64);
        assert!(encode(long).len() <= 64);
        let short = "act:focus:proj".to_string();
        assert_eq!(encode(short.clone()), short);
        assert!(short.len() <= 64);
    }

    #[test]
    fn full_text_flood_never_evicts_callback_entry() {
        let handle = encode("pm:a:".to_string() + &"z".repeat(80));
        assert!(handle.starts_with("x:"));
        for i in 0..(CAP + 100) {
            put(format!("full text blob {i}"));
        }
        assert!(decode(&handle).is_some());
    }
}
