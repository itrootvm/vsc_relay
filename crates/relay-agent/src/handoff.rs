use anyhow::{anyhow, Result};
use relay_adapters::family::Family;
use relay_compass::handoff::{
    build_handoff, handoff_prompt, render_handoff_markdown, HandoffContext, HandoffLimits,
};
use relay_compass::ledger::{build_contract_ledger, contract_input_from_steps};
use relay_compass::steps::SemanticStep;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

const BRIEF_NAME: &str = "HANDOFF.md";
const PREVIOUS_NAME: &str = "HANDOFF.prev.md";

#[derive(Debug, Clone)]
pub struct HandoffTarget {
    pub alias: String,
    pub workspace: PathBuf,
    pub title: String,
    pub chat: Option<HandoffChat>,
    pub app: Option<String>,
    pub cli: Option<(String, Vec<String>)>,
}

#[derive(Debug, Clone)]
pub struct HandoffChat {
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct PendingHandoff {
    pub source_alias: String,
    pub source_session: String,
    pub source_agent: String,
    pub source_workspace: PathBuf,
    pub source_branch: Option<String>,
    pub targets: Vec<HandoffTarget>,
}

#[derive(Debug, Clone)]
pub struct HandoffResult {
    pub path: PathBuf,
    pub prompt: String,
    pub bytes: usize,
    pub proven: usize,
    pub remaining: usize,
    pub compact_author: Option<String>,
}

static PENDING: OnceLock<Mutex<HashMap<u32, PendingHandoff>>> = OnceLock::new();
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

pub fn stash(pending: PendingHandoff) -> u32 {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let store = PENDING.get_or_init(|| Mutex::new(HashMap::new()));
    let mut store = store.lock().unwrap_or_else(|p| p.into_inner());
    while store.len() > 32 {
        let oldest = store.keys().min().copied();
        match oldest {
            Some(key) => {
                store.remove(&key);
            }
            None => break,
        }
    }
    store.insert(token, pending);
    token
}

pub fn recall(token: u32) -> Option<PendingHandoff> {
    let store = PENDING.get()?;
    let store = store.lock().unwrap_or_else(|p| p.into_inner());
    store.get(&token).cloned()
}

fn family_of(agent: &str) -> Family {
    Family::parse(agent).unwrap_or(Family::Claude)
}

fn steps_for(agent: &str, transcript: &Path) -> Vec<SemanticStep> {
    relay_adapters::family::semantic_steps(family_of(agent), transcript)
}

const PLAN_CHARS: usize = 12_000;
const TAIL_TURNS: usize = 32;
const TAIL_CHARS: usize = 900;
const COMPACT_SYSTEM: &str =
    "You compact your own engineering session for another agent. You report only what the \
     supplied material shows. You never invent files, commands, or outcomes.";

fn transcript_tail(agent: &str, transcript: &Path) -> String {
    let pairs = relay_adapters::family::tail_messages(family_of(agent), transcript, TAIL_TURNS);
    let mut out = String::new();
    for (role, text) in pairs {
        let who = match role {
            'U' => "user",
            'T' => "tool",
            _ => "agent",
        };
        let line = relay_core::state::truncate(text.trim(), TAIL_CHARS);
        out.push_str(&format!("{who}: {line}\n"));
    }
    out
}

fn own_backend(agent: &str) -> &'static str {
    family_of(agent).compact_backend()
}

pub fn compact_providers(
    agent: &str,
    configured: &crate::automation::Providers,
    discovered: &[crate::supervisor::discover::Discovered],
) -> crate::automation::Providers {
    let own = own_backend(agent);
    let available = discovered
        .iter()
        .any(|entry| entry.id == own && entry.available);
    if !available {
        return configured.clone();
    }
    let mut providers = configured.clone();
    providers.enabled = vec![own.to_string()];
    providers.strategy = crate::automation::Strategy::Single;
    providers
}

pub async fn agent_compact(
    pending: &PendingHandoff,
    transcript: &Path,
    brief: &relay_compass::handoff::HandoffBrief,
    configured: &crate::automation::Providers,
    now: i64,
) -> Option<relay_compass::handoff::HandoffCompact> {
    let tail = transcript_tail(&pending.source_agent, transcript);
    if tail.trim().is_empty() {
        return None;
    }
    let discovered = crate::supervisor::discover::discover_all().await;
    let providers = compact_providers(&pending.source_agent, configured, &discovered);
    let request = relay_compass::handoff::compact_request(brief, &tail);
    let pool = crate::supervisor::pool::Pool::new();
    let first = pool
        .improve(&providers, &discovered, COMPACT_SYSTEM, &request, now)
        .await;
    let attempt = match (first, providers.enabled == configured.enabled) {
        (Ok(done), _) => Ok(done),
        (Err(_), true) => Err(()),
        (Err(_), false) => pool
            .improve(configured, &discovered, COMPACT_SYSTEM, &request, now)
            .await
            .map_err(|_| ()),
    };
    attempt
        .ok()
        .map(|(backend, text)| relay_compass::handoff::HandoffCompact {
            text,
            author: backend.id().to_string(),
        })
}

