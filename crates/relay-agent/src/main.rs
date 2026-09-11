mod actions;
mod artifact_store;
mod auth;
mod automation;
mod clirun;
mod compass;
mod config;
mod control_cli;
mod decision_log;
mod dedup;
mod fsutil;
mod gate_controller;
mod handoff;
mod hooks;
mod hostname;
mod ingress;
mod inject;
mod install;
mod log_file;
mod media;
mod permission;
mod question;
mod reactor;
mod self_update;
mod shimctl;
mod stats;
mod supervisor;
mod telegram;
mod tool_effect;
mod updates;

use anyhow::{bail, Context};
use chrono::Utc;
use config::{hours_to_ms, Config};
use relay_adapters::{claude, codex};
use relay_control::Control;
use relay_core::event::{EventKind, EventSource, RelayEvent};
use relay_core::state::{ClaudeState, CodexState};
use relay_core::{workspace_alias, AgentKind, MachineId, WindowEntry};
use relay_discovery::{scan, Paths};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use telegram::{esc_html, keyboard, Telegram};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

type Registry = Arc<RwLock<Vec<WindowEntry>>>;
type Active = Arc<RwLock<HashMap<i64, (String, String)>>>;
type PendingMedia = Arc<RwLock<HashMap<u64, PendingMediaBatch>>>;

static MEDIA_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Copy)]
struct MediaLimits {
    max_download_bytes: u64,
    ttl_ms: i64,
    store_max_bytes: u64,
}

struct PendingMediaBatch {
    files: Vec<media::StagedFile>,
    caption: String,
    forward: Option<String>,
    options: Vec<(String, String)>,
}

struct DaemonArgs {
    once: bool,
    json: bool,
    sessions: bool,
    interval: Option<u64>,
    codex_max_age_ms: Option<i64>,
}

struct Emitted {
    alias: String,
    machine: String,
    event: RelayEvent,
    usage: Option<relay_core::state::TokenUsage>,
}

struct AutoDrivers {
    reactor: Arc<reactor::Reactor>,
    robot: Arc<supervisor::robot::Robot>,
}

#[derive(Clone)]
struct Track {
    label: String,
    fingerprint: Option<String>,
    mode: Option<String>,
}

struct WinCtx {
    machine_id: MachineId,
    workspace: PathBuf,
    branch: Option<String>,
    agent: AgentKind,
    session_ref: String,
    title: Option<String>,
    usage: Option<relay_core::state::TokenUsage>,
    mode: Option<String>,
}

