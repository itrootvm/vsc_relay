# Architecture

VSC Relay is a local bridge between VS Code agent sessions and a Telegram bot.

```text
Claude Code or Codex in VS Code
        |
        | local transcripts, hooks, optional shim
        v
vsc-relay-agent (macOS, Linux, Windows)
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

`relay-control` handles window focus and GUI fallback actions using macOS Accessibility,
Linux xdotool over X11, and the Windows Win32 API.

`relay-core` contains shared state, ids, and helper types.

`relay-ipc` provides the local IPC channel: Unix domain sockets on macOS and Linux, and
named pipes on Windows.

`relay-shim` wraps the Claude Code helper binary so the relay can send prompts and answer
questions through a local IPC channel while forwarding normal helper input and output.

`VSCRelay.app` is the SwiftUI setup app for token storage, pairing key setup, live logs,
service control, and shim install or removal.

`vsc-relay-gui` is the egui desktop app that provides the same setup and service control on
Linux and Windows.

## Data Flow

The relay reads local files written by VS Code agent extensions. It does not require screen
scraping to read status. It uses the platform control backend (macOS Accessibility, Linux
X11, or Windows Win32) only when focusing windows or using GUI fallback.

Telegram traffic is outbound HTTPS long polling from the agent to Telegram Bot API. The
relay does not open an inbound network port.

## Decision Log

Every decision the relay makes is written as one JSON line to `~/.vsc-relay/decisions.jsonl`:
approvals, asks, gate verdicts, danger matches together with the pattern that fired, cross
review verdicts, and Telegram delivery outcomes. The daemon rotates the file by size and keeps
about two weeks. `vsc-relay-agent decisions --since 24h --by <field>` counts it by any field,
so a question such as why so many approvals arrived today is one command instead of a read
through the text log.

## Cross Review

Cross review lives in `supervisor/review.rs`. For an idle chat it reads the opening goal from
the head of the transcript and a bounded tail of recent messages, picks the latest real
instruction from the human, and asks a reviewer from another model family through the same
provider pool Robot uses. The answer is a small JSON verdict: on track, drifting, off track or
blocked, the task as the reviewer understood it, the gaps it sees, and an optional correction.
Verdicts are always recorded. A correction is sent into the chat only when a person runs the
review, or in Robot with review steering on, and in both cases only after the escalation guard.
Claude chats receive it through the tap, Codex chats through the control backend. Cadence is
kept in `~/.vsc-relay/robot-review-state.json` with session ids hashed, so a restart does not
review every chat at once.

## Session Handoff

A session can be handed to another chat, in another workspace or another editor, from the
chat card in Telegram. The relay does not copy the transcript. It builds a brief from the
same deterministic reading the Compass already does, so the brief is produced without calling
a model:

- the contract, quoted from the authoritative anchor turn, never paraphrased
- obligations split into proven and open, each with its state, the proof layer it needs, the
  layer actually observed, and how many evidence links it has
- the files the source session modified
- the last commands, with the ones that failed marked
- the recent conversation, clipped per turn
- questions the source session asked and never got answered

On top of that deterministic record the brief carries a narrative compact written by the
sending agent itself. The relay asks the headless CLI of the same family that ran the session
(`claude-cli` for a Claude session, `codex-cli` for Codex) to compact its own recent work into
four fixed headings: what the session was doing, how it was approached, traps and dead ends,
and where it stands. The compact is asked for under the quoted contract and is told not to
invent files, commands, or results. If that CLI is not available the relay falls back to the
configured provider list, and if no provider answers the brief still ships with the
deterministic sections alone. In the file the compact is placed after the contract and marked
as context rather than requirement: where the two disagree, the contract wins.

A session from Cursor or Antigravity can be the source as well, not only Claude Code and Codex.
Each family stores its work differently and the relay reads each where it lies. Cursor keeps the
whole conversation in `~/.cursor/chats/<workspace>/<session>/store.db`, a content-addressed blob
store whose root record lists the message ids in order; the sibling `meta.json` carries the
working directory. Only that store holds tool results, so it is the source of truth rather than
the transcript Cursor also writes, which records calls without their outcomes. Antigravity keeps
a plain record in `~/.gemini/antigravity-cli/brain/<session>/.system_generated/logs/`, one JSON
object per step, from which the relay takes the user request, the agent's own prose, each tool
call and each result. Both are opened read only, so a live session is never disturbed.

The failure signal each format offers is different and the brief reflects that honestly. Cursor
records an exit code in the text of every shell result, Antigravity records it as a field on the
step and repeats it in prose. Where a code is present it decides, and only where none exists does
the reader fall back to reading the output for signs of failure. Antigravity does not record a
working directory of its own, so the relay derives it from the paths the session actually worked
on, which is a derivation and not a fact the session stated.

The compact for a foreign session is asked of that family's own CLI first, the same rule as for
Claude and Codex. When that CLI does not answer in time the relay falls back to the configured
provider list rather than shipping a brief with no narrative at all.

A handoff moves a session to another **agent**, not to another folder. The work stays where it
is. The destinations offered are the other Claude chats the relay can see and the editors
installed on the machine (Antigravity, Cursor, Windsurf, VSCodium, VS Code), and the brief is
written next to the work the session was already doing.

The strongest destination is the receiving agent's own CLI. Claude Code, Codex, Antigravity
(`agy`) and Cursor (`cursor-agent`) all accept an initial prompt on the command line, so the
relay starts the chosen one in a terminal already sitting in the project and already asked to
read the brief. Nothing is pasted and no editor UI has to be driven, which is what makes a
handoff to an assistant the relay cannot control work properly rather than by hand.

Delivery therefore follows the destination. A tapped Claude chat is handed the prompt directly
through the shim. A CLI agent is launched with the prompt as its first message. A desktop
editor is opened on that same project and you paste the prompt into its own chat.

Before the launch line is reported, the relay asks the chosen CLI whether it is signed in, so a
handoff that will stall on a login screen says so in the same message that started it. Codex
answers with `codex login status`, Cursor with `cursor-agent status`, Antigravity with
`agy models`, which only lists anything when credentials are present. Claude Code has no
headless equivalent, so nothing is claimed about it.

The CLI launch is macOS only for now: it writes the prompt to a file and opens Terminal on a
generated launcher script, so no shell quoting can corrupt a multi-line prompt. The script
closes its own window when the agent exits cleanly and keeps it open with the exit status when
it does not, so a working handoff leaves nothing behind and a failed one stays readable.

The brief is written as `HANDOFF.md` at the root of the receiving workspace; any earlier brief
there is rotated to `HANDOFF.prev.md`. Both are gitignored, since a brief is a local working
artifact.

The same flow is available in the macOS app, under Hand off in the session detail pane, and
from the terminal, which is also how it is tested. Each command takes `--json` for callers:

```bash
vsc-relay-agent handoff destinations SESSION_ID
vsc-relay-agent handoff SESSION_ID --to app:Antigravity
vsc-relay-agent handoff SESSION_ID --to chat:OTHER_SESSION_ID
vsc-relay-agent handoff receipt /path/to/workspace
```

`handoff SESSION_ID /path/to/workspace` still writes a brief into an arbitrary folder, for the
case where the work really is moving to a different project.

The Linux `relay-gui` does not surface handoff yet; there it is Telegram or the command line.

The receiving agent closes the loop. Before it changes any code it is asked to append a
receipt between fixed markers in the same file: the contract restated in its own words, what
it found already true in that repository, and the first step it will take. A Check receipt
button reads that block back and reports which contract anchors, meaning the paths, flags,
numbers and quoted strings the contract actually named, the receipt never mentions. Anchors
are matched as substrings so ordinary punctuation does not produce a false alarm, and a
contract that names no anchors reports nothing rather than a vacuous pass. A handover that
drifted from the requirement is therefore visible before the new session starts working.

One thing the brief deliberately does not do is invent a new assignment. It carries the
contract the source session was working under, so when the receiving agent is meant to do
something else, for example write the frontend after the backend session ends, that new
instruction comes from you on top of the prompt. The drift check then compares the receipt
against the brief's contract, not against the new instruction, and it will name the anchors of
the old contract the receipt left out. That is a report about the handover, not an error in the
receiving agent.

Because a long transcript can take seconds to read, the brief is built off the Telegram loop.

## Background Control

Claude Code background control depends on the optional shim. Without the shim, the relay can
still observe session status and use GUI fallback actions. Codex background control is not
implemented in `0.4.0`.
