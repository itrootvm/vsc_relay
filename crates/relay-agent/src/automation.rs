use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Manual,
    Auto,
    Robot,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::Auto => "auto",
            Mode::Robot => "robot",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_lowercase().as_str() {
            "manual" => Some(Mode::Manual),
            "auto" => Some(Mode::Auto),
            "robot" => Some(Mode::Robot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeEntry {
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRules {
    pub enabled: bool,
    pub max_attempts: u32,
    pub base_secs: u64,
    pub cap_secs: u64,
    pub notify: bool,
    pub wait_for_reset: bool,
}

impl Default for RetryRules {
    fn default() -> Self {
        RetryRules {
            enabled: true,
            max_attempts: 3,
            base_secs: 2,
            cap_secs: 60,
            notify: true,
            wait_for_reset: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub steer: bool,

    #[serde(default)]
    pub gate: bool,

    #[serde(default = "default_true")]
    pub feedback_protocol: bool,
    #[serde(default)]
    pub semantic: relay_semantic::config::SemanticConfig,
}

fn default_true() -> bool {
    true
}

impl Default for SmartConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            steer: false,
            gate: false,
            feedback_protocol: true,
            semantic: relay_semantic::config::SemanticConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewriteRules {
    pub robot: bool,
    pub manual: bool,
}

impl Default for RewriteRules {
    fn default() -> Self {
        RewriteRules {
            robot: true,
            manual: false,
        }
    }
}

fn default_answer_confidence() -> f64 {
    0.7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRules {
    pub auto_approve: bool,
    pub respect_danger_list: bool,
    pub auto_accept_plan_exit: bool,
    #[serde(default)]
    pub auto_answer_questions: bool,
    #[serde(default = "default_answer_confidence")]
    pub auto_answer_min_confidence: f64,
    #[serde(default)]
    pub guard_dangerous: bool,
    pub retry: RetryRules,
}

impl Default for AutoRules {
    fn default() -> Self {
        AutoRules {
            auto_approve: true,
            respect_danger_list: true,
            auto_accept_plan_exit: true,
            auto_answer_questions: false,
            auto_answer_min_confidence: default_answer_confidence(),
            guard_dangerous: false,
            retry: RetryRules::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOpts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "one")]
    pub weight: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_per_ktok: Option<f64>,
}

impl Default for ProviderOpts {
    fn default() -> Self {
        ProviderOpts {
            model: None,
            weight: 1,
            max_usd: None,
            usd_per_ktok: None,
        }
    }
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    Single,
    Priority,
    RoundRobin,
    #[default]
    CostOptimized,
}

impl Strategy {
    pub fn label(&self) -> &'static str {
        match self {
            Strategy::Single => "single",
            Strategy::Priority => "priority",
            Strategy::RoundRobin => "round_robin",
            Strategy::CostOptimized => "cost_optimized",
        }
    }

    pub fn all() -> [Strategy; 4] {
        [
            Strategy::Single,
            Strategy::Priority,
            Strategy::RoundRobin,
            Strategy::CostOptimized,
        ]
    }

    pub fn parse(s: &str) -> Option<Strategy> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "single" => Some(Strategy::Single),
            "priority" => Some(Strategy::Priority),
            "round_robin" | "roundrobin" | "rr" => Some(Strategy::RoundRobin),
            "cost_optimized" | "cost" | "costoptimized" => Some(Strategy::CostOptimized),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Providers {
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default)]
    pub per_provider: BTreeMap<String, ProviderOpts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
}

impl Providers {
    pub fn is_enabled(&self, id: &str) -> bool {
        self.enabled.iter().any(|x| x == id)
    }

    pub fn toggle(&mut self, id: &str) {
        if let Some(pos) = self.enabled.iter().position(|x| x == id) {
            self.enabled.remove(pos);
        } else {
            self.enabled.push(id.to_string());
        }
    }

    pub fn set_model(&mut self, id: &str, model: &str) {
        self.per_provider.entry(id.to_string()).or_default().model = Some(model.to_string());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RobotRules {
    pub confirm_dangerous: bool,
    pub auto_answer_questions: bool,
    pub answer_strategy: String,
    pub expiry_secs: i64,
    pub max_steps: u32,
    pub providers: Providers,
}

impl Default for RobotRules {
    fn default() -> Self {
        RobotRules {
            confirm_dangerous: true,
            auto_answer_questions: true,
            answer_strategy: "first".to_string(),
            expiry_secs: 3600,
            max_steps: 20,
            providers: Providers::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDepth {
    Shallow,
    #[default]
    Normal,
    Deep,
}

impl ReviewDepth {
    pub fn label(self) -> &'static str {
        match self {
            ReviewDepth::Shallow => "shallow",
            ReviewDepth::Normal => "normal",
            ReviewDepth::Deep => "deep",
        }
    }

    pub fn parse(raw: &str) -> Option<ReviewDepth> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "shallow" | "light" => Some(ReviewDepth::Shallow),
            "normal" | "medium" => Some(ReviewDepth::Normal),
            "deep" | "full" => Some(ReviewDepth::Deep),
            _ => None,
        }
    }

    pub fn tail_messages(self) -> usize {
        match self {
            ReviewDepth::Shallow => 8,
            ReviewDepth::Normal => 20,
            ReviewDepth::Deep => 50,
        }
    }

    pub fn goal_head_bytes(self) -> u64 {
        match self {
            ReviewDepth::Shallow => 256 * 1024,
            ReviewDepth::Normal => 1024 * 1024,
            ReviewDepth::Deep => 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRules {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_reviewers")]
    pub reviewers: Vec<String>,
    #[serde(default = "default_review_interval")]
    pub every_secs: i64,
    #[serde(default)]
    pub depth: ReviewDepth,
    #[serde(default = "default_review_budget")]
    pub max_per_session: u32,
    #[serde(default)]
    pub steer: bool,
    #[serde(default = "default_true")]
    pub cross_family_only: bool,
}

fn default_reviewers() -> Vec<String> {
    vec![
        "codex-cli".to_string(),
        "claude-cli".to_string(),
        "antigravity".to_string(),
    ]
}

fn default_review_interval() -> i64 {
    900
}

fn default_review_budget() -> u32 {
    6
}

impl Default for ReviewRules {
    fn default() -> Self {
        ReviewRules {
            enabled: false,
            reviewers: default_reviewers(),
            every_secs: default_review_interval(),
            depth: ReviewDepth::default(),
            max_per_session: default_review_budget(),
            steer: false,
            cross_family_only: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationConfig {
    #[serde(default = "version_one")]
    pub version: u32,
    #[serde(default)]
    pub default: Mode,
    #[serde(default)]
    pub auto: AutoRules,
    #[serde(default)]
    pub robot: RobotRules,
    #[serde(default)]
    pub review: ReviewRules,
    #[serde(default)]
    pub rewrite: RewriteRules,
    #[serde(default)]
    pub smart: SmartConfig,
    #[serde(default)]
    pub workspaces: BTreeMap<String, ScopeEntry>,
    #[serde(default)]
    pub sessions: BTreeMap<String, ScopeEntry>,
}

fn version_one() -> u32 {
    1
}

impl Default for AutomationConfig {
    fn default() -> Self {
        AutomationConfig {
            version: 1,
            default: Mode::Manual,
            auto: AutoRules::default(),
            robot: RobotRules::default(),
            review: ReviewRules::default(),
            rewrite: RewriteRules::default(),
            smart: SmartConfig::default(),
            workspaces: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    }
}

pub enum Scope {
    Default,
    Workspace(String),
    Session(String),
}

impl AutomationConfig {
    pub fn resolve(&self, session_id: Option<&str>, alias: &str, now: i64) -> Mode {
        if let Some(sid) = session_id {
            if let Some(e) = self.sessions.get(sid) {
                if entry_active(e, now) {
                    return e.mode;
                }
            }
        }
        if let Some(e) = self.workspaces.get(alias) {
            if entry_active(e, now) {
                return e.mode;
            }
        }
        self.default
    }

    pub fn should_rewrite(&self, mode: Mode) -> bool {
        match mode {
            Mode::Robot => self.rewrite.robot,
            Mode::Manual => self.rewrite.manual,
            Mode::Auto => false,
        }
    }

    pub fn set(&mut self, scope: Scope, mode: Mode, expires_at: Option<i64>) {
        let entry = ScopeEntry { mode, expires_at };
        match scope {
            Scope::Default => self.default = mode,
            Scope::Workspace(a) => {
                self.workspaces.insert(a, entry);
            }
            Scope::Session(s) => {
                self.sessions.insert(s, entry);
            }
        }
    }

    pub fn clear(&mut self, scope: Scope) {
        match scope {
            Scope::Default => self.default = Mode::Manual,
            Scope::Workspace(a) => {
                self.workspaces.remove(&a);
            }
            Scope::Session(s) => {
                self.sessions.remove(&s);
            }
        }
    }

    pub fn load() -> AutomationConfig {
        match std::fs::read_to_string(path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => AutomationConfig::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        crate::fsutil::secure_write(&path(), text.as_bytes())
    }
}

fn entry_active(e: &ScopeEntry, now: i64) -> bool {
    match e.expires_at {
        Some(at) => at > now,
        None => true,
    }
}

pub fn path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(".vsc-relay").join("automation.json")
}

pub fn is_automation_command(arg: &str) -> bool {
    arg == "automation"
}

pub fn run(args: &[String]) -> Result<()> {
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let mut cfg = AutomationConfig::load();
    match sub {
        "get" => {
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        "list" => {
            println!("default: {}", cfg.default.label());
            for (a, e) in &cfg.workspaces {
                println!("workspace {a}: {}", e.mode.label());
            }
            for (s, e) in &cfg.sessions {
                println!("session {s}: {}", e.mode.label());
            }
        }
        "resolve" => {
            let alias = req(args.get(2), "alias")?;
            let sid = args.get(3).map(|s| s.trim()).filter(|s| !s.is_empty());
            println!("{}", cfg.resolve(sid, &alias, now_secs()).label());
        }
        "set-default" => {
            let mode = parse_mode_arg(args.get(2))?;
            cfg.set(Scope::Default, mode, None);
            cfg.save()?;
            println!("default = {}", mode.label());
        }
        "set-workspace" => {
            let alias = req(args.get(2), "alias")?;
            let mode = parse_mode_arg(args.get(3))?;
            let expires = expires_from(args.get(4));
            cfg.set(Scope::Workspace(alias.clone()), mode, expires);
            cfg.save()?;
            println!("workspace {alias} = {}", mode.label());
        }
        "set-session" => {
            let sid = req(args.get(2), "session_id")?;
            let mode = parse_mode_arg(args.get(3))?;
            let expires = expires_from(args.get(4));
            cfg.set(Scope::Session(sid.clone()), mode, expires);
            cfg.save()?;
            println!("session {sid} = {}", mode.label());
        }
        "clear-workspace" => {
            let alias = req(args.get(2), "alias")?;
            cfg.clear(Scope::Workspace(alias.clone()));
            cfg.save()?;
            println!("workspace {alias} cleared (inherits default)");
        }
        "clear-session" => {
            let sid = req(args.get(2), "session_id")?;
            cfg.clear(Scope::Session(sid.clone()));
            cfg.save()?;
            println!("session {sid} cleared (inherits default)");
        }
        "review" => {
            match args.get(2).map(String::as_str) {
                Some("on") => cfg.review.enabled = true,
                Some("off") => cfg.review.enabled = false,
                Some("steer") => match args.get(3).map(String::as_str) {
                    Some("on") => cfg.review.steer = true,
                    Some("off") => cfg.review.steer = false,
                    _ => bail!("usage: automation review steer <on|off>"),
                },
                Some("same-family") => match args.get(3).map(String::as_str) {
                    Some("on") => cfg.review.cross_family_only = false,
                    Some("off") => cfg.review.cross_family_only = true,
                    _ => bail!("usage: automation review same-family <on|off>"),
                },
                Some("reviewers") => {
                    let list = req(args.get(3), "comma separated reviewer ids")?;
                    let picked: Vec<String> = list
                        .split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                        .collect();
                    if picked.is_empty() {
                        bail!("usage: automation review reviewers codex-cli,claude-cli");
                    }
                    if let Some(bad) = picked
                        .iter()
                        .find(|id| crate::supervisor::discover::Backend::parse(id).is_none())
                    {
                        bail!("unknown reviewer {bad}");
                    }
                    cfg.review.reviewers = picked;
                }
                Some("every") => {
                    let secs = req(args.get(3), "seconds")?;
                    let secs: i64 = secs.parse().unwrap_or(0);
                    if secs < 60 {
                        bail!("usage: automation review every <seconds, at least 60>");
                    }
                    cfg.review.every_secs = secs;
                }
                Some("depth") => {
                    let raw = req(args.get(3), "shallow|normal|deep")?;
                    cfg.review.depth = ReviewDepth::parse(&raw)
                        .ok_or_else(|| anyhow::anyhow!("bad depth {raw}"))?;
                }
                Some("budget") => {
                    let raw = req(args.get(3), "reviews per session")?;
                    cfg.review.max_per_session = raw.parse().unwrap_or(0);
                }
                _ => bail!(
                    "usage: automation review <on|off|steer|same-family|reviewers|every|depth|budget|run|probe> ..."
                ),
            }
            cfg.save()?;
            println!(
                "review {} | reviewers {} | every {}s | depth {} | budget {} | steer {} | cross-family-only {}",
                if cfg.review.enabled { "on" } else { "off" },
                cfg.review.reviewers.join(","),
                cfg.review.every_secs,
                cfg.review.depth.label(),
                cfg.review.max_per_session,
                if cfg.review.steer { "on" } else { "off" },
                cfg.review.cross_family_only,
            );
        }
        "provider" => {
            let id = req(args.get(2), "provider id")?;
            match args.get(3).map(String::as_str) {
                Some("on") => {
                    if !cfg.robot.providers.is_enabled(&id) {
                        cfg.robot.providers.toggle(&id);
                    }
                }
                Some("off") => {
                    if cfg.robot.providers.is_enabled(&id) {
                        cfg.robot.providers.toggle(&id);
                    }
                }
                _ => bail!("usage: automation provider <id> <on|off>"),
            }
            cfg.save()?;
            let on = cfg.robot.providers.is_enabled(&id);
            println!("provider {id} {}", if on { "on" } else { "off" });
        }
        "strategy" => {
            let s = req(args.get(2), "strategy")?;
            let strat =
                Strategy::parse(&s).ok_or_else(|| anyhow::anyhow!("bad strategy {s}"))?;
            cfg.robot.providers.strategy = strat;
            cfg.save()?;
            println!("strategy = {}", strat.label());
        }
        "provider-model" => {
            let id = req(args.get(2), "provider id")?;
            let model = req(args.get(3), "model")?;
            cfg.robot.providers.set_model(&id, &model);
            cfg.save()?;
            println!("{id} model = {model}");
        }
        "provider-key" => {
            let id = req(args.get(2), "provider id")?;
            match args.get(3).map(String::as_str) {
                Some("clear") => {
                    crate::supervisor::keys::clear(&id)?;
                    println!("provider {id} key cleared");
                }
                other => {
                    let key = match other {
                        Some("-") | None => read_secret_stdin()?,
                        Some(k) => k.to_string(),
                    };
                    if key.trim().is_empty() {
                        bail!("empty key");
                    }
                    crate::supervisor::keys::set(&id, key.trim())?;
                    println!("provider {id} key set");
                }
            }
        }
        "rewrite" => {
            let which = req(args.get(2), "robot|manual")?;
            let on = match args.get(3).map(String::as_str) {
                Some("on") => true,
                Some("off") => false,
                _ => bail!("usage: automation rewrite <robot|manual> <on|off>"),
            };
            match which.as_str() {
                "robot" => cfg.rewrite.robot = on,
                "manual" => cfg.rewrite.manual = on,
                _ => bail!("usage: automation rewrite <robot|manual> <on|off>"),
            }
            cfg.save()?;
            println!("rewrite {which} {}", if on { "on" } else { "off" });
        }
        "smart" => match args.get(2).map(String::as_str) {
            Some("status") => {
                println!(
                    "smart={} steer={} gate={} feedback={}",
                    cfg.smart.enabled,
                    cfg.smart.steer,
                    cfg.smart.gate,
                    cfg.smart.feedback_protocol
                );
            }
            Some("on") => {
                cfg.smart.enabled = true;
                crate::install::install_session_start_hooks()?;
                cfg.save()?;
                println!("smart on");
            }
            Some("off") => {
                cfg.smart.enabled = false;
                cfg.smart.steer = false;
                cfg.smart.gate = false;
                cfg.save()?;
                println!("smart off");
            }
            Some("steer") => {
                let on = match args.get(3).map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => bail!("usage: automation smart steer <on|off>"),
                };
                cfg.smart.steer = on;
                cfg.save()?;
                println!("smart steer {}", if on { "on" } else { "off" });
            }
            Some("gate") => {
                let on = match args.get(3).map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => bail!("usage: automation smart gate <on|off>"),
                };
                if on {
                    crate::install::install_gate_hooks()?;
                    cfg.smart.enabled = true;
                }
                cfg.smart.gate = on;
                cfg.save()?;
                println!("smart gate {}", if on { "on" } else { "off" });
            }
            Some("feedback") => {
                let on = match args.get(3).map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => bail!("usage: automation smart feedback <on|off>"),
                };
                cfg.smart.feedback_protocol = on;
                cfg.save()?;
                println!("smart feedback protocol {}", if on { "on" } else { "off" });
            }
            Some("budget") => match args.get(3).map(String::as_str) {
                Some("off") => {
                    cfg.robot.providers.budget_usd = None;
                    cfg.save()?;
                    println!("smart budget off");
                }
                Some(v) => match v.parse::<f64>() {
                    Ok(usd) if usd > 0.0 => {
                        cfg.robot.providers.budget_usd = Some(usd);
                        cfg.save()?;
                        println!("smart budget = ${usd}");
                    }
                    _ => bail!("usage: automation smart budget <USD|off>"),
                },
                None => bail!("usage: automation smart budget <USD|off>"),
            },
            Some("provider") => {
                let preset = req(args.get(3), "provider")?;
                cfg.smart.semantic.apply_preset(&preset)?;
                cfg.save()?;
                match cfg.smart.semantic.cli_provider() {
                    Some(cli) => println!("smart provider = agent_cli ({cli})"),
                    None => println!("smart provider = {}", cfg.smart.semantic.backend.label()),
                }
                if let Some(disclosure) = cfg.smart.semantic.off_machine_disclosure() {
                    println!("WARNING: {disclosure}");
                }
            }
            Some("model") => {
                let model = req(args.get(3), "model")?;
                cfg.smart.semantic.model = model.clone();
                cfg.save()?;
                println!("smart model = {model}");
            }
            Some("endpoint") => {
                let endpoint = req(args.get(3), "endpoint")?;
                cfg.smart.semantic.endpoint = if endpoint == "clear" {
                    None
                } else {
                    Some(endpoint.clone())
                };
                cfg.save()?;
                println!("smart endpoint = {}", cfg.smart.semantic.endpoint.as_deref().unwrap_or("default"));
                if let Some(disclosure) = cfg.smart.semantic.off_machine_disclosure() {
                    println!("WARNING: {disclosure}");
                }
            }
            Some("local-dir") => {
                let dir = req(args.get(3), "bundle path|builtin")?;
                cfg.smart.semantic.local_dir = if dir == "builtin" {
                    None
                } else {
                    Some(PathBuf::from(&dir))
                };
                cfg.save()?;
                println!("smart local bundle = {}", cfg.smart.semantic.model_dir().display());
            }
            Some("trust") => {
                let on = match args.get(3).map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => bail!("usage: automation smart trust <on|off>"),
                };
                cfg.smart.semantic.allow_uncalibrated_steer = on;
                cfg.save()?;
                println!("smart uncalibrated steering {}", if on { "allowed" } else { "blocked" });
            }
            Some("key") => {
                match args.get(3).map(String::as_str) {
                    Some("clear") => {
                        crate::supervisor::keys::clear("semantic")?;
                        println!("smart provider key cleared");
                    }
                    other => {
                        let key = match other {
                            Some("-") | None => read_secret_stdin()?,
                            Some(key) => key.to_string(),
                        };
                        if key.trim().is_empty() {
                            bail!("empty key");
                        }
                        crate::supervisor::keys::set("semantic", key.trim())?;
                        println!("smart provider key set");
                    }
                }
            }
            Some("install-local") | Some("check") | Some("train-local") => {
                bail!("this smart command must be run through vsc-relay-agent")
            }
            _ => bail!("usage: automation smart <status|on|off|steer on|off|gate on|off|feedback on|off|budget USD|off|provider PRESET (off|local|ollama|openrouter|nvidia|openai-compatible|claude|codex|gemini|cursor|antigravity)|model ID|endpoint URL|local-dir PATH|trust on|off|key [VALUE|-|clear]|install-local|train-local DATASET OUTPUT [BASE]|check>"),
        },
        _ => bail!(
            "usage: automation <get|list|set-default|set-workspace|set-session|clear-workspace|clear-session|review|provider|strategy|provider-model|provider-key|rewrite|smart|compass|compass-link|compass-unlink|compass-mark|compass-profile|discover|ask|route|improve> ..."
        ),
    }
    Ok(())
}

fn req(v: Option<&String>, name: &str) -> Result<String> {
    match v.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s.to_string()),
        None => bail!("missing {name}"),
    }
}

fn parse_mode_arg(v: Option<&String>) -> Result<Mode> {
    let s = req(v, "mode")?;
    Mode::parse(&s).ok_or_else(|| anyhow::anyhow!("bad mode {s} (manual|auto|robot)"))
}

fn expires_from(v: Option<&String>) -> Option<i64> {
    let secs: i64 = v?.trim().parse().ok()?;
    if secs <= 0 {
        return None;
    }
    Some(now_secs() + secs)
}

fn read_secret_stdin() -> Result<String> {
    use std::io::BufRead;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_defaults_off_and_survives_partial_config() {
        assert!(!AutomationConfig::default().smart.enabled);
        assert!(!AutomationConfig::default().smart.steer);
        assert!(!AutomationConfig::default().smart.gate);
        assert!(AutomationConfig::default().smart.feedback_protocol);
        let c: AutomationConfig = serde_json::from_str(r#"{"default":"manual"}"#).unwrap();
        assert!(
            !c.smart.enabled,
            "absent smart block must default to disabled"
        );
        assert!(!c.smart.steer, "absent steer flag must default to disabled");
        assert!(!c.smart.gate, "absent gate flag must default to disabled");
        assert!(
            c.smart.feedback_protocol,
            "feedback protocol should be ready but inert while smart is off"
        );
        let partial: AutomationConfig =
            serde_json::from_str(r#"{"smart":{"enabled":true}}"#).unwrap();
        assert!(partial.smart.enabled);
        assert!(!partial.smart.gate);
        assert!(partial.smart.feedback_protocol);
        assert!(
            !partial.smart.steer,
            "steer must stay off when only enabled is set"
        );
    }

    #[test]
    fn should_rewrite_by_mode() {
        let mut c = AutomationConfig::default();
        assert!(c.should_rewrite(Mode::Robot));
        assert!(!c.should_rewrite(Mode::Auto));
        assert!(!c.should_rewrite(Mode::Manual));
        c.rewrite.manual = true;
        c.rewrite.robot = false;
        assert!(c.should_rewrite(Mode::Manual));
        assert!(!c.should_rewrite(Mode::Robot));
        assert!(!c.should_rewrite(Mode::Auto), "auto never rewrites");
    }

    #[test]
    fn resolve_order_session_over_workspace_over_default() {
        let mut c = AutomationConfig::default();
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Manual);
        c.set(Scope::Default, Mode::Auto, None);
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Auto);
        c.set(Scope::Workspace("proj".into()), Mode::Robot, None);
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Robot);
        c.set(Scope::Session("s1".into()), Mode::Manual, Some(200));
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Manual);
    }

    #[test]
    fn expired_session_falls_through() {
        let mut c = AutomationConfig::default();
        c.set(Scope::Workspace("proj".into()), Mode::Auto, None);
        c.set(Scope::Session("s1".into()), Mode::Robot, Some(150));
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Robot);
        assert_eq!(c.resolve(Some("s1"), "proj", 200), Mode::Auto);
    }

    #[test]
    fn manual_pins_and_clear_removes_override() {
        let mut c = AutomationConfig::default();
        c.set(Scope::Default, Mode::Auto, None);
        c.set(Scope::Session("s1".into()), Mode::Robot, None);
        assert!(c.sessions.contains_key("s1"));
        c.set(Scope::Session("s1".into()), Mode::Manual, None);
        assert!(c.sessions.contains_key("s1"));
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Manual);
        c.clear(Scope::Session("s1".into()));
        assert!(!c.sessions.contains_key("s1"));
        assert_eq!(c.resolve(Some("s1"), "proj", 100), Mode::Auto);
    }

    #[test]
    fn partial_json_fills_defaults() {
        let c: AutomationConfig = serde_json::from_str(r#"{"default":"auto"}"#).unwrap();
        assert_eq!(c.default, Mode::Auto);
        assert_eq!(c.version, 1);
        assert!(c.auto.auto_approve);
        assert_eq!(c.robot.max_steps, 20);
        assert_eq!(c.auto.retry.max_attempts, 3);
    }

    #[test]
    fn strategy_parse_label_roundtrip() {
        for s in Strategy::all() {
            assert_eq!(Strategy::parse(s.label()), Some(s));
        }
        assert_eq!(Strategy::parse("rr"), Some(Strategy::RoundRobin));
        assert_eq!(Strategy::parse("cost"), Some(Strategy::CostOptimized));
        assert_eq!(Strategy::parse("nope"), None);
    }

    #[test]
    fn providers_toggle_and_model() {
        let mut p = Providers::default();
        assert!(!p.is_enabled("ollama"));
        p.toggle("ollama");
        assert!(p.is_enabled("ollama"));
        p.toggle("ollama");
        assert!(!p.is_enabled("ollama"));
        p.set_model("openrouter", "anthropic/claude-sonnet-5");
        assert_eq!(
            p.per_provider.get("openrouter").unwrap().model.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
        assert_eq!(p.per_provider.get("openrouter").unwrap().weight, 1);
    }

    #[test]
    fn roundtrip_serde() {
        let mut c = AutomationConfig::default();
        c.set(Scope::Workspace("p".into()), Mode::Auto, None);
        let text = serde_json::to_string(&c).unwrap();
        let back: AutomationConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.resolve(None, "p", 0), Mode::Auto);
    }
}
