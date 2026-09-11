# Changelog

## 0.5.0 - 2026-09-11

- Cross review. A model from a different family reads a chat and says whether the work still
  serves the latest instruction the human gave: on track, drifting, off track or blocked, what it
  takes the task to be, what is missing, and an optional correction. Claude chats are never
  graded by Claude and Codex chats never by Codex. Every chat card in Telegram, Claude and Codex
  alike, has a Cross review button that asks which reviewer to use and how deep to read, then
  posts the verdict and sends any correction straight into the chat. The macOS app has the same
  menu on each session, and `vsc-relay-agent automation review run <id>` does it from a
  terminal. It can also run on a schedule with `automation review on`; the reviewer order, the
  gap between reviews, the depth and a per-chat budget are set from `/auto`, the app or the CLI.
  In Auto a scheduled review is only recorded and posted. Only Robot with `review steer on` acts
  on it, and every correction passes the same escalation guard as auto-steer.
- A reviewer grades the latest real instruction, not a short follow-up. The first version took
  the last human message as the task, so a chat whose last line asked when the work would be
  ready came back as on track with nothing to say. Follow-ups are still shown to the reviewer as
  context.
- Telegram messages are no longer lost on a flaky route. A send made one attempt, and any
  connection failure dropped the message with a single log line; on a machine where the direct
  route to Telegram was being cut, 8 of 56 messages over six hours never arrived. A send that
  fails before reaching Telegram is now tried up to three times, and since two failures switch
  the route, the last try goes the other way. A failure after the request left is not repeated,
  so nothing is delivered twice. `decisions --by outcome` shows `send_recovered` and
  `send_lost`.
- The periodic check for whether the direct route is back no longer sends a message twice. It
  used the real request as the probe and, when that went through, sent it again over the chosen
  route. The logs held eleven of these in four days. The probe is now a `getMe` call.
- Every decision the relay makes is written to `~/.vsc-relay/decisions.jsonl`, one JSON object
  per line, and `vsc-relay-agent decisions --since 24h --by outcome` counts it by any field.
  That includes branches which used to leave no trace, such as the hook returning ask, a
  repeated tool call answered from the dedup cache, and the answer to a Telegram card.
- Far fewer approval cards. The built-in danger list matched `format ` as a plain substring and
  caught `\pset format unaligned` in psql scripts, 71 of 77 flags on a sample of real commands.
  The pattern is gone; disk formatting is still covered by `format c:`, `diskpart` and `mkfs`. A
  card now names the pattern that fired, and `vsc-relay-agent danger-check` reads commands on
  stdin and says which pattern would match.
- Only one relay can run on a machine. The single-instance lock on macOS and Linux always said
  go ahead, so a second daemon started freely and fought the first for the bot. It is now a
  real exclusive lock, and a second copy exits and names the process that holds it.
- The daemon writes and rotates its own log. Rotation lived in the macOS app, so a relay started
  by launchd had none, grew its log until the disk filled, and then died on the next write.
  Losing the log file no longer stops the process.
- The gate can read compound shell commands. It looked at the first word only, and most commands
  start with `cd`, so three quarters of Bash calls were unreadable. Chains, pipes and redirects
  are now read segment by segment and about nine percent stay unreadable, mostly heredocs. The
  new reading is recorded but does not change decisions until `VSC_RELAY_SEGMENTED_GATE=1` is
  set.
- Gate bookkeeping explains itself. A refused typed receipt says which check refused it, a proof
  that arrives for no open gate is logged instead of dropped, and a gate that waits thirty
  minutes without proof expires instead of holding an action forever.
- Large transcripts no longer pin a CPU core. The compass read every transcript whole on every
  pass, which on 200 MB files kept the daemon near 90 percent of a core; it now reads the start
  of the file and a bounded tail, and the daemon sits at about half that. The completion ladder,
  which Auto now runs in record-only mode, reads the same window off the async runtime and takes
  the goal from the real start of the session instead of from the middle of it.
