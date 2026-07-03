use crate::{Control, FocusProbe};
use anyhow::{bail, Context, Result};
use relay_core::AgentKind;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const APP_SUFFIXES: &[(&str, &str)] = &[
    (" - Visual Studio Code - Insiders", "Code - Insiders"),
    (" - Visual Studio Code", "Code"),
    (" - Cursor", "Cursor"),
    (" - Windsurf", "Windsurf"),
    (" - VSCodium", "VSCodium"),
    (" - Code - OSS", "Code - OSS"),
];

pub struct LinuxControl;

impl LinuxControl {
    pub fn new() -> Self {
        LinuxControl
    }
}

impl Default for LinuxControl {
    fn default() -> Self {
        Self::new()
    }
}

impl Control for LinuxControl {
    fn focus_window(&self, alias: &str) -> Result<()> {
        with_focus("focus", alias, false, |_| Ok(()))
    }

    fn probe_focus(&self, alias: &str) -> Result<FocusProbe> {
        ensure_gui()?;
        let obs = find_and_activate(alias, true)?;
        Ok(obs.probe(alias))
    }

    fn send_claude_session(&self, alias: &str, session_id: &str, text: &str) -> Result<()> {
        send_claude_uri(alias, Some(session_id), text)
    }

    fn send_claude_sidebar(&self, alias: &str, text: &str) -> Result<()> {
        ensure_gui()?;
        set_clipboard(text)?;
        with_focus("send_sidebar", alias, true, |_| {
            claude_focus()?;
            sleep(Duration::from_millis(200));
            paste()?;
            sleep(Duration::from_millis(200));
            key("Return")
        })
    }

    fn send_prompt(&self, alias: &str, agent: AgentKind, text: &str) -> Result<()> {
        match agent {
            AgentKind::ClaudeCode => send_claude_uri(alias, None, text),
            AgentKind::Codex => {
                ensure_gui()?;
                set_clipboard(text)?;
                with_focus("send_codex", alias, true, |_| {
                    focus_input(AgentKind::Codex)?;
                    sleep(Duration::from_millis(600));
                    paste()?;
                    sleep(Duration::from_millis(200));
                    key("Return")
                })
            }
        }
    }

    fn stop(&self, alias: &str, _agent: AgentKind) -> Result<()> {
        with_focus("stop", alias, true, |_| {
            sleep(Duration::from_millis(150));
            key("Escape")
        })
    }

    fn accept(&self, alias: &str) -> Result<()> {
        with_focus("accept", alias, true, |_| {
            sleep(Duration::from_millis(150));
            key("Return")
        })
    }

    fn pick_option(&self, alias: &str, option_index: usize) -> Result<()> {
        with_focus("pick", alias, true, |_| {
            sleep(Duration::from_millis(250));
            for _ in 0..option_index {
                key("Down")?;
                sleep(Duration::from_millis(80));
            }
            sleep(Duration::from_millis(100));
            key("space")
        })
    }

    fn cont(&self, alias: &str, agent: AgentKind) -> Result<()> {
        self.send_prompt(alias, agent, "continue")
    }

    fn cycle_mode(&self, alias: &str, agent: AgentKind) -> Result<()> {
        if agent != AgentKind::ClaudeCode {
            bail!("mode cycling only implemented for Claude");
        }
        with_focus("mode", alias, true, |_| {
            claude_focus()?;
            sleep(Duration::from_millis(100));
            key("shift+Tab")
        })
    }

    fn slash(&self, alias: &str, agent: AgentKind, cmd: &str) -> Result<()> {
        ensure_gui()?;
        let normalized = if cmd.starts_with('/') {
            cmd.to_string()
        } else {
            format!("/{cmd}")
        };
        set_clipboard(&normalized)?;
        with_focus("slash", alias, true, |_| {
            focus_input(agent)?;
            sleep(Duration::from_millis(200));
            paste()?;
            sleep(Duration::from_millis(150));
            key("Return")
        })
    }
}

fn claude_focus() -> Result<()> {
    key("ctrl+Escape")?;
    sleep(Duration::from_millis(350));
    Ok(())
}

fn focus_input(agent: AgentKind) -> Result<()> {
    match agent {
        AgentKind::ClaudeCode => claude_focus(),
        AgentKind::Codex => palette_command("Open Codex Sidebar"),
    }
}

fn palette_command(title: &str) -> Result<()> {
    key("ctrl+shift+p")?;
    sleep(Duration::from_millis(400));
    type_text(title)?;
    sleep(Duration::from_millis(450));
    key("Return")?;
    sleep(Duration::from_millis(400));
    Ok(())
}

