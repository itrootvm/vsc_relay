use crate::{Control, FocusProbe};
use anyhow::{bail, Context, Result};
use relay_core::AgentKind;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const KEY_RETURN: u32 = 36;
const KEY_TAB: u32 = 48;
const KEY_ESCAPE: u32 = 53;
const KEY_SPACE: u32 = 49;
const KEY_ARROW_DOWN: u32 = 125;

const EDITORS: &[&str] = &["Code", "Cursor"];
const US: char = '\u{1f}';

pub struct MacControl;

impl MacControl {
    pub fn new() -> Self {
        MacControl
    }
}

impl Default for MacControl {
    fn default() -> Self {
        Self::new()
    }
}

impl Control for MacControl {
    fn focus_window(&self, alias: &str) -> Result<()> {
        verified_action("focus", alias, "raise", "", false)
    }

    fn probe_focus(&self, alias: &str) -> Result<FocusProbe> {
        probe_and_raise(alias)
    }

    fn send_claude_session(&self, alias: &str, session_id: &str, text: &str) -> Result<()> {
        send_claude_uri(alias, Some(session_id), text)
    }

    fn send_claude_sidebar(&self, alias: &str, text: &str) -> Result<()> {
        set_clipboard(text)?;
        let keys = format!(
            "{}delay 0.2\nkeystroke \"v\" using {{command down}}\ndelay 0.2\nkey code {KEY_RETURN}\n",
            claude_focus_keys()
        );
        verified_action("send_sidebar", alias, "cmd-esc+paste+enter", &keys, true)
    }

    fn send_prompt(&self, alias: &str, agent: AgentKind, text: &str) -> Result<()> {
        match agent {
            AgentKind::ClaudeCode => send_claude_uri(alias, None, text),
            AgentKind::Codex => {
                set_clipboard(text)?;
                let keys = format!(
                    "{}delay 0.6\nkeystroke \"v\" using {{command down}}\ndelay 0.2\nkey code {KEY_RETURN}\n",
                    focus_input_script(agent)
                );
                verified_action("send_codex", alias, "focus+paste+enter", &keys, true)
            }
        }
    }

    fn stop(&self, alias: &str, _agent: AgentKind) -> Result<()> {
        verified_action(
            "stop",
            alias,
            "escape",
            &format!("delay 0.15\nkey code {KEY_ESCAPE}\n"),
            true,
        )
    }

    fn accept(&self, alias: &str) -> Result<()> {
        verified_action(
            "accept",
            alias,
            "return",
            &format!("delay 0.15\nkey code {KEY_RETURN}\n"),
            true,
        )
    }

    fn pick_option(&self, alias: &str, option_index: usize) -> Result<()> {
        let mut keys = String::from("delay 0.25\n");
        for _ in 0..option_index {
            keys.push_str(&format!("key code {KEY_ARROW_DOWN}\ndelay 0.08\n"));
        }
        keys.push_str(&format!("delay 0.1\nkey code {KEY_SPACE}\n"));
        verified_action("pick", alias, "arrows+space", &keys, true)
    }

    fn cont(&self, alias: &str, agent: AgentKind) -> Result<()> {
        self.send_prompt(alias, agent, "continue")
    }

    fn cycle_mode(&self, alias: &str, agent: AgentKind) -> Result<()> {
        if agent != AgentKind::ClaudeCode {
            bail!("mode cycling only implemented for Claude");
        }
        let keys = format!(
            "{}delay 0.1\nkey code {KEY_TAB} using {{shift down}}\n",
            focus_input_script(agent)
        );
        verified_action("mode", alias, "focus+shift-tab", &keys, true)
    }

