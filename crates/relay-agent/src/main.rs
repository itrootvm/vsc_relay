mod actions;
mod auth;
mod config;
mod control_cli;
mod fsutil;
mod hooks;
mod hostname;
mod ingress;
mod inject;
mod install;
mod permission;
mod question;
mod shimctl;
mod stats;
mod telegram;
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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use telegram::{esc_html, keyboard, Telegram};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

type Registry = Arc<RwLock<Vec<WindowEntry>>>;
type Active = Arc<RwLock<HashMap<i64, (String, String)>>>;

struct DaemonArgs {
    once: bool,
    json: bool,
    interval: Option<u64>,
    codex_max_age_ms: Option<i64>,
}

struct Emitted {
    alias: String,
    machine: String,
    event: RelayEvent,
}

#[derive(Clone)]
struct Track {
    label: String,
    fingerprint: Option<String>,
}

struct WinCtx {
    machine_id: MachineId,
    workspace: PathBuf,
    branch: Option<String>,
    agent: AgentKind,
    session_ref: String,
    title: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = raw.first() {
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
    }

    let args = parse_daemon_args(&raw)?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .with_ansi(false)
        .init();

    let mut cfg = Config::from_env();
    if let Some(interval) = args.interval {
        cfg.interval = interval;
    }
    if let Some(max_age) = args.codex_max_age_ms {
        cfg.codex_max_age_ms = max_age;
    }
    let paths = Paths::discover()?;
    let machine_id = MachineId(cfg.machine_name.clone());
    info!(machine = %cfg.machine_name, telegram = cfg.telegram_token.is_some(), "vsc-relay-agent starting");

    let registry: Registry = Arc::new(RwLock::new(Vec::new()));
    let active: Active = Arc::new(RwLock::new(HashMap::new()));
    let (tx, mut rx) = mpsc::unbounded_channel::<Emitted>();
    let ctl: Arc<dyn Control> = Arc::from(relay_control::platform());
    let pending: ingress::Pending = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let auth = Arc::new(auth::Auth::load(
        cfg.allowed_chats.clone(),
        cfg.pair_secret.clone(),
    ));

    fsutil::secure_dir(
        &dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
            .join(".vsc-relay"),
    );

