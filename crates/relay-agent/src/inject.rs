use anyhow::{Context, Result};
use relay_ipc::Endpoint;
use std::io::Write;
use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn resolve_pid(session_id: &str) -> Option<u32> {
    let dir = home().join(".claude").join("sessions");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let txt = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("sessionId").and_then(|s| s.as_str()) == Some(session_id) {
            if let Some(pid) = v.get("pid").and_then(|x| x.as_u64()) {
                return Some(pid as u32);
            }
        }
    }
    None
}

pub fn available_pid(session_id: &str) -> Option<u32> {
    let pid = resolve_pid(session_id)?;
    if relay_ipc::endpoint_available(&Endpoint::Inject(pid)) && pid_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    relay_ipc::process_alive(pid)
}

pub(crate) fn session_id_of(pid: u32) -> Option<String> {
    let f = home()
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    let txt = std::fs::read_to_string(f).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    v.get("sessionId")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

fn session_ok(captured: Option<&str>, current: Option<&str>) -> bool {
    match (captured, current) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

pub(crate) fn session_stable(pid: u32, captured: &Option<String>) -> bool {
    pid_alive(pid) && session_ok(captured.as_deref(), session_id_of(pid).as_deref())
}

pub fn sweep_stale_sockets() {
    relay_ipc::sweep_stale();
}

fn send_line(pid: u32, v: &serde_json::Value) -> Result<()> {
    let line = format!("{v}\n");
    let mut s =
        relay_ipc::connect_blocking(&Endpoint::Inject(pid)).context("connect inject socket")?;
    s.write_all(line.as_bytes()).context("write inject")?;
    s.flush().ok();
    Ok(())
}

pub fn send_user_message(pid: u32, text: &str) -> Result<()> {
    let msg = serde_json::json!({
        "type": "user",
        "uuid": gen_uuid(),
        "session_id": "",
        "parent_tool_use_id": null,
        "message": { "role": "user", "content": [ {"type": "text", "text": text} ] }
    });
    send_line(pid, &msg)
}

pub fn send_raw(pid: u32, value: &serde_json::Value) -> Result<()> {
    send_line(pid, value)
}

pub fn send_control(pid: u32, request: serde_json::Value) -> Result<()> {
    let msg = serde_json::json!({
        "type": "control_request",
        "request_id": format!("relay-{}", gen_uuid()),
        "request": request
    });
    send_line(pid, &msg)
}

pub fn set_model(pid: u32, model: &str) -> Result<()> {
    send_control(
        pid,
        serde_json::json!({"subtype":"apply_flag_settings","settings":{"model":model}}),
    )
}

pub fn set_effort(pid: u32, level: &str) -> Result<()> {
    send_control(
        pid,
        serde_json::json!({"subtype":"apply_flag_settings","settings":{"effortLevel":level}}),
    )
}

pub fn set_mode(pid: u32, mode: &str) -> Result<()> {
    send_control(
        pid,
        serde_json::json!({"subtype":"set_permission_mode","mode":mode}),
    )
}

fn gen_uuid() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::getrandom(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::session_ok;

    #[test]
    fn session_ok_blocks_only_on_positive_mismatch() {
        assert!(session_ok(Some("s1"), Some("s1")));
        assert!(!session_ok(Some("s1"), Some("s2")));
        assert!(session_ok(Some("s1"), None));
        assert!(session_ok(None, Some("s2")));
        assert!(session_ok(None, None));
    }
}