fn train_local_semantic_bundle(
    dataset: &str,
    output: &str,
    base: Option<&str>,
) -> anyhow::Result<()> {
    const TRAINER: &[u8] = include_bytes!("../../relay-semantic/scripts/train_nli_bundle.py");
    let mut command = std::process::Command::new("python3");
    command
        .arg("-")
        .arg("--dataset")
        .arg(dataset)
        .arg("--output")
        .arg(output)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    if let Some(base) = base.filter(|value| !value.trim().is_empty()) {
        command.arg("--base").arg(base);
    }
    let mut child = command.spawn().context("start semantic bundle trainer")?;
    child
        .stdin
        .take()
        .context("open semantic trainer stdin")?
        .write_all(TRAINER)?;
    let status = child.wait()?;
    if !status.success() {
        bail!("semantic bundle training failed with {status}");
    }
    let output = std::fs::canonicalize(output).context("trained bundle output is missing")?;
    let mut config = automation::AutomationConfig::load();
    config.smart.semantic.apply_preset("local")?;
    config.smart.semantic.local_dir = Some(output.clone());
    config.save()?;
    println!("smart local bundle = {}", output.display());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    compass::migrate_dossiers()?;
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = raw.first() {
        if first == "send" {
            let alias = raw.get(1).map(String::as_str).unwrap_or("");
            let sid = raw.get(2).map(String::as_str).unwrap_or("");
            let (text, media) = parse_send_args(raw.get(3..).unwrap_or(&[]));
            return run_send(alias, sid, &text, &media).await;
        }
        if first == "danger-check" {
            use std::io::BufRead;
            let patterns = hooks::danger_patterns();
            let mut flagged = 0usize;
            let mut total = 0usize;
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                let command = serde_json::from_str::<String>(&line).unwrap_or(line);
                if command.trim().is_empty() {
                    continue;
                }
                total += 1;
                let lowered = command.to_lowercase();
                let hit = patterns.iter().find(|pattern| lowered.contains(*pattern));
                let head: String = command
                    .split_whitespace()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" ");
                let head: String = head.chars().take(70).collect();
                match hit {
                    Some(pattern) => {
                        flagged += 1;
                        println!("FLAGGED  [{pattern}]  {head}");
                    }
                    None => println!("clear             {head}"),
                }
            }
            println!(
                "\n{flagged} of {total} flagged by {} patterns",
                patterns.len()
            );
            return Ok(());
        }
        if first == "decisions" {
            let value_of = |flag: &str, fallback: &str| -> String {
                raw.iter()
                    .position(|arg| arg == flag)
                    .and_then(|at| raw.get(at + 1))
                    .cloned()
                    .unwrap_or_else(|| fallback.to_string())
            };
            return decision_log::report(&value_of("--since", "24h"), &value_of("--by", "outcome"));
        }
        if first == "handoff" {
            let json = raw.iter().any(|a| a == "--json");
            let positional: Vec<&str> = raw
                .iter()
                .skip(1)
                .map(String::as_str)
                .filter(|a| !a.starts_with("--"))
                .collect();
            if positional.first() == Some(&"targets") {
                return run_handoff_targets(json);
            }
            if positional.first() == Some(&"destinations") {
                let session = positional
                    .get(1)
                    .context("usage: handoff destinations SESSION_ID [--json]")?;
                return run_handoff_destinations(session, json);
            }
            if let Some(to) = raw.iter().position(|a| a == "--to") {
                let destination = raw
                    .get(to + 1)
                    .context("usage: handoff SESSION_ID --to DESTINATION_ID")?;
                let session = positional
                    .first()
                    .context("usage: handoff SESSION_ID --to DESTINATION_ID")?;
                return run_handoff_to(session, destination, json).await;
            }
            if positional.first() == Some(&"receipt") {
                let workspace = positional
                    .get(1)
                    .context("usage: handoff receipt TARGET_WORKSPACE [--json]")?;
                return run_handoff_receipt(workspace, json);
            }
            let session = positional.first().context(
                "usage: handoff SESSION_ID TARGET_WORKSPACE | handoff receipt WORKSPACE",
            )?;
            let workspace = positional.get(1).context(
                "usage: handoff SESSION_ID TARGET_WORKSPACE | handoff receipt WORKSPACE",
            )?;
            return run_handoff(session, workspace, json).await;
        }
        if control_cli::is_control_command(first) {
            return control_cli::run(&raw);
        }
        if hooks::is_hook_command(first) {
            return hooks::run_hook(&raw);
        }
        if install::is_install_command(first) {
            return install::run();
        }
        if shimctl::is_shim_command(first) {
            return shimctl::run(&raw);
        }
        if automation::is_automation_command(first) {
            if raw.get(1).map(String::as_str) == Some("smart")
                && raw.get(2).map(String::as_str) == Some("train-local")
            {
                let dataset = raw.get(3).context(
                    "usage: automation smart train-local DATASET.jsonl OUTPUT_DIR [BASE_MODEL]",
                )?;
                let output = raw.get(4).context(
                    "usage: automation smart train-local DATASET.jsonl OUTPUT_DIR [BASE_MODEL]",
                )?;
                return train_local_semantic_bundle(
                    dataset,
                    output,
                    raw.get(5).map(String::as_str),
                );
            }
            if raw.get(1).map(String::as_str) == Some("review")
                && raw.get(2).map(String::as_str) == Some("run")
            {
                let session = raw
                    .get(3)
                    .context("usage: automation review run <session_id>")?;
                let flag = |name: &str| {
                    raw.iter()
                        .position(|arg| arg == name)
                        .and_then(|at| raw.get(at + 1))
                        .map(String::as_str)
                };
                return review_run(session, flag("--reviewer"), flag("--depth")).await;
            }
            if raw.get(1).map(String::as_str) == Some("review")
                && raw.get(2).map(String::as_str) == Some("probe")
            {
                let family = raw
                    .get(3)
                    .context("usage: automation review probe <claude|codex> <transcript.jsonl>")?;
                let transcript = raw
                    .get(4)
                    .context("usage: automation review probe <claude|codex> <transcript.jsonl>")?;
                return review_probe(family, transcript).await;
            }
            if raw.get(1).map(String::as_str) == Some("smart")
                && raw.get(2).map(String::as_str) == Some("install-local")
            {
                let path = relay_semantic::install::install_builtin().await?;
                println!("local semantic model installed at {}", path.display());
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("smart")
                && raw.get(2).map(String::as_str) == Some("check")
            {
                let cfg = automation::AutomationConfig::load();
                println!(
                    "{}",
                    relay_semantic::check(
                        &cfg.smart.semantic,
                        compass::semantic_key(&cfg.smart.semantic)
                    )
                    .await?
                );
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                if session_id.is_empty() {
                    bail!("usage: automation compass <session_id>");
                }
                println!("{}", compass::inspect(session_id)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-link") {
                let child = raw.get(2).map(String::as_str).unwrap_or("");
                let parent = raw.get(3).map(String::as_str).unwrap_or("");
                println!("{}", compass::link_continuation(child, parent)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-unlink") {
                let child = raw.get(2).map(String::as_str).unwrap_or("");
                println!("{}", compass::unlink_continuation(child)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-mark") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                let label = raw.get(3).map(String::as_str).unwrap_or("");
                println!("{}", compass::mark_problem(session_id, label)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-pin") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                let target = raw.get(3).map(String::as_str).unwrap_or("");
                let obligation = raw.get(4).map(String::as_str).unwrap_or("");
                let Some(epoch) = raw.get(5).and_then(|value| value.parse::<u32>().ok()) else {
                    bail!("usage: automation compass-pin <session_id> <target> <obligation_id> <epoch> [--controller]");
                };
                let controller = raw.iter().any(|arg| arg == "--controller");
                println!(
                    "{}",
                    compass::record_gate_pin(session_id, target, obligation, epoch, controller)?
                );
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-pins") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                if session_id.is_empty() {
                    bail!("usage: automation compass-pins <session_id>");
                }
                println!("{}", compass::list_gate_pins(session_id)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("usage") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                if session_id.is_empty() {
                    bail!("usage: automation usage <session_id>");
                }
                println!("{}", compass::usage_report(session_id)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-unpin") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                if session_id.is_empty() {
                    bail!("usage: automation compass-unpin <session_id> [target]");
                }
                let target = raw
                    .get(3)
                    .map(String::as_str)
                    .filter(|value| !value.starts_with("--"));
                println!("{}", compass::clear_gate_pins(session_id, target)?);
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-profile") {
                match raw.get(2).map(String::as_str).unwrap_or("status") {
                    "status" => println!("{}", compass::information_profile_status()?),
                    "bootstrap" => println!("{}", compass::bootstrap_information_profile()?),
                    _ => bail!("usage: automation compass-profile <status|bootstrap>"),
                }
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-assess") {
                let session_id = raw.get(2).map(String::as_str).unwrap_or("");
                if session_id.is_empty() {
                    bail!("usage: automation compass-assess <session_id>");
                }
                let cfg = automation::AutomationConfig::load();
                match compass::assess_smart(session_id, &cfg.smart.semantic).await? {
                    Some(staged) => println!("{}", compass::render_assessment(&staged)),
                    None => println!("no staged assessment (feature idle or too few steps)"),
                }
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("compass-assess-codex") {
                let thread_id = raw.get(2).map(String::as_str).unwrap_or("");
                let rollout = raw.get(3).map(String::as_str).unwrap_or("");
                if thread_id.is_empty() || rollout.is_empty() {
                    bail!("usage: automation compass-assess-codex <thread_id> <rollout_path>");
                }
                let text = std::fs::read_to_string(rollout)?;
                let state = relay_core::state::reduce_codex(&text).state;
                let cfg = automation::AutomationConfig::load();
                match compass::assess_codex_smart(
                    thread_id,
                    std::path::Path::new(rollout),
                    &state,
                    &cfg.smart.semantic,
                )
                .await?
                {
                    Some(staged) => println!("{}", compass::render_assessment(&staged)),
                    None => println!("no staged assessment (feature idle or too few steps)"),
                }
                return Ok(());
            }
            if raw.get(1).map(String::as_str) == Some("discover") {
                let only = raw
                    .get(2)
                    .map(String::as_str)
                    .filter(|s| !s.starts_with("--"));
                return supervisor::discover::print_discovery(
                    raw.iter().any(|a| a == "--json"),
                    only,
                )
                .await;
            }
            if raw.get(1).map(String::as_str) == Some("ask") {
                let backend = raw.get(2).map(String::as_str).unwrap_or("");
                let model = raw.get(3).map(String::as_str).unwrap_or("");
                let prompt = raw.get(4..).map(|s| s.join(" ")).unwrap_or_default();
                return supervisor::run_ask(backend, model, &prompt).await;
            }
            if raw.get(1).map(String::as_str) == Some("route") {
                let prompt = raw.get(2..).map(|s| s.join(" ")).unwrap_or_default();
                return supervisor::run_route(&prompt).await;
            }
            if raw.get(1).map(String::as_str) == Some("improve") {
                let prompt = raw.get(2..).map(|s| s.join(" ")).unwrap_or_default();
                return supervisor::run_improve(&prompt).await;
            }
            if raw.get(1).map(String::as_str) == Some("models") {
                let id = raw.get(2).map(String::as_str).unwrap_or("");
                return supervisor::discover::print_models(id).await;
            }
            if raw.get(1).map(String::as_str) == Some("health") {
                let only = raw
                    .iter()
                    .skip(2)
                    .find(|arg| !arg.starts_with("--"))
                    .map(String::as_str);
                return supervisor::run_health(raw.iter().any(|a| a == "--json"), only).await;
            }
            if raw.get(1).map(String::as_str) == Some("login") {
                let id = raw.get(2).map(String::as_str).unwrap_or("");
                return supervisor::run_login(id).await;
            }
            return automation::run(&raw);
        }
        if self_update::is_self_update_command(first) {
            return self_update::run(&raw).await;
        }
    }

    let args = parse_daemon_args(&raw)?;

    if args.sessions {
        if let Some(snapshot) = fresh_sessions_snapshot() {
            println!("{snapshot}");
            return Ok(());
        }
        let mut cfg = Config::from_env();
        if let Some(max_age) = args.codex_max_age_ms {
            cfg.codex_max_age_ms = max_age;
        }
        let paths = Paths::discover()?;
        let machine_id = MachineId(cfg.machine_name.clone());
        let registry: Registry = Arc::new(RwLock::new(Vec::new()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Emitted>();
        let mut tracker = HashMap::new();
        scan_and_emit(
            &paths,
            &machine_id,
            cfg.codex_max_age_ms,
            &registry,
            &mut tracker,
            &tx,
            None,
        )
        .await;
        drop(tx);
        while rx.recv().await.is_some() {}
        print_sessions_json(&registry).await;
        return Ok(());
    }

    hide_own_console();

    let log_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".vsc-relay")
        .join("agent.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .with_ansi(false)
        .with_writer(log_file::writer(&log_path))
        .init();

    if !relay_ipc::acquire_single_instance("vsc-relay-agent") {
        warn!(
            holder = relay_ipc::single_instance_holder("vsc-relay-agent").unwrap_or(0),
            "another vsc-relay-agent already holds the machine lock; exiting so the two never share one bot"
        );
        return Ok(());
    }
    self_update::sweep_old();

    let mut cfg = Config::from_env();
    if let Some(interval) = args.interval {
        cfg.interval = interval;
    }
    if let Some(max_age) = args.codex_max_age_ms {
        cfg.codex_max_age_ms = max_age;
    }
    let paths = Paths::discover()?;
    let machine_id = MachineId(cfg.machine_name.clone());
    info!(machine = %cfg.machine_name, telegram = cfg.telegram_token.is_some(), proxy = telegram::configured_proxy().map(|p| format!("fallback {}", telegram::redact_proxy(&p))).unwrap_or_else(|| "none, direct only".to_string()), api = %telegram::configured_api_base(), "vsc-relay-agent starting");

    let registry: Registry = Arc::new(RwLock::new(Vec::new()));
    let active: Active = Arc::new(RwLock::new(HashMap::new()));
    let media_pending: PendingMedia = Arc::new(RwLock::new(HashMap::new()));
    let media_limits = MediaLimits {
        max_download_bytes: cfg.media_max_download_bytes,
        ttl_ms: cfg.media_ttl_ms,
        store_max_bytes: cfg.media_store_max_bytes,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Emitted>();
    let ctl: Arc<dyn Control> = Arc::from(relay_control::platform());
    let pending: ingress::Pending = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let hook_dedup = Arc::new(ingress::HookDedup::default());
    let auth = Arc::new(auth::Auth::load(
        cfg.allowed_chats.clone(),
        cfg.pair_secret.clone(),
    ));

    fsutil::secure_dir(
        &dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".vsc-relay"),
    );

    media::sweep(cfg.media_ttl_ms, cfg.media_store_max_bytes);

    if std::env::var_os("VSC_RELAY_NO_RESHIM").is_none() {
        match shimctl::install_shim() {
            Ok(msg) => info!("auto-reshim: {}", msg.replace('\n', "; ")),
            Err(e) => warn!("auto-reshim skipped: {e}"),
        }
    }

    if args.once {
        let mut tracker = HashMap::new();
        scan_and_emit(
            &paths,
            &machine_id,
            cfg.codex_max_age_ms,
            &registry,
            &mut tracker,
            &tx,
            None,
        )
        .await;
        drop(tx);
        while let Some(em) = rx.recv().await {
            print_event(&em.alias, &em.event, args.json);
        }
        return Ok(());
    }

    tokio::spawn(async {
        loop {
            let _ = tokio::task::spawn_blocking(inject::sweep_stale_sockets).await;
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });

    let drivers = Arc::new(AutoDrivers {
        reactor: reactor::Reactor::new(tx.clone()),
        robot: supervisor::robot::Robot::new(tx.clone()),
    });
    let watch = {
        let paths = paths.clone();
        let machine_id = machine_id.clone();
        let registry = registry.clone();
        let fallback = cfg.interval.max(15);
        let max_age = cfg.codex_max_age_ms;
        let drivers = drivers.clone();
        tokio::spawn(async move {
            let mut tracker = HashMap::new();
            let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
            let _watcher = spawn_watcher(&paths, fs_tx);
            if _watcher.is_none() {
                warn!("fs watcher unavailable; falling back to polling every {fallback}s");
            }
            loop {
                scan_and_emit(
                    &paths,
                    &machine_id,
                    max_age,
                    &registry,
                    &mut tracker,
                    &tx,
                    Some(&drivers),
                )
                .await;
                tokio::select! {
                    _ = fs_rx.recv() => {
                        tokio::time::sleep(Duration::from_millis(400)).await;
                        while fs_rx.try_recv().is_ok() {}
                    }
                    _ = tokio::time::sleep(Duration::from_secs(fallback)) => {}
                }
            }
        })
    };

    if let Some(token) = cfg.telegram_token.clone() {
        let tg = Arc::new(Telegram::new(token)?);
        if let Err(e) = tg.set_my_commands(&bot_commands()).await {
            warn!("setMyCommands failed: {e:#}");
        }
        if !auth.has_secret() && cfg.allowed_chats.is_empty() {
            warn!("no RELAY_PAIR_SECRET and no TELEGRAM_ALLOWED_CHATS: control is disabled until a chat is authorized. Set RELAY_PAIR_SECRET to allow pairing via /auth <key>.");
        }
        for chat in auth.recipients().await {
            let _ = tg
                .send(
                    chat,
                    &format!(
                        "🚀 vsc-relay-agent online on <b>{}</b>",
                        esc_html(&cfg.machine_name)
                    ),
                    None,
                )
                .await;
        }

        let dedup = dedup::PromptDedup::new();
        let forwarder = {
            let tg = tg.clone();
            let auth = auth.clone();
            let dedup = dedup.clone();
            tokio::spawn(async move {
                while let Some(em) = rx.recv().await {
                    let auto_action = match &em.event.kind {
                        EventKind::AutoAction { action, .. } => action.as_str(),
                        _ => "",
                    };
                    info!(
                        target: "relay::trace",
                        pipeline = "event",
                        stage = "emitted",
                        kind = em.event.kind.tag(),
                        alias = %em.alias,
                        action = auto_action,
                        "relay event"
                    );
                    if let Some(u) = em.usage {
                        info!(
                            target: "relay::trace",
                            pipeline = "disk",
                            stage = "tokens",
                            kind = em.event.kind.tag(),
                            alias = %em.alias,
                            in_tok = u.input,
                            out_tok = u.output,
                            cache_read = u.cache_read,
                            cache_creation = u.cache_creation,
                            reasoning = u.reasoning,
                            "turn tokens"
                        );
                    }
                    stats::record(em.event.kind.tag());
                    if matches!(em.event.kind, EventKind::SessionStarted { .. }) {
                        let text = format!(
                            "🆕 <b>{}</b> · <code>{}</code>\nA new chat is open. It is in the list now.",
                            esc_html(&em.machine),
                            esc_html(&em.alias)
                        );
                        let kb = keyboard(vec![vec![(
                            "💬 Open this workspace".to_string(),
                            format!("m:w:{}", em.alias),
                        )]]);
                        for chat in auth.recipients().await {
                            let _ = tg.send_ex(chat, &text, Some(kb.clone()), true).await;
                        }
                        continue;
                    }
                    let (text, kb) = format_event(&em.machine, &em.alias, &em.event);
                    let silent = !em.event.actionable;
                    if let EventKind::QuestionAsked { question } = &em.event.kind {
                        if let Some(tuid) = question.tool_use_id.clone() {
                            if dedup.out_owns(&tuid).await {
                                info!(
                                    target: "relay::trace",
                                    pipeline = "disk",
                                    stage = "suppressed",
                                    kind = "question_asked",
                                    alias = %em.alias,
                                    "out-socket owns question; disk card suppressed"
                                );
                                continue;
                            }
                            let mut cards: Vec<(i64, i64)> = Vec::new();
                            for chat in auth.recipients().await {
                                match tg.send_ex(chat, &text, kb.clone(), silent).await {
                                    Ok(mid) => cards.push((chat, mid)),
                                    Err(e) => warn!("send notify failed: {e:#}"),
                                }
                            }
                            if let Some(stale) = dedup.register_disk(&tuid, cards).await {
                                for (c, m) in stale {
                                    let _ = tg
                                        .edit_message_text(c, m, dedup::COLLAPSE_NOTE, None)
                                        .await;
                                }
                            }
                            continue;
                        }
                    }
                    for chat in auth.recipients().await {
                        if let Err(e) = tg.send_ex(chat, &text, kb.clone(), silent).await {
                            warn!("send notify failed: {e:#}");
                        }
                    }
                }
            })
        };

        let ingress_task = tokio::spawn(ingress::serve(ingress::IngressCtx {
            machine: cfg.machine_name.clone(),
            tg: Some(tg.clone()),
            auth: auth.clone(),
            pending: pending.clone(),
            hook_dedup: hook_dedup.clone(),
            dedup: dedup.clone(),
        }));

        let questions = question::new();
        let perms = permission::new();
        tokio::spawn(question::start(
            questions.clone(),
            perms.clone(),
            tg.clone(),
            auth.clone(),
            dedup.clone(),
        ));
        tokio::spawn(updates::watch(tg.clone(), auth.clone()));
        tokio::spawn(updates::marketplace_watch(tg.clone(), auth.clone()));
        tokio::spawn(reshim_watch());
        tokio::spawn(untapped_watch(
            tg.clone(),
            auth.clone(),
            cfg.machine_name.clone(),
        ));

        let poller = tokio::spawn(poll_commands(
            tg.clone(),
            registry.clone(),
            active.clone(),
            ctl.clone(),
            auth.clone(),
            pending.clone(),
            questions.clone(),
            perms.clone(),
            media_pending.clone(),
            media_limits,
        ));

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = watch => {}
            _ = forwarder => {}
            _ = ingress_task => {}
            _ = poller => {}
        }
    } else {
        let ingress_task = tokio::spawn(ingress::serve(ingress::IngressCtx {
            machine: cfg.machine_name.clone(),
            tg: None,
            auth: auth.clone(),
            pending: pending.clone(),
            hook_dedup: hook_dedup.clone(),
            dedup: dedup::PromptDedup::new(),
        }));
        let printer = tokio::spawn(async move {
            while let Some(em) = rx.recv().await {
                print_event(&em.alias, &em.event, args.json);
            }
        });
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = watch => {}
            _ = ingress_task => {}
            _ = printer => {}
        }
    }

    Ok(())
}

#[cfg(windows)]
fn hide_own_console() {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    unsafe {
        let hwnd = GetConsoleWindow();
        if hwnd.is_null() {
            return;
        }
        let mut pids = [0u32; 4];
        let count = GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32);
        if count == 1 {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

#[cfg(not(windows))]
fn hide_own_console() {}

async fn reshim_watch() {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let st = shimctl::env_status();
        if st.extension && !st.shim_installed {
            match tokio::task::spawn_blocking(shimctl::install_shim).await {
                Ok(Ok(msg)) => info!("reshim: {}", msg.replace('\n', "; ")),
                Ok(Err(e)) => warn!("reshim skipped: {e}"),
                Err(e) => warn!("reshim task join: {e}"),
            }
        }
    }
}

async fn untapped_watch(tg: Arc<Telegram>, auth: Arc<auth::Auth>, machine: String) {
    use std::collections::HashSet;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let sessions_dir = home.join(".claude").join("sessions");
    let mut warned: HashSet<u32> = HashSet::new();
    let mut prev: HashSet<u32> = HashSet::new();
    loop {
        tokio::time::sleep(Duration::from_secs(45)).await;
        let mut tapped: HashSet<u32> = relay_ipc::live_out_pids().into_iter().collect();
        let live = live_session_pids(&sessions_dir);
        let mut untapped: HashSet<u32> = live.difference(&tapped).copied().collect();
        let mut fresh: Vec<u32> = untapped
            .intersection(&prev)
            .copied()
            .filter(|p| !warned.contains(p))
            .collect();
        if !fresh.is_empty() {
            let _ = tokio::task::spawn_blocking(shimctl::install_shim).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            tapped = relay_ipc::live_out_pids().into_iter().collect();
            untapped = live.difference(&tapped).copied().collect();
            fresh = untapped
                .intersection(&prev)
                .copied()
                .filter(|p| !warned.contains(p))
                .collect();
        }
        if !fresh.is_empty() {
            let mut aliases: Vec<String> = Vec::new();
            for p in &fresh {
                warned.insert(*p);
                let a = pid_alias(&sessions_dir, *p).unwrap_or_else(|| format!("pid {p}"));
                warn!(target: "relay::perm", pid = p, alias = %a, "session not tapped; permissions cannot be approved from Telegram");
                if !aliases.contains(&a) {
                    aliases.push(a);
                }
            }
            let text = format!(
                "⚠️ <b>{}</b>\n{} session(s) are running without remote control - restart them in VS Code to approve permissions from Telegram:\n<code>{}</code>",
                esc_html(&machine),
                fresh.len(),
                esc_html(&aliases.join(", "))
            );
            for chat in auth.recipients().await {
                let _ = tg.send(chat, &text, None).await;
            }
        }
        warned.retain(|p| untapped.contains(p));
        prev = untapped;
    }
}

fn live_session_pids(dir: &std::path::Path) -> std::collections::HashSet<u32> {
    let mut set = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            if let Some(pid) = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
            {
                if inject::pid_alive(pid) {
                    set.insert(pid);
                }
            }
        }
    }
    set
}

fn pid_alias(dir: &std::path::Path, pid: u32) -> Option<String> {
    let txt = std::fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&txt).ok()?;
    let cwd = v.get("cwd").and_then(|x| x.as_str())?;
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
}

fn parse_daemon_args(args: &[String]) -> anyhow::Result<DaemonArgs> {
    let mut parsed = DaemonArgs {
        once: false,
        json: false,
        sessions: false,
        interval: None,
        codex_max_age_ms: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "sessions" => parsed.sessions = true,
            "--once" => parsed.once = true,
            "--json" => parsed.json = true,
            "--interval" => {
                let value = it.next().context("usage: --interval <seconds>")?;
                parsed.interval = Some(parse_nonzero_u64("--interval", value)?);
            }
            "--codex-max-age-h" => {
                let value = it.next().context("usage: --codex-max-age-h <hours>")?;
                let hours = parse_nonzero_u64("--codex-max-age-h", value)?;
                parsed.codex_max_age_ms =
                    Some(hours_to_ms(hours).context("--codex-max-age-h is too large")?);
            }
            "--help" | "-h" => bail!("{}", usage()),
            other => bail!("unknown argument '{other}'\n{}", usage()),
        }
    }
    Ok(parsed)
}

fn parse_nonzero_u64(name: &str, value: &str) -> anyhow::Result<u64> {
    let parsed = value
        .trim()
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn usage() -> &'static str {
    "usage: vsc-relay-agent [--once] [--json] [--interval <seconds>] [--codex-max-age-h <hours>]"
}

static LAST_COMPASS_AUTOACTION: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> =
    std::sync::OnceLock::new();

fn push_compass_autoaction(
    kinds: &mut Vec<EventKind>,
    session_id: &str,
    action: &str,
    detail: String,
) {
    let mut last = LAST_COMPASS_AUTOACTION
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = format!("{session_id}\u{0}{action}");
    if last.get(&key).map(String::as_str) == Some(detail.as_str()) {
        return;
    }
    last.insert(key, detail.clone());
    kinds.push(EventKind::AutoAction {
        action: action.to_string(),
        detail,
    });
}

fn spawn_watcher(
    paths: &Paths,
    tx: mpsc::UnboundedSender<()>,
) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok()?;
    let targets = [
        (paths.claude_projects_dir(), RecursiveMode::Recursive),
        (paths.claude_ide_dir(), RecursiveMode::NonRecursive),
        (paths.claude_sessions_dir(), RecursiveMode::NonRecursive),
        (paths.codex_dir().join("sessions"), RecursiveMode::Recursive),
        (paths.codex_dir(), RecursiveMode::NonRecursive),
    ];
    let mut any = false;
    for (path, mode) in targets {
        if path.exists() && watcher.watch(&path, mode).is_ok() {
            any = true;
        }
    }
    if any {
        Some(watcher)
    } else {
        None
    }
}

async fn scan_and_emit(
    paths: &Paths,
    machine_id: &MachineId,
    max_age_ms: i64,
    registry: &Registry,
    tracker: &mut HashMap<String, Track>,
    tx: &mpsc::UnboundedSender<Emitted>,
    drivers: Option<&AutoDrivers>,
) {
    let mut windows = scan(paths);
    let now_ms = Utc::now().timestamp_millis();
    let auto_cfg = drivers.map(|_| automation::AutomationConfig::load());

    let initial_scan = tracker.is_empty();

    for w in &mut windows {
        let branch = w.git_branch.clone();
        let alias = workspace_alias(&w.workspace);

        for c in &mut w.claude {
            let read = if c.jsonl_path.exists() {
                claude::read_state(&c.jsonl_path, c.pid).ok()
            } else {
                None
            };
            let transcript_ready = read.is_some();
            let (state, ai_title, git_b, excerpt, usage, mode, tip_uuid) = match read {
                Some(res) => (
                    res.state.clone(),
                    res.reduction.ai_title.clone(),
                    res.reduction.git_branch.clone(),
                    res.reduction
                        .last_assistant_text
                        .clone()
                        .unwrap_or_default(),
                    res.reduction.last_turn_tokens,
                    res.reduction.mode.clone(),
                    res.reduction.tip_uuid.clone(),
                ),
                None => (
                    ClaudeState::Unknown,
                    None,
                    None,
                    String::new(),
                    None,
                    None,
                    None,
                ),
            };
            c.state = state.clone();
            c.title = ai_title;
            let ctx = WinCtx {
                machine_id: machine_id.clone(),
                workspace: w.workspace.clone(),
                branch: git_b.or_else(|| branch.clone()),
                agent: AgentKind::ClaudeCode,
                session_ref: c.session_id.clone(),
                title: c.name.clone(),
                usage,
                mode,
            };
            let entrypoint = c.entrypoint.clone().unwrap_or_default();
            let last_message = excerpt.clone();
            let first_observation = !tracker.contains_key(&key_of(&ctx));
            let pending_gui_completion = matches!(state, ClaudeState::Idle)
                && pending_gui_completion_ready(&c.session_id, &c.jsonl_path);
            let mut kinds = claude_events(
                tracker,
                &ctx,
                &state,
                entrypoint,
                excerpt,
                pending_gui_completion,
            );
            if initial_scan {
                kinds.retain(|kind| {
                    pending_gui_completion && matches!(kind, EventKind::TurnComplete { .. })
                });
            }
            let terminal_observation =
                matches!(state, ClaudeState::Idle | ClaudeState::Error { .. });
            let relay_mode = auto_cfg
                .as_ref()
                .map(|cfg| cfg.resolve(Some(&c.session_id), &alias, automation::now_secs()));
            if !first_observation
                && terminal_observation
                && transcript_ready
                && auto_cfg
                    .as_ref()
                    .map(|cfg| cfg.smart.enabled)
                    .unwrap_or(false)
                && relay_mode != Some(automation::Mode::Manual)
            {
                if let Some(notice) = compass::observe_session(&c.session_id) {
                    if notice.risk_alert {
                        push_compass_autoaction(
                            &mut kinds,
                            &c.session_id,
                            "compass_risk",
                            format!(
                                "risk {:.2}{}; review the session goal and recent progress",
                                notice.risk,
                                if notice.stuck { ", stuck" } else { "" }
                            ),
                        );
                    }
                    if let Some(staged) = &notice.staged {
                        if !staged.completion.is_verified_final() {
                            push_compass_autoaction(
                                &mut kinds,
                                &c.session_id,
                                "compass_staged",
                                staged.explanation.clone(),
                            );
                        }
                    }
                }
            }
            let emitted = emit_all(
                tracker,
                &ctx,
                &alias,
                &machine_id.0,
                kinds,
                state.label().to_string(),
                tx,
            );
            if pending_gui_completion && emitted {
                if let Err(error) = mark_gui_completion_notified(&c.session_id) {
                    warn!(
                        target: "relay::trace",
                        pipeline = "sessions",
                        stage = "completion_receipt_failed",
                        alias = %alias,
                        "GUI completion receipt update failed: {error:#}"
                    );
                }
            }
            if transcript_ready && !first_observation {
                if let (Some(drivers), Some(cfg)) = (drivers, auto_cfg.as_ref()) {
                    let nctx = reactor::NotifyCtx {
                        machine_id: machine_id.clone(),
                        workspace: w.workspace.clone(),
                        branch: ctx.branch.clone(),
                        agent: AgentKind::ClaudeCode,
                        session_ref: c.session_id.clone(),
                        title: c.name.clone(),
                        alias: alias.clone(),
                    };
                    drivers
                        .reactor
                        .observe_claude(
                            c.pid,
                            &c.session_id,
                            &c.jsonl_path,
                            &state,
                            tip_uuid.as_deref(),
                            cfg,
                            &nctx,
                        )
                        .await;
                    drivers
                        .robot
                        .observe_claude(
                            c.pid,
                            &c.session_id,
                            &c.jsonl_path,
                            &state,
                            tip_uuid.as_deref(),
                            &last_message,
                            cfg,
                            &nctx,
                        )
                        .await;
                }
            }
        }

        let attaches =
            codex::attach_all(&paths.codex_state_db(), &w.workspace, max_age_ms, now_ms, 8);
        let mut codex_agents = Vec::new();
        for (codex_index, a) in attaches.into_iter().enumerate() {
            let ctx = WinCtx {
                machine_id: machine_id.clone(),
                workspace: w.workspace.clone(),
                branch: branch.clone(),
                agent: AgentKind::Codex,
                session_ref: a.agent.thread_id.clone(),
                title: Some(a.agent.title.clone()),
                usage: a.usage,
                mode: a.approval_policy.clone(),
            };
            let excerpt = a.last_message.clone().unwrap_or_default();
            let cur_label = a.agent.state.label().to_string();
            let first_observation = !tracker.contains_key(&key_of(&ctx));
            let mut kinds = codex_events(
                tracker,
                &ctx,
                &a.agent.state,
                excerpt.clone(),
                a.last_duration_ms,
            );
            if initial_scan {
                kinds.clear();
            }
            emit_all(tracker, &ctx, &alias, &machine_id.0, kinds, cur_label, tx);

            if codex_index == 0 && !first_observation {
                if let (Some(drivers), Some(cfg)) = (drivers, auto_cfg.as_ref()) {
                    let nctx = reactor::NotifyCtx {
                        machine_id: machine_id.clone(),
                        workspace: w.workspace.clone(),
                        branch: ctx.branch.clone(),
                        agent: AgentKind::Codex,
                        session_ref: a.agent.thread_id.clone(),
                        title: Some(a.agent.title.clone()),
                        alias: alias.clone(),
                    };
                    drivers
                        .robot
                        .observe_codex(
                            &a.agent.thread_id,
                            &a.agent.rollout_path,
                            &a.agent.state,
                            &excerpt,
                            cfg,
                            &nctx,
                        )
                        .await;
                }
            }
            codex_agents.push(a.agent);
        }
        w.codex = codex_agents;
    }

    *registry.write().await = windows;
    persist_sessions_snapshot(registry).await;
}

fn key_of(ctx: &WinCtx) -> String {
    format!("{}|{}", ctx.agent.as_str(), ctx.session_ref)
}

fn is_busy(label: &str) -> bool {
    matches!(
        label,
        "working" | "subagent" | "awaiting_permission" | "pending_question"
    )
}

fn claude_events(
    tracker: &mut HashMap<String, Track>,
    ctx: &WinCtx,
    state: &ClaudeState,
    entrypoint: String,
    excerpt: String,
    pending_gui_completion: bool,
) -> Vec<EventKind> {
    let prev = tracker.get(&key_of(ctx)).cloned();
    let mut kinds = Vec::new();
    if prev.is_none() {
        kinds.push(EventKind::SessionStarted { entrypoint });
    }
    let prev_label = prev.as_ref().map(|t| t.label.clone());
    if let Some(cur) = ctx.mode.as_deref() {
        let prev_mode = prev.as_ref().and_then(|t| t.mode.clone());
        if prev_mode.as_deref() != Some(cur) {
            let is_alert = matches!(cur, "acceptEdits" | "bypassPermissions");
            if prev_mode.is_some() || is_alert {
                kinds.push(EventKind::ModeChanged {
                    from: prev_mode.unwrap_or_else(|| "unknown".into()),
                    to: cur.to_string(),
                    alert: is_alert,
                });
            }
        }
    }
    match state {
        ClaudeState::PendingQuestion(q) => {
            kinds.push(EventKind::QuestionAsked {
                question: q.clone(),
            });
        }
        ClaudeState::AwaitingPermission {
            tool,
            target,
            suspected,
        } => kinds.push(EventKind::AwaitingPermission {
            tool: tool.clone(),
            target: target.clone(),
            suspected: *suspected,
        }),
        ClaudeState::Error { message } => kinds.push(EventKind::Error {
            message: message.clone(),
        }),
        ClaudeState::Idle
            if pending_gui_completion || prev_label.as_deref().map(is_busy).unwrap_or(false) =>
        {
            kinds.push(EventKind::TurnComplete {
                last_message_excerpt: relay_core::state::truncate(&excerpt, 8000),
                duration_ms: None,
            });
        }
        _ => {}
    }
    kinds
}

fn codex_events(
    tracker: &mut HashMap<String, Track>,
    ctx: &WinCtx,
    state: &CodexState,
    excerpt: String,
    duration_ms: Option<u64>,
) -> Vec<EventKind> {
    let prev = tracker.get(&key_of(ctx)).cloned();
    let mut kinds = Vec::new();
    if prev.is_none() {
        kinds.push(EventKind::SessionStarted {
            entrypoint: "codex_vscode".to_string(),
        });
    }
    let was_busy = prev.as_ref().map(|t| is_busy(&t.label)).unwrap_or(false);
    if let Some(cur) = ctx.mode.as_deref() {
        let prev_mode = prev.as_ref().and_then(|t| t.mode.clone());
        if prev_mode.as_deref() != Some(cur) {
            let is_alert = cur == "never";
            if prev_mode.is_some() || is_alert {
                kinds.push(EventKind::ModeChanged {
                    from: prev_mode.unwrap_or_else(|| "unknown".into()),
                    to: cur.to_string(),
                    alert: is_alert,
                });
            }
        }
    }
    match state {
        CodexState::Idle | CodexState::NeedsReplyMaybe { .. } if was_busy => {
            kinds.push(EventKind::TurnComplete {
                last_message_excerpt: relay_core::state::truncate(&excerpt, 8000),
                duration_ms,
            });
        }
        CodexState::Error { message } => kinds.push(EventKind::Error {
            message: message.clone(),
        }),
        _ => {}
    }
    kinds
}

#[allow(clippy::too_many_arguments)]
fn emit_all(
    tracker: &mut HashMap<String, Track>,
    ctx: &WinCtx,
    alias: &str,
    machine: &str,
    kinds: Vec<EventKind>,
    cur_label: String,
    tx: &mpsc::UnboundedSender<Emitted>,
) -> bool {
    let key = key_of(ctx);
    let mut emitted = false;
    for kind in kinds {
        let event = RelayEvent::new(
            ctx.machine_id.clone(),
            ctx.workspace.clone(),
            ctx.branch.clone(),
            ctx.agent,
            ctx.session_ref.clone(),
            ctx.title.clone(),
            Utc::now(),
            kind,
            EventSource::Tail,
        );
        let last_fp = tracker.get(&key).and_then(|t| t.fingerprint.clone());
        if last_fp.as_deref() == Some(event.fingerprint.as_str()) {
            continue;
        }
        let fp = event.fingerprint.clone();
        emitted |= tx
            .send(Emitted {
                alias: alias.to_string(),
                machine: machine.to_string(),
                event,
                usage: ctx.usage,
            })
            .is_ok();
        tracker.insert(
            key.clone(),
            Track {
                label: cur_label.clone(),
                fingerprint: Some(fp),
                mode: ctx.mode.clone(),
            },
        );
    }
    tracker
        .entry(key)
        .and_modify(|t| {
            t.label = cur_label.clone();
            t.mode = ctx.mode.clone();
        })
        .or_insert(Track {
            label: cur_label,
            fingerprint: None,
            mode: ctx.mode.clone(),
        });
    emitted
}

fn event_detail(e: &RelayEvent) -> String {
    match &e.kind {
        EventKind::SessionStarted { entrypoint } => format!("session started ({entrypoint})"),
        EventKind::QuestionAsked { question } => {
            let q = question
                .questions
                .first()
                .map(|q| q.question.clone())
                .unwrap_or_default();
            format!("QUESTION: {}", relay_core::state::truncate(&q, 160))
        }
        EventKind::AwaitingPermission {
            tool,
            target,
            suspected,
        } => format!(
            "PERMISSION{}: {} {}",
            if *suspected { "?" } else { "" },
            tool,
            target.as_deref().unwrap_or("")
        ),
        EventKind::TurnComplete {
            last_message_excerpt,
            duration_ms,
        } => {
            let d = duration_ms
                .map(|m| format!(" ({}s)", m / 1000))
                .unwrap_or_default();
            format!(
                "DONE{}: {}",
                d,
                relay_core::state::truncate(last_message_excerpt, 280)
            )
        }
        EventKind::Error { message } => format!("ERROR: {message}"),
        EventKind::StateChanged { from, to } => format!("{from} -> {to}"),
        EventKind::ModeChanged { from, to, .. } => {
            let note = match to.as_str() {
                "bypassPermissions" => {
                    " - permissions are bypassed; the agent will not ask before running tools"
                }
                "acceptEdits" => " - edits may be auto-accepted without asking",
                "never" => " - Codex runs unattended; it will not ask before running tools",
                _ => "",
            };
            format!("MODE: {from} -> {to}{note}")
        }
        EventKind::SubagentActivity { count } => format!("subagents: {count}"),
        EventKind::AutoAction { action, detail } if action == "robot_verified_final" => {
            format!("ROBOT VERIFIED FINAL: {detail}")
        }
        EventKind::AutoAction { action, detail } if action == "robot_claimed_complete" => {
            format!("ROBOT CLAIMED COMPLETE: {detail}")
        }
        EventKind::AutoAction { action, detail } if action == "robot_safe_partial" => {
            format!("ROBOT SAFE PARTIAL: {detail}")
        }
        EventKind::AutoAction { action, detail } if action == "robot_final" => {
            format!("ROBOT FINAL (legacy): {detail}")
        }
        EventKind::AutoAction { action, detail } => format!("AUTO {action}: {detail}"),
        EventKind::SessionEnded => "session ended".to_string(),
    }
}

fn print_event(alias: &str, e: &RelayEvent, json: bool) {
    let ts = e.at.format("%H:%M:%S");
    let branch = e.git_branch.as_deref().unwrap_or("-");
    let name = e.title.as_deref().unwrap_or("-");
    println!(
        "[{ts}] {machine} {alias} ({branch}) {agent}/{name}: {detail}",
        machine = e.machine_id,
        agent = e.agent,
        detail = event_detail(e),
    );
    if json {
        if let Ok(s) = serde_json::to_string(e) {
            println!("  {s}");
        }
    }
}

#[derive(serde::Serialize)]
struct SessionCard {
    alias: String,
    agent: String,
    session_id: String,
    title: Option<String>,
    workspace: String,
    branch: Option<String>,
    state: String,
    mode: String,
    tapped: bool,
    rewrite: bool,
    last_activity_secs: Option<u64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PendingGuiSend {
    version: u8,
    submitted_at: i64,
    input_bytes: usize,
    notified_at: Option<i64>,
}

fn pending_gui_send_path(session_id: &str) -> PathBuf {
    let digest = blake3::hash(session_id.as_bytes()).to_hex();
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("pending-sends")
        .join(format!("{digest}.json"))
}

fn record_pending_gui_send(session_id: &str, input_bytes: usize) -> anyhow::Result<()> {
    let receipt = PendingGuiSend {
        version: 1,
        submitted_at: automation::now_secs(),
        input_bytes,
        notified_at: None,
    };
    let bytes = serde_json::to_vec(&receipt)?;
    Ok(fsutil::secure_write(
        &pending_gui_send_path(session_id),
        &bytes,
    )?)
}

fn pending_gui_completion_ready(session_id: &str, transcript: &std::path::Path) -> bool {
    let path = pending_gui_send_path(session_id);
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<PendingGuiSend>(&bytes) else {
        return false;
    };
    if receipt.version != 1 || receipt.notified_at.is_some() {
        return false;
    }
    std::fs::metadata(transcript)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|modified| modified.as_secs() as i64 >= receipt.submitted_at)
}

fn mark_gui_completion_notified(session_id: &str) -> anyhow::Result<()> {
    let path = pending_gui_send_path(session_id);
    let mut receipt: PendingGuiSend = serde_json::from_slice(&std::fs::read(&path)?)?;
    receipt.notified_at = Some(automation::now_secs());
    Ok(fsutil::secure_write(&path, &serde_json::to_vec(&receipt)?)?)
}

fn card_supersedes(candidate: &SessionCard, current: &SessionCard) -> bool {
    match (candidate.tapped, current.tapped) {
        (true, false) => return true,
        (false, true) => return false,
        _ => {}
    }
    candidate.last_activity_secs.unwrap_or(0) > current.last_activity_secs.unwrap_or(0)
}

fn dedup_session_cards(cards: Vec<SessionCard>) -> Vec<SessionCard> {
    use std::collections::hash_map::Entry;
    let mut order: Vec<String> = Vec::new();
    let mut best: HashMap<String, SessionCard> = HashMap::new();
    for card in cards {
        match best.entry(card.session_id.clone()) {
            Entry::Occupied(mut slot) => {
                if card_supersedes(&card, slot.get()) {
                    slot.insert(card);
                }
            }
            Entry::Vacant(slot) => {
                order.push(card.session_id.clone());
                slot.insert(card);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| best.remove(&id))
        .collect()
}

async fn sessions_json(registry: &Registry) -> String {
    let cfg = automation::AutomationConfig::load();
    let now = automation::now_secs();
    let mut cards: Vec<SessionCard> = Vec::new();
    let reg = registry.read().await;
    for w in reg.iter() {
        let alias = workspace_alias(&w.workspace);
        let workspace = w.workspace.to_string_lossy().to_string();
        for c in &w.claude {
            let mode = cfg.resolve(Some(&c.session_id), &alias, now);
            cards.push(SessionCard {
                alias: alias.clone(),
                agent: "claude".to_string(),
                session_id: c.session_id.clone(),
                title: c
                    .title
                    .as_deref()
                    .or(c.name.as_deref())
                    .map(|title| relay_core::state::truncate(title, 160)),
                workspace: workspace.clone(),
                branch: w.git_branch.clone(),
                state: c.state.label().to_string(),
                mode: mode.label().to_string(),
                tapped: inject::available_pid(&c.session_id).is_some(),
                rewrite: cfg.should_rewrite(mode),
                last_activity_secs: c.last_activity_secs,
            });
        }
        for a in &w.codex {
            let mode = cfg.resolve(Some(&a.thread_id), &alias, now);
            cards.push(SessionCard {
                alias: alias.clone(),
                agent: "codex".to_string(),
                session_id: a.thread_id.clone(),
                title: Some(relay_core::state::truncate(&a.title, 160)),
                workspace: workspace.clone(),
                branch: w.git_branch.clone(),
                state: a.state.label().to_string(),
                mode: mode.label().to_string(),
                tapped: false,
                rewrite: cfg.should_rewrite(mode),
                last_activity_secs: a.last_activity_secs,
            });
        }
    }
    serde_json::to_string_pretty(&dedup_session_cards(cards)).unwrap_or_else(|_| "[]".to_string())
}

fn sessions_snapshot_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("sessions.json")
}

fn fresh_sessions_snapshot() -> Option<String> {
    let path = sessions_snapshot_path();
    let age = std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()?;
    if age > Duration::from_secs(45) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok()?;
    Some(text)
}

async fn persist_sessions_snapshot(registry: &Registry) {
    let text = sessions_json(registry).await;
    if let Err(error) = fsutil::secure_write(&sessions_snapshot_path(), text.as_bytes()) {
        warn!(
            target: "relay::trace",
            pipeline = "sessions",
            stage = "snapshot_failed",
            "session snapshot write failed: {error:#}"
        );
    }
}

async fn print_sessions_json(registry: &Registry) {
    println!("{}", sessions_json(registry).await);
}

fn chunk_text(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in s.split_inclusive('\n') {
        if !cur.is_empty() && cur.chars().count() + line.chars().count() > max {
            out.push(std::mem::take(&mut cur));
        }
        if line.chars().count() > max {
            for ch in line.chars() {
                cur.push(ch);
                if cur.chars().count() >= max {
                    out.push(std::mem::take(&mut cur));
                }
            }
        } else {
            cur.push_str(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn format_event(machine: &str, alias: &str, e: &RelayEvent) -> (String, Option<serde_json::Value>) {
    let icon = match &e.kind {
        EventKind::QuestionAsked { .. } => "🟡",
        EventKind::AwaitingPermission { .. } => "🔴",
        EventKind::Error { .. } => "⚠️",
        EventKind::TurnComplete { .. } => "🟢",
        EventKind::SessionStarted { .. } => "•",
        EventKind::ModeChanged { alert: true, .. } => "🚨",
        EventKind::ModeChanged { alert: false, .. } => "🔀",
        EventKind::AutoAction { action, .. } if action == "robot_verified_final" => "✅",
        EventKind::AutoAction { action, .. } if action == "robot_claimed_complete" => "🟣",
        EventKind::AutoAction { action, .. } if action == "robot_safe_partial" => "🟠",
        EventKind::AutoAction { action, .. } if action == "robot_final" => "▫️",
        EventKind::AutoAction { .. } => "🤖",
        _ => "▫️",
    };
    let branch = e.git_branch.as_deref().unwrap_or("-");
    let name = relay_core::state::truncate(e.title.as_deref().unwrap_or("-"), 160);
    let mut text = format!(
        "{icon} <b>{}</b> · {}\n📁 <code>{}</code> ⌥ {}\n{} <b>{}</b>\n{}",
        e.agent,
        esc_html(machine),
        esc_html(alias),
        esc_html(branch),
        agent_glyph(e.agent),
        esc_html(&name),
        esc_html(&event_detail(e)),
    );

    if let EventKind::QuestionAsked { question } = &e.kind {
        for q in &question.questions {
            text.push_str("\n\n");
            if !q.header.trim().is_empty() {
                text.push_str(&format!("<b>{}</b>\n", esc_html(&q.header)));
            }
            text.push_str(&esc_html(&q.question));
            if q.multi_select {
                text.push_str(" <i>(multiple allowed)</i>");
            }
            for (i, o) in q.options.iter().enumerate() {
                text.push_str(&format!("\n  {}. <b>{}</b>", i + 1, esc_html(&o.label)));
                if !o.description.trim().is_empty() {
                    text.push_str(&format!(
                        " - {}",
                        esc_html(&relay_core::state::truncate(&o.description, 110))
                    ));
                }
            }
        }
    }

    let agent = e.agent.as_str();
    let kb = match &e.kind {
        EventKind::QuestionAsked { .. } => Some(keyboard(vec![vec![(
            "👁 Open in VS Code to answer".to_string(),
            format!("act:focus:{alias}"),
        )]])),
        EventKind::AwaitingPermission { .. } => Some(keyboard(vec![
            vec![
                ("✅ Yes / Approve".to_string(), format!("act:ok:{alias}")),
                (
                    "⛔ No / Deny".to_string(),
                    format!("act:stop:{alias}:{agent}"),
                ),
            ],
            vec![("👁 Focus".to_string(), format!("act:focus:{alias}"))],
        ])),
        EventKind::TurnComplete { .. } | EventKind::Error { .. } => {
            let mut rows: Vec<Vec<(String, String)>> = Vec::new();
            let full = match &e.kind {
                EventKind::TurnComplete {
                    last_message_excerpt,
                    ..
                } => last_message_excerpt.clone(),
                EventKind::Error { message } => message.clone(),
                _ => String::new(),
            };
            if full.chars().count() > 280 {
                let id = actions::put(full);
                rows.push(vec![("📄 Show full text".to_string(), format!("ft:{id}"))]);
            }
            let mut row = Vec::new();
            if e.agent == AgentKind::ClaudeCode {
                row.push((
                    "✍️ Send".to_string(),
                    format!("act:sayid:{alias}:{}", e.session_ref),
                ));
            } else {
                row.push((
                    "▶️ Continue".to_string(),
                    format!("act:cont:{alias}:{agent}"),
                ));
            }
            row.push(("👁 Focus".to_string(), format!("act:focus:{alias}")));
            rows.push(row);
            Some(keyboard(rows))
        }
        EventKind::ModeChanged { .. } => Some(keyboard(vec![vec![(
            "👁 Focus".to_string(),
            format!("act:focus:{alias}"),
        )]])),
        _ => None,
    };
    (text, kb)
}

fn agent_glyph(a: AgentKind) -> &'static str {
    match a {
        AgentKind::ClaudeCode => "🤖",
        AgentKind::Codex => "🧠",
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_commands(
    tg: Arc<Telegram>,
    registry: Registry,
    active: Active,
    ctl: Arc<dyn Control>,
    auth: Arc<auth::Auth>,
    pending: ingress::Pending,
    questions: question::Questions,
    perms: permission::Permissions,
    media_pending: PendingMedia,
    media_limits: MediaLimits,
) {
    let mut offset: i64 = 0;
    loop {
        match tg.get_updates(offset, 30).await {
            Ok(updates) => {
                for u in updates {
                    offset = u.update_id + 1;
                    let tg = tg.clone();
                    let registry = registry.clone();
                    let active = active.clone();
                    let ctl = ctl.clone();
                    let auth = auth.clone();
                    let pending = pending.clone();
                    let questions = questions.clone();
                    let perms = perms.clone();
                    let media_pending = media_pending.clone();
                    tokio::spawn(async move {
                        if let Some(m) = u.message {
                            handle_message(
                                &tg,
                                &registry,
                                &active,
                                &ctl,
                                &auth,
                                &media_pending,
                                media_limits,
                                m,
                            )
                            .await;
                        }
                        if let Some(cq) = u.callback_query {
                            handle_callback(
                                &tg,
                                &registry,
                                &active,
                                &ctl,
                                &auth,
                                &pending,
                                &questions,
                                &perms,
                                &media_pending,
                                cq,
                            )
                            .await;
                        }
                    });
                }
            }
            Err(e) => {
                warn!("getUpdates: {e:#}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

fn parse_agent(s: &str) -> Option<AgentKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(AgentKind::ClaudeCode),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    tg: &Telegram,
    registry: &Registry,
    active: &Active,
    ctl: &Arc<dyn Control>,
    auth: &Arc<auth::Auth>,
    media_pending: &PendingMedia,
    media_limits: MediaLimits,
    m: telegram::Message,
) {
    let chat = m.chat.id;
    let text = m.text.clone().unwrap_or_default();
    let toks: Vec<&str> = text.split_whitespace().collect();
    let cmd = toks.first().copied().unwrap_or("");

    if cmd == "/auth" {
        let key = toks.get(1).copied().unwrap_or("");
        let reply = match auth.try_pair(chat, key).await {
            auth::PairResult::Paired => {
                format!(
                    "✅ paired. this chat can now control the relay.\nchat id: <code>{chat}</code>"
                )
            }
            auth::PairResult::AlreadyAuthorized => "already authorized".to_string(),
            auth::PairResult::Wrong => "❌ wrong key".to_string(),
            auth::PairResult::Throttled(secs) => {
                format!("⛔ too many attempts. Try again in {} min.", secs / 60 + 1)
            }
            auth::PairResult::NoSecret => {
                "pairing is disabled (no RELAY_PAIR_SECRET set)".to_string()
            }
        };
        let _ = tg.send(chat, &reply, None).await;
        return;
    }

    if !auth.is_authorized(chat).await {
        let reply = if auth.has_secret() {
            format!("🔒 not authorized.\nSend <code>/auth &lt;key&gt;</code> to pair.\nyour chat id: <code>{chat}</code>")
        } else {
            format!("🔒 relay has no pairing secret. Ask the admin to set <code>RELAY_PAIR_SECRET</code> or add your chat id to <code>TELEGRAM_ALLOWED_CHATS</code>.\nyour chat id: <code>{chat}</code>")
        };
        let _ = tg.send(chat, &reply, None).await;
        return;
    }

    if m.has_media() {
        handle_media(tg, registry, active, media_pending, media_limits, &m).await;
        return;
    }

    if cmd == "/danger" {
        let sub = toks.get(1).copied().unwrap_or("");
        let arg = toks.get(2..).map(|s| s.join(" ")).unwrap_or_default();
        let _ = tg.send(chat, &danger_command(sub, arg.trim()), None).await;
        return;
    }

    if let Some(src) = m.reply_to_message.as_deref() {
        if let Some(t) = src.text.as_deref().and_then(parse_send_target) {
            let reply = match t.sid {
                Some(sid) => say_text_sid(ctl, &t.alias, &sid, &text).await,
                None => say_text(ctl, registry, &t.alias, t.agent, t.idx, t.sidebar, &text).await,
            };
            let _ = tg.send(chat, &reply, None).await;
            return;
        }
    }

    if !text.is_empty() && !text.starts_with('/') && m.reply_to_message.is_none() {
        let target = active.read().await.get(&chat).cloned();
        if let Some((alias, sid)) = target {
            let reply = say_text_sid(ctl, &alias, &sid, &text).await;
            let _ = tg.send(chat, &reply, None).await;
            return;
        }
    }

    match cmd {
        "/start" | "/menu" | "/windows" | "/win" => {
            let (text, kb) = home_view(registry).await;
            let _ = tg.send(chat, &text, Some(kb)).await;
            return;
        }
        "/auto" => {
            match toks.get(1).copied() {
                Some("providers") => {
                    let (text, kb) = automation_providers_view().await;
                    let _ = tg.send(chat, &text, Some(kb)).await;
                }
                Some("model") => {
                    let reply = match (toks.get(2), toks.get(3)) {
                        (Some(id), Some(model))
                            if supervisor::discover::Backend::parse(id).is_some() =>
                        {
                            let mut cfg = automation::AutomationConfig::load();
                            cfg.robot.providers.set_model(id, model);
                            let _ = cfg.save();
                            format!("✅ {id} model = {model}")
                        }
                        _ => "usage: /auto model &lt;provider&gt; &lt;model&gt;".to_string(),
                    };
                    let _ = tg.send(chat, &reply, None).await;
                }
                other => {
                    if let Some(m) = other.and_then(automation::Mode::parse) {
                        let mut cfg = automation::AutomationConfig::load();
                        cfg.set(automation::Scope::Default, m, None);
                        let _ = cfg.save();
                    }
                    let (text, kb) = automation_home_view().await;
                    let _ = tg.send(chat, &text, Some(kb)).await;
                }
            }
            return;
        }
        "/help" => {
            let _ = tg.send(chat, &help_text(), None).await;
            return;
        }
        _ => {}
    }

    let reply = match cmd {
        "/status" => {
            if let Some(alias) = toks.get(1) {
                format_status(registry, alias).await
            } else {
                "usage: /status &lt;workspace&gt;".to_string()
            }
        }
        "/focus" => run_ctl(ctl, "focus", toks.clone()).await,
        "/say" => run_ctl(ctl, "say", toks.clone()).await,
        "/stop" => run_ctl(ctl, "stop", toks.clone()).await,
        "/cont" => run_ctl(ctl, "cont", toks.clone()).await,
        "/mode" => run_ctl(ctl, "mode", toks.clone()).await,
        "/slash" => run_ctl(ctl, "slash", toks.clone()).await,
        _ => return,
    };
    let _ = tg.send(chat, &reply, None).await;
}

fn parse_send_args(args: &[String]) -> (String, Vec<String>) {
    let mut media = Vec::new();
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--media" {
            if let Some(p) = it.next() {
                media.push(p.clone());
            }
        } else if let Some(p) = a.strip_prefix("--media=") {
            media.push(p.to_string());
        } else {
            words.push(a.clone());
        }
    }
    (words.join(" "), media)
}

fn cli_media_preamble(paths: &[String]) -> String {
    let n = paths.len();
    let noun = if n == 1 { "file" } else { "files" };
    let mut out = format!("[Attached {n} {noun}]\n");
    for (i, p) in paths.iter().enumerate() {
        let abs = std::fs::canonicalize(p)
            .map(|c| c.display().to_string())
            .unwrap_or_else(|_| p.clone());
        out.push_str(&format!("{}. {abs}\n", i + 1));
    }
    out.push_str(
        "Inspect the attached file(s) with your own tools (open images, read documents, \
         transcribe audio/video if you can) and act on them.",
    );
    out
}

async fn run_send(
    alias: &str,
    session_id: &str,
    text: &str,
    media: &[String],
) -> anyhow::Result<()> {
    if session_id.is_empty() || (text.trim().is_empty() && media.is_empty()) {
        anyhow::bail!("usage: vsc-relay-agent send <alias> <session_id> [--media PATH]... <text>");
    }
    let Some(pid) = inject::available_pid(session_id) else {
        anyhow::bail!("session not tapped (no background channel)");
    };
    let send = if text.trim().is_empty() {
        String::new()
    } else {
        rewrite_for_send(alias, session_id, text)
            .await
            .unwrap_or_else(|| text.to_string())
    };
    let rewrote = !text.trim().is_empty() && send != text;
    let delivered = if media.is_empty() {
        send.clone()
    } else {
        let preamble = cli_media_preamble(media);
        if send.trim().is_empty() {
            preamble
        } else {
            format!("{send}\n\n{preamble}")
        }
    };
    let original_bytes = text.len();
    let delivered_bytes = delivered.len();
    let text2 = delivered.clone();
    tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2)).await??;
    if let Err(error) = record_pending_gui_send(session_id, original_bytes) {
        println!("completion receipt persistence failed: {error:#}");
    }
    if !media.is_empty() {
        println!("sent with {} attachment(s)", media.len());
    } else if rewrote {
        println!("improved & sent");
        println!("{send}");
    } else {
        println!("sent");
    }
    match notify_gui_send_receipt(alias, session_id, rewrote, original_bytes, delivered_bytes).await {
        Ok(0) => println!(
            "telegram receipt skipped (no configured bot or authorized recipients); input={original_bytes}B delivered={delivered_bytes}B"
        ),
        Ok(count) => println!(
            "telegram receipt sent to {count} chat(s); input={original_bytes}B delivered={delivered_bytes}B"
        ),
        Err(error) => println!(
            "telegram receipt failed (prompt was delivered): {error:#}; input={original_bytes}B delivered={delivered_bytes}B"
        ),
    }
    println!(
        "{}",
        serde_json::json!({
            "protocol": "vsc-relay.send-result.v1",
            "status": "accepted",
            "alias": alias,
            "session_id": session_id,
            "rewrote": rewrote,
            "input_bytes": original_bytes,
            "delivered_bytes": delivered_bytes,
            "attachments": media.len(),
            "delivered_text": (rewrote || !media.is_empty()).then_some(delivered),
        })
    );
    Ok(())
}

async fn notify_gui_send_receipt(
    alias: &str,
    session_id: &str,
    rewrote: bool,
    original_bytes: usize,
    delivered_bytes: usize,
) -> anyhow::Result<usize> {
    let cfg = Config::from_env();
    let Some(token) = cfg.telegram_token.clone() else {
        return Ok(0);
    };
    let auth = auth::Auth::load(cfg.allowed_chats.clone(), cfg.pair_secret.clone());
    let recipients = auth.recipients().await;
    if recipients.is_empty() {
        return Ok(0);
    }
    let tg = Telegram::new(token)?;
    let session = relay_core::state::truncate(session_id, 8);
    let rewrite = if rewrote { " · improved" } else { "" };
    let body = format!(
        "✉️ <b>Prompt accepted{rewrite}</b> · {}\n📁 <code>{}</code> · <code>{}</code>\n{} B → {} B\n<i>Claude turn completion arrives as 🟢 DONE. Robot then reports 🟣 CLAIMED, 🟠 SAFE PARTIAL, or evidence-backed ✅ VERIFIED FINAL.</i>",
        esc_html(&cfg.machine_name),
        esc_html(alias),
        esc_html(&session),
        original_bytes,
        delivered_bytes,
    );
    let mut sent = 0usize;
    let mut last_error = None;
    for chat in recipients {
        match tg.send(chat, &body, None).await {
            Ok(_) => sent += 1,
            Err(error) => last_error = Some(error),
        }
    }
    if sent == 0 {
        if let Some(error) = last_error {
            return Err(error);
        }
    }
    Ok(sent)
}

fn find_claude_jsonl(session_id: &str) -> Option<std::path::PathBuf> {
    let root = dirs::home_dir()?.join(".claude").join("projects");
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let p = e.path().join(format!("{session_id}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn session_tail_context(session_id: &str) -> String {
    let Some(path) = find_claude_jsonl(session_id) else {
        return String::new();
    };
    let tail = relay_adapters::claude::tail_messages(&path, 6);
    if tail.is_empty() {
        return String::new();
    }
    let mut out = String::from("Recent conversation:\n");
    for (who, msg) in &tail {
        let role = if *who == 'U' { "USER" } else { "AGENT" };
        out.push_str(&format!(
            "{role}: {}\n",
            relay_core::state::truncate(msg.trim(), 300)
        ));
    }
    relay_core::state::truncate(&out, 3000)
}

async fn rewrite_for_send(alias: &str, session_id: &str, original: &str) -> Option<String> {
    let cfg = automation::AutomationConfig::load();
    let mode = cfg.resolve(Some(session_id), alias, automation::now_secs());
    if !cfg.should_rewrite(mode) || cfg.robot.providers.enabled.is_empty() {
        return None;
    }
    let context = session_tail_context(session_id);
    let discovered = supervisor::discover::discover_all().await;
    let pool = supervisor::pool::Pool::new();
    match pool
        .improve(
            &cfg.robot.providers,
            &discovered,
            supervisor::IMPROVE_SYSTEM,
            &supervisor::improve_user_prompt(&context, original),
            automation::now_secs(),
        )
        .await
    {
        Ok((_, text)) if text.trim() != original.trim() && !text.trim().is_empty() => {
            if cfg.smart.enabled {
                let guarded = compass::guard_rewrite(session_id, original, &text);
                if !guarded.accepted {
                    tracing::info!(
                        target: "relay::trace", pipeline = "compass", stage = "drift_reject",
                        reason = %guarded.reason, risk = guarded.risk, alias = %alias,
                        "rewrite drifts off goal; sending original"
                    );
                    return None;
                }
                return (guarded.selected.trim() != original.trim()).then_some(guarded.selected);
            }
            Some(text)
        }
        _ => None,
    }
}

async fn say_text(
    ctl: &Arc<dyn Control>,
    registry: &Registry,
    alias: &str,
    agent: AgentKind,
    idx: Option<usize>,
    sidebar: bool,
    text: &str,
) -> String {
    let session = if agent == AgentKind::ClaudeCode {
        match idx {
            Some(i) => registry
                .read()
                .await
                .iter()
                .find(|w| workspace_alias(&w.workspace) == alias)
                .and_then(|w| w.claude.get(i))
                .map(|c| c.session_id.clone()),
            None => None,
        }
    } else {
        None
    };

    if agent == AgentKind::ClaudeCode {
        if let Some(sid) = session.clone() {
            if let Some(pid) = inject::available_pid(&sid) {
                let send = rewrite_for_send(alias, &sid, text)
                    .await
                    .unwrap_or_else(|| text.to_string());
                let rewrote = send != text;
                let text2 = send.clone();
                let res =
                    tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2))
                        .await;
                return match res {
                    Ok(Ok(())) if rewrote => {
                        format!("✏️ improved & sent (background → VS Code):\n{send}")
                    }
                    Ok(Ok(())) => "✉️ sent (background → shows in VS Code too)".to_string(),
                    Ok(Err(e)) => format!("❌ inject: {e}"),
                    Err(e) => format!("❌ task: {e}"),
                };
            }
        }
    }

    let _ = (ctl, sidebar, session, text);
    if agent == AgentKind::Codex {
        return "❌ Background send to Codex is not available yet.".to_string();
    }
    "❌ No background channel for this chat. Background send needs the shim, and only chats \
     opened after the shim was installed have it. Open a new Claude chat."
        .to_string()
}

async fn handle_media(
    tg: &Telegram,
    registry: &Registry,
    active: &Active,
    media_pending: &PendingMedia,
    limits: MediaLimits,
    m: &telegram::Message,
) {
    let chat = m.chat.id;
    let refs = m.media_refs();
    if refs.is_empty() {
        return;
    }

    let mut staged: Vec<media::StagedFile> = Vec::new();
    for (i, r) in refs.iter().enumerate() {
        match download_and_stage(tg, chat, m.message_id, i, r, limits.max_download_bytes).await {
            Ok(file) => {
                let mut probe = file.clone();
                let enriched = tokio::task::spawn_blocking(move || {
                    media::enrich(&mut probe);
                    probe
                })
                .await;
                staged.push(enriched.unwrap_or(file));
            }
            Err(e) => warn!("telegram media stage failed: {e:#}"),
        }
    }

    if staged.is_empty() {
        let _ = tg
            .send(chat, "❌ could not fetch the attached media", None)
            .await;
        return;
    }

    media::sweep(limits.ttl_ms, limits.store_max_bytes);

    let count = staged.len();
    let total_bytes: u64 = staged.iter().map(|f| f.bytes).sum();

    let caption = m
        .caption
        .clone()
        .or_else(|| m.text.clone())
        .unwrap_or_default();
    let forward = m.forward_label();
    let preamble = build_media_preamble(&staged, forward.as_deref());

    if let Some(src) = m.reply_to_message.as_deref() {
        if let Some(t) = src.text.as_deref().and_then(parse_send_target) {
            if let Some(sid) = resolve_target_sid(registry, &t).await {
                emit_media_event(&t.alias, count, total_bytes);
                let reply = say_media_sid(&t.alias, &sid, count, &caption, &preamble).await;
                active.write().await.insert(chat, (t.alias.clone(), sid));
                let _ = tg.send(chat, &reply, None).await;
                return;
            }
        }
    }

    let target = active.read().await.get(&chat).cloned();
    if let Some((alias, sid)) = target {
        emit_media_event(&alias, count, total_bytes);
        let reply = say_media_sid(&alias, &sid, count, &caption, &preamble).await;
        let _ = tg.send(chat, &reply, None).await;
        return;
    }

    let options = claude_session_options(registry).await;
    if options.is_empty() {
        let _ = tg
            .send(
                chat,
                "📎 media received, but no live Claude session is available to route to. Open a chat, then send again.",
                None,
            )
            .await;
        return;
    }

    let token = MEDIA_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let kb = media_picker_keyboard(token, &options);
    media_pending.write().await.insert(
        token,
        PendingMediaBatch {
            files: staged,
            caption,
            forward,
            options,
        },
    );
    let noun = if count == 1 {
        "attachment"
    } else {
        "attachments"
    };
    let msg = format!("📎 {count} {noun} received. Where should they go?");
    let _ = tg.send(chat, &msg, Some(kb)).await;
}

async fn download_and_stage(
    tg: &Telegram,
    chat: i64,
    message_id: i64,
    index: usize,
    r: &telegram::MediaRef,
    max_bytes: u64,
) -> anyhow::Result<media::StagedFile> {
    let file_path = tg.get_file(&r.file_id).await?;
    let bytes = tg.download_file(&file_path, max_bytes).await?;
    let staged = media::stage(
        chat,
        message_id,
        index,
        r.kind,
        r.file_name.as_deref(),
        &bytes,
    )?;
    Ok(staged)
}

async fn resolve_target_sid(registry: &Registry, t: &SendTarget) -> Option<String> {
    if let Some(sid) = &t.sid {
        return Some(sid.clone());
    }
    if t.agent == AgentKind::ClaudeCode {
        if let Some(i) = t.idx {
            return registry
                .read()
                .await
                .iter()
                .find(|w| workspace_alias(&w.workspace) == t.alias)
                .and_then(|w| w.claude.get(i))
                .map(|c| c.session_id.clone());
        }
    }
    None
}

async fn claude_session_options(registry: &Registry) -> Vec<(String, String)> {
    let ws = registry.read().await;
    let mut out = Vec::new();
    for w in ws.iter() {
        let alias = workspace_alias(&w.workspace);
        for c in &w.claude {
            if inject::available_pid(&c.session_id).is_some() {
                out.push((alias.clone(), c.session_id.clone()));
            }
        }
    }
    out
}

fn media_picker_keyboard(token: u64, options: &[(String, String)]) -> serde_json::Value {
    let rows: Vec<Vec<(String, String)>> = options
        .iter()
        .enumerate()
        .map(|(i, (alias, sid))| {
            let short = sid.get(0..6).unwrap_or(sid.as_str());
            vec![(
                format!("🤖 {alias} ({short})"),
                format!("act:media:{token}:{i}"),
            )]
        })
        .collect();
    keyboard(rows)
}

fn emit_media_event(alias: &str, count: usize, bytes: u64) {
    info!(
        target: "relay::trace",
        pipeline = "event",
        stage = "emitted",
        kind = "media_received",
        alias = %alias,
        count,
        bytes,
        "media received"
    );
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn build_media_preamble(files: &[media::StagedFile], forward: Option<&str>) -> String {
    let n = files.len();
    let noun = if n == 1 { "file" } else { "files" };
    let mut out = format!("[Attached from Telegram: {n} {noun}]\n");
    for (i, f) in files.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} — {}",
            i + 1,
            f.kind.as_str(),
            f.path.display()
        ));
        if let Some(name) = &f.original_name {
            out.push_str(&format!(" (original name: {name})"));
        }
        out.push_str(&format!(" [{}]", human_size(f.bytes)));
        out.push('\n');
        if let Some(sidecar) = &f.sidecar {
            out.push_str(&format!(
                "   preview/transcript file: {}\n",
                sidecar.display()
            ));
        }
        if let Some(t) = &f.transcript {
            let snippet = relay_core::state::truncate(t.trim(), 500);
            out.push_str(&format!("   transcript: {snippet}\n"));
        }
    }
    if let Some(src) = forward {
        out.push_str(&format!("Forwarded from: {src}\n"));
    }
    out.push_str(
        "Inspect the attached file(s) with your own tools (open images, read documents, \
         transcribe audio/video if you can) and act on them.",
    );
    out
}

async fn say_media_sid(
    alias: &str,
    sid: &str,
    count: usize,
    caption: &str,
    preamble: &str,
) -> String {
    let Some(pid) = inject::available_pid(sid) else {
        return "❌ No background channel for this chat. Open a new Claude chat.".to_string();
    };
    let caption_final = if caption.trim().is_empty() {
        String::new()
    } else {
        rewrite_for_send(alias, sid, caption)
            .await
            .unwrap_or_else(|| caption.to_string())
    };
    let text = if caption_final.trim().is_empty() {
        preamble.to_string()
    } else {
        format!("{caption_final}\n\n{preamble}")
    };
    let text2 = text.clone();
    let res = tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2)).await;
    match res {
        Ok(Ok(())) => {
            let noun = if count == 1 {
                "attachment"
            } else {
                "attachments"
            };
            format!("📎 delivered {count} {noun} to {alias} (background → VS Code)")
        }
        Ok(Err(e)) => format!("❌ inject: {e}"),
        Err(e) => format!("❌ task: {e}"),
    }
}

async fn media_pick(
    tg: &Telegram,
    active: &Active,
    media_pending: &PendingMedia,
    chat: Option<i64>,
    msg_id: Option<i64>,
    token: &str,
    idx: &str,
) -> String {
    let (Ok(token), Ok(idx)) = (token.parse::<u64>(), idx.parse::<usize>()) else {
        return "bad media selection".to_string();
    };
    let batch = media_pending.write().await.remove(&token);
    let Some(batch) = batch else {
        return "media selection expired".to_string();
    };
    let Some((alias, sid)) = batch.options.get(idx).cloned() else {
        return "unknown target".to_string();
    };
    let preamble = build_media_preamble(&batch.files, batch.forward.as_deref());
    let total_bytes: u64 = batch.files.iter().map(|f| f.bytes).sum();
    emit_media_event(&alias, batch.files.len(), total_bytes);
    let reply = say_media_sid(&alias, &sid, batch.files.len(), &batch.caption, &preamble).await;
    if let Some(c) = chat {
        active.write().await.insert(c, (alias, sid));
        if let Some(mid) = msg_id {
            let _ = tg.edit_message_text(c, mid, &reply, None).await;
        }
    }
    "📎 routed".to_string()
}

#[cfg(test)]
mod media_tests {
    use super::*;
    use std::path::PathBuf;

    fn sf(path: &str, kind: telegram::MediaKind, bytes: u64) -> media::StagedFile {
        media::StagedFile {
            path: PathBuf::from(path),
            kind,
            original_name: None,
            bytes,
            sidecar: None,
            transcript: None,
        }
    }

    #[test]
    fn preamble_lists_paths_size_forward_and_directive() {
        let files = vec![
            sf("/m/1-0-photo.jpg", telegram::MediaKind::Photo, 2_400_000),
            sf("/m/1-1-voice.ogg", telegram::MediaKind::Voice, 8_000),
        ];
        let p = build_media_preamble(&files, Some("Alex"));
        assert!(p.contains("/m/1-0-photo.jpg"));
        assert!(p.contains("/m/1-1-voice.ogg"));
        assert!(p.contains("2.3 MB"), "size hint missing: {p}");
        assert!(p.contains("Forwarded from: Alex"));
        assert!(p.to_lowercase().contains("inspect"));
    }

    #[test]
    fn preamble_includes_transcript_and_sidecar() {
        let mut f = sf("/m/2-0-voice.ogg", telegram::MediaKind::Voice, 5000);
        f.sidecar = Some(PathBuf::from("/m/2-0-voice.txt"));
        f.transcript = Some("hello world".to_string());
        let p = build_media_preamble(&[f], None);
        assert!(p.contains("/m/2-0-voice.txt"));
        assert!(p.contains("transcript: hello world"));
        assert!(!p.contains("Forwarded from"));
    }

    #[test]
    fn end_to_end_local_media_pipeline() {
        let root = std::env::temp_dir().join(format!("relay-media-e2e-{}", std::process::id()));
        let msg: telegram::Message = serde_json::from_value(serde_json::json!({
            "message_id": 55,
            "chat": {"id": 900},
            "caption": "check this layout",
            "photo": [
                {"file_id": "small", "width": 90, "height": 60, "file_size": 900},
                {"file_id": "big", "width": 1280, "height": 720, "file_size": 150000}
            ],
            "forward_origin": {"type": "user", "sender_user": {"id": 1, "first_name": "Sam"}}
        }))
        .unwrap();

        assert!(msg.has_media());
        let refs = msg.media_refs();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].file_id, "big");

        let mut staged = Vec::new();
        for (i, r) in refs.iter().enumerate() {
            let f = media::stage_in(
                &root,
                900,
                msg.message_id,
                i,
                r.kind,
                r.file_name.as_deref(),
                b"fake-downloaded-image-bytes",
            )
            .expect("stage");
            staged.push(f);
        }

        assert!(staged[0].path.exists());
        assert!(staged[0].path.starts_with(root.join("900")));

        let caption = msg.caption.clone().unwrap_or_default();
        let preamble = build_media_preamble(&staged, msg.forward_label().as_deref());
        assert!(preamble.contains(&staged[0].path.display().to_string()));
        assert!(preamble.contains("Forwarded from: Sam"));
        assert!(preamble.to_lowercase().contains("inspect"));
        assert_eq!(caption, "check this layout");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_send_args_splits_media_flags_from_text() {
        let args: Vec<String> = [
            "--media",
            "/a/b.png",
            "fix",
            "the",
            "--media=/c/d.pdf",
            "layout",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (text, media) = parse_send_args(&args);
        assert_eq!(text, "fix the layout");
        assert_eq!(media, vec!["/a/b.png".to_string(), "/c/d.pdf".to_string()]);
    }

    #[test]
    fn picker_keyboard_has_one_row_per_option() {
        let opts = vec![
            ("proj-a".to_string(), "abcdef-1".to_string()),
            ("proj-b".to_string(), "ghijkl-2".to_string()),
        ];
        let kb = media_picker_keyboard(7, &opts);
        let rows = kb["inline_keyboard"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0]["callback_data"], "act:media:7:0");
        assert_eq!(rows[1][0]["callback_data"], "act:media:7:1");
    }
}

fn danger_command(sub: &str, arg: &str) -> String {
    let file = hooks::danger_file();
    let mut list = hooks::danger_patterns();
    match sub {
        "" | "list" => {
            let src = if file.exists() {
                "danger.txt"
            } else {
                "defaults"
            };
            let body = list
                .iter()
                .map(|p| format!("• <code>{}</code>", esc_html(p)))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "🛡 <b>Blocked commands</b> ({src}) - stopped by the hook:\n{body}\n\n<i>/danger add rm -rf</i>\n<i>/danger del rm -rf</i>"
            )
        }
        "add" => {
            let p = arg.trim().to_lowercase();
            if p.is_empty() {
                return "usage: /danger add &lt;pattern&gt;".to_string();
            }
            if !list.iter().any(|x| x == &p) {
                list.push(p.clone());
            }
            match write_danger(&file, &list) {
                Ok(()) => format!("✅ added: <code>{}</code>", esc_html(&p)),
                Err(e) => format!("❌ {e}"),
            }
        }
        "del" | "rm" | "remove" => {
            let p = arg.trim().to_lowercase();
            let before = list.len();
            list.retain(|x| x != &p);
            if list.len() == before {
                return format!("not found: <code>{}</code>", esc_html(&p));
            }
            match write_danger(&file, &list) {
                Ok(()) => format!("🗑 removed: <code>{}</code>", esc_html(&p)),
                Err(e) => format!("❌ {e}"),
            }
        }
        _ => "usage: /danger [list | add &lt;p&gt; | del &lt;p&gt;]".to_string(),
    }
}