- `build_app.sh` could package an app with nothing in `Contents/MacOS`. The agent binary inside
  the bundle runs constantly as a hook, so changing its mode raced with it and the script
  stopped halfway while still leaving a bundle behind. Binaries are now staged and moved into
  place, and the build fails when the app or either binary is missing.
- The Linux and Windows desktop app pulls `webbrowser` 1.2.4, which fixes RUSTSEC-2026-0257, a
  way to inject browser arguments through the `BROWSER` variable on Unix. The macOS app never
  used that crate.
- Test fixtures no longer carry real host names, user names, addresses or project names.
- The static Linux build leaves out the local ONNX semantic model, because ONNX Runtime ships
  no build for musl. The Ollama, OpenAI compatible and agent CLI backends work there, and
  choosing the local model on Linux says so instead of failing obscurely. macOS and Windows keep
  it.
- Handoff now works in the reverse direction: a Cursor or an Antigravity session can be the
  source, not only Claude Code and Codex. Two adapters read those sessions where each family
  keeps them, a content-addressed blob store for Cursor and a step log for Antigravity, and
  produce the same steps the contract ledger already consumes, including tool kinds, call and
  result correlation, and failure marked from the recorded exit code rather than guessed from
  the output. A family layer replaces the string comparisons that used to decide between Claude
  and Codex, so every remaining call site is an exhaustive match and a fifth family would be a
  compile error rather than a silent fall back to Claude.
- A narrative compact falls back to the configured providers when a session's own CLI does not
  answer. Antigravity in particular rarely answers inside the ninety second cap, and the brief
  used to ship with no compact at all in that case.
- A question no longer arrives as two cards. Claude Code fires its Notification hook a few
  seconds after the interactive card is already sent, and the generic card that produced
  carried Approve and Deny buttons that press Enter in the window through Accessibility. That
  cannot answer an AskUserQuestion, because the answer lives inside a webview the platform
  will not let us reach, so the second card looked broken when tapped. While an interactive
  question or permission card is live for a workspace, the notification card for it is
  suppressed and the reason is logged, the same way the disk card already yields to the
  out-socket.
- Opening the macOS app no longer starts a second relay. It used to launch its own copy without
  looking, so with the launchd job installed two of them polled the same bot and Telegram
  answered `Conflict: terminated by other getUpdates request`, which shows up as cards arriving
  late, twice, or not at all. The app now takes over a relay that is already running, whoever
  started it, and only starts one when there is none.
- The relay can now be kept alive by launchd. `./launchd.sh install` registers a job with
  KeepAlive and RunAtLoad, so a relay that dies comes back within about ten seconds and one that
  is not running at boot starts by itself. Until now nothing supervised it: when the process
  went away the bot simply stopped answering, with no log line to say why, and it stayed down
  until someone noticed. Verified by killing the daemon with SIGKILL and watching it return.
  The job needs no secrets of its own because the daemon reads `~/.config/vsc-relay/relay.env`.
- Quitting the macOS app no longer stops a relay the app did not start. Adoption made the app
  able to stop an outside daemon, and its quit path used the same stop, so closing a viewer
  could take the service down with it. Only the Stop button reaches an adopted daemon now.
- The macOS app finds an already running relay on every refresh, not only when its window first
  appears. A service restarted while the window stayed open used to leave the header reading
  Stopped over a working relay, with the line underneath still saying Running from an earlier
  start. That line is now cleared when nothing is found, so the two cannot disagree.
- The Telegram connection now survives losing either route on its own. A configured proxy is no
  longer the only way out; it becomes the fallback behind the direct route. Two consecutive
  transport failures switch to the other route and say so in the log, a success clears the
  count, and while on the fallback the direct route is retried every fifteen minutes and taken
  back as soon as it answers. Both clients use an eight second connect timeout, so a blocked
  route fails fast instead of sitting out the long poll. Verified by stopping the proxy on a
  live relay: it moved to the direct route eleven seconds later and kept polling without error.
