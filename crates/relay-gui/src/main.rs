#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const APP_TITLE: &str = "VS Code Agent Relay";
const RELEASES_URL: &str = "https://github.com/itrootvm/vsc_relay/releases/latest";
const LOG_CAP: usize = 1000;
const ENV_KEYS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "RELAY_PAIR_SECRET",
    "TELEGRAM_ALLOWED_CHATS",
    "RELAY_MACHINE_NAME",
    "RELAY_INTERVAL",
    "RELAY_CODEX_MAX_AGE_H",
    "VSC_RELAY_AUTO_UPDATE",
];

enum UpdateEvent {
    Available(String),
    UpToDate,
    Done(bool, String),
}

fn hidden_cmd<S: AsRef<std::ffi::OsStr>>(program: S) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_TITLE)
            .with_inner_size([760.0, 580.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(RelayApp::new(cc)))),
    )
}

struct RelayApp {
    token: String,
    secret: String,
    reveal_token: bool,
    reveal_secret: bool,
    env_map: BTreeMap<String, String>,

    running: bool,
    child: Option<Child>,
    child_pid: Option<u32>,

    log: Vec<String>,
    log_rx: Receiver<String>,
    log_tx: Sender<String>,
    send_status_rx: Receiver<String>,
    send_status_tx: Sender<String>,
    note: String,

    sessions: Vec<SessionCard>,
    sessions_tx: Sender<Vec<SessionCard>>,
    sessions_rx: Receiver<Vec<SessionCard>>,
    active_session: Option<String>,
    compose: String,
    compose_media: String,
    last_sessions: Instant,

    providers: Vec<ProviderRow>,
    providers_tx: Sender<Vec<ProviderRow>>,
    providers_rx: Receiver<Vec<ProviderRow>>,
    provider_health: std::collections::HashMap<String, (String, String)>,
    health_tx: Sender<Vec<(String, String, String)>>,
    health_rx: Receiver<Vec<(String, String, String)>>,
    key_input: std::collections::HashMap<String, String>,
    checking_health: bool,

    show_settings: bool,
    show_help: bool,

    vscode_ok: bool,
    extension_ok: bool,
    shim_installed: bool,
    ext_version: String,
    sessions_today: i64,
    turns_today: i64,
    launch_at_login: bool,
    auto_mode: String,
    smart_on: bool,
    steer_on: bool,
    gate_on: bool,
    feedback_on: bool,
    budget_input: String,
    usage_session: String,
    usage_text: String,
    pins_text: String,
    pin_target: String,
    pin_obligation: String,
    pin_epoch: String,
    semantic_provider: String,
    semantic_model: String,
    semantic_endpoint: String,
    semantic_local_dir: String,
    semantic_train_dataset: String,
    semantic_train_output: String,
    semantic_trust: bool,
    semantic_key: String,

    auto_update: bool,
    update_available: bool,
    update_note: String,
    updating: bool,
    update_tx: Sender<UpdateEvent>,
    update_rx: Receiver<UpdateEvent>,

    last_env: Instant,
    last_stats: Instant,
    last_update_check: Instant,

    agent_bin: PathBuf,
    cfg_dir: PathBuf,
    env_file: PathBuf,
    relay_dir: PathBuf,
    stats_file: PathBuf,
    log_file: PathBuf,
}