fn write_danger(file: &std::path::Path, list: &[String]) -> std::io::Result<()> {
    let body = format!(
        "# vsc-relay dangerous command patterns (one per line, case-insensitive substring)\n{}\n",
        list.join("\n")
    );
    fsutil::secure_write(file, body.as_bytes())
}

fn chat_bg_pid(w: &WindowEntry, idx: usize) -> Option<u32> {
    let sid = &w.claude.get(idx)?.session_id;
    inject::available_pid(sid)
}

fn parse_alias_idx(s: &str) -> Option<(String, usize)> {
    let pos = s.rfind(':')?;
    let idx = s[pos + 1..].parse::<usize>().ok()?;
    Some((s[..pos].to_string(), idx))
}

fn active_model(cwd: &std::path::Path, sid: &str) -> Option<(String, String)> {
    let home = dirs::home_dir()?;
    let path = home
        .join(".claude")
        .join("projects")
        .join(relay_discovery::encode_cwd(cwd))
        .join(format!("{sid}.jsonl"));
    let text = std::fs::read_to_string(path).ok()?;
    let mut found: Option<String> = None;
    for line in text.lines() {
        if let Some(pos) = line.find("\"model\":\"") {
            let rest = &line[pos + 9..];
            if let Some(end) = rest.find('"') {
                let m = &rest[..end];
                if m.starts_with("claude-") {
                    found = Some(m.to_string());
                }
            }
        }
    }
    found.map(|id| (model_tier(&id), model_friendly(&id)))
}