- A new chat now announces itself in Telegram, silently and once, with a button into its
  workspace. Session-started events were dropped before reaching the bot, so a chat opened after
  the last card was sent existed in the relay but appeared nowhere the user could see, and the
  window list they already had could never grow. The initial scan still stays quiet, so
  restarting the editor or the relay does not produce a burst.
- A chat with no shim is no longer refused a message. When the background channel is missing the
  text is typed into the window through the same path the GUI fallback already used, and the
  reply says which way it went. The old code held a working control handle and discarded it.
- The Bot API endpoint is configurable through `VSC_RELAY_TELEGRAM_API`. Calls and file
  downloads both follow it, so a reverse proxy you control or a self-hosted `telegram-bot-api`
  server can stand in for api.telegram.org when the direct route is blocked. The startup line
  reports the endpoint in use.
- The Telegram connection can go through a proxy. `VSC_RELAY_PROXY` takes precedence, then
  `ALL_PROXY` and `HTTPS_PROXY`, and SOCKS5 is now supported as well as HTTP and HTTPS. The
  startup line reports the proxy in use or `direct`. Until now a blocked route to
  api.telegram.org left the relay running and answering nothing, with no way to route around
  it, since the client was built without SOCKS support and no setting existed.
- Every private file the relay writes now uses a temporary name unique per write, not one
  shared by the whole process. Two writers of the same path inside one process used to share
  `<name>.tmp.<pid>`, so one could rename the other's half-written bytes into place and publish
  a file that fails its own checksum. A new test drives eight threads at one path and fails on
  the old naming with a zero-byte file, which is how the hazard was confirmed. This was found
  while chasing an intermittent "dossier write failed" from `automation compass`; that failure
  itself did not reproduce and stays unexplained, so the change is hardening, not a diagnosis.
- A terminal opened for a CLI handoff now closes itself when the agent leaves cleanly, and
  stays open with the exit status printed when it does not. Every handoff used to leak a
  window that lived until it was closed by hand, which after a session of testing left dozens
  of them open. The launcher and prompt files under `~/.vsc-relay/handoff` are also pruned to
  the last twenty handoffs instead of growing without limit.
- Fixed the Antigravity handoff, which started the CLI but never delivered the brief. `agy`
  takes the initial prompt as the value of its `-i` flag, so appending the approval flag after
  it made `--dangerously-skip-permissions` the prompt and dropped the brief entirely: the
  session opened and answered a question about a command line flag. Flags a CLI needs are now
  placed before the flag that introduces the prompt, and a test fails if any destination would
  receive its prompt away from that flag.
- The Antigravity CLI destination now reports whether it is signed in, like Codex and Cursor
  already did. Its check is `agy models`, which only answers when the CLI has credentials, and
  a success is reported as "signed in" rather than by echoing the first model name. A test now
  fails if any CLI destination other than Claude Code has no way to report its auth state.
- Documented two things a full two-agent handoff run made visible: a session started from a
  terminal is seen by the relay through its hooks but cannot be commanded from Telegram
  because the shim is loaded by the editor, and the brief carries the source contract rather
  than a new assignment, so the drift check reports the old contract's anchors when the
  receiving agent was pointed at different work.

- Added session handoff. The chat card has a Hand off button that picks another live chat and
  writes a HANDOFF.md brief into that workspace, rotating any earlier brief to
  HANDOFF.prev.md. The brief is built deterministically from the source transcript, not copied
  from it: the contract quoted from the authoritative anchor turn, obligations split into
  proven and open with their proof layers and evidence counts, the files that were modified,
  the last commands with failures marked, the recent conversation, and questions that were
  never answered. Telegram replies with the path and an English prompt that tells the
  receiving agent to read the file, treat the contract as authoritative, and start from the
  open items, so the flow also works with agents the relay cannot drive. When the target chat
  is tapped, one button sends that prompt into it directly. The brief is built off the
  Telegram loop, so a large transcript cannot stall the bot.