impl RelayApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let cfg_dir = gui_config_dir();
        let env_file = cfg_dir.join("relay.env");
        let relay_dir = home.join(".vsc-relay");
        let stats_file = relay_dir.join("stats.json");
        let log_file = relay_dir.join("agent.log");
        let (log_tx, log_rx) = channel();
        let (send_status_tx, send_status_rx) = channel();
        let (update_tx, update_rx) = channel();
        let (sessions_tx, sessions_rx) = channel();
        let (providers_tx, providers_rx) = channel();
        let (health_tx, health_rx) = channel();

        let env_map = parse_env_file(&env_file);
        let token = env_map
            .get("TELEGRAM_BOT_TOKEN")
            .cloned()
            .unwrap_or_default();
        let secret = env_map
            .get("RELAY_PAIR_SECRET")
            .cloned()
            .unwrap_or_default();
        let auto_update = env_map
            .get("VSC_RELAY_AUTO_UPDATE")
            .map(|s| s == "1")
            .unwrap_or(false);

        let mut app = Self {
            token,
            secret,
            reveal_token: false,
            reveal_secret: false,
            env_map,
            running: false,
            child: None,
            child_pid: None,
            log: Vec::new(),
            log_rx,
            log_tx,
            send_status_rx,
            send_status_tx,
            note: String::new(),
            sessions: Vec::new(),
            sessions_tx,
            sessions_rx,
            active_session: None,
            compose: String::new(),
            compose_media: String::new(),
            last_sessions: Instant::now(),
            providers: Vec::new(),
            providers_tx,
            providers_rx,
            provider_health: std::collections::HashMap::new(),
            health_tx,
            health_rx,
            key_input: std::collections::HashMap::new(),
            checking_health: false,
            show_settings: false,
            show_help: false,
            vscode_ok: true,
            extension_ok: true,
            shim_installed: false,
            ext_version: "-".to_string(),
            sessions_today: 0,
            turns_today: 0,
            launch_at_login: false,
            auto_mode: "manual".to_string(),
            smart_on: false,
            steer_on: false,
            gate_on: false,
            feedback_on: true,
            budget_input: String::new(),
            usage_session: String::new(),
            usage_text: String::new(),
            pins_text: String::new(),
            pin_target: String::new(),
            pin_obligation: String::new(),
            pin_epoch: String::new(),
            semantic_provider: "local".to_string(),
            semantic_model: String::new(),
            semantic_endpoint: String::new(),
            semantic_local_dir: String::new(),
            semantic_train_dataset: String::new(),
            semantic_train_output: String::new(),
            semantic_trust: false,
            semantic_key: String::new(),
            auto_update,
            update_available: false,
            update_note: String::new(),
            updating: false,
            update_tx,
            update_rx,
            last_env: Instant::now(),
            last_stats: Instant::now(),
            last_update_check: Instant::now(),
            agent_bin: find_agent_binary(),
            cfg_dir,
            env_file,
            relay_dir,
            stats_file,
            log_file,
        };
        app.launch_at_login = app.is_autostart_enabled();
        app.load_auto_mode();
        app.load_smart();
        app.refresh_env();
        app.refresh_stats();
        app.refresh_sessions();
        app.refresh_providers();
        app.check_update();
        if app.has_secrets() {
            app.start();
        }
        app
    }

    fn check_update(&self) {
        let bin = self.agent_bin.clone();
        let tx = self.update_tx.clone();
        std::thread::spawn(move || {
            let ev = match hidden_cmd(&bin).arg("self-update").arg("--check").output() {
                Ok(o) => {
                    let out = String::from_utf8_lossy(&o.stdout);
                    if out.contains("update=true") {
                        let latest = out
                            .split_whitespace()
                            .find_map(|t| t.strip_prefix("latest="))
                            .unwrap_or("new")
                            .to_string();
                        UpdateEvent::Available(latest)
                    } else {
                        UpdateEvent::UpToDate
                    }
                }
                Err(_) => UpdateEvent::UpToDate,
            };
            let _ = tx.send(ev);
        });
    }

    fn do_update(&mut self) {
        if self.updating {
            return;
        }
        self.updating = true;
        self.update_note = "Updating…".to_string();
        let bin = self.agent_bin.clone();
        let tx = self.update_tx.clone();
        let logtx = self.log_tx.clone();
        std::thread::spawn(move || {
            let (ok, msg) = match hidden_cmd(&bin).arg("self-update").output() {
                Ok(o) => {
                    for l in String::from_utf8_lossy(&o.stdout).lines() {
                        let _ = logtx.send(l.to_string());
                    }
                    for l in String::from_utf8_lossy(&o.stderr).lines() {
                        let _ = logtx.send(l.to_string());
                    }
                    (o.status.success(), String::new())
                }
                Err(e) => (false, e.to_string()),
            };
            let _ = tx.send(UpdateEvent::Done(ok, msg));
        });
    }

    fn set_auto_update(&mut self, on: bool) {
        self.auto_update = on;
        self.env_map.insert(
            "VSC_RELAY_AUTO_UPDATE".to_string(),
            if on { "1" } else { "0" }.to_string(),
        );
        self.save_env();
    }

    fn relaunch(&mut self) {
        self.stop();
        if let Ok(exe) = std::env::current_exe() {
            let _ = hidden_cmd(exe).stdin(Stdio::null()).spawn();
        }
        std::process::exit(0);
    }

    fn has_secrets(&self) -> bool {
        !self.token.trim().is_empty() && !self.secret.trim().is_empty()
    }

    #[cfg(not(windows))]
    fn autostart_path(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".config")
            .join("autostart")
            .join("vsc-relay.desktop")
    }

    #[cfg(not(windows))]
    fn is_autostart_enabled(&self) -> bool {
        self.autostart_path().exists()
    }

    #[cfg(windows)]
    fn is_autostart_enabled(&self) -> bool {
        hidden_cmd("reg")
            .args([
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "VSCRelay",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn refresh_env(&mut self) {
        let Ok(out) = hidden_cmd(&self.agent_bin).arg("env-check").output() else {
            return;
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            let on = v == "true";
            match k {
                "vscode" => self.vscode_ok = on,
                "extension" => self.extension_ok = on,
                "shim_installed" => self.shim_installed = on,
                "version" => self.ext_version = v.to_string(),
                _ => {}
            }
        }
    }

    fn refresh_stats(&mut self) {
        let Ok(data) = std::fs::read(&self.stats_file) else {
            return;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) else {
            return;
        };
        self.sessions_today = v.get("sessions").and_then(|x| x.as_i64()).unwrap_or(0);
        self.turns_today = v.get("turns").and_then(|x| x.as_i64()).unwrap_or(0);
    }

    fn refresh_sessions(&self) {
        let bin = self.agent_bin.clone();
        let tx = self.sessions_tx.clone();
        std::thread::spawn(move || {
            if let Ok(o) = hidden_cmd(&bin).arg("sessions").output() {
                let _ = tx.send(parse_sessions(&o.stdout));
            }
        });
    }

    fn send_prompt(&self, alias: &str, sid: &str, text: &str, media: &[String]) {
        let bin = self.agent_bin.clone();
        let tx = self.log_tx.clone();
        let status_tx = self.send_status_tx.clone();
        let log_path = self.log_file.clone();
        let env_map = self.env_map.clone();
        let token = self.token.clone();
        let secret = self.secret.clone();
        let (alias, sid, text) = (alias.to_string(), sid.to_string(), text.to_string());
        let media: Vec<String> = media.to_vec();
        std::thread::spawn(move || {
            let bytes = text.len();
            let mut command = hidden_cmd(&bin);
            command.arg("send").arg(&alias).arg(&sid);
            for m in &media {
                command.arg("--media").arg(m);
            }
            if !text.is_empty() {
                command.arg(&text);
            }
            for (key, value) in env_map {
                command.env(key, value);
            }
            command.env("TELEGRAM_BOT_TOKEN", token);
            command.env("RELAY_PAIR_SECRET", secret);
            match command.output() {
                Ok(o) => {
                    for line in String::from_utf8_lossy(&o.stdout).lines() {
                        let _ = tx.send(line.to_string());
                    }
                    for line in String::from_utf8_lossy(&o.stderr).lines() {
                        let _ = tx.send(format!("send: {line}"));
                    }
                    let summary = if o.status.success() {
                        format!("Sent {bytes} B to {alias}; Telegram receipt requested")
                    } else {
                        format!("Send failed for {alias} (exit {})", o.status)
                    };
                    append_runtime_log(&log_path, &format!("[gui_send] {summary}"));
                    let _ = status_tx.send(summary);
                }
                Err(e) => {
                    let _ = tx.send(format!("send failed: {e}"));
                    let summary = format!("Send failed for {alias}: {e}");
                    append_runtime_log(&log_path, &format!("[gui_send] {summary}"));
                    let _ = status_tx.send(summary);
                }
            }
        });
    }

    fn refresh_providers(&self) {
        let bin = self.agent_bin.clone();
        let tx = self.providers_tx.clone();
        std::thread::spawn(move || {
            let disc = hidden_cmd(&bin)
                .args(["automation", "discover", "--json"])
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default();
            let get = hidden_cmd(&bin)
                .args(["automation", "get"])
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default();
            let _ = tx.send(parse_providers(&disc, &get));
        });
    }

    fn check_health(&mut self) {
        self.checking_health = true;
        let bin = self.agent_bin.clone();
        let tx = self.health_tx.clone();
        std::thread::spawn(move || {
            let out = hidden_cmd(&bin)
                .args(["automation", "health", "--json"])
                .output()
                .map(|o| o.stdout)
                .unwrap_or_default();
            let _ = tx.send(parse_health(&out));
        });
    }

    fn provider_action(&self, args: &[&str]) {
        let bin = self.agent_bin.clone();
        let tx = self.log_tx.clone();
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        std::thread::spawn(move || {
            if let Ok(o) = hidden_cmd(&bin).args(&owned).output() {
                for line in String::from_utf8_lossy(&o.stdout).lines() {
                    let _ = tx.send(line.to_string());
                }
                for line in String::from_utf8_lossy(&o.stderr).lines() {
                    let _ = tx.send(line.to_string());
                }
            }
        });
    }

    fn semantic_key_action(&self, key: String) {
        let bin = self.agent_bin.clone();
        let tx = self.log_tx.clone();
        std::thread::spawn(move || {
            let mut command = hidden_cmd(&bin);
            command
                .args(["automation", "smart", "key", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match command.spawn() {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(key.as_bytes());
                    }
                    if let Ok(output) = child.wait_with_output() {
                        for line in String::from_utf8_lossy(&output.stdout).lines() {
                            let _ = tx.send(line.to_string());
                        }
                        for line in String::from_utf8_lossy(&output.stderr).lines() {
                            let _ = tx.send(line.to_string());
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(format!("semantic key: {error}"));
                }
            }
        });
    }

    fn save_env(&mut self) {
        self.env_map
            .insert("TELEGRAM_BOT_TOKEN".to_string(), self.token.clone());
        self.env_map
            .insert("RELAY_PAIR_SECRET".to_string(), self.secret.clone());
        let _ = std::fs::create_dir_all(&self.cfg_dir);
        set_mode(&self.cfg_dir, 0o700);
        let mut body = String::new();
        for key in ENV_KEYS {
            let val = self.env_map.get(*key).cloned().unwrap_or_default();
            body.push_str(&format!("{key}={val}\n"));
        }
        for (k, v) in &self.env_map {
            if !ENV_KEYS.contains(&k.as_str()) {
                body.push_str(&format!("{k}={v}\n"));
            }
        }
        if std::fs::write(&self.env_file, body).is_ok() {
            set_mode(&self.env_file, 0o600);
        }
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        if self.token.trim().is_empty() {
            self.note = "Add a bot token in Settings first.".to_string();
            self.show_settings = true;
            return;
        }
        if self.secret.trim().is_empty() {
            self.note = "Set a pairing key in Settings first.".to_string();
            self.show_settings = true;
            return;
        }
        self.save_env();
        kill_stray_daemons();
        let _ = hidden_cmd(&self.agent_bin)
            .arg("install-hooks")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::create_dir_all(&self.relay_dir);
        set_mode(&self.relay_dir, 0o700);

        let mut cmd = hidden_cmd(&self.agent_bin);
        for (k, v) in &self.env_map {
            cmd.env(k, v);
        }
        cmd.env("TELEGRAM_BOT_TOKEN", &self.token);
        cmd.env("RELAY_PAIR_SECRET", &self.secret);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                self.child_pid = Some(child.id());
                if let Some(out) = child.stdout.take() {
                    let tx = self.log_tx.clone();
                    std::thread::spawn(move || pump(out, tx));
                }
                if let Some(err) = child.stderr.take() {
                    let tx = self.log_tx.clone();
                    std::thread::spawn(move || pump(err, tx));
                }
                self.child = Some(child);
                self.running = true;
                self.note = "Running. In Telegram, send /auth <your key> to the bot.".to_string();
            }
            Err(e) => {
                self.note = format!("Failed to start: {e}");
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.running = false;
        self.child_pid = None;
    }

    fn install_shim(&mut self) {
        self.run_agent_note(&["shim-install"]);
        self.refresh_env();
    }

    fn uninstall_shim(&mut self) {
        self.run_agent_note(&["shim-uninstall"]);
        self.refresh_env();
    }

    fn load_auto_mode(&mut self) {
        if let Ok(o) = hidden_cmd(&self.agent_bin)
            .args(["automation", "list"])
            .output()
        {
            let out = String::from_utf8_lossy(&o.stdout);
            for line in out.lines() {
                if let Some(rest) = line.strip_prefix("default:") {
                    self.auto_mode = rest.trim().to_string();
                }
            }
        }
    }

    fn load_smart(&mut self) {
        if let Ok(o) = hidden_cmd(&self.agent_bin)
            .args(["automation", "get"])
            .output()
        {
            let out = String::from_utf8_lossy(&o.stdout);
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&out) {
                self.smart_on = value
                    .pointer("/smart/enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.steer_on = value
                    .pointer("/smart/steer")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.gate_on = value
                    .pointer("/smart/gate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.feedback_on = value
                    .pointer("/smart/feedback_protocol")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                self.budget_input = value
                    .pointer("/robot/providers/budget_usd")
                    .and_then(|v| v.as_f64())
                    .map(|v| format!("{v}"))
                    .unwrap_or_default();
                let semantic = value.pointer("/smart/semantic");
                let backend = semantic
                    .and_then(|value| value.get("backend"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("local");
                self.semantic_endpoint = semantic
                    .and_then(|value| value.get("endpoint"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                self.semantic_provider =
                    if backend == "open_ai_compatible" || backend == "openai_compatible" {
                        if self.semantic_endpoint.contains("openrouter.ai") {
                            "openrouter"
                        } else if self.semantic_endpoint.contains("api.nvidia.com") {
                            "nvidia"
                        } else {
                            "openai-compatible"
                        }
                    } else {
                        backend
                    }
                    .to_string();
                self.semantic_model = semantic
                    .and_then(|value| value.get("model"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                self.semantic_local_dir = semantic
                    .and_then(|value| value.get("local_dir"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string();
                self.semantic_trust = semantic
                    .and_then(|value| value.get("allow_uncalibrated_steer"))
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
            }
        }
    }

    fn run_agent_note(&mut self, args: &[&str]) {
        match hidden_cmd(&self.agent_bin).args(args).output() {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout);
                for line in out.lines() {
                    self.push_log(line.to_string());
                }
                let err = String::from_utf8_lossy(&o.stderr);
                for line in err.lines() {
                    self.push_log(line.to_string());
                }
            }
            Err(e) => self.push_log(format!("{}: {e}", args.join(" "))),
        }
    }

    fn capture_agent(&self, args: &[&str]) -> String {
        match hidden_cmd(&self.agent_bin).args(args).output() {
            Ok(o) => {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                let err = String::from_utf8_lossy(&o.stderr);
                if !err.trim().is_empty() {
                    text.push_str(&err);
                }
                if text.trim().is_empty() {
                    text = "(no output)".to_string();
                }
                text
            }
            Err(e) => format!("{}: {e}", args.join(" ")),
        }
    }

    fn refresh_usage(&mut self) {
        let sid = self.usage_session.trim().to_string();
        if sid.is_empty() {
            self.usage_text = "enter a session id".to_string();
            return;
        }
        self.usage_text = self.capture_agent(&["automation", "usage", &sid]);
    }

    fn list_pins(&mut self) {
        let sid = self.usage_session.trim().to_string();
        if sid.is_empty() {
            self.pins_text = "enter a session id".to_string();
            return;
        }
        self.pins_text = self.capture_agent(&["automation", "compass-pins", &sid]);
    }

    fn clear_pins(&mut self) {
        let sid = self.usage_session.trim().to_string();
        if sid.is_empty() {
            return;
        }
        let _ = self.capture_agent(&["automation", "compass-unpin", &sid]);
        self.list_pins();
    }

    fn add_pin(&mut self) {
        let sid = self.usage_session.trim().to_string();
        let target = self.pin_target.trim().to_string();
        let obligation = self.pin_obligation.trim().to_string();
        let epoch = self.pin_epoch.trim().to_string();
        if sid.is_empty() || target.is_empty() || obligation.is_empty() || epoch.is_empty() {
            return;
        }
        let _ = self.capture_agent(&[
            "automation",
            "compass-pin",
            &sid,
            &target,
            &obligation,
            &epoch,
        ]);
        self.list_pins();
    }

    #[cfg(not(windows))]
    fn set_launch_at_login(&mut self, on: bool) {
        let path = self.autostart_path();
        if on {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "vsc-relay-gui".to_string());
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let entry = format!(
                "[Desktop Entry]\nType=Application\nName={APP_TITLE}\nExec={exe}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
            );
            let _ = std::fs::write(&path, entry);
        } else {
            let _ = std::fs::remove_file(&path);
        }
        self.launch_at_login = on;
    }

    #[cfg(windows)]
    fn set_launch_at_login(&mut self, on: bool) {
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        if on {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "vsc-relay-gui.exe".to_string());
            let _ = hidden_cmd("reg")
                .args([
                    "add", key, "/v", "VSCRelay", "/t", "REG_SZ", "/d", &exe, "/f",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        } else {
            let _ = hidden_cmd("reg")
                .args(["delete", key, "/v", "VSCRelay", "/f"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        self.launch_at_login = on;
    }

    fn generate_secret(&mut self) {
        self.secret = random_hex(24);
        self.reveal_secret = true;
    }

    fn push_log(&mut self, s: String) {
        for line in s.split('\n') {
            if !line.is_empty() {
                self.log.push(line.to_string());
            }
        }
        if self.log.len() > LOG_CAP {
            let excess = self.log.len() - LOG_CAP;
            self.log.drain(0..excess);
        }
    }

    fn warning(&self) -> Option<&'static str> {
        if !self.vscode_ok {
            Some("VS Code was not found. Install it and open a project.")
        } else if !self.extension_ok {
            Some("Claude Code extension not found in VS Code.")
        } else if !self.shim_installed {
            Some("Background control is off. Install the shim, or the extension may have updated.")
        } else {
            None
        }
    }
}

impl Drop for RelayApp {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl eframe::App for RelayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(line) = self.log_rx.try_recv() {
            self.push_log(line);
        }
        while let Ok(status) = self.send_status_rx.try_recv() {
            self.note = status;
        }
        while let Ok(ev) = self.update_rx.try_recv() {
            match ev {
                UpdateEvent::Available(v) => {
                    self.update_available = true;
                    self.update_note = format!("Update {v} available");
                    if self.auto_update && !self.updating {
                        self.do_update();
                    }
                }
                UpdateEvent::UpToDate => {
                    self.update_available = false;
                    self.update_note.clear();
                }
                UpdateEvent::Done(ok, msg) => {
                    self.updating = false;
                    if ok {
                        self.relaunch();
                    } else {
                        self.note = format!("Update failed: {msg}");
                    }
                }
            }
        }
        if self.last_update_check.elapsed() > Duration::from_secs(6 * 60 * 60) {
            self.check_update();
            self.last_update_check = Instant::now();
        }
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                self.running = false;
                self.child = None;
                self.child_pid = None;
                self.push_log("[relay stopped]".to_string());
            }
        }
        if self.last_env.elapsed() > Duration::from_secs(15) {
            self.refresh_env();
            self.last_env = Instant::now();
        }
        if self.last_stats.elapsed() > Duration::from_secs(3) {
            self.refresh_stats();
            self.last_stats = Instant::now();
        }
        while let Ok(list) = self.sessions_rx.try_recv() {
            self.sessions = list;
        }
        if self.running && self.last_sessions.elapsed() > Duration::from_secs(4) {
            self.refresh_sessions();
            self.last_sessions = Instant::now();
        }
        while let Ok(list) = self.providers_rx.try_recv() {
            self.providers = list;
        }
        while let Ok(list) = self.health_rx.try_recv() {
            self.checking_health = false;
            self.provider_health = list.into_iter().map(|(id, s, d)| (id, (s, d))).collect();
        }

        let mut act = Actions::default();

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let dot = if self.running {
                    egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
                } else {
                    egui::Color32::GRAY
                };
                ui.label(egui::RichText::new("●").color(dot).size(18.0));
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new(APP_TITLE).heading());
                    let status = if self.running {
                        format!("Running (pid {})", self.child_pid.unwrap_or(0))
                    } else {
                        "Stopped".to_string()
                    };
                    ui.label(egui::RichText::new(status).color(dot));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Settings").clicked() {
                        self.show_settings = true;
                    }
                    if ui.button("Help").clicked() {
                        self.show_help = true;
                    }
                    if self.ext_version != "-" {
                        ui.label(
                            egui::RichText::new(format!("Claude Code {}", self.ext_version)).weak(),
                        );
                    }
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.update_available || self.updating {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(0x14, 0x3a, 0x52))
                    .inner_margin(egui::Margin::same(8.0))
                    .rounding(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("⬆");
                            ui.label(if self.updating {
                                "Updating and restarting…"
                            } else {
                                self.update_note.as_str()
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if self.updating {
                                        ui.spinner();
                                    } else {
                                        if ui.button("Update now").clicked() {
                                            act.do_update = true;
                                        }
                                        if ui.button("Releases").clicked() {
                                            act.open_releases = true;
                                        }
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(8.0);
            }

            if let Some(msg) = self.warning() {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(0x4a, 0x3a, 0x10))
                    .inner_margin(egui::Margin::same(8.0))
                    .rounding(6.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("⚠");
                            ui.label(msg);
                            if self.extension_ok && !self.shim_installed {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Install shim").clicked() {
                                            act.install_shim = true;
                                        }
                                    },
                                );
                            }
                        });
                    });
                ui.add_space(8.0);
            }

            ui.horizontal(|ui| {
                stat_card(ui, "Sessions today", &self.sessions_today.to_string());
                stat_card(ui, "Turns today", &self.turns_today.to_string());
                stat_card(ui, "Shim", if self.shim_installed { "on" } else { "off" });
            });
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                if self.running {
                    if ui
                        .button(egui::RichText::new("■ Stop").color(egui::Color32::WHITE))
                        .clicked()
                    {
                        act.stop = true;
                    }
                } else if ui
                    .add(egui::Button::new(
                        egui::RichText::new("▶ Start").color(egui::Color32::WHITE),
                    ))
                    .clicked()
                {
                    act.start = true;
                }
                ui.separator();
                if self.shim_installed {
                    if ui.button("Remove shim").clicked() {
                        act.uninstall_shim = true;
                    }
                } else if ui.button("Install shim").clicked() {
                    act.install_shim = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear log").clicked() {
                        act.clear_log = true;
                    }
                    if ui.button("Releases").clicked() {
                        act.open_releases = true;
                    }
                });
            });

            if !self.note.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&self.note).weak());
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Sessions").strong());
                ui.label(egui::RichText::new(format!("({})", self.sessions.len())).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("↻").clicked() {
                        act.refresh_sessions = true;
                    }
                });
            });
            ui.add_space(4.0);
            let cards = self.sessions.clone();
            egui::ScrollArea::vertical()
                .id_source("sessions")
                .max_height(260.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if cards.is_empty() {
                        ui.label(egui::RichText::new("no active sessions").weak());
                    }
                    for s in &cards {
                        session_card(
                            ui,
                            s,
                            &mut act,
                            &mut self.active_session,
                            &mut self.compose,
                            &mut self.compose_media,
                        );
                    }
                });

            ui.add_space(8.0);
            egui::Frame::none()
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::same(6.0))
                .rounding(6.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_source("log")
                        .max_height(140.0)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in &self.log {
                                ui.label(egui::RichText::new(line).monospace().size(11.0));
                            }
                            if self.log.is_empty() {
                                ui.label(egui::RichText::new("no output yet").weak().monospace());
                            }
                        });
                });
        });

        self.settings_window(ctx, &mut act);
        self.help_window(ctx);

        if act.start {
            self.start();
        }
        if act.stop {
            self.stop();
        }
        if act.install_shim {
            self.install_shim();
        }
        if act.uninstall_shim {
            self.uninstall_shim();
        }
        if act.clear_log {
            self.log.clear();
        }
        if act.open_releases {
            open_url(RELEASES_URL);
        }
        if act.generate_secret {
            self.generate_secret();
        }
        if act.save_settings {
            self.save_env();
            self.note = "Saved.".to_string();
            self.show_settings = false;
        }
        if let Some(on) = act.set_launch {
            self.set_launch_at_login(on);
        }
        if act.check_update {
            self.note = "Checking for updates…".to_string();
            self.check_update();
        }
        if act.do_update {
            self.do_update();
        }
        if let Some(on) = act.set_auto_update {
            self.set_auto_update(on);
        }
        if let Some(on) = act.set_smart {
            self.run_agent_note(&["automation", "smart", if on { "on" } else { "off" }]);
        }
        if let Some(on) = act.set_steer {
            self.run_agent_note(&[
                "automation",
                "smart",
                "steer",
                if on { "on" } else { "off" },
            ]);
        }
        if let Some(on) = act.set_gate {
            self.run_agent_note(&["automation", "smart", "gate", if on { "on" } else { "off" }]);
        }
        if let Some(on) = act.set_feedback {
            self.run_agent_note(&[
                "automation",
                "smart",
                "feedback",
                if on { "on" } else { "off" },
            ]);
        }
        if let Some(val) = &act.set_budget {
            self.run_agent_note(&["automation", "smart", "budget", val.as_str()]);
        }
        if act.refresh_usage {
            self.refresh_usage();
        }
        if act.list_pins {
            self.list_pins();
        }
        if act.clear_pins {
            self.clear_pins();
        }
        if act.add_pin {
            self.add_pin();
        }
        if let Some(mode) = &act.set_auto_mode {
            self.run_agent_note(&["automation", "set-default", mode.as_str()]);
        }
        if act.refresh_sessions {
            self.refresh_sessions();
        }
        if let Some((sid, mode)) = &act.set_session_mode {
            self.run_agent_note(&["automation", "set-session", sid, mode]);
            self.refresh_sessions();
        }
        if let Some((alias, sid, text, media)) = act.send_prompt.take() {
            self.note = format!("Sending {} B to {alias}…", text.len());
            self.send_prompt(&alias, &sid, &text, &media);
            self.compose.clear();
            self.compose_media.clear();
        }
        if act.refresh_providers {
            self.refresh_providers();
        }
        if act.check_health {
            self.check_health();
        }
        if let Some(cmd) = act.provider_cmd.take() {
            let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
            self.provider_action(&refs);
            self.refresh_providers();
        }
        if let Some(key) = act.semantic_key.take() {
            self.semantic_key_action(key);
        }
        if let Some(id) = act.clear_key.take() {
            self.key_input.remove(&id);
        }
        if let Some(id) = &act.login_backend {
            open_login_terminal(&self.agent_bin, id);
            self.note = format!("Opened a terminal to log into {id}.");
        }

        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop();
    }
}

