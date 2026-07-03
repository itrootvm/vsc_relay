# Architecture

VSC Relay is a local bridge between VS Code agent sessions and a Telegram bot.

```text
Claude Code or Codex in VS Code
        |
        | local transcripts, hooks, optional shim
        v
vsc-relay-agent on macOS
        |
        | Telegram Bot API over outbound HTTPS
        v
Telegram chat
```

## Components

`relay-agent` discovers VS Code windows and agent sessions, watches transcript files, runs
the Telegram bot, accepts local hook events, and executes control actions.

`relay-discovery` finds VS Code windows, workspaces, Claude Code sessions, Codex sessions,
and git metadata.

`relay-adapters` parses Claude Code and Codex session data into shared relay state.

`relay-control` handles macOS window focus and GUI fallback actions.

`relay-core` contains shared state, ids, and helper types.

`relay-shim` wraps the Claude Code helper binary so the relay can send prompts and answer
questions through a local socket while forwarding normal helper input and output.

`VSCRelay.app` is the SwiftUI setup app for token storage, pairing key setup, live logs,
service control, and shim install or removal.

## Data Flow

The relay reads local files written by VS Code agent extensions. It does not require screen
scraping to read status. It uses macOS Accessibility only when focusing windows or using GUI
fallback.

Telegram traffic is outbound HTTPS long polling from your Mac to Telegram Bot API. The
relay does not open an inbound network port.

## Background Control

Claude Code background control depends on the optional shim. Without the shim, the relay can
still observe session status and use GUI fallback actions. Codex background control is not
implemented in `0.1.4`.