- The handoff brief now carries a narrative compact written by the sending agent itself. The
  headless CLI of the same family that ran the session compacts its own recent work into four
  fixed headings (what it was doing, how it was approached, traps and dead ends, where it
  stands), asked for under the quoted contract and told not to invent files, commands, or
  results. If that CLI is unavailable the configured provider list is used, and if no provider
  answers the brief still ships with its deterministic sections. The compact sits after the
  contract and is marked as context, not requirement.
- A handoff moves a session to another agent, not to another folder. The destination list is
  the other Claude chats plus the editors installed on this machine, and the brief is written
  next to the work the session was already doing rather than into an unrelated project. A
  tapped chat receives the prompt directly; an editor is opened on that same project and you
  paste the prompt into its chat, which is what makes Antigravity and Cursor reachable. The
  first version offered a list of workspaces instead, which read as moving the work to a
  different project and did not express what a handoff is for.
- The handoff brief now carries the plan the source session was last working from, quoted under
  its own heading ahead of the compact, so a fresh plan moves with the session instead of being
  lost. Claude records a plan as an attachment in the transcript; the newest one is taken.
- Fixed foreign markdown breaking the brief. Conversation turns were printed raw, so a heading
  inside a quoted turn became a heading of the brief itself and the document structure came out
  scrambled. Turns and the plan are now blockquoted and obligations and questions are collapsed
  to a single line, so only the brief's own headings can appear at the top level.
- A handoff can now land directly in the receiving agent's own CLI. Claude Code, Codex,
  Antigravity (`agy`) and Cursor (`cursor-agent`) all take an initial prompt as an argument, so
  the relay starts the chosen one in a terminal already in the project and already asked to read
  the brief. Nothing is pasted and no editor UI is driven, which is what makes handing a session
  to an assistant the relay cannot control work properly. The prompt is passed through a file and
  a generated launcher script so no shell quoting can corrupt it. Only the CLIs actually
  installed are offered; the launch itself is macOS only for now.
- Workspace trust and tool approval are no longer conflated when starting a CLI agent. Trusting
  the directory is implied by choosing it as the destination, so a trust flag is always passed
  where the CLI has one, while the approval bypass is passed only when the source session is in
  Auto or Robot. Before this, a Manual handoff stalled on a trust dialog and an Auto one granted
  both at once.
- A session created by an agent CLI, with no editor window open on its folder, can now be handed
  off. The source workspace falls back to the working directory recorded in the transcript.
- The receiving agent now signs for the handoff. Before it changes any code it is asked to
  append a receipt to the same file between fixed markers: the contract in its own words, what
  it found already true in that repository, and its first step. A Check receipt button reads
  that block back and names the contract anchors, the paths, flags, numbers and quoted strings
  the contract itself used, that the receipt never mentions. Anchors are matched as substrings
  so punctuation does not raise a false alarm, and a contract that names no anchors reports
  nothing rather than a vacuous pass.
- The macOS app can hand a session off too. The session detail pane has a Hand off section that
  loads the available agents as soon as a session is selected, groups them into Claude chats and
  editors and agent CLIs, writes the brief, and then shows the path, the proven and open counts, who wrote the
  compact and how it was delivered, with buttons to copy the prompt for the receiving editor and
  to read the receipt back with its missing-anchor report. The picker and the Hand off button are
  always visible, and not loaded, loading, and nothing available read differently instead of
  sharing one ambiguous line. The Linux relay-gui does not have this yet.
- Added a handoff command line for the same flow, which is also how it is tested:
  `handoff destinations SESSION_ID` lists the agents that can take a session over,
  `handoff SESSION_ID --to DESTINATION` writes the brief and delivers it, `handoff receipt
  WORKSPACE` reads the receipt back and reports missing anchors, and the lower level
  `handoff SESSION_ID WORKSPACE` still writes a brief into an arbitrary folder. Each takes
  `--json`.