pub fn prepare(
    pending: &PendingHandoff,
    transcript: &Path,
) -> Result<relay_compass::handoff::HandoffBrief> {
    let steps = steps_for(&pending.source_agent, transcript);
    if steps.is_empty() {
        return Err(anyhow!("the source session has no readable transcript yet"));
    }
    let anchor = contract_input_from_steps(&steps)
        .anchor_text
        .unwrap_or_default();
    let ledger = build_contract_ledger(&steps, &[], &anchor);
    let mut brief = build_handoff(&steps, &ledger, &HandoffLimits::default());
    brief.plan = relay_adapters::family::latest_plan(family_of(&pending.source_agent), transcript)
        .map(|plan| relay_core::state::truncate(plan.trim(), PLAN_CHARS));
    Ok(brief)
}

pub fn write_brief(
    pending: &PendingHandoff,
    brief: &relay_compass::handoff::HandoffBrief,
    target: &HandoffTarget,
    generated_at: &str,
) -> Result<HandoffResult> {
    let context = HandoffContext {
        workspace: pending.source_workspace.display().to_string(),
        branch: pending.source_branch.clone(),
        source_agent: pending.source_agent.clone(),
        source_session: pending.source_session.clone(),
        generated_at: generated_at.to_string(),
    };
    let rendered = render_handoff_markdown(brief, &context);

    let path = target.workspace.join(BRIEF_NAME);
    if path.exists() {
        let _ = std::fs::rename(&path, target.workspace.join(PREVIOUS_NAME));
    }
    std::fs::write(&path, rendered.as_bytes())?;

    Ok(HandoffResult {
        prompt: handoff_prompt(&path.display().to_string()),
        bytes: rendered.len(),
        proven: brief.proven.len(),
        remaining: brief.remaining.len(),
        compact_author: brief.compact.as_ref().map(|c| c.author.clone()),
        path,
    })
}

const EDITOR_APPS: &[&str] = &[
    "Antigravity",
    "Cursor",
    "Windsurf",
    "VSCodium",
    "Visual Studio Code",
];

pub struct CliAgent {
    pub bin: &'static str,
    pub label: &'static str,
    pub prompt_flags: &'static [&'static str],
    pub trust_flags: &'static [&'static str],
    pub unattended_flags: &'static [&'static str],
    pub auth_check: &'static [&'static str],
    pub auth_ok: Option<&'static str>,
}

const CLI_AGENTS: &[CliAgent] = &[
    CliAgent {
        bin: "claude",
        label: "Claude Code CLI",
        prompt_flags: &[],
        trust_flags: &[],
        unattended_flags: &["--dangerously-skip-permissions"],
        auth_check: &[],
        auth_ok: None,
    },
    CliAgent {
        bin: "codex",
        label: "Codex CLI",
        prompt_flags: &[],
        trust_flags: &[],
        unattended_flags: &["--dangerously-bypass-approvals-and-sandbox"],
        auth_check: &["login", "status"],
        auth_ok: None,
    },
    CliAgent {
        bin: "agy",
        label: "Antigravity CLI",
        prompt_flags: &["-i"],
        trust_flags: &[],
        unattended_flags: &["--dangerously-skip-permissions"],
        auth_check: &["models"],
        auth_ok: Some("signed in"),
    },
    CliAgent {
        bin: "cursor-agent",
        label: "Cursor CLI",
        prompt_flags: &[],
        trust_flags: &["--trust"],
        unattended_flags: &["--force"],
        auth_check: &["status"],
        auth_ok: None,
    },
];

pub fn cli_agent(bin: &str) -> Option<&'static CliAgent> {
    CLI_AGENTS.iter().find(|agent| agent.bin == bin)
}

pub fn cli_blocker(bin: &str) -> Option<String> {
    if bin != "claude" {
        return None;
    }
    let config = dirs::home_dir()?.join(".claude.json");
    let text = std::fs::read_to_string(config).ok()?;
    first_run_pending(&text).then(|| {
        "claude has never finished its first run in a terminal, so it would stop on the theme \
         question instead of reading the brief; run claude once yourself, answer it, then hand \
         off again"
            .to_string()
    })
}