impl RelayApp {
    fn settings_window(&mut self, ctx: &egui::Context, act: &mut Actions) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Telegram bot token").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token)
                            .password(!self.reveal_token)
                            .desired_width(360.0)
                            .hint_text("123456789:token-from-BotFather"),
                    );
                    if ui
                        .button(if self.reveal_token { "Hide" } else { "Show" })
                        .clicked()
                    {
                        self.reveal_token = !self.reveal_token;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "From @BotFather. Stored in ~/.config/vsc-relay/relay.env (0600).",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(10.0);

                ui.label(egui::RichText::new("Pairing key (required)").strong());
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.secret)
                            .password(!self.reveal_secret)
                            .desired_width(300.0)
                            .hint_text("long random secret"),
                    );
                    if ui
                        .button(if self.reveal_secret { "Hide" } else { "Show" })
                        .clicked()
                    {
                        self.reveal_secret = !self.reveal_secret;
                    }
                    if ui.button("Generate").clicked() {
                        act.generate_secret = true;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Send it once to the bot as /auth <key> to authorize your chat.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.separator();

                let mut login = self.launch_at_login;
                if ui.checkbox(&mut login, "Launch at login").changed() {
                    act.set_launch = Some(login);
                }
                ui.label(
                    egui::RichText::new(
                        "Adds an XDG autostart entry; works on any desktop environment.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.separator();

                ui.label(egui::RichText::new("Automation default").strong());
                ui.horizontal(|ui| {
                    for (val, label) in [("manual", "Manual"), ("auto", "Auto"), ("robot", "Robot")]
                    {
                        if ui
                            .selectable_label(self.auto_mode == val, label)
                            .clicked()
                        {
                            self.auto_mode = val.to_string();
                            act.set_auto_mode = Some(val.to_string());
                        }
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Manual drives nothing without you. Auto auto-approves safe tools and auto-retries transient errors; danger always asks. Robot lets an AI supervisor drive. Pin individual chats from the Telegram bot with /auto.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.separator();

                ui.label(egui::RichText::new("Compass (smart layer)").strong());
                if ui.checkbox(&mut self.smart_on, "Enable compass").changed() {
                    if !self.smart_on {
                        self.steer_on = false;
                        self.gate_on = false;
                        act.set_steer = Some(false);
                        act.set_gate = Some(false);
                    }
                    act.set_smart = Some(self.smart_on);
                }
                ui.add_enabled_ui(self.smart_on, |ui| {
                    if ui
                        .checkbox(
                            &mut self.feedback_on,
                            "Session health telemetry (Claude + Codex)",
                        )
                        .changed()
                    {
                        act.set_feedback = Some(self.feedback_on);
                    }
                    if ui
                        .checkbox(&mut self.steer_on, "Allow auto-steer (Robot only)")
                        .changed()
                    {
                        act.set_steer = Some(self.steer_on);
                    }
                    if ui
                        .checkbox(
                            &mut self.gate_on,
                            "Deterministic pre-mutation gate (off by default)",
                        )
                        .changed()
                    {
                        act.set_gate = Some(self.gate_on);
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Budget cap (USD)");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.budget_input)
                                .desired_width(120.0)
                                .hint_text("5.0"),
                        );
                        if ui.button("Set").clicked() && !self.budget_input.trim().is_empty() {
                            act.set_budget = Some(self.budget_input.trim().to_string());
                        }
                        if ui.button("Clear").clicked() {
                            self.budget_input.clear();
                            act.set_budget = Some("off".to_string());
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Session");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.usage_session)
                                .desired_width(220.0)
                                .hint_text("session id"),
                        );
                        if ui.button("Refresh usage").clicked() {
                            act.refresh_usage = true;
                        }
                    });
                    if !self.usage_text.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.usage_text)
                                .monospace()
                                .weak()
                                .size(11.0),
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Gate pins").weak().size(11.0));
                        if ui.button("List pins").clicked() {
                            act.list_pins = true;
                        }
                        if ui.button("Clear pins").clicked() {
                            act.clear_pins = true;
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pin_target)
                                .desired_width(120.0)
                                .hint_text("target"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pin_obligation)
                                .desired_width(120.0)
                                .hint_text("obligation"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pin_epoch)
                                .desired_width(80.0)
                                .hint_text("epoch"),
                        );
                        if ui.button("Add pin").clicked() {
                            act.add_pin = true;
                        }
                    });
                    if !self.pins_text.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.pins_text)
                                .monospace()
                                .weak()
                                .size(11.0),
                        );
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Semantic ML extracts typed facts; policy and safety guards stay deterministic. If the backend is unavailable or abstains, base Robot continues unchanged.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Semantic backend");
                    egui::ComboBox::from_id_source("semantic_provider")
                        .selected_text(&self.semantic_provider)
                        .show_ui(ui, |ui| {
                            for (value, label) in [
                                ("off", "Off"),
                                ("local", "Built-in local NLI"),
                                ("ollama", "Ollama"),
                                ("openrouter", "OpenRouter"),
                                ("nvidia", "NVIDIA NIM"),
                                ("openai-compatible", "OpenAI-compatible / custom"),
                            ] {
                                if ui
                                    .selectable_label(self.semantic_provider == value, label)
                                    .clicked()
                                {
                                    self.semantic_provider = value.to_string();
                                    act.provider_cmd = Some(vec![
                                        "automation".to_string(),
                                        "smart".to_string(),
                                        "provider".to_string(),
                                        value.to_string(),
                                    ]);
                                }
                            }
                        });
                });
                if self.semantic_provider == "local" {
                    ui.horizontal(|ui| {
                        ui.label("Custom ONNX bundle");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_local_dir)
                                .desired_width(300.0)
                                .hint_text("/absolute/path/to/bundle"),
                        );
                        if ui.button("Use bundle").clicked()
                            && !self.semantic_local_dir.trim().is_empty()
                        {
                            act.provider_cmd = Some(vec![
                                "automation".to_string(),
                                "smart".to_string(),
                                "local-dir".to_string(),
                                self.semantic_local_dir.trim().to_string(),
                            ]);
                        }
                        if ui.button("Use built-in").clicked() {
                            self.semantic_local_dir.clear();
                            act.provider_cmd = Some(vec![
                                "automation".to_string(),
                                "smart".to_string(),
                                "local-dir".to_string(),
                                "builtin".to_string(),
                            ]);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Train own NLI");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_train_dataset)
                                .desired_width(210.0)
                                .hint_text("labeled dataset.jsonl"),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_train_output)
                                .desired_width(210.0)
                                .hint_text("new bundle directory"),
                        );
                        if ui.button("Train + select").clicked()
                            && !self.semantic_train_dataset.trim().is_empty()
                            && !self.semantic_train_output.trim().is_empty()
                        {
                            act.provider_cmd = Some(vec![
                                "automation".to_string(),
                                "smart".to_string(),
                                "train-local".to_string(),
                                self.semantic_train_dataset.trim().to_string(),
                                self.semantic_train_output.trim().to_string(),
                            ]);
                        }
                    });
                } else if self.semantic_provider != "off" {
                    ui.horizontal(|ui| {
                        ui.label("Model");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_model)
                                .desired_width(300.0)
                                .hint_text("provider model id"),
                        );
                        if ui.button("Apply").clicked() && !self.semantic_model.trim().is_empty() {
                            act.provider_cmd = Some(vec![
                                "automation".to_string(),
                                "smart".to_string(),
                                "model".to_string(),
                                self.semantic_model.trim().to_string(),
                            ]);
                        }
                    });
                }
                if self.semantic_provider == "openai-compatible" {
                    ui.horizontal(|ui| {
                        ui.label("Endpoint");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_endpoint)
                                .desired_width(280.0)
                                .hint_text("https://host/v1"),
                        );
                        if ui.button("Apply").clicked()
                            && !self.semantic_endpoint.trim().is_empty()
                        {
                            act.provider_cmd = Some(vec![
                                "automation".to_string(),
                                "smart".to_string(),
                                "endpoint".to_string(),
                                self.semantic_endpoint.trim().to_string(),
                            ]);
                        }
                    });
                }
                if !matches!(self.semantic_provider.as_str(), "local" | "off" | "ollama") {
                    ui.horizontal(|ui| {
                        ui.label("API key");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.semantic_key)
                                .password(true)
                                .desired_width(260.0)
                                .hint_text("stored in 0600 key store"),
                        );
                        if ui.button("Set").clicked() && !self.semantic_key.trim().is_empty() {
                            act.semantic_key = Some(self.semantic_key.trim().to_string());
                            self.semantic_key.clear();
                        }
                    });
                }
                if ui
                    .checkbox(
                        &mut self.semantic_trust,
                        "Permit uncalibrated backend facts to reach auto-steer",
                    )
                    .changed()
                {
                    act.provider_cmd = Some(vec![
                        "automation".to_string(),
                        "smart".to_string(),
                        "trust".to_string(),
                        if self.semantic_trust { "on" } else { "off" }.to_string(),
                    ]);
                }
                ui.horizontal(|ui| {
                    if ui.button("Install built-in bootstrap (~124 MB)").clicked() {
                        act.provider_cmd = Some(vec![
                            "automation".to_string(),
                            "smart".to_string(),
                            "install-local".to_string(),
                        ]);
                    }
                    if ui.button("Check semantic backend").clicked() {
                        act.provider_cmd = Some(vec![
                            "automation".to_string(),
                            "smart".to_string(),
                            "check".to_string(),
                        ]);
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "The built-in NLI model is a low-memory shadow bootstrap, not a production-calibrated detector. A trained custom ONNX bundle or provider can be selected; uncalibrated steering is blocked by default.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(10.0);
                ui.separator();

                ui.label(egui::RichText::new("Providers").strong());
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        act.refresh_providers = true;
                    }
                    if self.checking_health {
                        ui.add(egui::Spinner::new());
                        ui.label(egui::RichText::new("probing…").weak().size(11.0));
                    } else if ui.button("Check health").clicked() {
                        act.check_health = true;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "Enable providers for Robot, set a key, or Login to a CLI. Check health runs a real probe and reads login/errors.",
                    )
                    .weak()
                    .size(11.0),
                );
                ui.add_space(4.0);
                let rows = self.providers.clone();
                if rows.is_empty() {
                    ui.label(
                        egui::RichText::new("click Refresh to list providers")
                            .weak()
                            .size(11.0),
                    );
                }
                egui::ScrollArea::vertical()
                    .id_source("providers")
                    .max_height(180.0)
                    .show(ui, |ui| {
                        for p in &rows {
                            egui::Frame::none()
                                .fill(ui.visuals().faint_bg_color)
                                .inner_margin(egui::Margin::same(8.0))
                                .rounding(6.0)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let mut en = p.enabled;
                                        if ui.checkbox(&mut en, "").changed() {
                                            act.provider_cmd = Some(vec![
                                                "automation".to_string(),
                                                "provider".to_string(),
                                                p.id.clone(),
                                                if en { "on" } else { "off" }.to_string(),
                                            ]);
                                        }
                                        ui.label(egui::RichText::new(&p.id).strong());
                                        if let Some((status, _)) = self.provider_health.get(&p.id) {
                                            let (label, color) = health_badge(status);
                                            ui.label(
                                                egui::RichText::new(label).color(color).size(11.0),
                                            );
                                        } else {
                                            let dot = if p.available {
                                                egui::Color32::from_rgb(0x3f, 0xb9, 0x50)
                                            } else {
                                                egui::Color32::GRAY
                                            };
                                            ui.label(egui::RichText::new("●").color(dot).size(11.0));
                                        }
                                        if !p.model.is_empty() {
                                            ui.label(
                                                egui::RichText::new(format!("model: {}", p.model))
                                                    .weak()
                                                    .size(10.0),
                                            );
                                        }
                                    });
                                    if let Some((status, detail)) = self.provider_health.get(&p.id) {
                                        if status != "ok" && !detail.is_empty() {
                                            ui.label(
                                                egui::RichText::new(truncate_str(detail, 120))
                                                    .weak()
                                                    .size(10.0),
                                            );
                                        }
                                    } else if !p.available {
                                        ui.label(
                                            egui::RichText::new(truncate_str(&p.reason, 120))
                                                .weak()
                                                .size(10.0),
                                        );
                                    }
                                    ui.horizontal(|ui| {
                                        let is_cli =
                                            p.id.ends_with("-cli") || p.id == "antigravity";
                                        if p.id != "ollama" {
                                            let entry =
                                                self.key_input.entry(p.id.clone()).or_default();
                                            ui.add(
                                                egui::TextEdit::singleline(entry)
                                                    .password(true)
                                                    .hint_text("API key")
                                                    .desired_width(180.0),
                                            );
                                            if ui.button("Set key").clicked() {
                                                let key = entry.clone();
                                                if !key.trim().is_empty() {
                                                    act.provider_cmd = Some(vec![
                                                        "automation".to_string(),
                                                        "provider-key".to_string(),
                                                        p.id.clone(),
                                                        key,
                                                    ]);
                                                    act.clear_key = Some(p.id.clone());
                                                }
                                            }
                                        }
                                        if is_cli && ui.button("Login").clicked() {
                                            act.login_backend = Some(p.id.clone());
                                        }
                                    });
                                });
                            ui.add_space(4.0);
                        }
                    });
                ui.add_space(6.0);
                ui.separator();

                let mut au = self.auto_update;
                if ui.checkbox(&mut au, "Auto-update").changed() {
                    act.set_auto_update = Some(au);
                }
                ui.horizontal(|ui| {
                    if ui.button("Check for updates").clicked() {
                        act.check_update = true;
                    }
                    ui.label(
                        egui::RichText::new("Downloads the latest release and swaps the binaries.")
                            .weak()
                            .size(11.0),
                    );
                });
                ui.add_space(10.0);
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can_save =
                            !self.token.trim().is_empty() && !self.secret.trim().is_empty();
                        if ui
                            .add_enabled(can_save, egui::Button::new("Save"))
                            .clicked()
                        {
                            act.save_settings = true;
                        }
                    });
                });
            });
        if !act.save_settings {
            self.show_settings = open;
        }
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Help")
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for (title, body) in HELP {
                            ui.label(egui::RichText::new(*title).strong());
                            ui.label(egui::RichText::new(*body).weak());
                            ui.add_space(8.0);
                        }
                    });
            });
        self.show_help = open;
    }
}

