# FAQ

## Is This A Cloud Service?

No. The relay runs locally on your own machine (macOS, Linux, or Windows). Telegram traffic
goes through Telegram Bot API by outbound HTTPS because Telegram bots require it.

## Does It Work Without The Shim?

Yes for reading status, notifications, focus, and GUI fallback. Claude Code background
prompts and question answers require the shim.

## Does It Support Codex?

Codex support is experimental. The relay can observe Codex sessions and focus the
right VS Code window, but it cannot answer Codex questions or control Codex in the
background.

## Does It Need Accessibility Permission?

Only for window focus and GUI fallback actions. Reading session state does not require
screen scraping. Accessibility is the macOS backend; on Linux the same actions use X11
(xdotool) and on Windows they use the Win32 API with an interactive desktop session.

## Where Are Logs?

Runtime logs and state live under `~/.vsc-relay`.

## Can Several Machines Use One Bot?

This is planned but not a primary workflow. Use one bot per machine if you want the
least confusing setup.

## Does The App Support Intel Macs?

The current packaged app targets Apple Silicon. Universal Intel support is on the roadmap.
