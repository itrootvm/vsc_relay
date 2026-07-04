use anyhow::Result;
use relay_core::AgentKind;

#[derive(Debug, Clone)]
pub struct FocusProbe {
    pub verified: bool,
    pub target_proc: Option<String>,
    pub match_count: u32,
    pub matched_window: String,
    pub frontmost_app: String,
    pub focused_before: String,
    pub focused_after: String,
    pub reason: String,
}

pub trait Control: Send + Sync {
    fn focus_window(&self, alias: &str) -> Result<()>;
    fn probe_focus(&self, alias: &str) -> Result<FocusProbe>;
    fn send_prompt(&self, alias: &str, agent: AgentKind, text: &str) -> Result<()>;
    fn send_claude_session(&self, alias: &str, session_id: &str, text: &str) -> Result<()>;
    fn send_claude_sidebar(&self, alias: &str, text: &str) -> Result<()>;
    fn stop(&self, alias: &str, agent: AgentKind) -> Result<()>;
    fn accept(&self, alias: &str) -> Result<()>;
    fn pick_option(&self, alias: &str, option_index: usize) -> Result<()>;
    fn cont(&self, alias: &str, agent: AgentKind) -> Result<()>;
    fn cycle_mode(&self, alias: &str, agent: AgentKind) -> Result<()>;
    fn slash(&self, alias: &str, agent: AgentKind, cmd: &str) -> Result<()>;
}

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub fn platform() -> Box<dyn Control> {
    Box::new(macos::MacControl::new())
}

#[cfg(target_os = "linux")]
pub fn platform() -> Box<dyn Control> {
    Box::new(linux::LinuxControl::new())
}

#[cfg(target_os = "windows")]
pub fn platform() -> Box<dyn Control> {
    Box::new(windows::WindowsControl::new())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn platform() -> Box<dyn Control> {
    Box::new(unsupported::Unsupported)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod unsupported {
    use super::{Control, FocusProbe};
    use anyhow::Result;
    use relay_core::AgentKind;

    pub struct Unsupported;

    fn err() -> anyhow::Error {
        anyhow::anyhow!(
            "GUI control is not implemented on this platform; the background shim path still works for Claude Code"
        )
    }

    impl Control for Unsupported {
        fn focus_window(&self, _alias: &str) -> Result<()> {
            Err(err())
        }
        fn probe_focus(&self, _alias: &str) -> Result<FocusProbe> {
            Err(err())
        }
        fn send_prompt(&self, _alias: &str, _agent: AgentKind, _text: &str) -> Result<()> {
            Err(err())
        }
        fn send_claude_session(&self, _alias: &str, _session_id: &str, _text: &str) -> Result<()> {
            Err(err())
        }
        fn send_claude_sidebar(&self, _alias: &str, _text: &str) -> Result<()> {
            Err(err())
        }
        fn stop(&self, _alias: &str, _agent: AgentKind) -> Result<()> {
            Err(err())
        }
        fn accept(&self, _alias: &str) -> Result<()> {
            Err(err())
        }
        fn pick_option(&self, _alias: &str, _option_index: usize) -> Result<()> {
            Err(err())
        }
        fn cont(&self, _alias: &str, _agent: AgentKind) -> Result<()> {
            Err(err())
        }
        fn cycle_mode(&self, _alias: &str, _agent: AgentKind) -> Result<()> {
            Err(err())
        }
        fn slash(&self, _alias: &str, _agent: AgentKind, _cmd: &str) -> Result<()> {
            Err(err())
        }
    }
}
