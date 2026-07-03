use anyhow::{bail, Result};
use relay_core::AgentKind;

pub fn is_control_command(arg: &str) -> bool {
    matches!(
        arg,
        "focus" | "say" | "stop" | "cont" | "mode" | "slash" | "selftest"
    )
}

fn parse_agent(s: &str) -> Result<AgentKind> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude" => Ok(AgentKind::ClaudeCode),
        "codex" => Ok(AgentKind::Codex),
        other => bail!("unknown agent '{other}' (use claude|codex)"),
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .try_init();
    let ctl = relay_control::platform();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "selftest" => {
            let alias = required(args, 1, "usage: selftest <alias>")?;
            let p = ctl.probe_focus(alias)?;
            println!("selftest {alias}");
            println!("  verified       : {}", p.verified);
            println!("  reason         : {}", p.reason);
            println!(
                "  target_proc    : {}",
                p.target_proc.as_deref().unwrap_or("-")
            );
            println!("  match_count    : {}", p.match_count);
            println!("  matched_window : {}", p.matched_window);
            println!("  frontmost_app  : {}", p.frontmost_app);
            println!("  focused_before : {}", p.focused_before);
            println!("  focused_after  : {}", p.focused_after);
            if p.match_count > 1 {
                println!(
                    "  ⚠️  {} windows share this alias - ambiguous target",
                    p.match_count
                );
            }
            if !p.verified {
                std::process::exit(2);
            }
        }
        "focus" => {
            let alias = required(args, 1, "usage: focus <alias>")?;
            ctl.focus_window(alias)?;
            println!("focused {alias}");
        }
        "say" => {
            let alias = required(args, 1, "usage: say <alias> <claude|codex> <text...>")?;
            let agent = parse_agent(required(
                args,
                2,
                "usage: say <alias> <claude|codex> <text...>",
            )?)?;
            let text = tail(args, 3, "usage: say <alias> <claude|codex> <text...>")?;
            ctl.send_prompt(alias, agent, &text)?;
            println!("sent to {alias}/{agent}: {text}");
        }
        "stop" => {
            let alias = required(args, 1, "usage: stop <alias> <claude|codex>")?;
            let agent = parse_agent(required(args, 2, "usage: stop <alias> <claude|codex>")?)?;
            ctl.stop(alias, agent)?;
            println!("stopped {alias}/{agent}");
        }
        "cont" => {
            let alias = required(args, 1, "usage: cont <alias> <claude|codex>")?;
            let agent = parse_agent(required(args, 2, "usage: cont <alias> <claude|codex>")?)?;
            ctl.cont(alias, agent)?;
            println!("continued {alias}/{agent}");
        }
        "mode" => {
            let alias = required(args, 1, "usage: mode <alias>")?;
            ctl.cycle_mode(alias, AgentKind::ClaudeCode)?;
            println!("cycled mode {alias}");
        }
        "slash" => {
            let alias = required(args, 1, "usage: slash <alias> <claude|codex> <command>")?;
            let agent = parse_agent(required(
                args,
                2,
                "usage: slash <alias> <claude|codex> <command>",
            )?)?;
            let slash = tail(args, 3, "usage: slash <alias> <claude|codex> <command>")?;
            ctl.slash(alias, agent, &slash)?;
            println!("slash {slash} -> {alias}/{agent}");
        }
        other => bail!("unknown control command '{other}'"),
    }
    Ok(())
}

fn required<'a>(args: &'a [String], index: usize, usage: &str) -> Result<&'a str> {
    let value = args.get(index).map(|s| s.trim()).unwrap_or("");
    if value.is_empty() {
        bail!("{usage}");
    }
    Ok(value)
}

fn tail(args: &[String], index: usize, usage: &str) -> Result<String> {
    let value = args
        .get(index..)
        .map(|s| s.join(" "))
        .unwrap_or_default()
        .trim()
        .to_string();
    if value.is_empty() {
        bail!("{usage}");
    }
    Ok(value)
}
