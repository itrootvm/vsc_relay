use relay_ipc::{BlockingConn, BlockingListener, Endpoint};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

enum Msg {
    Data(Vec<u8>),
    Close,
}

const READER_CHAN_CAP: usize = 2048;
const RING_CAP: usize = 64;
const RING_TTL: Duration = Duration::from_secs(10 * 60);

type Shared = Arc<Vec<u8>>;

struct RingEntry {
    request_id: String,
    tool_use_id: String,
    line: Shared,
    at: Instant,
}

struct Hub {
    readers: Vec<SyncSender<Shared>>,
    ring: VecDeque<RingEntry>,
}

impl Hub {
    fn new() -> Arc<Mutex<Hub>> {
        Arc::new(Mutex::new(Hub {
            readers: Vec::new(),
            ring: VecDeque::new(),
        }))
    }

    fn register(&mut self) -> (Receiver<Shared>, Vec<Shared>) {
        let (tx, rx) = mpsc::sync_channel::<Shared>(READER_CHAN_CAP);
        self.readers.push(tx);
        let backlog = self.ring.iter().map(|e| e.line.clone()).collect();
        (rx, backlog)
    }

    fn broadcast(&mut self, line: &Shared) {
        self.readers
            .retain(|tx| matches!(tx.try_send(line.clone()), Ok(())));
    }

    fn note_request(&mut self, line: &Shared) {
        if let Some((request_id, tool_use_id)) = parse_can_use_tool(line) {
            self.ring.push_back(RingEntry {
                request_id,
                tool_use_id,
                line: line.clone(),
                at: Instant::now(),
            });
            while self.ring.len() > RING_CAP {
                self.ring.pop_front();
            }
        }
    }

    fn evict_request(&mut self, request_id: &str) {
        self.ring.retain(|e| e.request_id != request_id);
    }

    fn note_stdout(&mut self, line: &[u8], now: Instant) {
        if self.ring.is_empty() {
            return;
        }
        self.ring.retain(|e| {
            now.duration_since(e.at) < RING_TTL && !line_contains(line, e.tool_use_id.as_bytes())
        });
    }
}

fn line_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn parse_can_use_tool(line: &[u8]) -> Option<(String, String)> {
    if !line_contains(line, b"control_request") || !line_contains(line, b"can_use_tool") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("control_request") {
        return None;
    }
    let req = v.get("request")?;
    if req.get("subtype").and_then(|s| s.as_str()) != Some("can_use_tool") {
        return None;
    }
    let request_id = v.get("request_id").and_then(|x| x.as_str())?.to_string();
    if request_id.is_empty() {
        return None;
    }
    let tool_use_id = req
        .get("tool_use_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some((request_id, tool_use_id))
}

fn serve_reader(mut conn: BlockingConn, rx: Receiver<Shared>, backlog: Vec<Shared>) {
    for line in backlog {
        if conn.write_all(&line).and_then(|_| conn.flush()).is_err() {
            return;
        }
    }
    while let Ok(line) = rx.recv() {
        if conn.write_all(&line).and_then(|_| conn.flush()).is_err() {
            return;
        }
    }
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
        .with_file_name(format!("claude.real{}", std::env::consts::EXE_SUFFIX))
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

fn dirs_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

#[cfg(unix)]
fn spawn_child(
    real: &Path,
    arg0: &OsString,
    rest: &[OsString],
) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    Command::new(real)
        .arg0(arg0)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
}

#[cfg(windows)]
fn spawn_child(
    real: &Path,
    _arg0: &OsString,
    rest: &[OsString],
) -> std::io::Result<std::process::Child> {
    Command::new(real)
        .args(rest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
}

#[cfg(unix)]
fn fallback_exec(real: &Path, arg0: &OsString, rest: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    run_log("fallback exec (proxy setup failed)");
    let err = Command::new(real).arg0(arg0).args(rest).exec();
    eprintln!("vsc-claude-shim: exec fallback failed: {err}");
    std::process::exit(127);
}

#[cfg(windows)]
fn fallback_exec(real: &Path, _arg0: &OsString, rest: &[OsString]) -> ! {
    run_log("fallback spawn (proxy setup failed)");
    let status = Command::new(real)
        .args(rest)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(err) => {
            eprintln!("vsc-claude-shim: spawn fallback failed: {err}");
            std::process::exit(127);
        }
    }
}

#[cfg(unix)]
fn confine_child(_child: &std::process::Child) {}

#[cfg(windows)]
fn confine_child(child: &std::process::Child) {
    use std::os::raw::c_void;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
    }
}

const LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;