    fn slash(&self, alias: &str, agent: AgentKind, cmd: &str) -> Result<()> {
        let normalized = if cmd.starts_with('/') {
            cmd.to_string()
        } else {
            format!("/{cmd}")
        };
        set_clipboard(&normalized)?;
        let keys = format!(
            "{}delay 0.2\nkeystroke \"v\" using {{command down}}\ndelay 0.15\nkey code {KEY_RETURN}\n",
            focus_input_script(agent)
        );
        verified_action("slash", alias, "focus+paste+enter", &keys, true)
    }
}

fn palette_command(title: &str) -> String {
    format!(
        "key code 35 using {{command down, shift down}}\n\
         delay 0.4\n\
         keystroke \"{}\"\n\
         delay 0.45\n\
         key code {KEY_RETURN}\n\
         delay 0.4\n",
        esc(title)
    )
}

fn claude_focus_keys() -> String {
    format!("key code {KEY_ESCAPE} using {{command down}}\ndelay 0.35\n")
}

fn focus_input_script(agent: AgentKind) -> String {
    match agent {
        AgentKind::ClaudeCode => claude_focus_keys(),
        AgentKind::Codex => palette_command("Open Codex Sidebar"),
    }
}

fn send_claude_uri(alias: &str, session: Option<&str>, text: &str) -> Result<()> {
    verified_action("say_uri_open", alias, "raise", "", false)?;
    std::thread::sleep(Duration::from_millis(400));
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
    let status = Command::new("open")
        .arg(&uri)
        .status()
        .context("open vscode uri")?;
    if !status.success() {
        bail!("open uri failed");
    }
    std::thread::sleep(Duration::from_millis(1800));
    verified_action(
        "say_uri_submit",
        alias,
        "return",
        &format!("delay 0.15\nkey code {KEY_RETURN}\n"),
        false,
    )
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

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn set_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn pbcopy")?;
    child
        .stdin
        .as_mut()
        .context("pbcopy stdin")?
        .write_all(text.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("pbcopy failed");
    }
    Ok(())
}