fn send_claude_uri(alias: &str, session: Option<&str>, text: &str) -> Result<()> {
    with_focus("say_uri_open", alias, false, |_| Ok(()))?;
    sleep(Duration::from_millis(400));
    let uri = match session {
        Some(sid) => format!(
            "vscode://Anthropic.claude-code/open?session={}&prompt={}",
            percent_encode(sid),
            percent_encode(text)
        ),
        None => format!(
            "vscode://Anthropic.claude-code/open?prompt={}",
            percent_encode(text)
        ),
    };
    open_uri(&uri)?;
    sleep(Duration::from_millis(1800));
    with_focus("say_uri_submit", alias, false, |_| {
        sleep(Duration::from_millis(150));
        key("Return")
    })
}

struct Win {
    id: String,
    title: String,
}

struct Observed {
    match_count: u32,
    matched_title: String,
    target_proc: Option<String>,
    focused_before: String,
    focused_after: String,
    front_id_before: String,
    front_id_after: String,
}

impl Observed {
    fn probe(&self, alias: &str) -> FocusProbe {
        let focus_ok = title_matches(&self.focused_after, alias);
        let verified = self.match_count >= 1 && focus_ok;
        let reason = if self.match_count == 0 {
            no_window_reason(alias)
        } else if !focus_ok {
            format!(
                "focused window '{}' is not workspace '{alias}'",
                self.focused_after
            )
        } else {
            "ok".to_string()
        };
        FocusProbe {
            verified,
            target_proc: self.target_proc.clone(),
            match_count: self.match_count,
            matched_window: self.matched_title.clone(),
            frontmost_app: self.target_proc.clone().unwrap_or_default(),
            focused_before: self.focused_before.clone(),
            focused_after: self.focused_after.clone(),
            reason,
        }
    }
}

fn with_focus<F>(action: &str, alias: &str, restore: bool, keys: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<()>,
{
    ensure_gui()?;
    let obs = find_and_activate(alias, true)?;
    let probe = obs.probe(alias);
    if probe.match_count > 1 {
        tracing::warn!(
            action,
            alias,
            match_count = probe.match_count,
            "relay.control: multiple windows share alias - ambiguous target"
        );
    }
    tracing::info!(
        action,
        alias,
        matched_window = %probe.matched_window,
        focused_before = %probe.focused_before,
        focused_after = %probe.focused_after,
        frontmost_app = %probe.frontmost_app,
        verified = probe.verified,
        "relay.control"
    );
    if !probe.verified {
        bail!(
            "window not found / focus not on target ({alias}): {}",
            probe.reason
        );
    }
    let target_id = obs.front_id_after.clone();
    keys(&target_id)?;
    if restore && !obs.front_id_before.is_empty() && obs.front_id_before != target_id {
        sleep(Duration::from_millis(300));
        let _ = activate(&obs.front_id_before);
    }
    Ok(())
}

fn find_and_activate(alias: &str, do_activate: bool) -> Result<Observed> {
    let wins = editor_windows()?;
    let matches: Vec<&Win> = wins
        .iter()
        .filter(|w| title_matches(&w.title, alias))
        .collect();
    let match_count = matches.len() as u32;

    let front_id_before = active_id();
    let focused_before = front_id_before
        .as_deref()
        .and_then(window_title)
        .unwrap_or_default();

    let target = matches.first();
    let target_proc = target.map(|w| editor_name(&w.title).to_string());
    let matched_title = target.map(|w| w.title.clone()).unwrap_or_default();

    let mut focused_after = focused_before.clone();
    let mut front_id_after = front_id_before.clone().unwrap_or_default();

    if let Some(w) = target {
        if do_activate {
            activate(&w.id)?;
            for _ in 0..14 {
                sleep(Duration::from_millis(50));
                if let Some(cur) = active_id() {
                    let title = window_title(&cur).unwrap_or_default();
                    focused_after = title;
                    front_id_after = cur;
                    if title_matches(&focused_after, alias) {
                        break;
                    }
                }
            }
        } else {
            front_id_after = w.id.clone();
            focused_after = w.title.clone();
        }
    }

    Ok(Observed {
        match_count,
        matched_title,
        target_proc,
        focused_before,
        focused_after,
        front_id_before: front_id_before.unwrap_or_default(),
        front_id_after,
    })
}

fn editor_windows() -> Result<Vec<Win>> {
    let res = Command::new("xdotool")
        .args(["search", "--name", "."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("spawn xdotool")?;
    let stderr = String::from_utf8_lossy(&res.stderr);
    if !res.status.success() && !stderr.trim().is_empty() {
        bail!("xdotool could not query windows: {}", stderr.trim());
    }
    let ids = String::from_utf8_lossy(&res.stdout);
    let mut out = Vec::new();
    for id in ids.split_whitespace() {
        let title = window_title(id).unwrap_or_default();
        if !title.is_empty() && strip_app_suffix(&title).is_some() {
            out.push(Win {
                id: id.to_string(),
                title,
            });
        }
    }
    Ok(out)
}

fn active_id() -> Option<String> {
    let s = output("xdotool", &["getactivewindow"]).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn window_title(id: &str) -> Option<String> {
    output("xdotool", &["getwindowname", id])
        .ok()
        .map(|s| s.trim().to_string())
}

fn activate(id: &str) -> Result<()> {
    output("xdotool", &["windowactivate", id]).map(|_| ())
}

fn key(spec: &str) -> Result<()> {
    output("xdotool", &["key", "--clearmodifiers", spec]).map(|_| ())
}

fn type_text(text: &str) -> Result<()> {
    output(
        "xdotool",
        &["type", "--clearmodifiers", "--delay", "12", "--", text],
    )
    .map(|_| ())
}

fn paste() -> Result<()> {
    key("ctrl+v")
}

fn open_uri(uri: &str) -> Result<()> {
    require_tool("xdg-open")?;
    let status = Command::new("xdg-open")
        .arg(uri)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawn xdg-open")?;
    if !status.success() {
        bail!("xdg-open failed for vscode uri (is a vscode:// handler registered?)");
    }
    Ok(())
}

fn set_clipboard(text: &str) -> Result<()> {
    if which("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn xclip")?;
        child
            .stdin
            .as_mut()
            .context("xclip stdin")?
            .write_all(text.as_bytes())?;
        drop(child.stdin.take());
        let status = child.wait()?;
        if !status.success() {
            bail!("xclip failed");
        }
        return Ok(());
    }
    if which("xsel") {
        let mut child = Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn xsel")?;
        child
            .stdin
            .as_mut()
            .context("xsel stdin")?
            .write_all(text.as_bytes())?;
        drop(child.stdin.take());
        let status = child.wait()?;
        if !status.success() {
            bail!("xsel failed");
        }
        return Ok(());
    }
    bail!("clipboard tool missing: install xclip (sudo apt install xclip) for paste-based actions")
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawn {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn which(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program} >/dev/null 2>&1"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn require_tool(program: &str) -> Result<()> {
    if which(program) {
        Ok(())
    } else {
        bail!("required tool '{program}' not found; install it (e.g. sudo apt install xdotool xclip xdg-utils)")
    }
}

fn ensure_gui() -> Result<()> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("no X display (DISPLAY is unset); window focus / GUI control needs an X11 or XWayland session. The background shim path still controls Claude Code without a display.");
    }
    require_tool("xdotool")?;
    if is_wayland() {
        tracing::warn!(
            "relay.control: Wayland session detected; key injection into other windows may be blocked by the compositor. Prefer an X11/Xorg session for GUI fallback."
        );
    }
    Ok(())
}

fn is_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false)
}

