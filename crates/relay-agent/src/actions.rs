use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

const CAP: usize = 4096;
const MAX_INLINE: usize = 60;

struct Store {
    map: HashMap<u32, String>,
    order: VecDeque<u32>,
    next: u32,
}

static STORE: OnceLock<Mutex<Store>> = OnceLock::new();

fn store() -> &'static Mutex<Store> {
    STORE.get_or_init(|| {
        Mutex::new(Store {
            map: HashMap::new(),
            order: VecDeque::new(),
            next: 1,
        })
    })
}

pub fn put(data: String) -> u32 {
    let mut s = store().lock().unwrap();
    let id = s.next;
    s.next = if s.next == u32::MAX { 1 } else { s.next + 1 };
    s.map.insert(id, data);
    s.order.push_back(id);
    while s.order.len() > CAP {
        if let Some(old) = s.order.pop_front() {
            s.map.remove(&old);
        }
    }
    id
}

pub fn get(id: u32) -> Option<String> {
    store().lock().unwrap().map.get(&id).cloned()
}

pub fn encode(data: String) -> String {
    if data.len() <= MAX_INLINE && !data.starts_with("x:") {
        return data;
    }
    format!("x:{}", put(data))
}

pub fn decode(data: &str) -> Option<String> {
    let id = data.strip_prefix("x:")?.parse::<u32>().ok()?;
    get(id)
}