- Antigravity is now discovered alongside Code, Insiders, VSCodium, Cursor, Code - OSS, and
  Windsurf. It stores workspace state in the same layout, so its windows appear in the window
  list, and it is offered as a handoff destination when it is installed.
- Handoff briefs are gitignored, since HANDOFF.md is a local working artifact.

- Bounded the transcript reader that the daemon runs on every scan. The reduction cache kept
  one reducer per transcript for the life of the process and never evicted a closed session,
  a partially received line was rescanned from its start on every 1 MB chunk, the whole
  partial line was JSON-parsed on every chunk, and the line buffer kept the capacity of the
  largest line it had ever seen. A real 1.6 GB transcript with 4.5 MB lines therefore cost
  several full parses per line and pinned megabytes per cached session. The cache now holds at
  most 64 reducers and evicts the least recently used, only newly appended bytes are scanned
  for line breaks, the trailing partial line is validated once at end of file instead of once
  per chunk, and an oversized buffer is released after use. Reductions are unchanged and are
  still checked against a full reduction of the same transcript.
- The Compass feedback protocol is no longer re-sent on every SessionStart. It is delivered
  when the session context is built or rebuilt (startup, clear, compact) and skipped on
  resume, where the transcript already carries it. One long session had accumulated 288 copies
  of the protocol block, which wastes context and reads as noise to the agent.
- Added a Chat modes screen to the automation menu. It lists every live chat with its resolved
  mode, marks whether the mode is pinned or inherited, and opens the per-chat mode picker in
  one tap, so a running session can be switched between Manual, Auto, and Robot without
  walking the window and chat menus first.

- Reverted a contract-ledger meaning-authority overreach. A later document-shaped turn (for example
  a UI review pasted into the session) was being materialized into the contract as provisional
  obligations, and runtime evidence could then promote one to authoritative. That let runtime proof
  author or elevate an unconfirmed human requirement, and it leaked a late review into the contract
  of a real two-block incident fixture. A late document now stays a recorded candidate and never
  becomes an obligation on its own; runtime proof verifies existing obligations but never mints or
  elevates one; the only path from a candidate to a contract obligation is explicit human promotion.
  Removed the provisional/excluded admission states, the candidate materialization, and the
  proof-gated promotion.
- Fixed a GUI crash (SIGTRAP in Dictionary) when two session cards shared a session id, and closed
  it at both layers. The session list can transiently carry the same session under two windows
  during a rescan; the agent now emits a set that is unique by session id (keeping the live/most
  recently active card on a collision), so the documented consumer contract holds at the boundary,
  and the GUI's prompt-history reconcile keeps the latest state on a collision instead of trapping.
- Fixed the GUI leaving Telegram off after launch when the Keychain held the bot token but not
  the pairing key. The controller only restarted the daemon with credentials once both were
  present, but Telegram delivery needs just the token (the pairing key is only used for /auth), so
  the daemon stayed on its tokenless first start. It now brings Telegram online as soon as the
  token loads from the Keychain.
- Added opt-in question auto-answer for Auto-mode sessions. When auto.auto_answer_questions is on
  and a session is in Auto, an AskUserQuestion with a single, single-select question is answered by
  an available CLI provider that reads the question, the option descriptions, and recent session
  context, and either picks an option or gives a short free-text reply. It only commits when the
  provider's confidence clears auto.auto_answer_min_confidence; any miss (no provider, low
  confidence, multiple or multi-select questions, no session) falls back to forwarding the question
  to Telegram as before. Auto-answers are announced in Telegram. Off by default; toggle in the
  automation menu.
- Added an opt-in dangerous-command guard for Auto-mode sessions. When auto.guard_dangerous is on
  and a session is in Auto, a destructive command is blocked and the agent is nudged to reach the
  goal a safer way instead of being forwarded for approval, with a Telegram notice. Manual still
  asks, Robot already replans. Off by default; toggle in the automation menu.