fn model_tier(id: &str) -> String {
    let s = id.strip_prefix("claude-").unwrap_or(id);
    for t in ["opus", "sonnet", "haiku", "fable"] {
        if s.starts_with(t) {
            return t.to_string();
        }
    }
    "opus".to_string()
}

fn model_friendly(id: &str) -> String {
    let s = id.strip_prefix("claude-").unwrap_or(id);
    let parts: Vec<&str> = s.split('-').collect();
    let family = parts.first().copied().unwrap_or("");
    let mut chars = family.chars();
    let cap = match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    let ver = parts
        .iter()
        .skip(1)
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(".");
    if ver.is_empty() {
        cap
    } else {
        format!("{cap} {ver}")
    }
}

fn setting_picker(
    kind: &str,
    alias: &str,
    idx: usize,
    active: Option<(String, String)>,
) -> (String, serde_json::Value) {
    let (title, act, opts): (&str, &str, Vec<(&str, &str)>) = match kind {
        "model" => (
            "🧠 Model",
            "setmodel",
            vec![
                ("Default", "default"),
                ("Opus", "opus"),
                ("Opus 1M", "opus[1m]"),
                ("Fable", "fable"),
                ("Sonnet", "sonnet"),
                ("Haiku", "haiku"),
            ],
        ),
        "effort" => (
            "⚡ Effort",
            "seteffort",
            vec![
                ("Low", "low"),
                ("Medium", "medium"),
                ("High", "high"),
                ("xHigh", "xhigh"),
            ],
        ),
        _ => (
            "🔀 Mode",
            "setmode",
            vec![
                ("Default", "default"),
                ("Accept Edits", "acceptEdits"),
                ("Plan", "plan"),
                ("Bypass", "bypassPermissions"),
            ],
        ),
    };
    let active_tier = active.as_ref().map(|(t, _)| t.clone());
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for chunk in opts.chunks(2) {
        rows.push(
            chunk
                .iter()
                .map(|(l, v)| {
                    let mark = if active_tier.as_deref() == Some(v) {
                        "✓ "
                    } else {
                        ""
                    };
                    (format!("{mark}{l}"), format!("act:{act}:{alias}:{idx}:{v}"))
                })
                .collect(),
        );
    }
    rows.push(vec![(
        "⬅️ Back".to_string(),
        format!("m:c:{alias}:claude:{idx}"),
    )]);
    let mut header = format!(
        "{title} - pick for chat #{idx} in <b>{}</b>",
        esc_html(alias)
    );
    if let Some((_, friendly)) = &active {
        header.push_str(&format!(
            "\nActive in VS Code: <b>{}</b>",
            esc_html(friendly)
        ));
    }
    (header, keyboard(rows))
}

async fn apply_setting(
    registry: &Registry,
    alias: &str,
    idx: &str,
    kind: &str,
    val: &str,
) -> String {
    let i: usize = match idx.parse() {
        Ok(v) => v,
        Err(_) => return "bad idx".to_string(),
    };
    let pid = {
        let ws = registry.read().await;
        ws.iter()
            .find(|w| workspace_alias(&w.workspace) == alias)
            .and_then(|w| chat_bg_pid(w, i))
    };
    match pid {
        Some(pid) => {
            let kind_s = kind.to_string();
            let val_s = val.to_string();
            let res = tokio::task::spawn_blocking(move || match kind_s.as_str() {
                "model" => inject::set_model(pid, &val_s),
                "effort" => inject::set_effort(pid, &val_s),
                "mode" => inject::set_mode(pid, &val_s),
                _ => Ok(()),
            })
            .await;
            match res {
                Ok(Ok(())) => format!("✅ {kind} = {val}"),
                Ok(Err(e)) => format!("❌ {e}"),
                Err(e) => format!("❌ {e}"),
            }
        }
        None => "❌ background channel unavailable for this chat (no shim)".to_string(),
    }
}

async fn say_text_sid(ctl: &Arc<dyn Control>, alias: &str, sid: &str, text: &str) -> String {
    if let Some(pid) = inject::available_pid(sid) {
        let send = rewrite_for_send(alias, sid, text)
            .await
            .unwrap_or_else(|| text.to_string());
        let rewrote = send != text;
        let text2 = send.clone();
        let res = tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2)).await;
        return match res {
            Ok(Ok(())) if rewrote => {
                format!("✏️ improved & sent (background → VS Code):\n{send}")
            }
            Ok(Ok(())) => "✉️ sent (background → shows in VS Code too)".to_string(),
            Ok(Err(e)) => format!("❌ inject: {e}"),
            Err(e) => format!("❌ task: {e}"),
        };
    }
    let send = rewrite_for_send(alias, sid, text)
        .await
        .unwrap_or_else(|| text.to_string());
    let ctl = ctl.clone();
    let alias_s = alias.to_string();
    let sid_s = sid.to_string();
    let typed = send.clone();
    let res =
        tokio::task::spawn_blocking(move || ctl.send_claude_session(&alias_s, &sid_s, &typed))
            .await;
    match res {
        Ok(Ok(())) => format!(
            "✉️ typed into the window (no shim on this chat, so it went through the keyboard):\n{send}"
        ),
        Ok(Err(e)) => format!(
            "❌ this chat has no background channel and the window could not be typed into: {e}"
        ),
        Err(e) => format!("❌ task: {e}"),
    }
}

fn parse_chat_spec(spec: &str) -> Option<(String, AgentKind, usize)> {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let idx: usize = parts[parts.len() - 1].parse().ok()?;
    let agent = parse_agent(parts[parts.len() - 2])?;
    let alias = parts[..parts.len() - 2].join(":");
    Some((alias, agent, idx))
}

struct SendTarget {
    alias: String,
    agent: AgentKind,
    idx: Option<usize>,
    sid: Option<String>,
    sidebar: bool,
}

fn parse_send_target(prompt: &str) -> Option<SendTarget> {
    let line = prompt.lines().find(|l| l.contains("→"))?;
    let rest = line.split('→').nth(1)?.trim();
    let parts: Vec<&str> = rest.split('/').map(|s| s.trim()).collect();
    let alias = parts.first()?.to_string();
    let agent = parse_agent(parts.get(1)?)?;
    let third = parts.get(2).copied().unwrap_or("");
    let sid = third.strip_prefix("sid:").map(|s| s.to_string());
    let idx = if sid.is_some() {
        None
    } else {
        third.parse::<usize>().ok()
    };
    let sidebar = parts.get(3).map(|s| *s == "sidebar").unwrap_or(false);
    Some(SendTarget {
        alias,
        agent,
        idx,
        sid,
        sidebar,
    })
}

async fn run_ctl(ctl: &Arc<dyn Control>, verb: &str, toks: Vec<&str>) -> String {
    let ctl = ctl.clone();
    let args: Vec<String> = toks.iter().skip(1).map(|s| s.to_string()).collect();
    let verb = verb.to_string();
    let res = tokio::task::spawn_blocking(move || dispatch_ctl(&ctl, &verb, &args)).await;
    match res {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => format!("❌ {e}"),
        Err(e) => format!("❌ task: {e}"),
    }
}