#[derive(Default)]
struct Actions {
    start: bool,
    stop: bool,
    install_shim: bool,
    uninstall_shim: bool,
    clear_log: bool,
    open_releases: bool,
    generate_secret: bool,
    save_settings: bool,
    set_launch: Option<bool>,
    check_update: bool,
    do_update: bool,
    set_auto_update: Option<bool>,
    set_auto_mode: Option<String>,
    set_smart: Option<bool>,
    set_steer: Option<bool>,
    set_gate: Option<bool>,
    set_feedback: Option<bool>,
    set_budget: Option<String>,
    refresh_usage: bool,
    list_pins: bool,
    clear_pins: bool,
    add_pin: bool,
    refresh_sessions: bool,
    set_session_mode: Option<(String, String)>,
    send_prompt: Option<(String, String, String, Vec<String>)>,
    refresh_providers: bool,
    check_health: bool,
    provider_cmd: Option<Vec<String>>,
    semantic_key: Option<String>,
    login_backend: Option<String>,
    clear_key: Option<String>,
}

const HELP: &[(&str, &str)] = &[
    (
        "Getting started",
        "Open Settings, paste your Telegram bot token from @BotFather, set a pairing key, then click Start. In Telegram send /auth <key> to your bot, then /menu.",
    ),
    (
        "Background control",
        "Install shim gives background send, question answers, and Allow/Deny on permission prompts. Only chats opened after the shim is installed use it.",
    ),
    (
        "Automation modes",
        "Manual drives nothing without you. Auto is rule-based: auto-approves safe tools and auto-retries transient API errors; danger-listed commands always ask. Robot lets an AI supervisor drive a chat. Set the default here; pin individual chats from the Telegram bot with /auto.",
    ),
    (
        "Sessions",
        "The Sessions panel lists live chats with state, mode, and channel. Switch a session's mode inline, or Open a card to type a prompt and send it into the real chat in the background. In Robot mode (and Manual if you enable rewrite) the prompt is improved by a supervisor model before it lands.",
    ),
    (
        "Window focus",
        "Focus and GUI fallback need an X11 (or XWayland) session and xdotool (sudo apt install xdotool xclip xdg-utils). The background shim path does not need a display.",
    ),
    (
        "Where things live",
        "Token and pairing key are in ~/.config/vsc-relay/relay.env (0600). Logs and state are under ~/.vsc-relay. The systemd unit reads the same env file.",
    ),
    (
        "Safety",
        "Nothing works for a chat until it pairs with your key. The token is never sent to a chat. The relay does not listen on the network.",
    ),
];

