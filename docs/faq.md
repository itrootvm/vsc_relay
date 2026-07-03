# FAQ

## Is This A Cloud Service?

No. The relay runs on your Mac. Telegram traffic goes through Telegram Bot API by outbound
HTTPS because Telegram bots require it.

## Does It Work Without The Shim?

Yes for reading status, notifications, focus, and GUI fallback. Claude Code background
prompts and question answers require the shim.

## Does It Support Codex?

Codex support is experimental. The relay can observe Codex sessions and focus the
right VS Code window, but it cannot answer Codex questions or control Codex in the
background.

## Does It Need Accessibility Permission?

Only for window focus and GUI fallback actions. Reading session state does not require
screen scraping.

## Where Are Logs?

Runtime logs and state live under `~/.vsc-relay`.

## Can Several Macs Use One Bot?

This is planned but not a primary workflow. Use one bot per Mac if you want the
least confusing setup.

## Does The App Support Intel Macs?

The current packaged app targets Apple Silicon. Universal Intel support is on the roadmap.