fn dispatch_ctl(ctl: &Arc<dyn Control>, verb: &str, args: &[String]) -> anyhow::Result<String> {
    let alias = required_arg(args, 0, "workspace is required")?;
    match verb {
        "focus" => {
            ctl.focus_window(alias)?;
            Ok(format!("👁 focused {alias}"))
        }
        "say" => {
            let agent = parse_agent(args.get(1).map(|s| s.as_str()).unwrap_or(""))
                .ok_or_else(|| anyhow::anyhow!("usage: /say <ws> <claude|codex> <text>"))?;
            let msg = args
                .get(2..)
                .map(|s| s.join(" "))
                .unwrap_or_default()
                .trim()
                .to_string();
            anyhow::ensure!(!msg.is_empty(), "empty message");
            ctl.send_prompt(alias, agent, &msg)?;
            Ok(format!("✉️ sent to {alias}/{agent}"))
        }
        "stop" => {
            let agent = parse_agent(args.get(1).map(|s| s.as_str()).unwrap_or(""))
                .ok_or_else(|| anyhow::anyhow!("usage: /stop <ws> <claude|codex>"))?;
            ctl.stop(alias, agent)?;
            Ok(format!("⏹ stopped {alias}/{agent}"))
        }
        "accept" => {
            ctl.accept(alias)?;
            Ok(format!("✅ accepted {alias}"))
        }
        "pick" => {
            let n = required_arg(args, 1, "usage: pick <ws> <option_index>")?;
            let idx: usize = n
                .parse()
                .map_err(|_| anyhow::anyhow!("bad option index {n}"))?;
            ctl.pick_option(alias, idx)?;
            Ok(format!("✅ picked option {} in {alias}", idx + 1))
        }
        "cont" => {
            let agent = parse_agent(args.get(1).map(|s| s.as_str()).unwrap_or(""))
                .ok_or_else(|| anyhow::anyhow!("usage: /cont <ws> <claude|codex>"))?;
            ctl.cont(alias, agent)?;
            Ok(format!("▶️ continued {alias}/{agent}"))
        }
        "mode" => {
            ctl.cycle_mode(alias, AgentKind::ClaudeCode)?;
            Ok(format!("🔀 cycled mode {alias}"))
        }
        "slash" => {
            let agent = parse_agent(args.get(1).map(|s| s.as_str()).unwrap_or(""))
                .ok_or_else(|| anyhow::anyhow!("usage: /slash <ws> <agent> <cmd>"))?;
            let scmd = args
                .get(2..)
                .map(|s| s.join(" "))
                .unwrap_or_default()
                .trim()
                .to_string();
            anyhow::ensure!(!scmd.is_empty(), "empty slash command");
            ctl.slash(alias, agent, &scmd)?;
            Ok(format!("/{scmd} → {alias}/{agent}"))
        }
        other => anyhow::bail!("unknown verb {other}"),
    }
}

fn required_arg<'a>(args: &'a [String], index: usize, message: &str) -> anyhow::Result<&'a str> {
    let value = args.get(index).map(|s| s.trim()).unwrap_or("");
    anyhow::ensure!(!value.is_empty(), "{message}");
    Ok(value)
}

fn sanitize_route(data: &str) -> String {
    data.split(':')
        .enumerate()
        .map(|(i, tok)| {
            if i < 2 || tok.chars().all(|c| c.is_ascii_digit()) {
                tok
            } else {
                "·"
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod route_tests {
    use super::sanitize_route;

    #[test]
    fn keeps_verb_and_indices_redacts_ids() {
        assert_eq!(sanitize_route("aq:p:5:2:0"), "aq:p:5:2:0");
        assert_eq!(sanitize_route("aq:s:12345"), "aq:s:12345");
        assert_eq!(sanitize_route("pm:a:9f3c-uuid-secret"), "pm:a:·");
        assert_eq!(
            sanitize_route("act:sayid:my-project:sid-uuid"),
            "act:sayid:·:·"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_callback(
    tg: &Arc<Telegram>,
    registry: &Registry,
    active: &Active,
    ctl: &Arc<dyn Control>,
    auth: &Arc<auth::Auth>,
    pending: &ingress::Pending,
    questions: &question::Questions,
    perms: &permission::Permissions,
    media_pending: &PendingMedia,
    cq: telegram::CallbackQuery,
) {
    if !auth.is_authorized(cq.from.id).await {
        let _ = tg.answer_callback(&cq.id, "🔒 not authorized").await;
        return;
    }
    let data = cq.data.clone().unwrap_or_default();
    let data = actions::decode(&data).unwrap_or(data);
    let chat = cq.message.as_ref().map(|m| m.chat.id);
    let msg_id = cq.message.as_ref().map(|m| m.message_id);
    info!(
        target: "relay::trace",
        stage = "pressed", direction = "from_telegram",
        route = %sanitize_route(&data), tg_chat = chat.unwrap_or(0), from = cq.from.id,
        "telegram button pressed"
    );

    if let Some(rest) = data.strip_prefix("aq:") {
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.as_slice() {
            ["p", pid, qi, oi] => {
                if let (Ok(pid), Ok(qi), Ok(oi)) =
                    (pid.parse::<u32>(), qi.parse::<usize>(), oi.parse::<usize>())
                {
                    if let Some((text, kb)) = question::handle_pick(questions, pid, qi, oi).await {
                        if let (Some(c), Some(mid)) = (chat, msg_id) {
                            let _ = tg.edit_message_text(c, mid, &text, Some(kb)).await;
                        }
                    }
                }
                let _ = tg.answer_callback(&cq.id, "").await;
            }
            ["s", pid] => {
                let msg = match pid.parse::<u32>() {
                    Ok(pid) => match question::handle_submit(questions, pid).await {
                        Ok(cards) => {
                            for (c, m) in cards {
                                let _ = tg
                                    .edit_message_text(c, m, "✅ answer sent (background)", None)
                                    .await;
                            }
                            "✅ sent".to_string()
                        }
                        Err(e) => e,
                    },
                    Err(_) => "bad pid".to_string(),
                };
                let _ = tg.answer_callback(&cq.id, &msg).await;
            }
            _ => {
                let _ = tg.answer_callback(&cq.id, "?").await;
            }
        }
        return;
    }

    if let Some(ids) = data.strip_prefix("ft:") {
        let full = ids.parse::<u32>().ok().and_then(actions::get);
        match (chat, full) {
            (Some(c), Some(text)) => {
                for chunk in chunk_text(&text, 3500) {
                    let _ = tg
                        .send(c, &format!("<pre>{}</pre>", esc_html(&chunk)), None)
                        .await;
                }
                let _ = tg.answer_callback(&cq.id, "").await;
            }
            _ => {
                let _ = tg.answer_callback(&cq.id, "text no longer available").await;
            }
        }
        return;
    }

    if let Some(rest) = data.strip_prefix("pm:") {
        let mut it = rest.splitn(2, ':');
        let verb = it.next().unwrap_or("");
        let allow = verb == "a";
        let ok = match it.next() {
            Some(enc) => {
                let reqid = actions::decode(enc).unwrap_or_else(|| enc.to_string());
                permission::resolve(perms, &reqid, allow).await
            }
            None => Err("bad id".to_string()),
        };
        let (note, label) = match ok {
            Ok(cards) => {
                let label = if allow { "✅ allowed" } else { "⛔ denied" };
                for (c, m) in cards {
                    let _ = tg.edit_message_text(c, m, label, None).await;
                }
                (label.to_string(), label)
            }
            Err(e) => (e, ""),
        };
        let _ = tg
            .answer_callback(&cq.id, if label.is_empty() { &note } else { label })
            .await;
        return;
    }

    if let Some(reqid) = data.strip_prefix("approve|") {
        let ok = ingress::resolve(pending, reqid, true).await;
        let _ = tg
            .answer_callback(&cq.id, if ok { "✅ approved" } else { "expired" })
            .await;
        if let (Some(c), Some(mid)) = (chat, msg_id) {
            let _ = tg.edit_message_text(c, mid, "✅ approved", None).await;
        }
        return;
    }
    if let Some(reqid) = data.strip_prefix("deny|") {
        let ok = ingress::resolve(pending, reqid, false).await;
        let _ = tg
            .answer_callback(&cq.id, if ok { "⛔ denied" } else { "expired" })
            .await;
        if let (Some(c), Some(mid)) = (chat, msg_id) {
            let _ = tg.edit_message_text(c, mid, "⛔ denied", None).await;
        }
        return;
    }

    if let Some(rest) = data.strip_prefix("m:") {
        let (text, kb) = if rest == "home" {
            home_view(registry).await
        } else if let Some(alias) = rest.strip_prefix("w:") {
            window_view(registry, alias).await
        } else if let Some(spec) = rest.strip_prefix("c:") {
            match parse_chat_spec(spec) {
                Some((alias, agent, idx)) => {
                    if agent == AgentKind::ClaudeCode {
                        let sid = {
                            let ws = registry.read().await;
                            ws.iter()
                                .find(|w| workspace_alias(&w.workspace) == alias)
                                .and_then(|w| w.claude.get(idx))
                                .map(|c| c.session_id.clone())
                        };
                        if let (Some(c), Some(sid)) = (chat, sid) {
                            active.write().await.insert(c, (alias.clone(), sid));
                        }
                    }
                    chat_view(registry, &alias, agent, idx).await
                }
                None => (
                    "bad chat ref".to_string(),
                    keyboard(vec![vec![("⬅️ Back".to_string(), "m:home".to_string())]]),
                ),
            }
        } else if let Some(k) = ["model", "effort", "mode"]
            .into_iter()
            .find(|k| rest.starts_with(&format!("{k}:")))
        {
            match parse_alias_idx(&rest[k.len() + 1..]) {
                Some((alias, idx)) => {
                    let active = if k == "model" {
                        let target = {
                            let ws = registry.read().await;
                            ws.iter()
                                .find(|w| workspace_alias(&w.workspace) == alias)
                                .and_then(|w| {
                                    w.claude
                                        .get(idx)
                                        .map(|c| (w.workspace.clone(), c.session_id.clone()))
                                })
                        };
                        target.and_then(|(cwd, sid)| active_model(&cwd, &sid))
                    } else {
                        None
                    };
                    setting_picker(k, &alias, idx, active)
                }
                None => (
                    "bad ref".to_string(),
                    keyboard(vec![vec![("⬅️ Back".to_string(), "m:home".to_string())]]),
                ),
            }
        } else {
            (
                "unknown view".to_string(),
                keyboard(vec![vec![("⬅️ Back".to_string(), "m:home".to_string())]]),
            )
        };
        if let (Some(c), Some(mid)) = (chat, msg_id) {
            let _ = tg.edit_message_text(c, mid, &text, Some(kb)).await;
        }
        let _ = tg.answer_callback(&cq.id, "").await;
        return;
    }

    if let Some(rest) = data.strip_prefix("au:") {
        let (text, kb) = if rest == "home" {
            automation_home_view().await
        } else if rest == "rules" {
            (
                automation_rules_text(),
                keyboard(vec![vec![("⬅️ Back".to_string(), "au:home".to_string())]]),
            )
        } else if let Some(m) = rest.strip_prefix("def:").and_then(automation::Mode::parse) {
            let mut cfg = automation::AutomationConfig::load();
            cfg.set(automation::Scope::Default, m, None);
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "default", mode = m.label(), "automation default set");
            automation_home_view().await
        } else if rest == "chats" {
            automation_chats_view(registry).await
        } else if rest == "prov" {
            automation_providers_view().await
        } else if rest == "smart" {
            let mut cfg = automation::AutomationConfig::load();
            let enable = !cfg.smart.enabled;
            let install_ok = !enable
                || match install::install_session_start_hooks() {
                    Ok(()) => true,
                    Err(error) => {
                        warn!(target: "relay::trace", stage = "automation", scope = "smart", "could not install SessionStart hooks: {error}");
                        false
                    }
                };
            if install_ok {
                cfg.smart.enabled = enable;
            }
            if !cfg.smart.enabled {
                cfg.smart.steer = false;
                cfg.smart.gate = false;
            }
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "smart", enabled = cfg.smart.enabled, "smart toggled");
            automation_home_view().await
        } else if rest == "steer" {
            let mut cfg = automation::AutomationConfig::load();
            if cfg.smart.enabled {
                cfg.smart.steer = !cfg.smart.steer;
                let _ = cfg.save();
                info!(target: "relay::trace", stage = "automation", scope = "smart", steer = cfg.smart.steer, "smart steer toggled");
            }
            automation_home_view().await
        } else if rest == "feedback" {
            let mut cfg = automation::AutomationConfig::load();
            if cfg.smart.enabled {
                cfg.smart.feedback_protocol = !cfg.smart.feedback_protocol;
                let _ = cfg.save();
                info!(target: "relay::trace", stage = "automation", scope = "smart", feedback = cfg.smart.feedback_protocol, "smart feedback protocol toggled");
            }
            automation_home_view().await
        } else if rest == "gate" {
            let mut cfg = automation::AutomationConfig::load();
            if cfg.smart.gate {
                cfg.smart.gate = false;
            } else {
                match install::install_gate_hooks() {
                    Ok(()) => {
                        cfg.smart.enabled = true;
                        cfg.smart.gate = true;
                    }
                    Err(error) => {
                        warn!(target: "relay::trace", stage = "automation", scope = "smart", "could not install gate hooks: {error}")
                    }
                }
            }
            let _ = cfg.save();
            automation_home_view().await
        } else if rest == "answerq" {
            let mut cfg = automation::AutomationConfig::load();
            cfg.auto.auto_answer_questions = !cfg.auto.auto_answer_questions;
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "auto", answer_questions = cfg.auto.auto_answer_questions, "auto answer-questions toggled");
            automation_home_view().await
        } else if rest == "guard" {
            let mut cfg = automation::AutomationConfig::load();
            cfg.auto.guard_dangerous = !cfg.auto.guard_dangerous;
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "auto", guard_dangerous = cfg.auto.guard_dangerous, "auto guard-dangerous toggled");
            automation_home_view().await
        } else if rest == "review" {
            let mut cfg = automation::AutomationConfig::load();
            cfg.review.enabled = !cfg.review.enabled;
            if !cfg.review.enabled {
                cfg.review.steer = false;
            }
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "review", enabled = cfg.review.enabled, "cross review toggled");
            automation_home_view().await
        } else if rest == "rsteer" {
            let mut cfg = automation::AutomationConfig::load();
            if cfg.review.enabled {
                cfg.review.steer = !cfg.review.steer;
                let _ = cfg.save();
                info!(target: "relay::trace", stage = "automation", scope = "review", steer = cfg.review.steer, "cross review steering toggled");
            }
            automation_home_view().await
        } else if rest == "rdepth" {
            let mut cfg = automation::AutomationConfig::load();
            cfg.review.depth = next_review_depth(cfg.review.depth);
            let _ = cfg.save();
            automation_home_view().await
        } else if rest == "revery" {
            let mut cfg = automation::AutomationConfig::load();
            cfg.review.every_secs = next_review_cadence(cfg.review.every_secs);
            let _ = cfg.save();
            automation_home_view().await
        } else if rest == "rprov" {
            review_reviewers_view().await
        } else if let Some(id) = rest.strip_prefix("rp:") {
            if supervisor::discover::Backend::parse(id).is_some() {
                let mut cfg = automation::AutomationConfig::load();
                match cfg.review.reviewers.iter().position(|name| name == id) {
                    Some(at) => {
                        cfg.review.reviewers.remove(at);
                    }
                    None => cfg.review.reviewers.push(id.to_string()),
                }
                let _ = cfg.save();
                info!(target: "relay::trace", stage = "automation", scope = "review", "reviewer toggled");
            }
            review_reviewers_view().await
        } else if let Some(spec) = rest.strip_prefix("rnow:") {
            match parse_chat_ref(spec) {
                Some((alias, kind, idx)) => session_review_menu(&alias, kind, idx, None).await,
                None => automation_home_view().await,
            }
        } else if let Some(spec) = rest.strip_prefix("rm:") {
            let picked = spec.rsplit_once(':').and_then(|(chat_ref, code)| {
                Some((
                    parse_chat_ref(chat_ref)?,
                    supervisor::review::depth_from_code(code.chars().next()?),
                ))
            });
            match picked {
                Some(((alias, kind, idx), depth)) => {
                    session_review_menu(&alias, kind, idx, depth).await
                }
                None => automation_home_view().await,
            }
        } else if let Some(spec) = rest.strip_prefix("rr:") {
            let picked = spec
                .rsplit_once(':')
                .and_then(|(chat_ref, pick)| Some((parse_chat_ref(chat_ref)?, pick.to_string())));
            match picked {
                Some(((alias, kind, idx), pick)) => {
                    let mut codes = pick.chars();
                    let reviewer = codes
                        .next()
                        .and_then(supervisor::review::reviewer_from_code);
                    let depth = codes.next().and_then(supervisor::review::depth_from_code);
                    session_review_run(
                        registry,
                        &alias,
                        kind,
                        idx,
                        reviewer,
                        depth,
                        tg.clone(),
                        chat,
                        ctl.clone(),
                    )
                    .await
                }
                None => automation_home_view().await,
            }
        } else if let Some(id) = rest.strip_prefix("pt:") {
            if supervisor::discover::Backend::parse(id).is_some() {
                let mut cfg = automation::AutomationConfig::load();
                cfg.robot.providers.toggle(id);
                let _ = cfg.save();
                info!(target: "relay::trace", stage = "automation", scope = "provider", "provider toggled");
            }
            automation_providers_view().await
        } else if let Some(s) = rest
            .strip_prefix("ps:")
            .and_then(automation::Strategy::parse)
        {
            let mut cfg = automation::AutomationConfig::load();
            cfg.robot.providers.strategy = s;
            let _ = cfg.save();
            info!(target: "relay::trace", stage = "automation", scope = "strategy", strategy = s.label(), "provider strategy set");
            automation_providers_view().await
        } else if let Some(spec) = rest.strip_prefix("ss:") {
            let mut it = spec.rsplitn(3, ':');
            let mode = it.next().and_then(automation::Mode::parse);
            let idx = it.next().and_then(|s| s.parse::<usize>().ok());
            let alias = it.next().map(|s| s.to_string());
            match (alias, idx, mode) {
                (Some(alias), Some(idx), Some(m)) => {
                    if let Some(sid) = claude_session_id(registry, &alias, idx).await {
                        let mut cfg = automation::AutomationConfig::load();
                        cfg.set(automation::Scope::Session(sid), m, None);
                        let _ = cfg.save();
                        info!(target: "relay::trace", stage = "automation", scope = "session", mode = m.label(), "automation chat pinned");
                    }
                    session_auto_view(registry, &alias, idx).await
                }
                _ => automation_home_view().await,
            }
        } else if let Some(spec) = rest.strip_prefix("sx:") {
            let mut it = spec.rsplitn(2, ':');
            let idx = it.next().and_then(|s| s.parse::<usize>().ok());
            let alias = it.next().map(|s| s.to_string());
            match (alias, idx) {
                (Some(alias), Some(idx)) => {
                    if let Some(sid) = claude_session_id(registry, &alias, idx).await {
                        let mut cfg = automation::AutomationConfig::load();
                        cfg.clear(automation::Scope::Session(sid));
                        let _ = cfg.save();
                        info!(target: "relay::trace", stage = "automation", scope = "session", mode = "clear", "automation chat cleared");
                    }
                    session_auto_view(registry, &alias, idx).await
                }
                _ => automation_home_view().await,
            }
        } else if let Some(spec) = rest.strip_prefix("s:") {
            let mut it = spec.rsplitn(2, ':');
            let idx = it.next().and_then(|s| s.parse::<usize>().ok());
            let alias = it.next().map(|s| s.to_string());
            match (alias, idx) {
                (Some(alias), Some(idx)) => session_auto_view(registry, &alias, idx).await,
                _ => automation_home_view().await,
            }
        } else {
            automation_home_view().await
        };
        if let (Some(c), Some(mid)) = (chat, msg_id) {
            let _ = tg.edit_message_text(c, mid, &text, Some(kb)).await;
        }
        let _ = tg.answer_callback(&cq.id, "").await;
        return;
    }

    if let Some(rest) = data.strip_prefix("ho:") {
        let (text, kb) = if let Some(spec) = rest.strip_prefix("go:") {
            let mut it = spec.rsplitn(2, ':');
            let ordinal = it.next().and_then(|s| s.parse::<usize>().ok());
            let token = it.next().and_then(|s| s.parse::<u32>().ok());
            handoff_run_view(token, ordinal).await
        } else if let Some(spec) = rest.strip_prefix("receipt:") {
            let mut it = spec.rsplitn(2, ':');
            let ordinal = it.next().and_then(|s| s.parse::<usize>().ok());
            let token = it.next().and_then(|s| s.parse::<u32>().ok());
            handoff_receipt_view(token, ordinal).await
        } else if let Some(spec) = rest.strip_prefix("inject:") {
            let mut it = spec.rsplitn(2, ':');
            let ordinal = it.next().and_then(|s| s.parse::<usize>().ok());
            let token = it.next().and_then(|s| s.parse::<u32>().ok());
            handoff_inject_view(token, ordinal).await
        } else {
            let mut it = rest.rsplitn(2, ':');
            let idx = it.next().and_then(|s| s.parse::<usize>().ok());
            let alias = it.next().map(|s| s.to_string());
            match (alias, idx) {
                (Some(alias), Some(idx)) => handoff_pick_view(registry, &alias, idx).await,
                _ => (
                    "handoff needs a chat".to_string(),
                    keyboard(vec![vec![("⬅️ Windows".to_string(), "m:home".to_string())]]),
                ),
            }
        };
        if let (Some(c), Some(mid)) = (chat, msg_id) {
            let _ = tg.edit_message_text(c, mid, &text, Some(kb)).await;
        }
        let _ = tg.answer_callback(&cq.id, "").await;
        return;
    }

    if let Some(rest) = data.strip_prefix("act:") {
        let ap: Vec<&str> = rest.split(':').collect();
        let result = match ap.as_slice() {
            ["say", alias, agent] => {
                if let Some(c) = chat {
                    let prompt = format!("✍️ Reply with your prompt\n→ {alias} / {agent}");
                    let _ = tg.send(c, &prompt, Some(telegram::force_reply())).await;
                }
                "reply with your prompt".to_string()
            }
            ["say", alias, agent, idx] => {
                if let Some(c) = chat {
                    let prompt = format!("✍️ Reply with your prompt\n→ {alias} / {agent} / {idx}");
                    let _ = tg.send(c, &prompt, Some(telegram::force_reply())).await;
                }
                "reply with your prompt".to_string()
            }
            ["sayid", alias, sid] => {
                if let Some(c) = chat {
                    active
                        .write()
                        .await
                        .insert(c, (alias.to_string(), sid.to_string()));
                    let prompt = format!(
                        "✍️ Type your message - it goes to this chat (background). Active: {alias}\n→ {alias} / claude / sid:{sid}"
                    );
                    let _ = tg.send(c, &prompt, Some(telegram::force_reply())).await;
                }
                "reply to write into the chat".to_string()
            }
            ["sbar", alias, agent, idx] => {
                if let Some(c) = chat {
                    let prompt = format!(
                        "💬 Reply - goes into the SIDEBAR chat\n→ {alias} / {agent} / {idx} / sidebar"
                    );
                    let _ = tg.send(c, &prompt, Some(telegram::force_reply())).await;
                }
                "reply for sidebar".to_string()
            }
            ["stop", alias, agent] => run_ctl(ctl, "stop", vec!["", alias, agent]).await,
            ["ok", alias] => run_ctl(ctl, "accept", vec!["", alias]).await,
            ["pick", alias, oi] => run_ctl(ctl, "pick", vec!["", alias, oi]).await,
            ["cont", alias, agent] => run_ctl(ctl, "cont", vec!["", alias, agent]).await,
            ["cont2", alias, idx] => match idx.parse::<usize>() {
                Ok(i) => {
                    say_text(
                        ctl,
                        registry,
                        alias,
                        AgentKind::ClaudeCode,
                        Some(i),
                        false,
                        "continue",
                    )
                    .await
                }
                _ => "bad idx".to_string(),
            },
            ["setmodel", alias, idx, val] => {
                apply_setting(registry, alias, idx, "model", val).await
            }
            ["seteffort", alias, idx, val] => {
                apply_setting(registry, alias, idx, "effort", val).await
            }
            ["setmode", alias, idx, val] => apply_setting(registry, alias, idx, "mode", val).await,
            ["mode", alias] => run_ctl(ctl, "mode", vec!["", alias]).await,
            ["focus", alias] => run_ctl(ctl, "focus", vec!["", alias]).await,
            ["read", alias, agent, idx] => match (parse_agent(agent), idx.parse::<usize>(), chat) {
                (Some(ag), Ok(i), Some(c)) => {
                    let body = build_tail(registry, alias, ag, i).await;
                    let _ = tg.send(c, &body, None).await;
                    "📄 sent".to_string()
                }
                _ => "bad read ref".to_string(),
            },
            ["media", token, idx] => {
                media_pick(tg, active, media_pending, chat, msg_id, token, idx).await
            }
            _ => "unknown action".to_string(),
        };
        let _ = tg.answer_callback(&cq.id, &result).await;
        return;
    }

    let parts: Vec<&str> = data.split('|').collect();
    let result = match parts.as_slice() {
        ["focus", alias] => run_ctl(ctl, "focus", vec!["", alias]).await,
        ["stop", alias, agent] => run_ctl(ctl, "stop", vec!["", alias, agent]).await,
        ["cont", alias, agent] => run_ctl(ctl, "cont", vec!["", alias, agent]).await,
        _ => "unknown action".to_string(),
    };
    let _ = tg.answer_callback(&cq.id, &result).await;
}

fn claude_display(c: &relay_core::ClaudeAgent) -> String {
    c.title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| {
            c.name
                .clone()
                .unwrap_or_else(|| relay_core::state::truncate(&c.session_id, 8))
        })
}

fn state_glyph(label: &str) -> &'static str {
    match label {
        "awaiting_permission" => "🔴",
        "pending_question" => "🟡",
        "needs_reply?" => "🟠",
        "error" => "⚠️",
        "working" => "⚙️",
        "subagent" => "🌀",
        "aborted" | "interrupted" | "denied" => "⏹",
        "idle" => "🟢",
        _ => "⚪️",
    }
}

