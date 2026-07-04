use crate::{Control, FocusProbe};
use anyhow::{bail, Context, Result};
use relay_core::AgentKind;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;
use windows_sys::Win32::Foundation::{BOOL, HANDLE, HWND, LPARAM};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, KEYBDINPUT};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow, ShowWindow,
};

const APP_SUFFIXES: &[(&str, &str)] = &[
    (" - Visual Studio Code - Insiders", "Code - Insiders"),
    (" - Visual Studio Code", "Code"),
    (" - Cursor", "Cursor"),
    (" - Windsurf", "Windsurf"),
    (" - VSCodium", "VSCodium"),
    (" - Code - OSS", "Code - OSS"),
];

const INPUT_KEYBOARD: u32 = 1;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const SW_RESTORE: i32 = 9;
const SW_SHOWNORMAL: i32 = 1;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

const VK_RETURN: u16 = 0x0D;
const VK_ESCAPE: u16 = 0x1B;
const VK_TAB: u16 = 0x09;
const VK_SPACE: u16 = 0x20;
const VK_DOWN: u16 = 0x28;
const VK_UP: u16 = 0x26;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;

pub struct WindowsControl;

impl WindowsControl {
    pub fn new() -> Self {
        WindowsControl
    }
}

impl Default for WindowsControl {
    fn default() -> Self {
        Self::new()
    }
}

impl Control for WindowsControl {
    fn focus_window(&self, alias: &str) -> Result<()> {
        with_focus("focus", alias, false, |_| Ok(()))
    }

    fn probe_focus(&self, alias: &str) -> Result<FocusProbe> {
        let obs = find_and_activate(alias, true);
        Ok(obs.probe(alias))
    }

    fn send_claude_session(&self, alias: &str, session_id: &str, text: &str) -> Result<()> {
        send_claude_uri(alias, Some(session_id), text)
    }

    fn send_claude_sidebar(&self, alias: &str, text: &str) -> Result<()> {
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
    palette_command("Claude Code: Focus on Claude Code View")?;
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
    hwnd: isize,
    title: String,
}

struct Observed {
    match_count: u32,
    matched_title: String,
    target_proc: Option<String>,
    focused_before: String,
    focused_after: String,
    front_id_before: isize,
    front_id_after: isize,
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
    F: FnOnce(isize) -> Result<()>,
{
    let obs = find_and_activate(alias, true);
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
    let target_id = obs.front_id_after;
    keys(target_id)?;
    if restore && obs.front_id_before != 0 && obs.front_id_before != target_id {
        sleep(Duration::from_millis(300));
        activate(obs.front_id_before);
    }
    Ok(())
}

fn find_and_activate(alias: &str, do_activate: bool) -> Observed {
    let wins = editor_windows();
    let matches: Vec<&Win> = wins
        .iter()
        .filter(|w| title_matches(&w.title, alias))
        .collect();
    let match_count = matches.len() as u32;

    let front_id_before = foreground_hwnd();
    let focused_before = window_title(front_id_before);

    let target = matches.first();
    let target_proc = target.map(|w| editor_name(&w.title).to_string());
    let matched_title = target.map(|w| w.title.clone()).unwrap_or_default();

    let mut focused_after = focused_before.clone();
    let mut front_id_after = front_id_before;

    if let Some(w) = target {
        if do_activate {
            activate(w.hwnd);
            for _ in 0..14 {
                sleep(Duration::from_millis(50));
                let cur = foreground_hwnd();
                let title = window_title(cur);
                focused_after = title;
                front_id_after = cur;
                if title_matches(&focused_after, alias) {
                    break;
                }
            }
        } else {
            front_id_after = w.hwnd;
            focused_after = w.title.clone();
        }
    }

    Observed {
        match_count,
        matched_title,
        target_proc,
        focused_before,
        focused_after,
        front_id_before,
        front_id_after,
    }
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        if IsWindowVisible(hwnd) != 0 {
            let title = window_text(hwnd);
            if !title.is_empty() && strip_app_suffix(&title).is_some() {
                let wins = &mut *(lparam as *mut Vec<Win>);
                wins.push(Win {
                    hwnd: hwnd as isize,
                    title,
                });
            }
        }
    }
    1
}

fn editor_windows() -> Vec<Win> {
    let mut wins: Vec<Win> = Vec::new();
    unsafe {
        EnumWindows(Some(enum_proc), &mut wins as *mut _ as LPARAM);
    }
    wins
}

fn foreground_hwnd() -> isize {
    let h = unsafe { GetForegroundWindow() };
    h as isize
}

fn window_text(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len + 1) as usize];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