#[derive(Clone)]
struct SessionCard {
    alias: String,
    agent: String,
    session_id: String,
    title: String,
    state: String,
    mode: String,
    tapped: bool,
    rewrite: bool,
}

#[derive(Clone)]
struct ProviderRow {
    id: String,
    reason: String,
    available: bool,
    enabled: bool,
    model: String,
}

fn parse_providers(discover_json: &[u8], get_json: &[u8]) -> Vec<ProviderRow> {
    let disc: serde_json::Value = serde_json::from_slice(discover_json).unwrap_or_default();
    let cfg: serde_json::Value = serde_json::from_slice(get_json).unwrap_or_default();
    let enabled: Vec<String> = cfg
        .pointer("/robot/providers/enabled")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let per = cfg.pointer("/robot/providers/per_provider");
    let Some(arr) = disc.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|o| {
            let id = o.get("id")?.as_str()?.to_string();
            let model = per
                .and_then(|p| p.get(&id))
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            Some(ProviderRow {
                reason: o
                    .get("reason")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                available: o
                    .get("available")
                    .and_then(|x| x.as_bool())
                    .unwrap_or(false),
                enabled: enabled.contains(&id),
                model,
                id,
            })
        })
        .collect()
}

fn parse_health(data: &[u8]) -> Vec<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_slice(data).unwrap_or_default();
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|o| {
            Some((
                o.get("id")?.as_str()?.to_string(),
                o.get("status")?.as_str()?.to_string(),
                o.get("detail")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            ))
        })
        .collect()
}