fn open_capped(path: &Path) -> Option<std::fs::File> {
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() >= LOG_CAP_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    set_private_mode(&mut opts);
    if oversized {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    opts.open(path).ok()
}

#[cfg(unix)]
fn set_private_mode(opts: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
}

#[cfg(windows)]
fn set_private_mode(_opts: &mut std::fs::OpenOptions) {}

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

    let mut child = match spawn_child(&real, &arg0, &rest) {
        Ok(c) => c,
        Err(_) => fallback_exec(&real, &arg0, &rest),
    };

    confine_child(&child);

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

    let hub = Hub::new();
    if let Ok(mut listener) = BlockingListener::bind(&Endpoint::Out(child_pid)) {
        let hub_l = hub.clone();
        std::thread::spawn(move || loop {
            let conn = match listener.accept() {
                Ok(conn) => conn,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            let (rx, backlog) = match hub_l.lock() {
                Ok(mut h) => h.register(),
                Err(_) => continue,
            };
            std::thread::spawn(move || serve_reader(conn, rx, backlog));
        });
    }
    let ext_out: Arc<Mutex<std::io::Stdout>> = Arc::new(Mutex::new(std::io::stdout()));

    let hub_pump = hub.clone();
    let ext_out_pump = ext_out.clone();
    std::thread::spawn(move || {
        let mut r = BufReader::new(child_stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {
                    match ext_out_pump.lock() {
                        Ok(mut o) => {
                            if o.write_all(&line).is_err() {
                                break;
                            }
                            let _ = o.flush();
                        }
                        Err(_) => break,
                    }
                    let shared: Shared = Arc::new(line.clone());
                    if let Ok(mut h) = hub_pump.lock() {
                        h.note_stdout(&line, Instant::now());
                        h.note_request(&shared);
                        h.broadcast(&shared);
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

    if let Ok(mut listener) = BlockingListener::bind(&Endpoint::Inject(child_pid)) {
        let tx_inj = tx.clone();
        let ext_out_inj = ext_out.clone();
        let hub_inj = hub.clone();
        std::thread::spawn(move || loop {
            let conn = match listener.accept() {
                Ok(conn) => conn,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            let tx_c = tx_inj.clone();
            let eo = ext_out_inj.clone();
            let hub_c = hub_inj.clone();
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
                            if let Some(id) = &cancel {
                                if let Ok(mut h) = hub_c.lock() {
                                    h.evict_request(id);
                                }
                            }
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
        });
    }

    let tx_host = tx.clone();
    let dbg_sid = sid.clone();
    std::thread::spawn(move || {
        forward_host_stdin(tx_host, dbg_sid);
    });

    let status = child.wait();
    relay_ipc::cleanup(&Endpoint::Inject(child_pid));
    relay_ipc::cleanup(&Endpoint::Out(child_pid));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Hub {
        Hub {
            readers: Vec::new(),
            ring: VecDeque::new(),
        }
    }

    fn share(s: &str) -> Shared {
        Arc::new(s.as_bytes().to_vec())
    }

    #[test]
    fn parses_can_use_tool() {
        let line = br#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tu-1","input":{}}}"#;
        assert_eq!(
            parse_can_use_tool(line),
            Some(("req-1".to_string(), "tu-1".to_string()))
        );
    }

    #[test]
    fn ignores_non_can_use_tool() {
        let init =
            br#"{"type":"control_request","request_id":"r","request":{"subtype":"initialize"}}"#;
        assert_eq!(parse_can_use_tool(init), None);
        let asst = br#"{"type":"assistant","message":{"content":[]}}"#;
        assert_eq!(parse_can_use_tool(asst), None);
    }

    #[test]
    fn ring_holds_and_evicts_by_request_id() {
        let mut h = hub();
        h.note_request(&share(
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tu-1"}}"#,
        ));
        assert_eq!(h.ring.len(), 1);
        h.evict_request("req-1");
        assert_eq!(h.ring.len(), 0);
    }

    #[test]
    fn stdout_referencing_tool_use_id_evicts() {
        let mut h = hub();
        h.note_request(&share(
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tu-1"}}"#,
        ));
        h.note_stdout(
            br#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu-1"}]}}"#,
            Instant::now(),
        );
        assert_eq!(h.ring.len(), 0);
    }

    #[test]
    fn new_reader_gets_ring_backlog() {
        let mut h = hub();
        h.note_request(&share(
            r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tu-1"}}"#,
        ));
        let (_rx, backlog) = h.register();
        assert_eq!(backlog.len(), 1);
    }

    #[test]
    fn broadcast_drops_disconnected_reader() {
        let mut h = hub();
        let (rx, _backlog) = h.register();
        assert_eq!(h.readers.len(), 1);
        drop(rx);
        h.broadcast(&share("line\n"));
        assert_eq!(h.readers.len(), 0);
    }

    #[test]
    fn broadcast_drops_full_reader() {
        let mut h = hub();
        let (_rx, _backlog) = h.register();
        for _ in 0..(READER_CHAN_CAP + 2) {
            h.broadcast(&share("x\n"));
        }
        assert_eq!(h.readers.len(), 0);
    }
}