fn window_title(id: isize) -> String {
    if id == 0 {
        return String::new();
    }
    window_text(id as HWND)
}

fn activate(id: isize) {
    if id == 0 {
        return;
    }
    unsafe {
        let h = id as HWND;
        ShowWindow(h, SW_RESTORE);
        let fg = GetForegroundWindow();
        let cur_thread = GetCurrentThreadId();
        let fg_thread = GetWindowThreadProcessId(fg, null_mut());
        let attach = fg_thread != 0 && fg_thread != cur_thread;
        if attach {
            AttachThreadInput(cur_thread, fg_thread, 1);
        }
        SetForegroundWindow(h);
        BringWindowToTop(h);
        if attach {
            AttachThreadInput(cur_thread, fg_thread, 0);
        }
    }
}

unsafe fn send_key_event(vk: u16, scan: u16, flags: u32) {
    let mut input: INPUT = std::mem::zeroed();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous = INPUT_0 {
        ki: KEYBDINPUT {
            wVk: vk,
            wScan: scan,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        },
    };
    SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
}

fn key(spec: &str) -> Result<()> {
    let parts: Vec<&str> = spec.split('+').collect();
    let (main, mods) = parts.split_last().context("empty key spec")?;
    let main_vk = vk_for(main).with_context(|| format!("unknown key '{main}'"))?;
    let mod_vks: Vec<u16> = mods.iter().filter_map(|m| modifier_vk(m)).collect();
    unsafe {
        for &m in &mod_vks {
            send_key_event(m, 0, 0);
        }
        send_key_event(main_vk, 0, 0);
        send_key_event(main_vk, 0, KEYEVENTF_KEYUP);
        for &m in mod_vks.iter().rev() {
            send_key_event(m, 0, KEYEVENTF_KEYUP);
        }
    }
    sleep(Duration::from_millis(20));
    Ok(())
}

fn vk_for(k: &str) -> Option<u16> {
    Some(match k {
        "Return" => VK_RETURN,
        "Escape" => VK_ESCAPE,
        "Tab" => VK_TAB,
        "space" => VK_SPACE,
        "Down" => VK_DOWN,
        "Up" => VK_UP,
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap().to_ascii_uppercase();
            if c.is_ascii_alphanumeric() {
                c as u16
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

fn modifier_vk(m: &str) -> Option<u16> {
    Some(match m {
        "ctrl" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" => VK_MENU,
        _ => return None,
    })
}

fn type_text(text: &str) -> Result<()> {
    unsafe {
        for u in text.encode_utf16() {
            send_key_event(0, u, KEYEVENTF_UNICODE);
            send_key_event(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        }
    }
    Ok(())
}

fn paste() -> Result<()> {
    key("ctrl+v")
}

fn set_clipboard(text: &str) -> Result<()> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(null_mut()) == 0 {
            bail!("OpenClipboard failed");
        }
        EmptyClipboard();
        let bytes = wide.len() * 2;
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if hmem.is_null() {
            CloseClipboard();
            bail!("GlobalAlloc failed");
        }
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            CloseClipboard();
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        GlobalUnlock(hmem);
        if SetClipboardData(CF_UNICODETEXT, hmem as HANDLE).is_null() {
            CloseClipboard();
            bail!("SetClipboardData failed");
        }
        CloseClipboard();
    }
    Ok(())
}

fn open_uri(uri: &str) -> Result<()> {
    let verb: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = uri.encode_utf16().chain(std::iter::once(0)).collect();
    let r = unsafe {
        ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        )
    };
    if (r as isize) <= 32 {
        bail!("ShellExecuteW failed for vscode uri (is a vscode:// handler registered?)");
    }
    Ok(())
}

fn no_window_reason(alias: &str) -> String {
    format!("no VS Code window found for workspace '{alias}' (is it open on this desktop session?)")
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
    fn matches_windows_vscode_titles() {
        assert!(title_matches(
            ".env - vsc_relay - Visual Studio Code",
            "vsc_relay"
        ));
        assert!(title_matches("vsc_relay - Visual Studio Code", "vsc_relay"));
        assert!(title_matches("file - proj - Cursor", "proj"));
        assert!(!title_matches(
            ".env - vsc_relay - Visual Studio Code",
            "relay"
        ));
        assert!(!title_matches("some random window", "vsc_relay"));
    }

    #[test]
    fn vk_maps_letters_and_named_keys() {
        assert_eq!(vk_for("v"), Some(0x56));
        assert_eq!(vk_for("p"), Some(0x50));
        assert_eq!(vk_for("Return"), Some(VK_RETURN));
        assert_eq!(modifier_vk("ctrl"), Some(VK_CONTROL));
    }
}