fn priority(label: &str) -> u8 {
    match label {
        "awaiting_permission" => 5,
        "pending_question" => 4,
        "error" => 3,
        "needs_reply?" => 2,
        "working" | "subagent" => 1,
        _ => 0,
    }
}

fn window_glyph(w: &WindowEntry) -> &'static str {
    let mut best = "idle";
    let mut best_p = 0u8;
    for c in &w.claude {
        let l = c.state.label();
        if priority(l) >= best_p {
            best_p = priority(l);
            best = l;
        }
    }
    for cx in &w.codex {
        let l = cx.state.label();
        if priority(l) >= best_p {
            best_p = priority(l);
            best = l;
        }
    }
    state_glyph(best)
}

async fn build_tail(registry: &Registry, alias: &str, agent: AgentKind, idx: usize) -> String {
    let windows = registry.read().await;
    let Some(w) = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)
    else {
        return format!("no window '{alias}'");
    };
    let msgs = match agent {
        AgentKind::ClaudeCode => match w.claude.get(idx) {
            Some(c) => relay_adapters::claude::tail_messages(&c.jsonl_path, 8),
            None => Vec::new(),
        },
        AgentKind::Codex => match w.codex.get(idx) {
            Some(cx) => relay_adapters::codex::tail_messages(&cx.rollout_path, 8),
            None => Vec::new(),
        },
    };
    if msgs.is_empty() {
        return "📄 no messages yet".to_string();
    }
    let mut out = format!("📄 <b>{}</b> - recent", esc_html(alias));
    for (role, text) in msgs {
        let g = match role {
            'U' => "🧑",
            'A' => agent_glyph(agent),
            'T' => "🔧",
            _ => "•",
        };
        out.push_str(&format!(
            "\n\n{g} {}",
            esc_html(&relay_core::state::truncate(&text, 400))
        ));
    }
    out
}

async fn home_view(registry: &Registry) -> (String, serde_json::Value) {
    let windows = registry.read().await;
    let text = if windows.is_empty() {
        "🖥 <b>Windows</b>\n<i>none discovered yet</i>".to_string()
    } else {
        format!("🖥 <b>Windows</b> · {} open - tap one", windows.len())
    };
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for w in windows.iter() {
        let alias = workspace_alias(&w.workspace);
        let label = format!("{} {}  ·  {} 💬", window_glyph(w), alias, w.chat_count());
        rows.push(vec![(label, format!("m:w:{alias}"))]);
    }
    rows.push(vec![
        ("🤖 Automation".to_string(), "au:home".to_string()),
        ("🔄 Refresh".to_string(), "m:home".to_string()),
    ]);
    (text, keyboard(rows))
}

async fn window_view(registry: &Registry, alias: &str) -> (String, serde_json::Value) {
    let windows = registry.read().await;
    let Some(w) = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)
    else {
        return (
            format!("no window '{alias}'"),
            keyboard(vec![vec![("⬅️ Windows".to_string(), "m:home".to_string())]]),
        );
    };
    let branch = w.git_branch.as_deref().unwrap_or("-");
    let text = format!(
        "📁 <b>{}</b>\n⌥ {} · pick a chat 👇",
        esc_html(alias),
        esc_html(branch)
    );

    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for (i, c) in w.claude.iter().enumerate() {
        let star = if i == 0 { "⭐ " } else { "" };
        let label = format!(
            "{star}{} 🤖 {}",
            state_glyph(c.state.label()),
            relay_core::state::truncate(&claude_display(c), 48)
        );
        rows.push(vec![(label, format!("m:c:{alias}:claude:{i}"))]);
    }
    for (i, cx) in w.codex.iter().enumerate() {
        let star = if i == 0 { "⭐ " } else { "" };
        let label = format!(
            "{star}{} 🧠 {}",
            state_glyph(cx.state.label()),
            relay_core::state::truncate(&cx.title, 48)
        );
        rows.push(vec![(label, format!("m:c:{alias}:codex:{i}"))]);
    }
    rows.push(vec![
        ("👁 Focus".to_string(), format!("act:focus:{alias}")),
        ("🔄".to_string(), format!("m:w:{alias}")),
        ("⬅️ Windows".to_string(), "m:home".to_string()),
    ]);
    (text, keyboard(rows))
}

async fn chat_view(
    registry: &Registry,
    alias: &str,
    agent: AgentKind,
    idx: usize,
) -> (String, serde_json::Value) {
    let back = || keyboard(vec![vec![("⬅️ Back".to_string(), format!("m:w:{alias}"))]]);
    let windows = registry.read().await;
    let Some(w) = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)
    else {
        return (format!("no window '{alias}'"), back());
    };
    let a = agent.as_str();

    let (header, sub, label) = match agent {
        AgentKind::ClaudeCode => match w.claude.get(idx) {
            Some(c) => {
                let id = c
                    .name
                    .clone()
                    .unwrap_or_else(|| relay_core::state::truncate(&c.session_id, 8));
                match &c.title {
                    Some(t) if !t.trim().is_empty() => {
                        (t.clone(), Some(format!("🆔 {id}")), c.state.label())
                    }
                    _ => (id, None, c.state.label()),
                }
            }
            None => return (format!("chat gone in {alias}"), back()),
        },
        AgentKind::Codex => match w.codex.get(idx) {
            Some(cx) => (
                relay_core::state::truncate(&cx.title, 60),
                Some(format!("approval: {}", cx.approval_mode)),
                cx.state.label(),
            ),
            None => return (format!("chat gone in {alias}"), back()),
        },
    };

    let glyph = agent_glyph(agent);
    let mut text = format!(
        "{glyph} <b>{}</b>\n📁 {} · {} {}",
        esc_html(&header),
        esc_html(alias),
        state_glyph(label),
        label
    );
    if let Some(s) = sub {
        text.push_str(&format!("\n{}", esc_html(&s)));
    }

    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    if agent == AgentKind::ClaudeCode {
        let bg = chat_bg_pid(w, idx).is_some();
        text.push_str(if bg {
            "\n🟢 <i>active - just type in Telegram and it goes here (background)</i>"
        } else {
            "\n⚪️ <i>background unavailable (no shim) - via GUI</i>"
        });
        rows.push(vec![
            ("✍️ Send".to_string(), format!("act:say:{alias}:{a}:{idx}")),
            (
                "▶️ Continue".to_string(),
                format!("act:cont2:{alias}:{idx}"),
            ),
            ("⏹ Stop".to_string(), format!("act:stop:{alias}:{a}")),
        ]);
        rows.push(vec![
            ("🧠 Model".to_string(), format!("m:model:{alias}:{idx}")),
            ("⚡ Effort".to_string(), format!("m:effort:{alias}:{idx}")),
            ("🔀 Mode".to_string(), format!("m:mode:{alias}:{idx}")),
        ]);
        rows.push(vec![
            (
                "📄 Read chat".to_string(),
                format!("act:read:{alias}:{a}:{idx}"),
            ),
            ("🤖 Auto".to_string(), format!("au:s:{alias}:{idx}")),
        ]);
        rows.push(vec![(
            "🔍 Cross review".to_string(),
            format!("au:rnow:{alias}:c:{idx}"),
        )]);
        if matches!(agent, AgentKind::ClaudeCode) {
            rows.push(vec![(
                "📦 Hand off".to_string(),
                format!("ho:{alias}:{idx}"),
            )]);
        }
    } else {
        rows.push(vec![
            ("✍️ Send".to_string(), format!("act:say:{alias}:{a}:{idx}")),
            ("⏹ Stop".to_string(), format!("act:stop:{alias}:{a}")),
        ]);
        rows.push(vec![(
            "📄 Read chat".to_string(),
            format!("act:read:{alias}:{a}:{idx}"),
        )]);
        rows.push(vec![(
            "🔍 Cross review".to_string(),
            format!("au:rnow:{alias}:x:{idx}"),
        )]);
    }
    rows.push(vec![
        ("👁 Focus".to_string(), format!("act:focus:{alias}")),
        ("⬅️ Back".to_string(), format!("m:w:{alias}")),
    ]);
    (text, keyboard(rows))
}

async fn format_status(registry: &Registry, alias: &str) -> String {
    let windows = registry.read().await;
    let w = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias);
    match w {
        None => format!("no window for '{alias}'"),
        Some(w) => {
            let branch = w.git_branch.as_deref().unwrap_or("-");
            let mut out = format!("📁 <b>{}</b> ⌥ {}\n", esc_html(alias), esc_html(branch));
            for c in &w.claude {
                out.push_str(&format!(
                    "🤖 <b>{}</b> · {}\n",
                    esc_html(c.name.as_deref().unwrap_or("claude")),
                    c.state.label()
                ));
                if let Some(t) = &c.title {
                    out.push_str(&format!(
                        "   {}\n",
                        esc_html(&relay_core::state::truncate(t, 80))
                    ));
                }
            }
            for cx in &w.codex {
                out.push_str(&format!(
                    "🧠 <b>{}</b> · {} [{}]\n",
                    esc_html(&relay_core::state::truncate(&cx.title, 40)),
                    cx.state.label(),
                    esc_html(&cx.approval_mode)
                ));
            }
            out
        }
    }
}

fn mode_title(m: automation::Mode) -> &'static str {
    match m {
        automation::Mode::Manual => "🖐 Manual",
        automation::Mode::Auto => "⚙️ Auto",
        automation::Mode::Robot => "🧠 Robot",
    }
}

fn automation_rules_text() -> String {
    "🤖 <b>Automation rules</b>\n\n\
     🖐 <b>Manual</b> - nothing happens without you; every permission, question and error waits for your tap.\n\n\
     ⚙️ <b>Auto</b> - rule-based, no AI:\n\
     • safe tool permissions are auto-approved\n\
     • plan-mode exit is auto-accepted\n\
     • transient API errors auto-retry with backoff\n\
     • danger-list commands ALWAYS ask you\n\
     • questions still wait for you\n\
     • rate/usage limits pause and notify\n\n\
     🧠 <b>Robot</b> - an AI supervisor drives the chat: continue, retry, accept a plan, answer, or stop, under a per-session step cap. Every supervisor provider call is metered into a local usage ledger; USD caps (budget_usd, per-provider max_usd) halt a provider once its recorded spend is reached, and apply only to metered HTTP providers with a configured usd_per_ktok price (local and CLI providers record no cost).\n\n\
     🧭 <b>Compass</b> - an optional model-free layer that reads the session and judges direction. Off = base behavior. On (outside Robot) = shadow advisories only. Auto-steer governs the predictive corrective steer in Robot mode, under hard guards (Idle/Error, two independent sources, no pending question, at most three steers, then it escalates to you). Independently, in Robot a deterministic Contract-Ledger proof request may be injected when the ledger requires one, under budget and non-escalation guards, even with auto-steer off.\n\n\
     <i>Scope: a chat pin beats a workspace pin beats the default.</i>"
        .to_string()
}

async fn automation_home_view() -> (String, serde_json::Value) {
    let cfg = automation::AutomationConfig::load();
    let smart = if cfg.smart.enabled { "on" } else { "off" };
    let steer = if cfg.smart.steer {
        "auto-steer"
    } else {
        "shadow"
    };
    let feedback = if cfg.smart.feedback_protocol {
        "telemetry on"
    } else {
        "telemetry off"
    };
    let gate = if cfg.smart.gate {
        "gate on"
    } else {
        "gate off"
    };
    let review_line = if cfg.review.enabled {
        format!(
            "Cross review: <b>on</b> · {} · every {} · {} · {}",
            if cfg.review.steer {
                "corrects"
            } else {
                "observes"
            },
            review_cadence_label(cfg.review.every_secs),
            cfg.review.depth.label(),
            if cfg.review.reviewers.is_empty() {
                "no reviewer".to_string()
            } else {
                cfg.review.reviewers.join(" → ")
            }
        )
    } else {
        "Cross review: <b>off</b>".to_string()
    };
    let text = format!(
        "🤖 <b>Automation</b>\nDefault: <b>{}</b>\nWorkspace pins: {} · chat pins: {}\nCompass: <b>{}</b> · steering: <b>{}</b> · gate: <b>{}</b> · feedback: <b>{}</b>\n{review_line}\n\n<i>Manual = all you. Auto = rules (auto-approve safe, auto-retry transient; danger still asks). Robot = AI supervisor. Gate is an independent, deterministic Contract Ledger authority and is off by default. Cross review asks a different model family whether a chat still serves your last instruction.</i>",
        mode_title(cfg.default),
        cfg.workspaces.len(),
        cfg.sessions.len(),
        smart,
        steer,
        gate,
        feedback
    );
    let btn = |m: automation::Mode| {
        let mark = if cfg.default == m { "● " } else { "" };
        (
            format!("{mark}{}", mode_title(m)),
            format!("au:def:{}", m.label()),
        )
    };
    let smart_btn = (format!("🧭 Compass: {smart}"), "au:smart".to_string());
    let steer_btn = (
        format!(
            "{} Steering: {steer}",
            if cfg.smart.steer { "🚗" } else { "👁" }
        ),
        "au:steer".to_string(),
    );
    let feedback_btn = (
        format!(
            "{} Feedback: {feedback}",
            if cfg.smart.feedback_protocol {
                "📡"
            } else {
                "🔇"
            }
        ),
        "au:feedback".to_string(),
    );
    let gate_btn = (
        format!("{} Gate: {gate}", if cfg.smart.gate { "🛡" } else { "▫️" }),
        "au:gate".to_string(),
    );
    let answerq_btn = (
        format!(
            "{} Auto-answer Q: {}",
            if cfg.auto.auto_answer_questions {
                "💬"
            } else {
                "▫️"
            },
            if cfg.auto.auto_answer_questions {
                "on"
            } else {
                "off"
            }
        ),
        "au:answerq".to_string(),
    );
    let guard_btn = (
        format!(
            "{} Danger guard: {}",
            if cfg.auto.guard_dangerous {
                "🛡"
            } else {
                "▫️"
            },
            if cfg.auto.guard_dangerous {
                "on"
            } else {
                "off"
            }
        ),
        "au:guard".to_string(),
    );
    let review_btn = (
        format!(
            "{} Cross review: {}",
            if cfg.review.enabled { "🔍" } else { "▫️" },
            if cfg.review.enabled { "on" } else { "off" }
        ),
        "au:review".to_string(),
    );
    let review_steer_btn = (
        format!(
            "{} Review acts: {}",
            if cfg.review.steer { "🚗" } else { "👁" },
            if cfg.review.steer {
                "corrects"
            } else {
                "observes"
            }
        ),
        "au:rsteer".to_string(),
    );
    let review_depth_btn = (
        format!("🔬 Depth: {}", cfg.review.depth.label()),
        "au:rdepth".to_string(),
    );
    let review_every_btn = (
        format!("⏱ Every: {}", review_cadence_label(cfg.review.every_secs)),
        "au:revery".to_string(),
    );
    let reviewers_btn = (
        format!("👥 Reviewers: {}", cfg.review.reviewers.len()),
        "au:rprov".to_string(),
    );
    let kb = keyboard(vec![
        vec![
            btn(automation::Mode::Manual),
            btn(automation::Mode::Auto),
            btn(automation::Mode::Robot),
        ],
        vec![("🔀 Chat modes".to_string(), "au:chats".to_string())],
        vec![smart_btn, steer_btn],
        vec![gate_btn],
        vec![feedback_btn],
        vec![answerq_btn, guard_btn],
        vec![review_btn, review_steer_btn],
        vec![review_depth_btn, review_every_btn, reviewers_btn],
        vec![
            ("🔌 Providers".to_string(), "au:prov".to_string()),
            ("📖 Rules".to_string(), "au:rules".to_string()),
            ("🔄".to_string(), "au:home".to_string()),
        ],
        vec![("⬅️ Windows".to_string(), "m:home".to_string())],
    ]);
    (text, kb)
}

fn handoff_destinations(
    source_session: &str,
    source_workspace: &std::path::Path,
) -> anyhow::Result<Vec<handoff::Destination>> {
    let paths = Paths::discover()?;
    let mut out: Vec<handoff::Destination> = Vec::new();
    for w in scan(&paths) {
        for c in &w.claude {
            if c.session_id == source_session {
                continue;
            }
            let linked = inject::available_pid(&c.session_id).is_some();
            let here = w.workspace == source_workspace;
            out.push(handoff::Destination {
                id: format!("chat:{}", c.session_id),
                label: format!(
                    "Claude chat{} - {}",
                    if here {
                        String::new()
                    } else {
                        format!(" in {}", workspace_alias(&w.workspace))
                    },
                    relay_core::state::truncate(&claude_display(c), 40)
                ),
                workspace: w.workspace.clone(),
                kind: handoff::DestinationKind::Chat,
                session_id: Some(c.session_id.clone()),
                app: None,
                cli: None,
                linked,
            });
        }
    }
    out.sort_by_key(|d| d.workspace != source_workspace);
    for (bin, label, flags) in handoff::installed_clis() {
        out.push(handoff::Destination {
            id: format!("cli:{bin}"),
            label: format!("Continue in {label}"),
            workspace: source_workspace.to_path_buf(),
            kind: handoff::DestinationKind::Cli,
            session_id: None,
            app: None,
            cli: Some((bin, flags)),
            linked: false,
        });
    }
    for app in handoff::installed_apps() {
        out.push(handoff::Destination {
            id: format!("app:{app}"),
            label: format!("Open this project in {app}"),
            workspace: source_workspace.to_path_buf(),
            kind: handoff::DestinationKind::App,
            session_id: None,
            app: Some(app),
            cli: None,
            linked: false,
        });
    }
    Ok(out)
}

fn source_session_of(session: &str) -> anyhow::Result<relay_adapters::family::SourceSession> {
    relay_adapters::family::locate(session)
        .with_context(|| format!("no transcript found for session {session}"))
}

fn source_window_of(session: &str) -> anyhow::Result<(std::path::PathBuf, Option<String>)> {
    let paths = Paths::discover()?;
    if let Some(window) = scan(&paths)
        .into_iter()
        .find(|w| w.claude.iter().any(|c| c.session_id == session))
    {
        return Ok((window.workspace.clone(), window.git_branch.clone()));
    }
    let found = source_session_of(session)?;
    let workspace = found.workspace.with_context(|| {
        format!("session {session} is not in an open window and records no working directory")
    })?;
    let branch = relay_discovery::git::branch(&workspace);
    Ok((workspace, branch))
}

fn run_handoff_destinations(session: &str, json: bool) -> anyhow::Result<()> {
    let (workspace, _) = source_window_of(session)?;
    let rows = handoff_destinations(session, &workspace)?;
    if json {
        let payload: Vec<serde_json::Value> = rows
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "label": d.label,
                    "workspace": d.workspace.display().to_string(),
                    "kind": match d.kind {
                        handoff::DestinationKind::Chat => "chat",
                        handoff::DestinationKind::Cli => "cli",
                        handoff::DestinationKind::App => "app",
                    },
                    "linked": d.linked,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&payload)?);
    } else {
        for d in &rows {
            println!("{:<48} {}", d.id, d.label);
        }
    }
    Ok(())
}

fn run_handoff_targets(json: bool) -> anyhow::Result<()> {
    let paths = Paths::discover()?;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for w in scan(&paths) {
        let linked = w
            .claude
            .iter()
            .any(|c| inject::available_pid(&c.session_id).is_some());
        if json {
            rows.push(serde_json::json!({
                "alias": workspace_alias(&w.workspace),
                "workspace": w.workspace.display().to_string(),
                "branch": w.git_branch,
                "linked": linked,
            }));
        } else {
            println!(
                "{}  {:<24} {:<10} {}",
                if linked { "link" } else { "file" },
                workspace_alias(&w.workspace),
                w.git_branch.as_deref().unwrap_or("-"),
                w.workspace.display()
            );
        }
    }
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    }
    Ok(())
}

fn run_handoff_receipt(workspace: &str, json: bool) -> anyhow::Result<()> {
    let target = handoff::HandoffTarget {
        alias: workspace_alias(std::path::Path::new(workspace)),
        workspace: std::path::PathBuf::from(workspace),
        title: "-".to_string(),
        chat: None,
        app: None,
        cli: None,
    };
    let receipt = handoff::read_receipt(&target);
    let missing = match (&receipt, handoff::contract_of(&target)) {
        (Some(receipt), Some(contract)) => {
            Some(relay_compass::handoff::missing_anchors(&contract, receipt))
        }
        _ => None,
    };
    if json {
        println!(
            "{}",
            serde_json::json!({ "receipt": receipt, "missing_anchors": missing })
        );
        return Ok(());
    }
    let Some(receipt) = receipt else {
        println!("no receipt yet in {workspace}/HANDOFF.md");
        return Ok(());
    };
    println!("{receipt}\n");
    match missing {
        Some(missing) if missing.is_empty() => {
            println!("drift check: no contract anchor is missing from the receipt")
        }
        Some(missing) => println!(
            "drift check: the receipt never mentions {}",
            missing.join(", ")
        ),
        None => println!("drift check: skipped, contract section unreadable"),
    }
    Ok(())
}

