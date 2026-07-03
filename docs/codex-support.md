# Codex Support

Claude Code is the primary supported target. Codex support is intentionally narrower in
`0.1.4`.

## What Works

- Discovering Codex sessions in VS Code.
- Reading session status when local session data is available.
- Sending completion or error notifications.
- Focusing the matching VS Code window.
- GUI fallback actions when the user grants Accessibility permission.

## What Does Not Work Yet

- Background prompt injection.
- Background question answers.
- Permission approval or denial.
- Model or mode controls.
- Full command lifecycle tracking.

## Why It Is Limited

The current background control path is built around the Claude Code helper shim. Codex needs
a separate control path before it can be treated the same way.
