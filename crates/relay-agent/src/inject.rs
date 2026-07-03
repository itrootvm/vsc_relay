use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn socket_for_pid(pid: u32) -> PathBuf {
    home()
        .join(".vsc-relay")
        .join("inject")
        .join(format!("{pid}.sock"))
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
    if socket_for_pid(pid).exists() && pid_alive(pid) {
        Some(pid)
    } else {
        None
    }
}

fn pid_alive(pid: u32) -> bool {
    let res = unsafe { libc::kill(pid as i32, 0) };
    if res == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn sweep_stale_sockets() {
    for sub in ["inject", "out"] {
        let dir = home().join(".vsc-relay").join(sub);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(pid) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !pid_alive(pid) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn send_line(pid: u32, v: &serde_json::Value) -> Result<()> {
    let line = format!("{v}\n");
    let mut s = UnixStream::connect(socket_for_pid(pid)).context("connect inject socket")?;
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
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}