fn no_window_reason(alias: &str) -> String {
    let mut r =
        format!("no VS Code window found for workspace '{alias}' (is it open? can xdotool reach the display?)");
    if is_wayland() {
        r.push_str(" — Wayland session: native-Wayland editor windows are invisible to xdotool; use an Xorg/X11 session or force XWayland");
    }
    r
}

fn strip_app_suffix(title: &str) -> Option<&str> {
    for (suffix, _) in APP_SUFFIXES {
        if let Some(base) = title.strip_suffix(suffix) {
            return Some(base.trim());
        }
    }
    None
}

fn editor_name(title: &str) -> &'static str {
    for (suffix, name) in APP_SUFFIXES {
        if title.ends_with(suffix) {
            return name;
        }
    }
    "Code"
}

fn title_matches(title: &str, alias: &str) -> bool {
    match strip_app_suffix(title) {
        Some(base) => base == alias || base.ends_with(&format!(" - {alias}")),
        None => false,
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_linux_vscode_titles() {
        assert!(title_matches(
            ".env - vsc_relay - Visual Studio Code",
            "vsc_relay"
        ));
        assert!(title_matches("vsc_relay - Visual Studio Code", "vsc_relay"));
        assert!(title_matches(
            "Untitled-1 - kartel_bot - Visual Studio Code",
            "kartel_bot"
        ));
        assert!(title_matches(
            "main.rs - proj - Visual Studio Code - Insiders",
            "proj"
        ));
        assert!(title_matches("file - proj - Cursor", "proj"));
        assert!(!title_matches(
            ".env - vsc_relay - Visual Studio Code",
            "relay"
        ));
        assert!(!title_matches("some random window", "vsc_relay"));
    }

    #[test]
    fn extracts_editor_name() {
        assert_eq!(editor_name("x - Visual Studio Code"), "Code");
        assert_eq!(editor_name("x - Cursor"), "Cursor");
        assert_eq!(
            editor_name("x - Visual Studio Code - Insiders"),
            "Code - Insiders"
        );
    }
}
