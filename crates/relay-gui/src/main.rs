#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const APP_TITLE: &str = "VS Code Agent Relay";
const RELEASES_URL: &str = "https://github.com/itrootvm/vsc_relay/releases/latest";
const LOG_CAP: usize = 1000;
const LOG_ROTATE_BYTES: u64 = 2 * 1024 * 1024;
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
    note: String,

    show_settings: bool,
    show_help: bool,

    vscode_ok: bool,
    extension_ok: bool,
    shim_installed: bool,
    ext_version: String,
    sessions_today: i64,
    turns_today: i64,
    launch_at_login: bool,

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
        let cfg_dir = home.join(".config").join("vsc-relay");
        let env_file = cfg_dir.join("relay.env");
        let relay_dir = home.join(".vsc-relay");
        let stats_file = relay_dir.join("stats.json");
        let log_file = relay_dir.join("agent.log");
        let (log_tx, log_rx) = channel();
        let (update_tx, update_rx) = channel();

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
            note: String::new(),
            show_settings: false,
            show_help: false,
            vscode_ok: true,
            extension_ok: true,
            shim_installed: false,
            ext_version: "-".to_string(),
            sessions_today: 0,
            turns_today: 0,
            launch_at_login: false,
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
        app.launch_at_login = app.autostart_path().exists();
        app.refresh_env();
        app.refresh_stats();
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
            let ev = match Command::new(&bin)
                .arg("self-update")
                .arg("--check")
                .output()
            {
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
            let (ok, msg) = match Command::new(&bin).arg("self-update").output() {
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
            let _ = Command::new(exe).stdin(Stdio::null()).spawn();
        }
        std::process::exit(0);
    }

    fn has_secrets(&self) -> bool {
        !self.token.trim().is_empty() && !self.secret.trim().is_empty()
    }

    fn autostart_path(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".config")
            .join("autostart")
            .join("vsc-relay.desktop")
    }

    fn refresh_env(&mut self) {
        let Ok(out) = Command::new(&self.agent_bin).arg("env-check").output() else {
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
        let _ = Command::new(&self.agent_bin)
            .arg("install-hooks")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::create_dir_all(&self.relay_dir);
        set_mode(&self.relay_dir, 0o700);
        self.rotate_log();

        let mut cmd = Command::new(&self.agent_bin);
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
                let log_path = self.log_file.clone();
                if let Some(out) = child.stdout.take() {
                    let tx = self.log_tx.clone();
                    let lp = log_path.clone();
                    std::thread::spawn(move || pump(out, tx, lp));
                }
                if let Some(err) = child.stderr.take() {
                    let tx = self.log_tx.clone();
                    std::thread::spawn(move || pump(err, tx, log_path));
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

    fn run_agent_note(&mut self, args: &[&str]) {
        match Command::new(&self.agent_bin).args(args).output() {
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

    fn rotate_log(&self) {
        if let Ok(meta) = std::fs::metadata(&self.log_file) {
            if meta.len() > LOG_ROTATE_BYTES {
                let bak = self.relay_dir.join("agent.log.1");
                let _ = std::fs::rename(&self.log_file, &bak);
            }
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

            ui.add_space(8.0);
            egui::Frame::none()
                .fill(ui.visuals().extreme_bg_color)
                .inner_margin(egui::Margin::same(6.0))
                .rounding(6.0)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
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

fn find_agent_binary() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sib = exe.with_file_name("vsc-relay-agent");
        if sib.exists() {
            return sib;
        }
    }
    PathBuf::from("vsc-relay-agent")
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

fn pump(stream: impl Read, tx: Sender<String>, log_path: PathBuf) {
    let mut reader = BufReader::new(stream);
    let mut logf = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Some(f) = logf.as_mut() {
                    let _ = f.write_all(line.as_bytes());
                }
                let _ = tx.send(line.trim_end().to_string());
            }
            Err(_) => break,
        }
    }
}

#[cfg(windows)]
fn kill_stray_daemons() {}

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
fn set_mode(path: &PathBuf, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(windows)]
fn set_mode(_path: &PathBuf, _mode: u32) {}

fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}
