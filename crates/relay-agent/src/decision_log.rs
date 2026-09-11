use crate::log_file::RotatingLog;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const ROTATE_BYTES: u64 = 8 * 1024 * 1024;
const GENERATIONS: u8 = 16;

pub fn record(channel: &str, outcome: &str, fields: Value) {
    let mut record = Map::new();
    record.insert("v".into(), Value::from(1));
    record.insert("at".into(), Value::from(stamp()));
    record.insert("channel".into(), Value::from(channel));
    record.insert("outcome".into(), Value::from(outcome));
    if let Value::Object(extra) = fields {
        for (key, value) in extra {
            record.insert(key, value);
        }
    }
    let Ok(mut line) = serde_json::to_string(&Value::Object(record)) else {
        return;
    };
    line.push('\n');
    let Some(log) = log() else { return };
    let mut log = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = log.write_all(line.as_bytes());
    let _ = log.flush();
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn log() -> Option<&'static Mutex<RotatingLog>> {
    static LOG: OnceLock<Option<Mutex<RotatingLog>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let path = path()?;
        Some(Mutex::new(RotatingLog::with_limits(
            path,
            ROTATE_BYTES,
            GENERATIONS,
        )))
    })
    .as_ref()
}

fn path() -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("VSC_RELAY_DECISION_LOG") {
        if !from_env.trim().is_empty() {
            return Some(PathBuf::from(from_env));
        }
    }
    Some(dirs::home_dir()?.join(".vsc-relay").join("decisions.jsonl"))
}

fn tally(lines: impl Iterator<Item = String>, cutoff: &str, group_by: &str) -> Vec<(String, u64)> {
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for line in lines {
        let Ok(Value::Object(record)) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let at = record.get("at").and_then(Value::as_str).unwrap_or("");
        if at < cutoff {
            continue;
        }
        let key = record
            .get(group_by)
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "(unset)".to_string());
        *counts.entry(key).or_default() += 1;
    }
    let mut tally: Vec<(String, u64)> = counts.into_iter().collect();
    tally.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    tally
}

fn parse_window(since: &str) -> Option<i64> {
    let since = since.trim();
    let (value, unit) = since.split_at(since.len().checked_sub(1)?);
    let value: i64 = value.parse().ok()?;
    match unit {
        "m" => Some(value * 60),
        "h" => Some(value * 3600),
        "d" => Some(value * 86400),
        _ => None,
    }
}

fn generations() -> Vec<PathBuf> {
    let Some(live) = path() else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = (1..=GENERATIONS)
        .rev()
        .map(|n| {
            let mut name = live.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".{n}"));
            live.with_file_name(name)
        })
        .filter(|candidate| candidate.exists())
        .collect();
    files.push(live);
    files
}

pub fn report(since: &str, group_by: &str) -> anyhow::Result<()> {
    let window = parse_window(since)
        .ok_or_else(|| anyhow::anyhow!("--since takes a value like 30m, 24h or 7d"))?;
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(window))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let lines = generations()
        .into_iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|body| body.lines().map(str::to_string).collect::<Vec<_>>());
    let tally = tally(lines, &cutoff, group_by);
    if tally.is_empty() {
        println!("no decisions recorded in the last {since}");
        return Ok(());
    }
    let total: u64 = tally.iter().map(|(_, count)| count).sum();
    let width = tally
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 48);
    println!("decisions by {group_by}, last {since} ({total} total)");
    for (key, count) in tally {
        let key: String = key.chars().take(width).collect();
        println!(
            "  {key:<width$}  {count:>7}  {:>5.1}%",
            100.0 * count as f64 / total as f64
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_record_carries_the_two_fields_every_reading_groups_by() {
        let dir = std::env::temp_dir().join(format!("vsc-relay-decisions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("decisions.jsonl");

        let mut log = RotatingLog::with_limits(path.clone(), ROTATE_BYTES, GENERATIONS);
        let mut record = Map::new();
        record.insert("v".into(), Value::from(1));
        record.insert("channel".into(), Value::from("hook"));
        record.insert("outcome".into(), Value::from("ask"));
        if let Value::Object(extra) = json!({"tool": "Bash", "reason": "not dangerous"}) {
            for (key, value) in extra {
                record.insert(key, value);
            }
        }
        let mut line = serde_json::to_string(&Value::Object(record)).expect("json");
        line.push('\n');
        log.write_all(line.as_bytes()).expect("write");
        log.flush().expect("flush");

        let written = std::fs::read_to_string(&path).expect("read");
        let parsed: Value = serde_json::from_str(written.trim()).expect("one json object per line");
        assert_eq!(parsed["channel"], "hook");
        assert_eq!(parsed["outcome"], "ask");
        assert_eq!(
            parsed["tool"], "Bash",
            "caller fields survive alongside the common ones"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tally_counts_one_field_and_drops_anything_before_the_window() {
        let lines = vec![
            json!({"at":"2026-09-07T10:00:00.000Z","outcome":"ask","tool":"Bash"}).to_string(),
            json!({"at":"2026-09-07T10:00:01.000Z","outcome":"ask","tool":"Bash"}).to_string(),
            json!({"at":"2026-09-07T10:00:02.000Z","outcome":"deny","tool":"Write"}).to_string(),
            json!({"at":"2026-09-01T00:00:00.000Z","outcome":"deny","tool":"Write"}).to_string(),
            "not json at all".to_string(),
        ];

        let tally = tally(lines.into_iter(), "2026-09-07T00:00:00.000Z", "outcome");

        assert_eq!(
            tally,
            vec![("ask".to_string(), 2), ("deny".to_string(), 1)],
            "counts sort by frequency, the older record is outside the window, junk is skipped"
        );
    }

    #[test]
    fn a_window_reads_minutes_hours_and_days() {
        assert_eq!(parse_window("30m"), Some(1800));
        assert_eq!(parse_window("24h"), Some(86400));
        assert_eq!(parse_window("7d"), Some(604800));
        assert_eq!(parse_window("24"), None, "a bare number names no unit");
        assert_eq!(parse_window(""), None);
    }

    #[test]
    fn a_stamp_is_sortable_and_utc() {
        let stamp = stamp();
        assert!(
            stamp.ends_with('Z'),
            "records are stamped in UTC so windows line up with the text log: {stamp}"
        );
        assert!(
            chrono::DateTime::parse_from_rfc3339(&stamp).is_ok(),
            "the stamp parses back: {stamp}"
        );
    }
}
