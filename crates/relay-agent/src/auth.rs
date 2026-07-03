use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const MAX_FAILS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(900);
const WINDOW: Duration = Duration::from_secs(900);

pub enum PairResult {
    Paired,
    Wrong,
    AlreadyAuthorized,
    NoSecret,
    Throttled(u64),
}

struct Attempt {
    fails: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

pub struct Auth {
    allowed_env: HashSet<i64>,
    secret: Option<String>,
    authorized: RwLock<HashSet<i64>>,
    attempts: RwLock<HashMap<i64, Attempt>>,
    path: PathBuf,
}

impl Auth {
    pub fn load(allowed_env: Vec<i64>, secret: Option<String>) -> Self {
        let path = state_path();
        let mut authorized: HashSet<i64> = HashSet::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Vec<i64>>(&text) {
                authorized.extend(v);
            }
        }
        Auth {
            allowed_env: allowed_env.into_iter().collect(),
            secret,
            authorized: RwLock::new(authorized),
            attempts: RwLock::new(HashMap::new()),
            path,
        }
    }

    pub fn has_secret(&self) -> bool {
        self.secret.is_some()
    }

    pub async fn is_authorized(&self, chat: i64) -> bool {
        if self.allowed_env.contains(&chat) {
            return true;
        }
        self.authorized.read().await.contains(&chat)
    }

    pub async fn recipients(&self) -> Vec<i64> {
        let mut set = self.allowed_env.clone();
        set.extend(self.authorized.read().await.iter().copied());
        set.into_iter().collect()
    }

    pub async fn try_pair(&self, chat: i64, key: &str) -> PairResult {
        let Some(secret) = &self.secret else {
            return PairResult::NoSecret;
        };
        if self.is_authorized(chat).await {
            return PairResult::AlreadyAuthorized;
        }
        let now = Instant::now();
        {
            let mut map = self.attempts.write().await;
            if let Some(a) = map.get(&chat) {
                if let Some(until) = a.locked_until {
                    if until > now {
                        return PairResult::Throttled((until - now).as_secs() + 1);
                    }
                }
            }
            if let Some(a) = map.get_mut(&chat) {
                if now.duration_since(a.window_start) > WINDOW {
                    a.fails = 0;
                    a.window_start = now;
                    a.locked_until = None;
                }
            }
        }
        if constant_eq(key.trim(), secret) {
            self.attempts.write().await.remove(&chat);
            self.authorized.write().await.insert(chat);
            self.persist().await;
            PairResult::Paired
        } else {
            let mut map = self.attempts.write().await;
            let a = map.entry(chat).or_insert_with(|| Attempt {
                fails: 0,
                window_start: now,
                locked_until: None,
            });
            a.fails += 1;
            if a.fails >= MAX_FAILS {
                a.locked_until = Some(now + LOCKOUT);
                a.fails = 0;
                a.window_start = now;
                return PairResult::Throttled(LOCKOUT.as_secs());
            }
            PairResult::Wrong
        }
    }

    async fn persist(&self) {
        let ids: Vec<i64> = self.authorized.read().await.iter().copied().collect();
        if let Ok(text) = serde_json::to_string(&ids) {
            let _ = crate::fsutil::secure_write(&self.path, text.as_bytes());
        }
    }
}

fn state_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(".vsc-relay").join("authorized.json")
}

fn constant_eq(a: &str, b: &str) -> bool {
    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
}