async fn run_handoff_to(session: &str, destination_id: &str, json: bool) -> anyhow::Result<()> {
    let (source_workspace, source_branch) = source_window_of(session)?;
    let all = handoff_destinations(session, &source_workspace)?;
    let destination = handoff::destination_of(destination_id, &all)
        .with_context(|| format!("unknown destination {destination_id}"))?;

    let found = source_session_of(session)?;
    let transcript = found.transcript.clone();
    let pending = handoff::PendingHandoff {
        source_alias: workspace_alias(&source_workspace),
        source_session: session.to_string(),
        source_agent: found.family.id().to_string(),
        source_workspace: source_workspace.clone(),
        source_branch,
        targets: Vec::new(),
    };
    let target = handoff::HandoffTarget {
        alias: workspace_alias(&destination.workspace),
        workspace: destination.workspace.clone(),
        title: destination.label.clone(),
        chat: destination
            .session_id
            .clone()
            .map(|session_id| handoff::HandoffChat { session_id }),
        app: destination.app.clone(),
        cli: destination.cli.clone(),
    };

    let brief = handoff::prepare(&pending, &transcript)?;
    let cfg = automation::AutomationConfig::load();
    let mut brief = brief;
    brief.compact = handoff::agent_compact(
        &pending,
        &transcript,
        &brief,
        &cfg.robot.providers,
        automation::now_secs(),
    )
    .await;
    let stamp = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let result = handoff::write_brief(&pending, &brief, &target, &stamp)?;

    let delivery = match destination.kind {
        handoff::DestinationKind::Chat => {
            match destination
                .session_id
                .as_deref()
                .and_then(inject::available_pid)
            {
                Some(pid) => match inject::send_user_message(pid, &result.prompt) {
                    Ok(()) => "sent into the target chat".to_string(),
                    Err(err) => format!("could not send into the target chat: {err}"),
                },
                None => "target chat is not tapped, paste the prompt yourself".to_string(),
            }
        }
        handoff::DestinationKind::App => {
            let app = destination.app.clone().unwrap_or_default();
            match handoff::open_in_app(&app, &destination.workspace) {
                Ok(()) => format!("opened {app}, paste the prompt into its chat"),
                Err(err) => format!("could not open {app}: {err}"),
            }
        }
        handoff::DestinationKind::Cli => match &destination.cli {
            Some((bin, flags)) => {
                let unattended = cfg.resolve(
                    Some(session),
                    &workspace_alias(&source_workspace),
                    automation::now_secs(),
                ) != automation::Mode::Manual;
                if unattended {
                    match handoff::launch_cli_supervised(
                        bin,
                        flags,
                        &destination.workspace,
                        &result.prompt,
                        unattended,
                        session,
                        &cfg.robot.providers,
                        automation::now_secs(),
                    )
                    .await
                    {
                        Ok(run) => format!(
                            "ran {bin} under the relay, exit {}: {} prompt{} answered for you{}; log {}",
                            run.status.map(|c| c.to_string()).unwrap_or_else(|| "-".to_string()),
                            run.answered,
                            if run.answered == 1 { "" } else { "s" },
                            if run.stopped_by_guard {
                                ", stopped at a prompt only you should answer"
                            } else {
                                ""
                            },
                            run.log.display()
                        ),
                        Err(err) => format!("could not run {bin}: {err}"),
                    }
                } else {
                    match handoff::launch_cli(
                        bin,
                        flags,
                        &destination.workspace,
                        &result.prompt,
                        unattended,
                    ) {
                        Ok(()) => format!(
                            "started {bin} with the brief, {}{}",
                            if unattended {
                                "unattended - it approves its own tools"
                            } else {
                                "it will ask you before each tool"
                            },
                            match handoff::cli_auth_state(bin) {
                                Some(state) => format!("; {state}"),
                                None => String::new(),
                            }
                        ),
                        Err(err) => format!("could not start {bin}: {err}"),
                    }
                }
            }
            None => "no cli recorded for this destination".to_string(),
        },
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "path": result.path.display().to_string(),
                "bytes": result.bytes,
                "proven": result.proven,
                "remaining": result.remaining,
                "compact_author": result.compact_author,
                "prompt": result.prompt,
                "delivery": delivery,
            })
        );
        return Ok(());
    }
    println!("wrote {} ({} bytes)", result.path.display(), result.bytes);
    println!("delivery: {delivery}");
    println!("\n{}", result.prompt);
    Ok(())
}

async fn run_handoff(session: &str, workspace: &str, json: bool) -> anyhow::Result<()> {
    let target_workspace = std::path::PathBuf::from(workspace);
    if !target_workspace.is_dir() {
        anyhow::bail!("target workspace {workspace} is not a directory");
    }
    let found = source_session_of(session)?;
    let transcript = found.transcript.clone();
    let (source_workspace, source_branch) = source_window_of(session)?;
    let pending = handoff::PendingHandoff {
        source_alias: workspace_alias(&source_workspace),
        source_session: session.to_string(),
        source_agent: found.family.id().to_string(),
        source_workspace,
        source_branch,
        targets: Vec::new(),
    };
    let target = handoff::HandoffTarget {
        alias: workspace_alias(&target_workspace),
        workspace: target_workspace,
        title: "-".to_string(),
        chat: None,
        app: None,
        cli: None,
    };

    let started = std::time::Instant::now();
    let brief = handoff::prepare(&pending, &transcript)?;
    let read_secs = started.elapsed().as_secs_f32();
    if !json {
        println!(
            "read {} in {read_secs:.1}s: contract {}, {} proven, {} open, {} files, {} commands, {} turns",
            transcript.display(),
            if brief.contract.is_some() { "found" } else { "MISSING" },
            brief.proven.len(),
            brief.remaining.len(),
            brief.artifacts.len(),
            brief.recent_commands.len(),
            brief.recent_turns.len()
        );
    }

    let cfg = automation::AutomationConfig::load();
    let mut brief = brief;
    brief.compact = handoff::agent_compact(
        &pending,
        &transcript,
        &brief,
        &cfg.robot.providers,
        automation::now_secs(),
    )
    .await;
    if !json {
        match &brief.compact {
            Some(compact) => println!(
                "compact by {} ({} chars)",
                compact.author,
                compact.text.len()
            ),
            None => println!("compact: none (no provider answered)"),
        }
    }

    let stamp = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let result = handoff::write_brief(&pending, &brief, &target, &stamp)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "path": result.path.display().to_string(),
                "bytes": result.bytes,
                "proven": result.proven,
                "remaining": result.remaining,
                "compact_author": result.compact_author,
                "prompt": result.prompt,
            })
        );
        return Ok(());
    }
    println!("wrote {} ({} bytes)", result.path.display(), result.bytes);
    println!("\n{}", result.prompt);
    Ok(())
}

async fn handoff_pick_view(
    registry: &Registry,
    alias: &str,
    idx: usize,
) -> (String, serde_json::Value) {
    let back = keyboard(vec![vec![(
        "⬅️ Back".to_string(),
        format!("m:c:{alias}:claude:{idx}"),
    )]]);
    let windows = registry.read().await;
    let Some(source_window) = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)
    else {
        return ("window gone".to_string(), back);
    };
    let Some(source) = source_window.claude.get(idx) else {
        return ("chat gone".to_string(), back);
    };
    let source_session = source.session_id.clone();
    let source_workspace = source_window.workspace.clone();
    let source_branch = source_window.git_branch.clone();
    drop(windows);

    let destinations = match handoff_destinations(&source_session, &source_workspace) {
        Ok(destinations) if !destinations.is_empty() => destinations,
        Ok(_) => {
            return (
                "📦 <b>Hand off</b>\n\nNo other agent is available to take this over.".to_string(),
                back,
            )
        }
        Err(err) => return (format!("handoff failed: {err}"), back),
    };

    let targets: Vec<handoff::HandoffTarget> = destinations
        .iter()
        .map(|d| handoff::HandoffTarget {
            alias: workspace_alias(&d.workspace),
            workspace: d.workspace.clone(),
            title: d.label.clone(),
            chat: d
                .session_id
                .clone()
                .map(|session_id| handoff::HandoffChat { session_id }),
            app: d.app.clone(),
            cli: d.cli.clone(),
        })
        .collect();

    let pending = handoff::PendingHandoff {
        source_alias: alias.to_string(),
        source_session,
        source_agent: "claude".to_string(),
        source_workspace,
        source_branch,
        targets,
    };
    let token = handoff::stash(pending);

    let text = format!(
        "📦 <b>Hand off</b> · from {}\n\n<i>Move this session to another agent. HANDOFF.md is written next to the work: the contract in your own words, a compact this agent writes about its own work, what is proven, what is open, files touched and the recent conversation.</i>\n\n→ a tapped Claude chat, the prompt is sent straight in.\n· not tapped, or an editor that is opened on this project - you paste the prompt into its chat.",
        esc_html(alias)
    );
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for (ordinal, destination) in destinations.iter().enumerate() {
        let mark = match destination.kind {
            handoff::DestinationKind::Chat if destination.linked => "→",
            handoff::DestinationKind::Chat => "·",
            handoff::DestinationKind::Cli => "⌨",
            handoff::DestinationKind::App => "🖥",
        };
        rows.push(vec![(
            format!(
                "{mark} {}",
                relay_core::state::truncate(&destination.label, 46)
            ),
            format!("ho:go:{token}:{ordinal}"),
        )]);
    }
    rows.push(vec![(
        "⬅️ Back".to_string(),
        format!("m:c:{alias}:claude:{idx}"),
    )]);
    (text, keyboard(rows))
}

fn handoff_transcript(pending: &handoff::PendingHandoff) -> Option<std::path::PathBuf> {
    relay_adapters::family::locate(&pending.source_session).map(|found| found.transcript)
}

async fn handoff_run_view(
    token: Option<u32>,
    ordinal: Option<usize>,
) -> (String, serde_json::Value) {
    let home = keyboard(vec![vec![("⬅️ Windows".to_string(), "m:home".to_string())]]);
    let (Some(token), Some(ordinal)) = (token, ordinal) else {
        return ("handoff expired".to_string(), home);
    };
    let Some(pending) = handoff::recall(token) else {
        return ("handoff expired, open the chat again".to_string(), home);
    };
    let Some(target) = pending.targets.get(ordinal).cloned() else {
        return ("target gone".to_string(), home);
    };
    let Some(transcript) = handoff_transcript(&pending) else {
        return (
            "no readable transcript for the source chat".to_string(),
            home,
        );
    };
    let stamp = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();

    let prepared = {
        let pending = pending.clone();
        let transcript = transcript.clone();
        tokio::task::spawn_blocking(move || handoff::prepare(&pending, &transcript)).await
    };
    let brief = match prepared {
        Ok(Ok(brief)) => brief,
        Ok(Err(err)) => return (format!("handoff failed: {err}"), home),
        Err(err) => return (format!("handoff failed: {err}"), home),
    };

    let cfg = automation::AutomationConfig::load();
    let compact = handoff::agent_compact(
        &pending,
        &transcript,
        &brief,
        &cfg.robot.providers,
        automation::now_secs(),
    )
    .await;
    let mut brief = brief;
    brief.compact = compact;

    let built = {
        let pending = pending.clone();
        let target = target.clone();
        let brief = brief.clone();
        tokio::task::spawn_blocking(move || handoff::write_brief(&pending, &brief, &target, &stamp))
            .await
    };
    let built = match built {
        Ok(built) => built,
        Err(err) => return (format!("handoff failed: {err}"), home),
    };

    match built {
        Ok(result) => {
            info!(
                target: "relay::trace",
                stage = "handoff",
                from = %pending.source_alias,
                to = %target.alias,
                bytes = result.bytes,
                proven = result.proven,
                remaining = result.remaining,
                "handoff brief written"
            );
            let compact = match &result.compact_author {
                Some(author) => format!("compact by {author}"),
                None => "no compact (no provider answered)".to_string(),
            };
            let delivery = if let Some((bin, flags)) = &target.cli {
                let unattended = automation::AutomationConfig::load().resolve(
                    Some(&pending.source_session),
                    &pending.source_alias,
                    automation::now_secs(),
                ) != automation::Mode::Manual;
                match handoff::launch_cli(bin, flags, &target.workspace, &result.prompt, unattended)
                {
                    Ok(()) => format!(
                        "⌨ started {bin}, it already has the brief ({}){}",
                        if unattended {
                            "unattended"
                        } else {
                            "asks before each tool"
                        },
                        match handoff::cli_auth_state(bin) {
                            Some(state) => format!("; {state}"),
                            None => String::new(),
                        }
                    ),
                    Err(err) => format!("could not start {bin}: {err}"),
                }
            } else if let Some(app) = target.app.as_deref() {
                match handoff::open_in_app(app, &target.workspace) {
                    Ok(()) => {
                        format!("🖥 opened {app} on this project - paste the prompt into its chat")
                    }
                    Err(err) => format!("could not open {app}: {err}"),
                }
            } else if target.chat.is_some() {
                "→ tap Send prompt to deliver it into the target chat".to_string()
            } else {
                "· target chat is not tapped - paste the prompt yourself".to_string()
            };
            let text = format!(
                "📦 <b>Handed off</b> → {}\n\n<code>{}</code>\n{} proven · {} open · {} bytes · {}\n{}\n\n<b>Prompt for the receiving agent</b>\n<code>{}</code>",
                esc_html(&target.title),
                esc_html(&result.path.display().to_string()),
                result.proven,
                result.remaining,
                result.bytes,
                esc_html(&compact),
                esc_html(&delivery),
                esc_html(&result.prompt)
            );
            let kb = keyboard(vec![
                vec![(
                    "🚀 Send prompt to target".to_string(),
                    format!("ho:inject:{token}:{ordinal}"),
                )],
                vec![(
                    "📥 Check receipt".to_string(),
                    format!("ho:receipt:{token}:{ordinal}"),
                )],
                vec![("⬅️ Back".to_string(), format!("m:w:{}", target.alias))],
            ]);
            (text, kb)
        }
        Err(err) => (format!("handoff failed: {err}"), home),
    }
}

async fn handoff_receipt_view(
    token: Option<u32>,
    ordinal: Option<usize>,
) -> (String, serde_json::Value) {
    let home = keyboard(vec![vec![("⬅️ Windows".to_string(), "m:home".to_string())]]);
    let (Some(token), Some(ordinal)) = (token, ordinal) else {
        return ("handoff expired".to_string(), home);
    };
    let Some(pending) = handoff::recall(token) else {
        return ("handoff expired, open the chat again".to_string(), home);
    };
    let Some(target) = pending.targets.get(ordinal).cloned() else {
        return ("target gone".to_string(), home);
    };
    let kb = keyboard(vec![
        vec![(
            "🔄 Check again".to_string(),
            format!("ho:receipt:{token}:{ordinal}"),
        )],
        vec![("⬅️ Back".to_string(), format!("m:w:{}", target.alias))],
    ]);
    let Some(receipt) = handoff::read_receipt(&target) else {
        return (
            format!(
                "📥 <b>No receipt yet</b> from {}\n\n<i>The receiving agent writes it into HANDOFF.md once it has read the brief and looked at the repository.</i>",
                esc_html(&target.alias)
            ),
            kb,
        );
    };
    let drift = match handoff::contract_of(&target) {
        Some(contract) => {
            let missing = relay_compass::handoff::missing_anchors(&contract, &receipt);
            if missing.is_empty() {
                "✅ no contract anchor is missing from the receipt".to_string()
            } else {
                format!(
                    "⚠️ the receipt never mentions {} - check it before letting it run",
                    esc_html(&missing.join(", "))
                )
            }
        }
        None => "contract section unreadable, drift not checked".to_string(),
    };
    info!(target: "relay::trace", stage = "handoff", to = %target.alias, "handoff receipt read");
    (
        format!(
            "📥 <b>Receipt</b> from {}\n\n{}\n\n{}",
            esc_html(&target.alias),
            esc_html(&relay_core::state::truncate(&receipt, 1200)),
            drift
        ),
        kb,
    )
}

async fn handoff_inject_view(
    token: Option<u32>,
    ordinal: Option<usize>,
) -> (String, serde_json::Value) {
    let home = keyboard(vec![vec![("⬅️ Windows".to_string(), "m:home".to_string())]]);
    let (Some(token), Some(ordinal)) = (token, ordinal) else {
        return ("handoff expired".to_string(), home);
    };
    let Some(pending) = handoff::recall(token) else {
        return ("handoff expired, open the chat again".to_string(), home);
    };
    let Some(target) = pending.targets.get(ordinal).cloned() else {
        return ("target gone".to_string(), home);
    };
    let brief = target.workspace.join("HANDOFF.md");
    if !brief.exists() {
        return ("no brief on disk, hand off again".to_string(), home);
    }
    let prompt = relay_compass::handoff::handoff_prompt(&brief.display().to_string());
    let kb = keyboard(vec![vec![(
        "⬅️ Back".to_string(),
        format!("m:w:{}", target.alias),
    )]]);
    let Some(pid) = target
        .chat
        .as_ref()
        .and_then(|chat| inject::available_pid(&chat.session_id))
    else {
        return (
            format!(
                "The target chat is not tapped, so I cannot type into it. Paste this yourself:\n\n<code>{}</code>",
                esc_html(&prompt)
            ),
            kb,
        );
    };
    match inject::send_user_message(pid, &prompt) {
        Ok(()) => {
            info!(target: "relay::trace", stage = "handoff", to = %target.alias, "handoff prompt injected");
            (format!("🚀 Prompt sent to {}", esc_html(&target.alias)), kb)
        }
        Err(err) => (format!("could not send: {err}"), kb),
    }
}

async fn automation_chats_view(registry: &Registry) -> (String, serde_json::Value) {
    let cfg = automation::AutomationConfig::load();
    let now = automation::now_secs();
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    let mut listed = 0usize;
    for w in registry.read().await.iter() {
        let alias = workspace_alias(&w.workspace);
        for (idx, c) in w.claude.iter().enumerate() {
            let resolved = cfg.resolve(Some(&c.session_id), &alias, now);
            let pin = if cfg.sessions.contains_key(&c.session_id) {
                "📌"
            } else {
                "↳"
            };
            let label = format!(
                "{pin} {} · {} {}",
                mode_title(resolved),
                state_glyph(c.state.label()),
                relay_core::state::truncate(&claude_display(c), 32)
            );
            rows.push(vec![(label, format!("au:s:{alias}:{idx}"))]);
            listed += 1;
        }
    }
    let text = if listed == 0 {
        "🔀 <b>Chat modes</b>\n\nNo live chat found.".to_string()
    } else {
        format!(
            "🔀 <b>Chat modes</b>\nDefault: <b>{}</b> · live chats: {listed}\n\n<i>Pick a chat to switch it live. 📌 is pinned, ↳ inherits the default.</i>",
            mode_title(cfg.default)
        )
    };
    rows.push(vec![
        ("🔄".to_string(), "au:chats".to_string()),
        ("⬅️ Back".to_string(), "au:home".to_string()),
    ]);
    (text, keyboard(rows))
}

fn strategy_short(s: automation::Strategy) -> &'static str {
    match s {
        automation::Strategy::Single => "Single",
        automation::Strategy::Priority => "Priority",
        automation::Strategy::RoundRobin => "RR",
        automation::Strategy::CostOptimized => "Cost",
    }
}

async fn automation_providers_view() -> (String, serde_json::Value) {
    let cfg = automation::AutomationConfig::load();
    let prov = &cfg.robot.providers;
    let discovered = supervisor::discover::discover_all().await;
    let by_id: std::collections::HashMap<&str, &supervisor::discover::Discovered> =
        discovered.iter().map(|d| (d.id.as_str(), d)).collect();

    let mut text = format!(
        "🔌 <b>Robot providers</b>\nStrategy: <b>{}</b>\n\n☑ enabled · ✓ available\n",
        prov.strategy.label()
    );
    let mut toggle_rows: Vec<Vec<(String, String)>> = Vec::new();
    let mut pair: Vec<(String, String)> = Vec::new();
    for b in supervisor::discover::Backend::all() {
        let id = b.id();
        let d = by_id.get(id);
        let avail = d.map(|x| x.available).unwrap_or(false);
        let enabled = prov.is_enabled(id);
        let model = prov.per_provider.get(id).and_then(|o| o.model.clone());
        text.push_str(&format!(
            "{} {} <code>{}</code>{}\n",
            if enabled { "☑" } else { "☐" },
            if avail { "✓" } else { "·" },
            esc_html(id),
            model
                .map(|m| format!(" [{}]", esc_html(&m)))
                .unwrap_or_default()
        ));
        pair.push((
            format!("{} {id}", if enabled { "☑" } else { "☐" }),
            format!("au:pt:{id}"),
        ));
        if pair.len() == 2 {
            toggle_rows.push(std::mem::take(&mut pair));
        }
    }
    if !pair.is_empty() {
        toggle_rows.push(pair);
    }

    let strat_row: Vec<(String, String)> = automation::Strategy::all()
        .into_iter()
        .map(|s| {
            let mark = if prov.strategy == s { "● " } else { "" };
            (
                format!("{mark}{}", strategy_short(s)),
                format!("au:ps:{}", s.label()),
            )
        })
        .collect();
    toggle_rows.push(strat_row);
    toggle_rows.push(vec![
        ("🔄".to_string(), "au:prov".to_string()),
        ("⬅️ Back".to_string(), "au:home".to_string()),
    ]);
    text.push_str("\n<i>Set a model: /auto model &lt;provider&gt; &lt;model&gt;</i>");
    (text, keyboard(toggle_rows))
}

async fn claude_session_id(registry: &Registry, alias: &str, idx: usize) -> Option<String> {
    registry
        .read()
        .await
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)
        .and_then(|w| w.claude.get(idx))
        .map(|c| c.session_id.clone())
}

async fn chat_transcript(
    registry: &Registry,
    alias: &str,
    kind: char,
    idx: usize,
) -> Option<(String, std::path::PathBuf)> {
    let windows = registry.read().await;
    let w = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)?;
    match kind {
        'x' => w
            .codex
            .get(idx)
            .map(|cx| (cx.thread_id.clone(), cx.rollout_path.clone())),
        _ => w
            .claude
            .get(idx)
            .map(|c| (c.session_id.clone(), c.jsonl_path.clone())),
    }
}

fn parse_chat_ref(spec: &str) -> Option<(String, char, usize)> {
    let (head, idx) = spec.rsplit_once(':')?;
    let idx = idx.parse().ok()?;
    match head.rsplit_once(':') {
        Some((alias, "c")) => Some((alias.to_string(), 'c', idx)),
        Some((alias, "x")) => Some((alias.to_string(), 'x', idx)),
        _ => Some((head.to_string(), 'c', idx)),
    }
}

fn chat_family(kind: char) -> relay_adapters::family::Family {
    if kind == 'x' {
        relay_adapters::family::Family::Codex
    } else {
        relay_adapters::family::Family::Claude
    }
}

fn chat_card_callback(alias: &str, kind: char, idx: usize) -> String {
    format!(
        "m:c:{alias}:{}:{idx}",
        if kind == 'x' { "codex" } else { "claude" }
    )
}

fn review_cadence_label(secs: i64) -> String {
    if secs % 3600 == 0 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", secs / 60)
    }
}

fn next_review_cadence(secs: i64) -> i64 {
    match secs {
        s if s < 300 => 300,
        s if s < 900 => 900,
        s if s < 1800 => 1800,
        s if s < 3600 => 3600,
        _ => 300,
    }
}

fn next_review_depth(depth: automation::ReviewDepth) -> automation::ReviewDepth {
    match depth {
        automation::ReviewDepth::Shallow => automation::ReviewDepth::Normal,
        automation::ReviewDepth::Normal => automation::ReviewDepth::Deep,
        automation::ReviewDepth::Deep => automation::ReviewDepth::Shallow,
    }
}

async fn review_reviewers_view() -> (String, serde_json::Value) {
    let cfg = automation::AutomationConfig::load();
    let discovered = supervisor::discover::discover_all().await;
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    for backend in supervisor::discover::Backend::all() {
        let id = backend.id();
        let picked = cfg.review.reviewers.iter().any(|name| name == id);
        let available = discovered
            .iter()
            .any(|found| found.id == id && found.available);
        let order = cfg
            .review
            .reviewers
            .iter()
            .position(|name| name == id)
            .map(|at| format!("{}. ", at + 1))
            .unwrap_or_default();
        rows.push(vec![(
            format!(
                "{} {order}{id}{}",
                if picked { "✅" } else { "▫️" },
                if available { "" } else { " (not installed)" }
            ),
            format!("au:rp:{id}"),
        )]);
    }
    let text = format!(
        "🔍 <b>Reviewers</b>\nAsked in this order: <b>{}</b>\n\n<i>A reviewer is never asked to grade its own family, so Claude chats skip claude-cli and Codex chats skip codex-cli.</i>",
        if cfg.review.reviewers.is_empty() {
            "nobody".to_string()
        } else {
            cfg.review.reviewers.join(" → ")
        }
    );
    rows.push(vec![("⬅️ Back".to_string(), "au:home".to_string())]);
    (text, keyboard(rows))
}