fn open_login_terminal(bin: &std::path::Path, id: &str) {
    let b = bin.display().to_string();
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Terminal\"\ndo script \"'{b}' automation login {id}\"\nactivate\nend tell"
        );
        let _ = Command::new("osascript").arg("-e").arg(script).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let inner =
            format!("'{b}' automation login {id}; echo; read -n1 -r -p 'press a key to close'");
        for (term, pre) in [
            ("x-terminal-emulator", "-e"),
            ("gnome-terminal", "--"),
            ("konsole", "-e"),
            ("xterm", "-e"),
        ] {
            if Command::new(term)
                .arg(pre)
                .args(["bash", "-lc", &inner])
                .spawn()
                .is_ok()
            {
                break;
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd")
            .args(["/C", "start", "cmd", "/K"])
            .arg(format!("\"{b}\" automation login {id}"))
            .spawn();
    }
}

fn parse_sessions(data: &[u8]) -> Vec<SessionCard> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return Vec::new();
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let s = |o: &serde_json::Value, k: &str| {
        o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let b = |o: &serde_json::Value, k: &str| o.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    arr.iter()
        .map(|o| SessionCard {
            alias: s(o, "alias"),
            agent: s(o, "agent"),
            session_id: s(o, "session_id"),
            title: s(o, "title"),
            state: s(o, "state"),
            mode: {
                let m = s(o, "mode");
                if m.is_empty() {
                    "manual".to_string()
                } else {
                    m
                }
            },
            tapped: b(o, "tapped"),
            rewrite: b(o, "rewrite"),
        })
        .collect()
}