fn run_osa_out(script: &str) -> Result<String> {
    use std::io::Read;
    use std::time::Instant;
    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn osascript")?;
    let deadline = Duration::from_secs(12);
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().context("wait osascript")? {
            break s;
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "osascript timed out after {}s (GUI did not respond)",
                deadline.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    if !status.success() {
        bail!("osascript failed: {}", err.trim());
    }
    Ok(out.trim().to_string())
}

fn focus_action_script(alias: &str, keys_block: &str, restore: bool) -> String {
    let a = esc(alias);
    let editors = EDITORS
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let restore_block = if restore {
        "delay 0.35\n\
         if frontBefore is not \"\" and frontBefore is not targetProc then\n\
         try\n\
         tell application process frontBefore to set frontmost to true\n\
         end try\n\
         else if focusedBefore is not \"\" and focusedBefore is not matchedTitle then\n\
         try\n\
         tell application process targetProc\n\
         set rw to (first window whose name is focusedBefore)\n\
         perform action \"AXRaise\" of rw\n\
         set value of attribute \"AXMain\" of rw to true\n\
         end tell\n\
         end try\n\
         end if\n"
    } else {
        ""
    };
    format!(
        "set sep to (ASCII character 31)\n\
         set frontBefore to \"\"\n\
         set focusedBefore to \"\"\n\
         set targetProc to \"\"\n\
         set matchCount to 0\n\
         set matchedTitle to \"\"\n\
         set focusedAfter to \"\"\n\
         set frontAfter to \"\"\n\
         set didSend to false\n\
         tell application \"System Events\"\n\
         try\n\
         set frontBefore to name of first application process whose frontmost is true\n\
         end try\n\
         repeat with pn in {{{editors}}}\n\
         set procName to (pn as text)\n\
         if exists (application process procName) then\n\
         tell application process procName\n\
         set wins to (every window whose name ends with \"{a}\")\n\
         if (count of wins) > 0 then\n\
         set targetProc to procName\n\
         set matchCount to (count of wins)\n\
         set matchedTitle to name of item 1 of wins\n\
         end if\n\
         end tell\n\
         end if\n\
         if targetProc is not \"\" then exit repeat\n\
         end repeat\n\
         if targetProc is not \"\" then\n\
         tell application process targetProc\n\
         try\n\
         set focusedBefore to name of (value of attribute \"AXFocusedWindow\")\n\
         end try\n\
         set frontmost to true\n\
         set w to (first window whose name ends with \"{a}\")\n\
         try\n\
         perform action \"AXRaise\" of w\n\
         end try\n\
         try\n\
         set value of attribute \"AXMain\" of w to true\n\
         end try\n\
         end tell\n\
         repeat 12 times\n\
         delay 0.05\n\
         tell application process targetProc\n\
         try\n\
         set focusedAfter to name of (value of attribute \"AXFocusedWindow\")\n\
         end try\n\
         end tell\n\
         if focusedAfter ends with \"{a}\" then exit repeat\n\
         end repeat\n\
         try\n\
         set frontAfter to name of first application process whose frontmost is true\n\
         end try\n\
         if focusedAfter ends with \"{a}\" then\n\
         set didSend to true\n\
         {keys_block}\
         {restore_block}\
         end if\n\
         end if\n\
         end tell\n\
         return frontBefore & sep & focusedBefore & sep & (matchCount as text) & sep & matchedTitle & sep & targetProc & sep & frontAfter & sep & focusedAfter & sep & (didSend as text)\n"
    )
}

struct ActionResult {
    probe: FocusProbe,
    did_send: bool,
}

fn parse_action(out: &str, alias: &str) -> ActionResult {
    let f: Vec<&str> = out.split(US).collect();
    let get = |i: usize| f.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
    let match_count: u32 = get(2).parse().unwrap_or(0);
    let target_proc = get(4);
    let front_after = get(5);
    let focused_after = get(6);
    let did_send = get(7) == "true";
    let focus_ok = !focused_after.is_empty() && focused_after.ends_with(alias);
    let front_ok = !target_proc.is_empty() && front_after == target_proc;
    let verified = match_count >= 1 && front_ok && focus_ok;
    let reason = if match_count == 0 {
        format!("no window titled '… {alias}' in {EDITORS:?}")
    } else if !front_ok {
        format!("frontmost app '{front_after}' != target '{target_proc}'")
    } else if !focus_ok {
        format!("focused window '{focused_after}' does not end with alias '{alias}'")
    } else {
        "ok".to_string()
    };
    ActionResult {
        probe: FocusProbe {
            verified,
            target_proc: if target_proc.is_empty() {
                None
            } else {
                Some(target_proc)
            },
            match_count,
            matched_window: get(3),
            frontmost_app: front_after,
            focused_before: get(1),
            focused_after,
            reason,
        },
        did_send,
    }
}

fn probe_and_raise(alias: &str) -> Result<FocusProbe> {
    let out = run_osa_out(&focus_action_script(alias, "", false))?;
    Ok(parse_action(&out, alias).probe)
}

fn verified_action(
    action: &str,
    alias: &str,
    keys_desc: &str,
    keys_block: &str,
    restore: bool,
) -> Result<()> {
    let out = run_osa_out(&focus_action_script(alias, keys_block, restore))?;
    let r = parse_action(&out, alias);
    let p = &r.probe;
    if p.match_count > 1 {
        tracing::warn!(
            action,
            alias,
            match_count = p.match_count,
            "relay.control: multiple windows share alias - ambiguous target"
        );
    }
    tracing::info!(
        action,
        alias,
        matched_window = %p.matched_window,
        focused_before = %p.focused_before,
        focused_after = %p.focused_after,
        frontmost_app = %p.frontmost_app,
        keys = keys_desc,
        verified = p.verified,
        did_send = r.did_send,
        "relay.control"
    );
    if !p.verified || !r.did_send {
        bail!(
            "window not found / focus not on target ({alias}): {}",
            p.reason
        );
    }
    Ok(())
}
