# Codex Support

Claude Code is the primary supported target. Codex support is intentionally narrower in
`0.3.0`.

## What Works

- Discovering Codex sessions in VS Code.
- Reading session status when local session data is available.
- Sending completion or error notifications.
- Focusing the matching VS Code window.
- GUI fallback actions where the platform control backend is available (macOS Accessibility, Linux X11 with xdotool, or Windows).

## What Does Not Work Yet

- Background prompt injection.
- Background question answers.
- Permission approval or denial.
- Model or mode controls.
- Full command lifecycle tracking.

## Why It Is Limited

The current background control path is built around the Claude Code helper shim. Codex needs
a separate control path before it can be treated the same way.