- Compass advisory notifications (compass_staged, compass_risk) now respect the per-session
  relay mode: a session pinned to Manual is left alone and emits no compass notifications, the
  same way Manual already suppresses the gate hold and the Robot auto-steer. Auto and Robot
  sessions still get advisories. Previously these notifications fired whenever Compass was
  enabled regardless of mode, so switching a session to Manual did not silence them.
- Fixed a flood of repeated compass advisory notifications. The compass_staged assessment was
  pushed on every terminal observation whenever its action was not Observe, so a stuck or busy
  session re-sent the identical assessment each turn. Both compass_staged and compass_risk are
  now deduplicated per session by content, so an unchanged assessment is sent once and repeats
  are dropped; a genuinely changed assessment still notifies.
- Fixed permission prompts going silent in Telegram for Auto and Robot sessions when the
  deterministic contract gate was enabled (smart.gate). When the gate held a novelty mutation
  for proof and then exhausted its bounded proof window, it returned an ask_user decision and
  marked the tool use held. The held mark suppressed the outgoing permission card, so the
  request surfaced only as a local VS Code prompt and never reached Telegram. The gate now
  forwards a permission card and waits for the Telegram decision when it hands a mutation back
  to the user, carrying the gate reason on the card. The in band deny and probe loop stays
  quiet, since it resolves itself within the window; only the give up escalation is forwarded.
  The danger forward path and the gate escalation now share one forward and wait helper.

## 0.4.0 - 2026-07-07

- Fixed a double render of AskUserQuestion on tapped sessions. The transcript tail and the
  shim out socket both used to post a card for the same prompt; the two are now matched by
  tool use id, so a tapped session shows exactly one interactive card. An untapped session
  still gets its fallback card, so no prompt is dropped.
- Fixed a stall where a slow Telegram send could freeze Claude Code output inside VS Code.
  The shim now writes the editor stdout first and fans out to background readers over bounded
  per reader channels, so a slow or stuck reader can no longer block the output pump. The
  daemon reads its socket on a task separate from the Telegram send for the same reason.
- Added a replay ring in the shim. A background reader that reconnects, for example after the
  daemon restarts, is re-sent any still pending permission or question request, and answered
  requests are evicted, so a reconnect does not lose or duplicate a card. A reader disconnect
  while the session process is still alive is treated as a reconnect window rather than a
  closed session, so a pending card stays answerable.
- Added permission mode change alerts. A switch into acceptEdits or bypassPermissions raises
  an alert in Telegram; other transitions are shown without one.
- Added per turn token accounting for both Claude Code and Codex (input, output, cache read,
  and cache creation), recorded per session.
- Telegram answers and permission decisions are no longer injected into a session that has
  closed or whose pid has been reused. The pending card carries its session identity and the
  inject is skipped on a mismatch.
- Untapped sessions now show a card that points to VS Code instead of option buttons that
  could not answer the prompt.
- Auto reshim repairs a freshly updated extension version without a restart. Install status is
  tracked per extension directory, so a new version is tapped even when an older one is already
  shimmed.
- Added detailed lifecycle logging across both pipelines (arrived, sent, pressed, answered in
  VS Code, resolved, suppressed, mode changed) with per card latency and request and response
  byte sizes. Logs redact the bot token, tool input, option labels, and raw commands.
- Split the callback and full text stores so a burst of full text previews can no longer evict
  an active permission or send callback.

## 0.3.0 - 2026-07-05

- Windows 10/11 support alongside macOS and Linux. The daemon, shim, discovery, GUI, and
  Telegram control all run on Windows (x64). A new `relay-ipc` crate carries the IPC transport
  as Windows named pipes (per-user DACL, reject-remote-clients, first-instance guard) on
  Windows and Unix domain sockets on macOS/Linux, so the background shim path (send, answer
  questions, permissions, model/effort/mode) stays display-independent.
- New `relay-control` Windows backend for window focus and GUI fallback via Win32
  (`EnumWindows` / `SetForegroundWindow` + `AttachThreadInput` / `SendInput` / clipboard /
  `ShellExecuteW`); needs an interactive desktop session.
