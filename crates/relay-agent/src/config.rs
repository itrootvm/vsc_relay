use crate::hostname;

const DEFAULT_INTERVAL_SECS: u64 = 2;
const DEFAULT_CODEX_MAX_AGE_HOURS: u64 = 24;

pub struct Config {
    pub machine_name: String,
    pub telegram_token: Option<String>,
    pub allowed_chats: Vec<i64>,
    pub pair_secret: Option<String>,
    pub interval: u64,
    pub codex_max_age_ms: i64,
}

impl Config {
    pub fn from_env() -> Self {
        let machine_name = env_nonempty("RELAY_MACHINE_NAME").unwrap_or_else(hostname::hostname);
        let telegram_token = env_nonempty("TELEGRAM_BOT_TOKEN");
        let pair_secret = env_nonempty("RELAY_PAIR_SECRET");
        let allowed_chats = std::env::var("TELEGRAM_ALLOWED_CHATS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|s| s.trim().parse::<i64>().ok())
            .fold(Vec::new(), |mut out, chat| {
                if !out.contains(&chat) {
                    out.push(chat);
                }
                out
            });
        let interval = std::env::var("RELAY_INTERVAL")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_INTERVAL_SECS);
        let codex_max_age_ms = std::env::var("RELAY_CODEX_MAX_AGE_H")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|v| *v > 0)
            .and_then(hours_to_ms)
            .unwrap_or_else(|| hours_to_ms(DEFAULT_CODEX_MAX_AGE_HOURS).unwrap_or(i64::MAX));
        Self {
            machine_name,
            telegram_token,
            allowed_chats,
            pair_secret,
            interval,
            codex_max_age_ms,
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn hours_to_ms(hours: u64) -> Option<i64> {
    let ms = hours.checked_mul(60)?.checked_mul(60)?.checked_mul(1000)?;
    i64::try_from(ms).ok()
}