fn health_badge(status: &str) -> (&'static str, egui::Color32) {
    match status {
        "ok" => ("ok", egui::Color32::from_rgb(0x3f, 0xb9, 0x50)),
        "needs-login" => ("needs-login", egui::Color32::from_rgb(0xe0, 0xa4, 0x2a)),
        "unavailable" | "no-model" => ("unavailable", egui::Color32::GRAY),
        _ => ("error", egui::Color32::from_rgb(0xd9, 0x4a, 0x3a)),
    }
}

fn state_color(state: &str) -> egui::Color32 {
    match state {
        "working" | "subagent_running" => egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
        "error" => egui::Color32::from_rgb(0xd9, 0x4a, 0x3a),
        "pending_question" | "pending_permission" => egui::Color32::from_rgb(0xe0, 0xa4, 0x2a),
        "idle" => egui::Color32::from_rgb(0x5a, 0x9b, 0xd4),
        _ => egui::Color32::GRAY,
    }
}

fn mode_badge(ui: &mut egui::Ui, mode: &str) {
    let (label, color) = match mode {
        "auto" => ("AUTO", egui::Color32::from_rgb(0x2a, 0x8a, 0x4a)),
        "robot" => ("ROBOT", egui::Color32::from_rgb(0x7a, 0x4a, 0xc0)),
        _ => ("MANUAL", egui::Color32::GRAY),
    };
    ui.label(egui::RichText::new(label).size(10.0).strong().color(color));
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

fn parse_media_paths(raw: &str) -> Vec<String> {
    raw.split([',', '\n'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn session_card(
    ui: &mut egui::Ui,
    s: &SessionCard,
    act: &mut Actions,
    active: &mut Option<String>,
    compose: &mut String,
    compose_media: &mut String,
) {
    let is_open = active.as_deref() == Some(s.session_id.as_str());
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::same(10.0))
        .rounding(6.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("●")
                        .color(state_color(&s.state))
                        .size(14.0),
                );
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&s.alias).strong());
                        ui.label(
                            egui::RichText::new(format!("· {}", s.agent))
                                .weak()
                                .size(11.0),
                        );
                        mode_badge(ui, &s.mode);
                        if s.rewrite {
                            ui.label(
                                egui::RichText::new("✏ rewrite")
                                    .size(10.0)
                                    .color(egui::Color32::from_rgb(0x7a, 0x4a, 0xc0)),
                            );
                        }
                        if !s.tapped {
                            ui.label(egui::RichText::new("no channel").size(10.0).weak());
                        }
                    });
                    let title = if s.title.is_empty() {
                        s.session_id.clone()
                    } else {
                        s.title.clone()
                    };
                    ui.label(egui::RichText::new(truncate_str(&title, 64)).size(12.0));
                    ui.label(egui::RichText::new(&s.state).weak().size(10.0));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(if is_open { "Close" } else { "Open" }).clicked() {
                        *active = if is_open {
                            None
                        } else {
                            Some(s.session_id.clone())
                        };
                    }
                });
            });

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("mode:").weak().size(11.0));
                for m in ["manual", "auto", "robot"] {
                    let selected = s.mode == m;
                    if ui.selectable_label(selected, m).clicked() && !selected {
                        act.set_session_mode = Some((s.session_id.clone(), m.to_string()));
                    }
                }
            });

            if is_open {
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::multiline(compose)
                        .desired_rows(2)
                        .hint_text("type a prompt…")
                        .desired_width(f32::INFINITY),
                );
                ui.add(
                    egui::TextEdit::singleline(compose_media)
                        .hint_text("attach files: comma-separated paths…")
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    let media = parse_media_paths(compose_media);
                    let can = s.tapped && (!compose.trim().is_empty() || !media.is_empty());
                    if ui.add_enabled(can, egui::Button::new("Send")).clicked() {
                        act.send_prompt = Some((
                            s.alias.clone(),
                            s.session_id.clone(),
                            compose.clone(),
                            media,
                        ));
                    }
                    if s.rewrite {
                        ui.label(
                            egui::RichText::new("improved before sending")
                                .weak()
                                .size(10.0),
                        );
                    } else if !s.tapped {
                        ui.label(
                            egui::RichText::new("no background channel")
                                .weak()
                                .size(10.0),
                        );
                    }
                });
            }
        });
    ui.add_space(6.0);
}