- Windows shim wraps `claude.exe` (moved aside to `claude.real.exe`), spawns the real helper
  via CreateProcess with a spawn-and-wait fail-safe (no `exec`), and confines it to a Job
  Object so a killed shim cannot orphan the child.
- The agent loads `relay.env` from the platform config dir (`%APPDATA%\vsc-relay` on Windows,
  `~/.config/vsc-relay` on Unix) via dotenvy, and prevents a duplicate Telegram poller with a
  single-instance named mutex on Windows.
- Windows packaging: `build_windows.ps1` produces `dist/vsc-relay-<ver>-windows-x86_64.zip`
  with per-user (no-admin) `install.ps1` / `uninstall.ps1` that wire the Claude Code hooks,
  install the shim, and register a logon Scheduled Task in the interactive session. Windows
  self-update swaps the running `.exe` aside to `.old`. `release.yml` builds and publishes it.
- CI now runs fmt/clippy/test on macOS, Linux, and Windows.

## 0.2.0

- Linux support alongside macOS. The daemon, shim, discovery, and Telegram control all run
  on Linux; the shim background path (send, answer questions, permissions, model/effort/mode)
  is display-independent.
- New Linux desktop app `vsc-relay-gui` (egui, runs on any desktop environment): token /
  pairing-key settings, start/stop, live log, sessions/turns/shim/version dashboard, shim
  install, and launch-at-login via an XDG autostart entry. Config lives in the same
  `~/.config/vsc-relay/relay.env` the systemd service reads.
- New `relay-control` Linux backend for window focus and GUI fallback via `xdotool` /
  `xclip` / `xdg-open` on X11 (XWayland tolerated; native Wayland limited to the shim path),
  with VS Code / Insiders / VSCodium / Cursor / Code-OSS / Windsurf window matching.
- Shim discovery now scans multiple editor extension roots (`.vscode`, `.vscode-insiders`,
  `.vscode-oss`, `.vscodium`, `.cursor`, `.windsurf`).
- Linux packaging: `build_linux.sh` produces a static (musl) `dist/*.tar.gz` **and a `.deb`**
  (Debian/Ubuntu, `sudo apt install ./vsc-relay_*.deb`) with a systemd user service, desktop
  launcher, and `install.sh` / `uninstall.sh`; `release.sh` is now OS-aware.
- Linux self-update: `vsc-relay-agent self-update [--check]` downloads the latest release,
  verifies its sha256, atomically swaps the binaries, and re-wraps the shim. The GUI surfaces
  it as an update banner plus an **Auto-update** toggle in Settings.
- Multiple machines: run one bot per machine for now (each token is separate, so no 409); a
  single-bot multi-machine hub is on the roadmap.
- CI builds and publishes the macOS dmg, the Linux tarball, and the `.deb` from a single
  tagged release, after a shared fmt/clippy/test/audit gate.

## 0.1.5

- Bidirectional permission forwarding: permission prompts for every tool (Workflow, Artifact,
  Skill, Bash, AskUserQuestion, and the rest) now reach Telegram, and answering on one side
  clears the other. A Telegram answer dismisses the VS Code menu via a control_cancel_request;
  answering in VS Code voids the Telegram card.
- Redacted logging on the permission and hook paths that records only the tool and a byte
  count, never command arguments, file contents, or URLs beyond scheme and host.

## 0.1.4

- Clarified the public README with purpose, support matrix, install flow, Telegram commands,
  security model, and roadmap.
- Added security, contribution, architecture, installation, troubleshooting, FAQ, Codex, and
  threat-model documentation.
- Added CI for formatting, clippy, tests, and dependency audit.
- Added release provenance steps for DMG verification, checksums, SPDX SBOM, and GitHub
  artifact attestation.
- Synchronized release version signals across `VERSION`, Cargo workspace metadata, app
  bundle metadata, and release scripts.