async fn session_auto_view(
    registry: &Registry,
    alias: &str,
    idx: usize,
) -> (String, serde_json::Value) {
    let back = || format!("m:c:{alias}:claude:{idx}");
    let Some(sid) = claude_session_id(registry, alias, idx).await else {
        return (
            format!("chat gone in {alias}"),
            keyboard(vec![vec![("⬅️ Back".to_string(), back())]]),
        );
    };
    let cfg = automation::AutomationConfig::load();
    let resolved = cfg.resolve(Some(&sid), alias, automation::now_secs());
    let pinned = cfg.sessions.contains_key(&sid);
    let text = format!(
        "🤖 <b>Automation</b> · {}\nThis chat: <b>{}</b> ({})\n\n<i>Pin this chat to a mode, or clear to inherit.</i>",
        esc_html(alias),
        mode_title(resolved),
        if pinned { "pinned" } else { "inherited" }
    );
    let btn = |m: automation::Mode| {
        let mark = if pinned && resolved == m { "● " } else { "" };
        (
            format!("{mark}{}", mode_title(m)),
            format!("au:ss:{alias}:{idx}:{}", m.label()),
        )
    };
    let kb = keyboard(vec![
        vec![
            btn(automation::Mode::Manual),
            btn(automation::Mode::Auto),
            btn(automation::Mode::Robot),
        ],
        vec![
            (
                "🧹 Clear (inherit)".to_string(),
                format!("au:sx:{alias}:{idx}"),
            ),
            ("📖 Rules".to_string(), "au:rules".to_string()),
        ],
        vec![(
            "🔍 Cross review now".to_string(),
            format!("au:rnow:{alias}:c:{idx}"),
        )],
        vec![
            ("🔀 Chats".to_string(), "au:chats".to_string()),
            ("⬅️ Back".to_string(), back()),
        ],
    ]);
    (text, kb)
}

async fn deliver_review_correction(
    family: relay_adapters::family::Family,
    session_id: &str,
    alias: &str,
    correction: &str,
    ctl: &Arc<dyn Control>,
) -> String {
    if !supervisor::robot::steer_is_safe(correction) {
        decision_log::record(
            "review",
            "correction_unsafe",
            serde_json::json!({"alias": alias, "manual": true}),
        );
        return "Held back: it asked to widen scope or permissions.".to_string();
    }
    let text = correction.to_string();
    let sent = if family == relay_adapters::family::Family::Codex {
        let control = Arc::clone(ctl);
        let target = alias.to_string();
        tokio::task::spawn_blocking(move || control.send_prompt(&target, AgentKind::Codex, &text))
            .await
    } else {
        let Some(pid) = inject::available_pid(session_id) else {
            decision_log::record(
                "review",
                "correction_undeliverable",
                serde_json::json!({"alias": alias, "manual": true,
                    "reason": "the chat has no live injection channel"}),
            );
            return "Not sent: this chat has no background channel (no shim).".to_string();
        };
        tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text)).await
    };
    match sent {
        Ok(Ok(())) => {
            decision_log::record(
                "review",
                "corrected",
                serde_json::json!({"alias": alias, "manual": true, "reviewed": family.id()}),
            );
            "Sent into the chat.".to_string()
        }
        Ok(Err(error)) => {
            decision_log::record(
                "review",
                "correction_failed",
                serde_json::json!({"alias": alias, "manual": true,
                    "reason": error.to_string()}),
            );
            format!("Could not send it: {error}")
        }
        Err(error) => format!("Could not send it: {error}"),
    }
}

struct ReviewOutcome {
    reviewer: &'static str,
    depth: automation::ReviewDepth,
    review: supervisor::review::Review,
    delivery: Option<String>,
}

async fn perform_review(
    family: relay_adapters::family::Family,
    session_id: &str,
    alias: &str,
    transcript: std::path::PathBuf,
    reviewer: Option<&str>,
    depth: Option<automation::ReviewDepth>,
    ctl: &Arc<dyn Control>,
) -> anyhow::Result<ReviewOutcome> {
    use supervisor::review;

    let cfg = automation::AutomationConfig::load();
    let depth = depth.unwrap_or(cfg.review.depth);
    let providers = review::chosen_reviewers(family, &cfg.review, &cfg.robot.providers, reviewer);
    if providers.enabled.is_empty() {
        anyhow::bail!("no reviewer is enabled for a {} chat", family.label());
    }
    let (goal, tail) =
        tokio::task::spawn_blocking(move || review::session_view(family, &transcript, depth))
            .await?;
    if tail.is_empty() {
        anyhow::bail!("nothing readable in that chat yet");
    }
    let prompt = review::review_prompt(family, goal.as_deref(), &tail, depth);
    let discovered = supervisor::discover::discover_all().await;
    let pool = supervisor::pool::Pool::new();
    let (backend, verdict) = review::ask_reviewer(
        &pool,
        session_id,
        &providers,
        &discovered,
        &prompt,
        automation::now_secs(),
    )
    .await?;
    decision_log::record(
        "review",
        verdict.verdict.label(),
        serde_json::json!({
            "alias": alias,
            "reviewer": backend.id(),
            "reviewed": family.id(),
            "depth": depth.label(),
            "manual": true,
            "picked": reviewer,
            "goal_known": goal.is_some(),
            "goal_restated": verdict.goal_restated,
            "gaps": verdict.gaps,
            "confidence": verdict.confidence,
            "has_correction": verdict.correction.is_some(),
        }),
    );
    let delivery = match verdict.correction.as_deref() {
        Some(text) => Some(deliver_review_correction(family, session_id, alias, text, ctl).await),
        None => None,
    };
    Ok(ReviewOutcome {
        reviewer: backend.id(),
        depth,
        review: verdict,
        delivery,
    })
}

fn review_outcome_html(alias: &str, outcome: &ReviewOutcome) -> String {
    let gaps = if outcome.review.gaps.is_empty() {
        String::new()
    } else {
        format!(
            "\n\n<b>Gaps</b>\n{}",
            outcome
                .review
                .gaps
                .iter()
                .map(|gap| format!("• {}", esc_html(gap)))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let correction = match (
        outcome.review.correction.as_deref(),
        outcome.delivery.as_deref(),
    ) {
        (Some(text), Some(delivery)) => format!(
            "\n\n<b>Correction</b>\n{}\n\n{}",
            esc_html(text),
            esc_html(delivery)
        ),
        (Some(text), None) => format!("\n\n<b>Correction</b>\n{}", esc_html(text)),
        (None, _) => "\n\n<i>Nothing to correct, the chat was left alone.</i>".to_string(),
    };
    format!(
        "🔍 <b>Cross review</b> · {}\nReviewer: <b>{}</b> · depth {}\nVerdict: <b>{}</b>\n\n<b>Task as the reviewer read it</b>\n{}{gaps}{correction}",
        esc_html(alias),
        esc_html(outcome.reviewer),
        outcome.depth.label(),
        esc_html(outcome.review.verdict.label()),
        esc_html(&outcome.review.goal_restated),
    )
}

async fn session_review_menu(
    alias: &str,
    kind: char,
    idx: usize,
    depth: Option<automation::ReviewDepth>,
) -> (String, serde_json::Value) {
    use supervisor::review;

    let family = chat_family(kind);
    let cfg = automation::AutomationConfig::load();
    let depth = depth.unwrap_or(cfg.review.depth);
    let depth_code = review::depth_code(depth);
    let discovered = supervisor::discover::discover_all().await;
    let default_order =
        review::chosen_reviewers(family, &cfg.review, &cfg.robot.providers, None).enabled;
    let base = format!("{alias}:{kind}:{idx}");
    let mut rows: Vec<Vec<(String, String)>> = Vec::new();
    rows.push(vec![(
        format!(
            "▶️ Default: {}",
            if default_order.is_empty() {
                "nobody enabled".to_string()
            } else {
                default_order.join(" → ")
            }
        ),
        format!("au:rr:{base}:p{depth_code}"),
    )]);
    let mut pair: Vec<(String, String)> = Vec::new();
    for id in review::menu_reviewers(family) {
        let Some(code) = review::reviewer_code(id) else {
            continue;
        };
        let installed = discovered
            .iter()
            .any(|found| found.id == id && found.available);
        let label = if installed {
            format!("👤 {id}")
        } else {
            format!("▫️ {id} (not installed)")
        };
        pair.push((label, format!("au:rr:{base}:{code}{depth_code}")));
        if pair.len() == 2 {
            rows.push(std::mem::take(&mut pair));
        }
    }
    if !pair.is_empty() {
        rows.push(pair);
    }
    rows.push(
        [
            automation::ReviewDepth::Shallow,
            automation::ReviewDepth::Normal,
            automation::ReviewDepth::Deep,
        ]
        .into_iter()
        .map(|choice| {
            (
                format!(
                    "{}{}",
                    if choice == depth { "● " } else { "" },
                    choice.label()
                ),
                format!("au:rm:{base}:{}", review::depth_code(choice)),
            )
        })
        .collect(),
    );
    rows.push(vec![(
        "⬅️ Back".to_string(),
        chat_card_callback(alias, kind, idx),
    )]);
    let text = format!(
        "🔍 <b>Cross review</b> · {}\nWho should review this {} chat, and how deep?\n\nDepth: <b>{}</b> ({} recent messages)\n\n<i>The verdict comes back as a separate message and any correction goes straight into the chat. A chat is never reviewed by its own family.</i>",
        esc_html(alias),
        family.label(),
        depth.label(),
        depth.tail_messages()
    );
    (text, keyboard(rows))
}

#[allow(clippy::too_many_arguments)]
async fn session_review_run(
    registry: &Registry,
    alias: &str,
    kind: char,
    idx: usize,
    reviewer: Option<&'static str>,
    depth: Option<automation::ReviewDepth>,
    tg: Arc<Telegram>,
    chat: Option<i64>,
    ctl: Arc<dyn Control>,
) -> (String, serde_json::Value) {
    let family = chat_family(kind);
    let back = keyboard(vec![vec![
        ("⬅️ Back".to_string(), chat_card_callback(alias, kind, idx)),
        (
            "🔍 Again".to_string(),
            format!("au:rnow:{alias}:{kind}:{idx}"),
        ),
    ]]);
    let Some((session_id, transcript)) = chat_transcript(registry, alias, kind, idx).await else {
        return (format!("chat gone in {alias}"), back);
    };
    let cfg = automation::AutomationConfig::load();
    let asked =
        supervisor::review::chosen_reviewers(family, &cfg.review, &cfg.robot.providers, reviewer)
            .enabled;
    if asked.is_empty() {
        return (
            "🔍 <b>Cross review</b>\n\nNo reviewer is enabled for this chat. Pick one under Reviewers."
                .to_string(),
            back,
        );
    }
    let running = format!(
        "🔍 <b>Cross review</b> · {}\nAsking <b>{}</b>, depth <b>{}</b>, whether this {} chat still serves your last instruction.\n\n<i>The verdict arrives as a separate message in about a minute, and any correction is sent straight into the chat.</i>",
        esc_html(alias),
        esc_html(&asked.join(" → ")),
        depth.unwrap_or(cfg.review.depth).label(),
        family.label()
    );
    let alias_owned = alias.to_string();
    tokio::spawn(async move {
        let text = match perform_review(
            family,
            &session_id,
            &alias_owned,
            transcript,
            reviewer,
            depth,
            &ctl,
        )
        .await
        {
            Ok(outcome) => review_outcome_html(&alias_owned, &outcome),
            Err(error) => format!(
                "🔍 <b>Cross review</b> · {}\n\nNo reviewer answered: {}",
                esc_html(&alias_owned),
                esc_html(&format!("{error:#}"))
            ),
        };
        if let Some(chat) = chat {
            let _ = tg.send(chat, &text, None).await;
        }
    });
    (running, back)
}

fn bot_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        ("menu", "Windows and chats as buttons"),
        ("windows", "List open windows and chats"),
        ("status", "Details for a workspace"),
        ("say", "Send a prompt to a chat"),
        ("stop", "Interrupt the current turn"),
        ("cont", "Continue the chat"),
        ("mode", "Cycle the Claude permission mode"),
        ("auto", "Automation: manual / auto / robot"),
        ("slash", "Send a slash command to a chat"),
        ("focus", "Raise the window on screen"),
        ("danger", "Manage the blocked-command list"),
        ("auth", "Pair this chat with a key"),
        ("help", "Show all commands"),
    ]
}

fn help_text() -> String {
    "<b>vsc-relay</b>\n\
     /menu - interactive buttons (recommended)\n\
     /auth &lt;key&gt; - pair this chat\n\
     /windows - list windows &amp; chats\n\
     /status &lt;ws&gt; - details for a workspace\n\
     /say &lt;ws&gt; &lt;claude|codex&gt; &lt;text&gt; - send a prompt\n\
     /stop &lt;ws&gt; &lt;agent&gt; - interrupt\n\
     /cont &lt;ws&gt; &lt;agent&gt; - continue\n\
     /mode &lt;ws&gt; - cycle Claude mode\n\
     /auto [manual|auto|robot] - automation modes\n\
     /slash &lt;ws&gt; &lt;agent&gt; &lt;cmd&gt; - send a slash command\n\
     /focus &lt;ws&gt; - raise the window\n\
     send or forward a photo, voice, video, or file - it is staged locally and the \
     path is handed to the active session (or pick a target)"
        .to_string()
}

#[cfg(test)]
mod model_tests {
    use super::{model_friendly, model_tier};

    #[test]
    fn maps_model_ids() {
        assert_eq!(model_tier("claude-opus-4-8"), "opus");
        assert_eq!(model_friendly("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_friendly("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_friendly("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_tier("claude-fable-5"), "fable");
        assert_eq!(model_friendly("claude-fable-5"), "Fable 5");
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn ctx_mode(mode: Option<&str>) -> WinCtx {
        WinCtx {
            machine_id: MachineId("m".into()),
            workspace: PathBuf::from("/tmp/ws"),
            branch: None,
            agent: AgentKind::ClaudeCode,
            session_ref: "sess".into(),
            title: None,
            usage: None,
            mode: mode.map(|s| s.to_string()),
        }
    }

    fn seeded(ctx: &WinCtx, mode: Option<&str>) -> HashMap<String, Track> {
        let mut tracker = HashMap::new();
        tracker.insert(
            key_of(ctx),
            Track {
                label: "idle".into(),
                fingerprint: None,
                mode: mode.map(|s| s.to_string()),
            },
        );
        tracker
    }

    fn find_mode_change(kinds: &[EventKind]) -> Option<(String, String, bool)> {
        kinds.iter().find_map(|k| match k {
            EventKind::ModeChanged { from, to, alert } => Some((from.clone(), to.clone(), *alert)),
            _ => None,
        })
    }

    fn run(tracker: &mut HashMap<String, Track>, ctx: &WinCtx) -> Vec<EventKind> {
        claude_events(
            tracker,
            ctx,
            &ClaudeState::Idle,
            "cli".into(),
            String::new(),
            false,
        )
    }

    #[test]
    fn first_observed_bypass_alerts_from_unknown() {
        let ctx = ctx_mode(Some("bypassPermissions"));
        let mut tracker = HashMap::new();
        let mc = find_mode_change(&run(&mut tracker, &ctx)).expect("mode change");
        assert_eq!(mc, ("unknown".into(), "bypassPermissions".into(), true));
    }

    #[test]
    fn default_to_accept_edits_alerts() {
        let ctx = ctx_mode(Some("acceptEdits"));
        let mut tracker = seeded(&ctx, Some("default"));
        let mc = find_mode_change(&run(&mut tracker, &ctx)).expect("mode change");
        assert_eq!(mc, ("default".into(), "acceptEdits".into(), true));
    }

    #[test]
    fn default_to_plan_no_alert() {
        let ctx = ctx_mode(Some("plan"));
        let mut tracker = seeded(&ctx, Some("default"));
        let mc = find_mode_change(&run(&mut tracker, &ctx)).expect("mode change");
        assert_eq!(mc, ("default".into(), "plan".into(), false));
    }

    #[test]
    fn unchanged_mode_emits_nothing() {
        let ctx = ctx_mode(Some("default"));
        let mut tracker = seeded(&ctx, Some("default"));
        assert!(find_mode_change(&run(&mut tracker, &ctx)).is_none());
    }

    #[test]
    fn pending_gui_receipt_recovers_completion_after_daemon_restart() {
        let ctx = ctx_mode(Some("default"));
        let mut tracker = HashMap::new();
        let kinds = claude_events(
            &mut tracker,
            &ctx,
            &ClaudeState::Idle,
            "cli".into(),
            "finished".into(),
            true,
        );
        assert!(matches!(
            kinds.as_slice(),
            [
                EventKind::SessionStarted { .. },
                EventKind::TurnComplete { .. }
            ]
        ));
    }
}

#[cfg(test)]
mod compass_autoaction_tests {
    use super::*;

    #[test]
    fn identical_compass_assessment_is_emitted_once_then_suppressed() {
        let session = "regress-compass-dedup-unique-session";
        let mut kinds: Vec<EventKind> = Vec::new();
        let staged = "ask_proof on stale_evidence (posterior 0.76)".to_string();
        push_compass_autoaction(&mut kinds, session, "compass_staged", staged.clone());
        push_compass_autoaction(&mut kinds, session, "compass_staged", staged.clone());
        push_compass_autoaction(&mut kinds, session, "compass_staged", staged);
        assert_eq!(
            kinds.len(),
            1,
            "identical staged assessment must not repeat"
        );

        push_compass_autoaction(
            &mut kinds,
            session,
            "compass_staged",
            "ask_proof on completion_gap (posterior 0.90)".to_string(),
        );
        assert_eq!(kinds.len(), 2, "a changed assessment must notify again");

        let risk = "risk 0.83; review the session goal and recent progress".to_string();
        push_compass_autoaction(&mut kinds, session, "compass_risk", risk.clone());
        push_compass_autoaction(&mut kinds, session, "compass_risk", risk);
        assert_eq!(
            kinds.len(),
            3,
            "compass_risk deduped independently of compass_staged"
        );
    }
}

#[cfg(test)]
mod session_card_tests {
    use super::*;

    fn card(id: &str, tapped: bool, act: Option<u64>) -> SessionCard {
        SessionCard {
            alias: "a".into(),
            agent: "claude".into(),
            session_id: id.into(),
            title: None,
            workspace: "w".into(),
            branch: None,
            state: "idle".into(),
            mode: "manual".into(),
            tapped,
            rewrite: false,
            last_activity_secs: act,
        }
    }

    #[test]
    fn dedup_keeps_the_best_card_per_session_id_and_preserves_first_seen_order() {
        let cards = vec![
            card("A", false, Some(10)),
            card("B", false, Some(5)),
            card("A", true, Some(1)),
            card("B", false, Some(99)),
        ];
        let out = dedup_session_cards(cards);
        assert_eq!(out.len(), 2, "duplicate session ids collapse to one card");
        assert_eq!(out[0].session_id, "A");
        assert_eq!(out[1].session_id, "B");
        assert!(
            out[0].tapped,
            "a tapped (live) card wins over an untapped one"
        );
        assert_eq!(
            out[1].last_activity_secs,
            Some(99),
            "with equal tapped state the more recently active card wins"
        );
    }

    #[test]
    fn dedup_is_a_noop_for_unique_ids() {
        let out = dedup_session_cards(vec![card("X", false, None), card("Y", true, Some(3))]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].session_id, "X");
        assert_eq!(out[1].session_id, "Y");
    }
}

async fn review_probe(family: &str, transcript: &str) -> anyhow::Result<()> {
    use relay_adapters::family::Family;
    use supervisor::review;

    let family = Family::parse(family)
        .ok_or_else(|| anyhow::anyhow!("unknown agent family {family}; use claude or codex"))?;
    let path = std::path::PathBuf::from(transcript);
    if !path.is_file() {
        anyhow::bail!("no transcript at {}", path.display());
    }
    let cfg = automation::AutomationConfig::load();
    let depth = cfg.review.depth;
    let (goal, tail) = review::session_view(family, &path, depth);
    if tail.is_empty() {
        anyhow::bail!("nothing readable in {}", path.display());
    }
    let providers = review::reviewer_providers(family, &cfg.review, &cfg.robot.providers);
    println!(
        "reviewing {} with {} (depth {}, {} recent messages, goal {})",
        family.label(),
        if providers.enabled.is_empty() {
            "nobody".to_string()
        } else {
            providers.enabled.join(" then ")
        },
        depth.label(),
        tail.len(),
        if goal.is_some() { "known" } else { "unknown" },
    );
    let discovered = supervisor::discover::discover_all().await;
    let prompt = review::review_prompt(family, goal.as_deref(), &tail, depth);
    let pool = supervisor::pool::Pool::new();
    let (backend, review) = review::ask_reviewer(
        &pool,
        "review-probe",
        &providers,
        &discovered,
        &prompt,
        automation::now_secs(),
    )
    .await?;
    println!("reviewer: {}", backend.id());
    println!("verdict: {}", review.verdict.label());
    println!("goal as the reviewer read it: {}", review.goal_restated);
    for gap in &review.gaps {
        println!("gap: {gap}");
    }
    match review.correction.as_deref() {
        Some(correction) => println!("correction: {correction}"),
        None => println!("correction: none"),
    }
    if let Some(confidence) = review.confidence {
        println!("confidence: {confidence:.2}");
    }
    Ok(())
}

async fn review_run(
    session_id: &str,
    reviewer: Option<&str>,
    depth: Option<&str>,
) -> anyhow::Result<()> {
    use relay_adapters::family::Family;

    let paths = Paths::discover()?;
    let mut found: Option<(String, std::path::PathBuf, Family)> = None;
    for w in scan(&paths) {
        for c in &w.claude {
            if c.session_id == session_id {
                found = Some((
                    workspace_alias(&w.workspace),
                    c.jsonl_path.clone(),
                    Family::Claude,
                ));
            }
        }
        for cx in &w.codex {
            if cx.thread_id == session_id {
                found = Some((
                    workspace_alias(&w.workspace),
                    cx.rollout_path.clone(),
                    Family::Codex,
                ));
            }
        }
    }
    let Some((alias, transcript, family)) = found else {
        anyhow::bail!("no live Claude or Codex chat with id {session_id}");
    };
    let reviewer = match reviewer {
        Some(id) => Some(
            supervisor::discover::Backend::parse(id)
                .map(|backend| backend.id())
                .ok_or_else(|| anyhow::anyhow!("unknown reviewer {id}"))?,
        ),
        None => None,
    };
    let depth = match depth {
        Some(raw) => Some(
            automation::ReviewDepth::parse(raw)
                .ok_or_else(|| anyhow::anyhow!("bad depth {raw}; use shallow, normal or deep"))?,
        ),
        None => None,
    };
    let ctl: Arc<dyn Control> = Arc::from(relay_control::platform());
    println!("reviewing {alias} ({})", family.label());
    let outcome = perform_review(
        family, session_id, &alias, transcript, reviewer, depth, &ctl,
    )
    .await?;
    println!("reviewer: {}", outcome.reviewer);
    println!("depth: {}", outcome.depth.label());
    println!("verdict: {}", outcome.review.verdict.label());
    println!(
        "task as the reviewer read it: {}",
        outcome.review.goal_restated
    );
    for gap in &outcome.review.gaps {
        println!("gap: {gap}");
    }
    match (
        outcome.review.correction.as_deref(),
        outcome.delivery.as_deref(),
    ) {
        (Some(correction), Some(delivery)) => {
            println!("correction: {correction}");
            println!("delivery: {delivery}");
        }
        (Some(correction), None) => println!("correction: {correction}"),
        (None, _) => println!("correction: none, the chat was left alone"),
    }
    Ok(())
}