fn first_run_pending(config: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(config) else {
        return false;
    };
    let has = |key: &str| value.get(key).map(|v| !v.is_null()).unwrap_or(false);
    !has("theme") && !has("hasCompletedOnboarding")
}

pub fn cli_auth_state(bin: &str) -> Option<String> {
    let agent = cli_agent(bin)?;
    if agent.auth_check.is_empty() {
        return None;
    }
    let resolved = resolve_cli(bin)?;
    let output = std::process::Command::new(resolved)
        .args(agent.auth_check)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    output
        .status
        .success()
        .then(|| match agent.auth_ok {
            Some(label) => label.to_string(),
            None => line.trim().to_string(),
        })
        .or_else(|| Some(format!("not signed in ({})", line.trim())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationKind {
    Chat,
    App,
    Cli,
}

pub fn resolve_cli(bin: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = dirs::home_dir()?;
    for dir in [".local/bin", "bin"] {
        let candidate = home.join(dir).join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn installed_clis() -> Vec<(String, String, Vec<String>)> {
    CLI_AGENTS
        .iter()
        .filter(|agent| resolve_cli(agent.bin).is_some())
        .map(|agent| {
            (
                agent.bin.to_string(),
                agent.label.to_string(),
                agent.prompt_flags.iter().map(|f| f.to_string()).collect(),
            )
        })
        .collect()
}

fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

pub fn cli_argv(bin: &str, flags: &[String], unattended: bool) -> Vec<String> {
    let mut ordered: Vec<String> = Vec::new();
    if let Some(agent) = cli_agent(bin) {
        for flag in agent.trust_flags {
            ordered.push(flag.to_string());
        }
        if unattended {
            for flag in agent.unattended_flags {
                ordered.push(flag.to_string());
            }
        }
    }
    ordered.extend(flags.iter().cloned());
    ordered
}

const LAUNCHERS_KEPT: usize = 20;

pub fn launcher_script(
    resolved: &Path,
    flags: &[String],
    workspace: &Path,
    prompt: &Path,
) -> String {
    let flag_text = flags
        .iter()
        .map(|f| shell_quote(f))
        .collect::<Vec<_>>()
        .join(" ");
    let close_window = "/usr/bin/osascript -e 'on run argv' \
         -e 'tell application \"Terminal\"' \
         -e 'repeat with w in windows' \
         -e 'repeat with t in tabs of w' \
         -e 'if tty of t is (item 1 of argv) then close w' \
         -e 'end repeat' -e 'end repeat' -e 'end tell' -e 'end run' \
         \"$vsc_tty\" >/dev/null 2>&1";
    format!(
        "#!/bin/sh\n\
         cd {workspace} || exit 1\n\
         vsc_tty=$(tty)\n\
         {bin} {flags} \"$(cat {prompt})\"\n\
         vsc_status=$?\n\
         if [ \"$vsc_status\" -eq 0 ]; then\n\
         \x20 {close_window}\n\
         else\n\
         \x20 echo\n\
         \x20 echo \"vsc-relay: the agent exited with status $vsc_status, this window stays open\"\n\
         fi\n\
         exit \"$vsc_status\"\n",
        workspace = shell_quote(&workspace.display().to_string()),
        bin = shell_quote(&resolved.display().to_string()),
        flags = flag_text,
        prompt = shell_quote(&prompt.display().to_string()),
        close_window = close_window,
    )
}

fn prune_launchers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with("launch-") && !name.starts_with("prompt-") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.into_iter().skip(LAUNCHERS_KEPT * 2) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn run_log_path() -> PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("handoff");
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    dir.join(format!("run-{stamp}.log"))
}

#[allow(clippy::too_many_arguments)]
pub async fn launch_cli_supervised(
    bin: &str,
    flags: &[String],
    workspace: &Path,
    prompt: &str,
    unattended: bool,
    session_id: &str,
    providers: &crate::automation::Providers,
    now: i64,
) -> Result<crate::clirun::SupervisedRun> {
    let resolved = resolve_cli(bin).ok_or_else(|| anyhow!("{bin} is not installed"))?;
    let argv = cli_argv(bin, flags, unattended);
    crate::clirun::run_supervised(
        &resolved,
        &argv,
        workspace,
        prompt,
        session_id,
        providers,
        run_log_path(),
        now,
    )
    .await
}

pub fn launch_cli(
    bin: &str,
    flags: &[String],
    workspace: &Path,
    prompt: &str,
    unattended: bool,
) -> Result<()> {
    if let Some(blocker) = cli_blocker(bin) {
        return Err(anyhow!("{blocker}"));
    }
    let resolved = resolve_cli(bin).ok_or_else(|| anyhow!("{bin} is not installed"))?;
    let flags = cli_argv(bin, flags, unattended);
    let flags = &flags[..];
    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".vsc-relay")
        .join("handoff");
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let prompt_path = dir.join(format!("prompt-{stamp}.txt"));
    std::fs::write(&prompt_path, prompt.as_bytes())?;

    let script_path = dir.join(format!("launch-{stamp}.sh"));
    let script = launcher_script(&resolved, flags, workspace, &prompt_path);
    std::fs::write(&script_path, script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))?;
    }

    let status = std::process::Command::new("/usr/bin/open")
        .arg("-a")
        .arg("Terminal")
        .arg(&script_path)
        .status()?;
    if !status.success() {
        return Err(anyhow!("could not open a terminal for {bin}"));
    }
    prune_launchers(&dir);
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Destination {
    pub id: String,
    pub label: String,
    pub workspace: PathBuf,
    pub kind: DestinationKind,
    pub session_id: Option<String>,
    pub app: Option<String>,
    pub cli: Option<(String, Vec<String>)>,
    pub linked: bool,
}

fn app_bundle(app: &str) -> Option<PathBuf> {
    for root in ["/Applications", "/System/Applications"] {
        let path = PathBuf::from(root).join(format!("{app}.app"));
        if path.exists() {
            return Some(path);
        }
    }
    let home = dirs::home_dir()?
        .join("Applications")
        .join(format!("{app}.app"));
    home.exists().then_some(home)
}

pub fn installed_apps() -> Vec<String> {
    EDITOR_APPS
        .iter()
        .filter(|app| app_bundle(app).is_some())
        .map(|app| app.to_string())
        .collect()
}

pub fn open_in_app(app: &str, workspace: &Path) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/open")
        .arg("-a")
        .arg(app)
        .arg(workspace)
        .status()?;
    if !status.success() {
        return Err(anyhow!("could not open {app} on {}", workspace.display()));
    }
    Ok(())
}

pub fn destination_of(id: &str, all: &[Destination]) -> Option<Destination> {
    all.iter().find(|d| d.id == id).cloned()
}

pub fn read_receipt(target: &HandoffTarget) -> Option<String> {
    let path = target.workspace.join(BRIEF_NAME);
    let body = std::fs::read_to_string(path).ok()?;
    relay_compass::handoff::extract_receipt(&body)
}

pub fn contract_of(target: &HandoffTarget) -> Option<String> {
    let path = target.workspace.join(BRIEF_NAME);
    let body = std::fs::read_to_string(path).ok()?;
    let start = body.find("## Contract")? + "## Contract".len();
    let rest = &body[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    let quoted: String = rest[..end]
        .lines()
        .filter_map(|line| line.trim().strip_prefix("> "))
        .collect::<Vec<_>>()
        .join("\n");
    (!quoted.trim().is_empty()).then_some(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending() -> PendingHandoff {
        PendingHandoff {
            source_alias: "src".to_string(),
            source_session: "s1".to_string(),
            source_agent: "claude".to_string(),
            source_workspace: PathBuf::from("/src"),
            source_branch: Some("main".to_string()),
            targets: Vec::new(),
        }
    }

    #[test]
    fn a_launched_terminal_closes_itself_only_when_the_agent_left_cleanly() {
        let script = launcher_script(
            Path::new("/usr/local/bin/agy"),
            &["-i".to_string()],
            Path::new("/tmp/work"),
            Path::new("/tmp/prompt.txt"),
        );
        assert!(!script.contains("exec "), "an exec would leak the window");
        assert!(script.contains("vsc_tty=$(tty)"));
        assert!(script.contains("if [ \"$vsc_status\" -eq 0 ]"));
        let close_at = script.find("osascript").expect("no close for the window");
        let keep_at = script
            .find("this window stays open")
            .expect("no failure notice");
        assert!(close_at < keep_at, "the window is closed on failure too");
        assert!(script.trim_end().ends_with("exit \"$vsc_status\""));
    }

    #[test]
    fn a_prompt_flag_stays_next_to_the_prompt_it_introduces() {
        for agent in CLI_AGENTS {
            let flags: Vec<String> = agent.prompt_flags.iter().map(|f| f.to_string()).collect();
            for unattended in [false, true] {
                let argv = cli_argv(agent.bin, &flags, unattended);
                if let Some(last) = agent.prompt_flags.last() {
                    assert_eq!(
                        argv.last().map(String::as_str),
                        Some(*last),
                        "{} would receive its prompt away from {last}",
                        agent.bin
                    );
                }
                for flag in agent.trust_flags {
                    assert!(argv.iter().any(|f| f == flag));
                }
                assert_eq!(
                    argv.iter()
                        .any(|f| agent.unattended_flags.contains(&f.as_str())),
                    unattended && !agent.unattended_flags.is_empty()
                );
            }
        }
    }

    #[test]
    fn every_cli_destination_can_report_whether_it_is_signed_in() {
        for agent in CLI_AGENTS {
            if agent.bin == "claude" {
                continue;
            }
            assert!(
                !agent.auth_check.is_empty(),
                "{} has no way to report its auth state",
                agent.bin
            );
        }
        for agent in CLI_AGENTS {
            if agent.auth_ok.is_some() {
                assert!(
                    !agent.auth_check.is_empty(),
                    "{} claims a success label with no check behind it",
                    agent.bin
                );
            }
        }
    }

    #[test]
    fn the_compact_is_written_by_the_sources_own_cli_when_it_is_available() {
        use crate::supervisor::discover::Discovered;
        let found = |id: &str, available: bool| Discovered {
            id: id.to_string(),
            available,
            reason: String::new(),
            local: true,
            needs_key: false,
            models: vec!["m".to_string()],
            path: None,
        };
        let configured = crate::automation::Providers {
            enabled: vec!["antigravity".to_string()],
            strategy: crate::automation::Strategy::CostOptimized,
            ..Default::default()
        };

        let own = compact_providers(
            "claude",
            &configured,
            &[found("claude-cli", true), found("antigravity", true)],
        );
        assert_eq!(own.enabled, vec!["claude-cli".to_string()]);

        let fallback = compact_providers(
            "claude",
            &configured,
            &[found("claude-cli", false), found("antigravity", true)],
        );
        assert_eq!(fallback.enabled, configured.enabled);
    }

    #[test]
    fn a_stashed_handoff_comes_back_by_token() {
        let token = stash(pending());
        let back = recall(token).unwrap();
        assert_eq!(back.source_session, "s1");
        assert!(recall(token + 9_000).is_none());
    }

    #[test]
    fn writing_a_brief_rotates_the_previous_one_and_names_it_in_the_prompt() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source_dir =
            std::env::temp_dir().join(format!("relay-handoff-src-{}-{stamp}", std::process::id()));
        let target_dir =
            std::env::temp_dir().join(format!("relay-handoff-dst-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&target_dir).unwrap();

        let transcript = source_dir.join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"add retry to the uploader and cover it with a unit test\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"on it\"}]}}\n",
        )
        .unwrap();

        let target = HandoffTarget {
            alias: "dst".to_string(),
            workspace: target_dir.clone(),
            title: "dst".to_string(),
            chat: None,
            app: None,
            cli: None,
        };

        let brief = prepare(&pending(), &transcript).unwrap();
        let first = write_brief(&pending(), &brief, &target, "2026-08-05 13:00").unwrap();
        assert_eq!(first.path, target_dir.join(BRIEF_NAME));
        assert!(first.prompt.contains(&first.path.display().to_string()));
        let body = std::fs::read_to_string(&first.path).unwrap();
        assert!(body.contains("add retry to the uploader"));

        let second = write_brief(&pending(), &brief, &target, "2026-08-05 13:05").unwrap();
        assert!(second.path.exists());
        assert!(target_dir.join(PREVIOUS_NAME).exists());
        assert!(read_receipt(&target).is_none());
        assert!(contract_of(&target)
            .unwrap()
            .contains("add retry to the uploader"));

        let _ = std::fs::remove_dir_all(source_dir);
        let _ = std::fs::remove_dir_all(target_dir);
    }

    #[test]
    fn an_unreadable_source_is_refused_rather_than_writing_an_empty_brief() {
        let target_dir = std::env::temp_dir().join(format!(
            "relay-handoff-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&target_dir).unwrap();
        let missing = target_dir.join("nope.jsonl");
        assert!(prepare(&pending(), &missing).is_err());
        assert!(!target_dir.join(BRIEF_NAME).exists());
        let _ = std::fs::remove_dir_all(target_dir);
    }

    #[test]
    fn a_cli_that_would_stop_on_its_own_first_run_is_never_launched() {
        assert!(
            first_run_pending(r#"{"installMethod":"native","autoUpdates":false}"#),
            "a config with no theme and no onboarding flag means the wizard is still ahead"
        );
        assert!(!first_run_pending(r#"{"theme":"dark"}"#));
        assert!(!first_run_pending(r#"{"hasCompletedOnboarding":true}"#));
        assert!(!first_run_pending("not json at all"));
        assert_eq!(cli_blocker("codex"), None, "the check is claude specific");
    }
}
