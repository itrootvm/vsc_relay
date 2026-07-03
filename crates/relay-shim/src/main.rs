use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

enum Msg {
    Data(Vec<u8>),
    Close,
}

fn cancel_enabled() -> bool {
    if std::env::var_os("VSC_RELAY_NO_CANCEL").is_some() {
        return false;
    }
    !dirs_home().join(".vsc-relay").join("no-cancel").exists()
}

fn is_control_response(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .map(|s| s.contains("control_response"))
        .unwrap_or(false)
}

fn response_request_id(line: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("control_response") {
        return None;
    }
    v.get("response")
        .and_then(|r| r.get("request_id"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

fn emit_cancel(ext_out: &Arc<Mutex<std::io::Stdout>>, request_id: &str) {
    let v = serde_json::json!({"type": "control_cancel_request", "request_id": request_id});
    let mut line = v.to_string();
    line.push('\n');
    if let Ok(mut o) = ext_out.lock() {
        let _ = o.write_all(line.as_bytes());
        let _ = o.flush();
    }
}

fn real_path() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .with_file_name("claude.real")
}

fn session_id(args: &[OsString]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy();
        if s == "--resume" {
            return it.next().map(|v| v.to_string_lossy().to_string());
        }
        if let Some(v) = s.strip_prefix("--resume=") {
            return Some(v.to_string());
        }
    }
    None
}

fn inject_sock_path(pid: u32) -> PathBuf {
    dirs_home()
        .join(".vsc-relay")
        .join("inject")
        .join(format!("{pid}.sock"))
}

fn out_sock_path(pid: u32) -> PathBuf {
    dirs_home()
        .join(".vsc-relay")
        .join("out")
        .join(format!("{pid}.sock"))
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn fallback_exec(real: &PathBuf, arg0: &OsString, rest: &[OsString]) -> ! {
    run_log("fallback exec (proxy setup failed)");
    let err = Command::new(real).arg0(arg0).args(rest).exec();
    eprintln!("vsc-claude-shim: exec fallback failed: {err}");
    std::process::exit(127);
}

const LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;

fn ensure_secure_dir(dir: &std::path::Path) {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

fn open_capped(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() >= LOG_CAP_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).mode(0o600);
    if oversized {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(path).ok()
}

fn run_log(msg: &str) {
    let base = dirs_home().join(".vsc-relay");
    if !base.join("shim-debug").exists() {
        return;
    }
    if let Some(mut f) = open_capped(&base.join("shim-run.log")) {
        let pid = std::process::id();
        let _ = writeln!(f, "[shim {pid}] {msg}");
    }
}

fn main() {
    let real = real_path();
    let argv: Vec<OsString> = std::env::args_os().collect();
    let arg0 = argv.first().cloned().unwrap_or_else(|| real.clone().into());
    let rest: Vec<OsString> = argv.iter().skip(1).cloned().collect();
    let sid = session_id(&rest);

    let mut child = match Command::new(&real)
        .arg0(&arg0)
        .args(&rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => fallback_exec(&real, &arg0, &rest),
    };

    let child_pid = child.id();
    run_log(&format!("spawned child pid={child_pid} sid={sid:?}"));
    let child_stdin = match child.stdin.take() {
        Some(s) => s,
        None => fallback_exec(&real, &arg0, &rest),
    };
    let child_stdout = match child.stdout.take() {
        Some(s) => s,
        None => fallback_exec(&real, &arg0, &rest),
    };

    let readers: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let path = out_sock_path(child_pid);
        if let Some(dir) = path.parent() {
            ensure_secure_dir(dir);
        }
        let _ = std::fs::remove_file(&path);
        if let Ok(listener) = UnixListener::bind(&path) {
            let readers_l = readers.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    if let Ok(mut rs) = readers_l.lock() {
                        rs.push(conn);
                    }
                }
            });
        }
    }
    let ext_out: Arc<Mutex<std::io::Stdout>> = Arc::new(Mutex::new(std::io::stdout()));

    let readers_pump = readers.clone();
    let ext_out_pump = ext_out.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::new(child_stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(mut rs) = readers_pump.lock() {
                        rs.retain_mut(|s| s.write_all(&line).and_then(|_| s.flush()).is_ok());
                    }
                    match ext_out_pump.lock() {
                        Ok(mut o) => {
                            if o.write_all(&line).is_err() {
                                break;
                            }
                            let _ = o.flush();
                        }
                        Err(_) => break,
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (tx, rx) = mpsc::channel::<Msg>();

    std::thread::spawn(move || {
        let mut w = child_stdin;
        for msg in rx {
            match msg {
                Msg::Data(b) => {
                    if w.write_all(&b).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
                Msg::Close => break,
            }
        }
    });

    {
        let path = inject_sock_path(child_pid);
        if let Some(dir) = path.parent() {
            ensure_secure_dir(dir);
        }
        let _ = std::fs::remove_file(&path);
        if let Ok(listener) = UnixListener::bind(&path) {
            let tx_inj = tx.clone();
            let ext_out_inj = ext_out.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    let tx_c = tx_inj.clone();
                    let eo = ext_out_inj.clone();
                    std::thread::spawn(move || {
                        let mut r = BufReader::new(conn);
                        let mut line = Vec::new();
                        loop {
                            line.clear();
                            match r.read_until(b'\n', &mut line) {
                                Ok(0) => break,
                                Ok(_) => {
                                    let cancel = if is_control_response(&line) {
                                        response_request_id(&line)
                                    } else {
                                        None
                                    };
                                    let mut out = line.clone();
                                    if out.last() != Some(&b'\n') {
                                        out.push(b'\n');
                                    }
                                    if tx_c.send(Msg::Data(out)).is_err() {
                                        break;
                                    }
                                    if let Some(id) = cancel {
                                        if cancel_enabled() {
                                            emit_cancel(&eo, &id);
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    });
                }
            });
        }
    }

    let tx_host = tx.clone();
    let dbg_sid = sid.clone();
    std::thread::spawn(move || {
        forward_host_stdin(tx_host, dbg_sid);
    });

    let status = child.wait();
    let _ = std::fs::remove_file(inject_sock_path(child_pid));
    let _ = std::fs::remove_file(out_sock_path(child_pid));
    let code = status.ok().and_then(|s| s.code()).unwrap_or(0);
    run_log(&format!("child pid={child_pid} exited code={code}"));
    std::process::exit(code);
}

fn forward_host_stdin(tx: Sender<Msg>, sid: Option<String>) {
    let debug = dirs_home().join(".vsc-relay").join("shim-debug").exists();
    let mut reader = BufReader::new(std::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => {
                let _ = tx.send(Msg::Close);
                break;
            }
            Ok(_) => {
                if debug {
                    log_line(sid.as_deref(), &line);
                }
                if tx.send(Msg::Data(line.clone())).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(Msg::Close);
                break;
            }
        }
    }
}

fn log_line(sid: Option<&str>, line: &[u8]) {
    use std::io::Write as _;
    let path = dirs_home().join(".vsc-relay").join("shim-in.log");
    if let Some(mut f) = open_capped(&path) {
        let _ = write!(f, "[{}] ", sid.unwrap_or("?"));
        let _ = f.write_all(line);
    }
}

#[allow(dead_code)]
fn drain<R: Read>(mut r: R) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf) {
        if n == 0 {
            break;
        }
    }
}