    if args.once {
        let mut tracker = HashMap::new();
        scan_and_emit(
            &paths,
            &machine_id,
            cfg.codex_max_age_ms,
            &registry,
            &mut tracker,
            &tx,
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

    let watch = {
        let paths = paths.clone();
        let machine_id = machine_id.clone();
        let registry = registry.clone();
        let fallback = cfg.interval.max(15);
        let max_age = cfg.codex_max_age_ms;
        tokio::spawn(async move {
            let mut tracker = HashMap::new();
            let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
            let _watcher = spawn_watcher(&paths, fs_tx);
            if _watcher.is_none() {
                warn!("fs watcher unavailable; falling back to polling every {fallback}s");
            }
            loop {
                scan_and_emit(&paths, &machine_id, max_age, &registry, &mut tracker, &tx).await;
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
            warn!("setMyCommands failed: {e}");
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

        let forwarder = {
            let tg = tg.clone();
            let auth = auth.clone();
            tokio::spawn(async move {
                while let Some(em) = rx.recv().await {
                    info!("relay-event {} {}", em.event.kind.tag(), em.alias);
                    stats::record(em.event.kind.tag());
                    if matches!(em.event.kind, EventKind::SessionStarted { .. }) {
                        continue;
                    }
                    let (text, kb) = format_event(&em.machine, &em.alias, &em.event);
                    let silent = !em.event.actionable;
                    for chat in auth.recipients().await {
                        if let Err(e) = tg.send_ex(chat, &text, kb.clone(), silent).await {
                            warn!("send notify failed: {e}");
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
        }));

        let questions = question::new();
        let perms = permission::new();
        tokio::spawn(question::start(
            questions.clone(),
            perms.clone(),
            tg.clone(),
            auth.clone(),
        ));
        tokio::spawn(updates::watch(tg.clone(), auth.clone()));
        tokio::spawn(updates::marketplace_watch(tg.clone(), auth.clone()));

        let poller = tokio::spawn(poll_commands(
            tg.clone(),
            registry.clone(),
            active.clone(),
            ctl.clone(),
            auth.clone(),
            pending.clone(),
            questions.clone(),
            perms.clone(),
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

fn parse_daemon_args(args: &[String]) -> anyhow::Result<DaemonArgs> {
    let mut parsed = DaemonArgs {
        once: false,
        json: false,
        interval: None,
        codex_max_age_ms: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
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
) {
    let mut windows = scan(paths);
    let now_ms = Utc::now().timestamp_millis();

    for w in &mut windows {
        let branch = w.git_branch.clone();
        let alias = workspace_alias(&w.workspace);

        for c in &mut w.claude {
            let read = if c.jsonl_path.exists() {
                claude::read_state(&c.jsonl_path, c.pid).ok()
            } else {
                None
            };
            let (state, ai_title, git_b, excerpt) = match read {
                Some(res) => (
                    res.state.clone(),
                    res.reduction.ai_title.clone(),
                    res.reduction.git_branch.clone(),
                    res.reduction
                        .last_assistant_text
                        .clone()
                        .unwrap_or_default(),
                ),
                None => (ClaudeState::Idle, None, None, String::new()),
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
            };
            let entrypoint = c.entrypoint.clone().unwrap_or_default();
            let kinds = claude_events(tracker, &ctx, &state, entrypoint, excerpt);
            emit_all(
                tracker,
                &ctx,
                &alias,
                &machine_id.0,
                kinds,
                state.label().to_string(),
                tx,
            );
        }

        let attaches =
            codex::attach_all(&paths.codex_state_db(), &w.workspace, max_age_ms, now_ms, 8);
        let mut codex_agents = Vec::new();
        for a in attaches {
            let ctx = WinCtx {
                machine_id: machine_id.clone(),
                workspace: w.workspace.clone(),
                branch: branch.clone(),
                agent: AgentKind::Codex,
                session_ref: a.agent.thread_id.clone(),
                title: Some(a.agent.title.clone()),
            };
            let excerpt = a.last_message.clone().unwrap_or_default();
            let cur_label = a.agent.state.label().to_string();
            let kinds = codex_events(tracker, &ctx, &a.agent.state, excerpt, a.last_duration_ms);
            emit_all(tracker, &ctx, &alias, &machine_id.0, kinds, cur_label, tx);
            codex_agents.push(a.agent);
        }
        w.codex = codex_agents;
    }

    *registry.write().await = windows;
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
) -> Vec<EventKind> {
    let prev = tracker.get(&key_of(ctx)).cloned();
    let mut kinds = Vec::new();
    if prev.is_none() {
        kinds.push(EventKind::SessionStarted { entrypoint });
    }
    let prev_label = prev.as_ref().map(|t| t.label.clone());
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
        ClaudeState::Idle if prev_label.as_deref().map(is_busy).unwrap_or(false) => {
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
) {
    let key = key_of(ctx);
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
        let _ = tx.send(Emitted {
            alias: alias.to_string(),
            machine: machine.to_string(),
            event,
        });
        tracker.insert(
            key.clone(),
            Track {
                label: cur_label.clone(),
                fingerprint: Some(fp),
            },
        );
    }
    tracker
        .entry(key)
        .and_modify(|t| t.label = cur_label.clone())
        .or_insert(Track {
            label: cur_label,
            fingerprint: None,
        });
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
        EventKind::SubagentActivity { count } => format!("subagents: {count}"),
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
        _ => "▫️",
    };
    let branch = e.git_branch.as_deref().unwrap_or("-");
    let name = e.title.as_deref().unwrap_or("-");
    let mut text = format!(
        "{icon} <b>{}</b> · {}\n📁 <code>{}</code> ⌥ {}\n{} <b>{}</b>\n{}",
        e.agent,
        esc_html(machine),
        esc_html(alias),
        esc_html(branch),
        agent_glyph(e.agent),
        esc_html(name),
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
        EventKind::QuestionAsked { question } => {
            let nav = vec![("👁 Focus".to_string(), format!("act:focus:{alias}"))];
            let mut rows: Vec<Vec<(String, String)>> = Vec::new();
            let multi_q = question.questions.len() > 1;
            for (qi, q) in question.questions.iter().enumerate() {
                let mut row: Vec<(String, String)> = Vec::new();
                for (oi, o) in q.options.iter().enumerate() {
                    let label = if multi_q {
                        format!(
                            "Q{}·{} {}",
                            qi + 1,
                            oi + 1,
                            relay_core::state::truncate(&o.label, 14)
                        )
                    } else {
                        format!("{}. {}", oi + 1, relay_core::state::truncate(&o.label, 20))
                    };
                    row.push((label, format!("act:qpick:{alias}:{qi}:{oi}")));
                    if row.len() == 2 {
                        rows.push(std::mem::take(&mut row));
                    }
                }
                if !row.is_empty() {
                    rows.push(row);
                }
            }
            rows.push(vec![(
                "⛔ Cancel".to_string(),
                format!("act:stop:{alias}:{agent}"),
            )]);
            rows.push(nav);
            Some(keyboard(rows))
        }
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
                    tokio::spawn(async move {
                        if let Some(m) = u.message {
                            handle_message(&tg, &registry, &active, &ctl, &auth, m).await;
                        }
                        if let Some(cq) = u.callback_query {
                            handle_callback(
                                &tg, &registry, &active, &ctl, &auth, &pending, &questions, &perms,
                                cq,
                            )
                            .await;
                        }
                    });
                }
            }
            Err(e) => {
                warn!("getUpdates: {e}");
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

async fn handle_message(
    tg: &Telegram,
    registry: &Registry,
    active: &Active,
    ctl: &Arc<dyn Control>,
    auth: &Arc<auth::Auth>,
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
                let text2 = text.to_string();
                let res =
                    tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2))
                        .await;
                return match res {
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
        let text2 = text.to_string();
        let res = tokio::task::spawn_blocking(move || inject::send_user_message(pid, &text2)).await;
        return match res {
            Ok(Ok(())) => "✉️ sent (background → shows in VS Code too)".to_string(),
            Ok(Err(e)) => format!("❌ inject: {e}"),
            Err(e) => format!("❌ task: {e}"),
        };
    }
    let _ = (ctl, alias, sid, text);
    "❌ No background channel for this chat. It was opened before the shim was installed. \
     Open a new Claude chat and Send will go through in the background."
        .to_string()
}

async fn pending_option_label(
    registry: &Registry,
    alias: &str,
    qi: usize,
    oi: usize,
) -> Option<String> {
    let windows = registry.read().await;
    let w = windows
        .iter()
        .find(|w| workspace_alias(&w.workspace) == alias)?;
    for c in &w.claude {
        if let ClaudeState::PendingQuestion(q) = &c.state {
            if let Some(question) = q.questions.get(qi) {
                if let Some(opt) = question.options.get(oi) {
                    return Some(opt.label.clone());
                }
            }
        }
    }
    None
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

#[allow(clippy::too_many_arguments)]
async fn handle_callback(
    tg: &Telegram,
    registry: &Registry,
    active: &Active,
    ctl: &Arc<dyn Control>,
    auth: &Arc<auth::Auth>,
    pending: &ingress::Pending,
    questions: &question::Questions,
    perms: &permission::Permissions,
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
        let ok = match it.next().and_then(|p| p.parse::<u32>().ok()) {
            Some(pid) => permission::resolve(perms, pid, allow).await,
            None => Err("bad pid".to_string()),
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
            ["qpick", alias, qi, oi] => match (qi.parse::<usize>(), oi.parse::<usize>()) {
                (Ok(q), Ok(o)) => match pending_option_label(registry, alias, q, o).await {
                    Some(label) => {
                        say_text(
                            ctl,
                            registry,
                            alias,
                            AgentKind::ClaudeCode,
                            None,
                            true,
                            &label,
                        )
                        .await
                    }
                    None => "no pending question / option".to_string(),
                },
                _ => "bad option ref".to_string(),
            },
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
    rows.push(vec![("🔄 Refresh".to_string(), "m:home".to_string())]);
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
        rows.push(vec![(
            "📄 Read chat".to_string(),
            format!("act:read:{alias}:{a}:{idx}"),
        )]);
    } else {
        rows.push(vec![
            ("✍️ Send".to_string(), format!("act:say:{alias}:{a}:{idx}")),
            ("⏹ Stop".to_string(), format!("act:stop:{alias}:{a}")),
        ]);
        rows.push(vec![(
            "📄 Read chat".to_string(),
            format!("act:read:{alias}:{a}:{idx}"),
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

fn bot_commands() -> Vec<(&'static str, &'static str)> {
    vec![
        ("menu", "Windows and chats as buttons"),
        ("windows", "List open windows and chats"),
        ("status", "Details for a workspace"),
        ("say", "Send a prompt to a chat"),
        ("stop", "Interrupt the current turn"),
        ("cont", "Continue the chat"),
        ("mode", "Cycle the Claude permission mode"),
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
     /slash &lt;ws&gt; &lt;agent&gt; &lt;cmd&gt; - send a slash command\n\
     /focus &lt;ws&gt; - raise the window"
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