fn stat_card(ui: &mut egui::Ui, title: &str, value: &str) {
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .inner_margin(egui::Margin::same(10.0))
        .rounding(6.0)
        .show(ui, |ui| {
            ui.set_width(150.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(value).size(20.0).strong());
                ui.label(egui::RichText::new(title).weak().size(11.0));
            });
        });
}

fn gui_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::config_dir()
            .map(|d| d.join("vsc-relay"))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".config")
            .join("vsc-relay")
    }
}

fn find_agent_binary() -> PathBuf {
    let name = format!("vsc-relay-agent{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        let sib = exe.with_file_name(&name);
        if sib.exists() {
            return sib;
        }
    }
    PathBuf::from(name)
}

fn parse_env_file(path: &PathBuf) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn pump(stream: impl Read, tx: Sender<String>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let _ = tx.send(line.trim_end().to_string());
            }
            Err(_) => break,
        }
    }
}

fn append_runtime_log(path: &std::path::Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
        set_mode(path, 0o600);
    }
}

#[cfg(windows)]
fn kill_stray_daemons() {
    let _ = hidden_cmd("taskkill")
        .args(["/IM", "vsc-relay-agent.exe", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn kill_stray_daemons() {
    let me = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        if pid as u32 == me {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let parts: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        let Some(first) = parts.first() else {
            continue;
        };
        let exe = String::from_utf8_lossy(first);
        let base = exe.rsplit('/').next().unwrap_or(&exe);
        if base != "vsc-relay-agent" {
            continue;
        }
        if parts.iter().any(|p| *p == b"hook") {
            continue;
        }
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    if getrandom::getrandom(&mut buf).is_ok() {
        return buf.iter().map(|b| format!("{b:02x}")).collect();
    }
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:032x}{:08x}", std::process::id())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(windows)]
fn set_mode(path: &Path, _mode: u32) {
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let _ = hidden_cmd("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn open_url(url: &str) {
    let _ = hidden_cmd("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(windows)]
fn open_url(url: &str) {
    let _ = hidden_cmd("cmd")
        .args(["/C", "start", "", url])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
