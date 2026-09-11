# Operating on macOS

This is the day to day guide: how to get the relay running, how to keep it running, and how
to find out what it did. For first time setup of the bot itself, see `installation.md`.

## Install

1. Open `VSCRelay.dmg` and drag `VSCRelay.app` to Applications.
2. Launch it. On the first launch, right click the app and choose Open, so macOS allows an
   app that did not come from the App Store.
3. Open Settings, paste the Telegram bot token from BotFather, set a pairing key, click Start.
4. In Telegram, send `/auth <your key>` to the bot, then `/menu`.

The token and pairing key are stored in Keychain. A rebuilt app is a different signature, so
macOS asks for Keychain access again after an update. Choose Always Allow, otherwise the
daemon starts without a token and Telegram stays silent.

## Keeping it running

There are two ways to supervise the daemon, and only one of them should own it at a time.

**The app alone.** Start and Stop in the window. Good enough for a laptop that gets opened
and closed. Nothing restarts the daemon if the machine reboots.

**launchd.** Survives logout and reboot, and restarts the daemon if it crashes. From a clone
of the repo:

```bash
./launchd.sh install
./launchd.sh status
./launchd.sh uninstall
```

With the job installed, the app no longer starts a copy of its own. It asks launchd to start
the daemon and attaches to it, which the header shows as "Running (pid N)". Stop in the app
takes the launchd job down, so it stays stopped until you press Start again.

Only one daemon may run per machine. A second copy exits immediately and writes the pid of
the one holding the lock into `agent.log`. This is deliberate: two daemons polling the same
bot fight over every Telegram update.

To confirm there is exactly one, compare the two:

```bash
./launchd.sh status
cat ~/.vsc-relay/vsc-relay-agent.lock
```

Both should name the same pid.

## What is on disk

Everything lives in `~/.vsc-relay`:

| File | What it is |
| --- | --- |
| `agent.log`, `agent.log.1` ... `.12` | The running narrative. The daemon writes and rotates it itself, so rotation works whoever started it. |
| `decisions.jsonl`, `.1` ... `.16` | One line per decision the relay made. About two weeks at normal volume. |
| `automation.json` | Modes, per chat pins, the Auto and Robot rules, and cross review settings. |
| `robot-review-state.json` | When each chat was last cross reviewed. Session ids are stored hashed. |
| `danger.txt` | Optional. If this file exists and has content, it **replaces** the built in danger patterns rather than adding to them. |
| `vsc-relay-agent.lock` | Holds the pid of the daemon that owns this machine. |

## Modes

Set the default with `/auto` in Telegram, or pin one chat from its menu.

**Manual.** Every permission request reaches you. Nothing happens without an answer.

**Auto.** Routine approvals go through without asking. Commands matching the danger list are
forwarded to Telegram and wait up to 110 seconds; with no answer the relay falls back to the
prompt in VS Code, so work is never silently approved.

**Robot.** Dangerous actions are denied outright instead of being forwarded, so the agent has
to replan without them. Robot never asks you a question mid turn.

## Finding out what happened

The decision log answers "what did the relay decide, and why", without reading prose:

```bash
vsc-relay-agent decisions --since 24h --by outcome
vsc-relay-agent decisions --since 7d  --by tool
vsc-relay-agent decisions --since 24h --by alias
vsc-relay-agent decisions --since 24h --by reason
vsc-relay-agent decisions --since 24h --by danger_pattern
```

`--since` takes a value like `30m`, `24h` or `7d`. `--by` takes any field present in the
records. The binary inside the app works too:

```bash
/Applications/VSCRelay.app/Contents/Resources/vsc-relay-agent decisions --since 24h --by outcome
```

Every five minutes the daemon writes one summary line per tool showing how much of the
traffic the gate could actually read:

```bash
grep 'stage="census"' ~/.vsc-relay/agent.log | tail -5
```

## Cross review

Cross review asks a model from a different family to read a chat and say whether the work still
serves your last instruction. Claude chats are never graded by Claude, and Codex chats are never
graded by Codex.

Turn it on and shape it from `/auto` in Telegram, from Settings in the app, or from a terminal:

```bash
vsc-relay-agent automation review on
vsc-relay-agent automation review reviewers codex-cli,antigravity,claude-cli
vsc-relay-agent automation review every 900
vsc-relay-agent automation review depth normal
vsc-relay-agent automation review budget 6
```

`reviewers` is the order the relay asks them in. `every` is the gap in seconds between scheduled
reviews of one chat, and `budget` caps how many one chat gets. `depth` is how much of the chat
the reviewer reads: `shallow` is the last 8 messages, `normal` 20 and `deep` 50, always together
with the latest real instruction you gave, even when that is older than the window.

In Auto a scheduled review is only recorded and posted to Telegram. A correction reaches the chat
on its own only in Robot, and only after `automation review steer on`. Every correction first
passes the same guard as auto-steer, which refuses anything asking for sudo, force pushes,
sandbox escapes or deleting things.

To review one chat right now, open its card in Telegram and press Cross review. Pick a reviewer,
or the default order, and optionally a depth. The verdict arrives as a separate message within
about a minute, and a correction, if there is one, goes straight into the chat. Pressing the
button is the consent, so this works in any mode, for Claude and Codex chats alike. The session
card in the app has the same menu, and so does the terminal:

```bash
vsc-relay-agent automation review run <session-or-thread-id> --reviewer antigravity --depth deep
```

The reviewer grades your latest instruction, not a short follow-up. A question such as when the
work will be ready is shown to it as context, and the task it grades is the request before it.

## Telegram delivery

Where Telegram is blocked or unreliable, give the relay a proxy. It is a local setting: put it in
`~/.config/vsc-relay/relay.env` on the machine that runs the relay, never in the repository.

```
VSC_RELAY_PROXY=socks5://user:password@proxy.example:1080
```

With a proxy configured the relay starts on it, checks every fifteen minutes whether the direct
route answers, and moves back when it does. A message that fails before reaching Telegram is
tried up to three times, one and then three seconds apart, and since two failures in a row
switch the route, the last try goes the other way. A message that failed after leaving the
machine is not repeated, because Telegram may already have delivered it.

To see how delivery went:

```bash
vsc-relay-agent decisions --since 24h --by outcome | grep send_
```

`send_recovered` is a message a retry saved. `send_lost` is one no route could deliver, which
almost always means the direct route and the proxy were down at the same time, for example a
proxy that is only reachable while a VPN is connected.

## Tuning the danger list

Danger matching is a plain substring test, which is easy to get wrong in both directions. Check
a candidate list against real commands before trusting it. `danger-check` reads one command per
line and names the pattern that fires:

```bash
vsc-relay-agent danger-check <<'EOF'
rm -rf build
psql -c '\pset format unaligned' -f query.sql
kubectl delete pod postgres-0
EOF
```

Judge patterns on real traffic, never on intuition. A rule that fires on routine work teaches
people to approve without reading, which is worse than having no rule.

## Terminal only

No app, no window:

```bash
cp .env.example .env
./svc.sh start
./svc.sh status
./svc.sh logs
```

Put `TELEGRAM_BOT_TOKEN` and `RELAY_PAIR_SECRET` in `.env` first. Do not run this alongside
the app or the launchd job; the lock will simply refuse the second one.

## Background control for Claude Code

The shim adds background prompts and question answering:

```bash
./shim.sh install
./shim.sh status
./shim.sh uninstall
```

Only chats started after the install use it.

## When something looks wrong

**Two daemons, or none.** Compare `./launchd.sh status` with the lock file. If the app and
launchd both try to own the daemon, uninstall the launchd job or stop the app.

**A flood of approvals.** Ask which rule is firing:

```bash
vsc-relay-agent decisions --since 24h --by danger_pattern
```

The Telegram card also names the matching pattern on the line beginning with a magnifier.

**Telegram has gone quiet.** Look for a route switch in the log:

```bash
grep 'stage="route"' ~/.vsc-relay/agent.log | tail
```

**The log is growing fast.** Check the census lines first. A large `unknown_shape` count for
`Bash` means the gate cannot read most commands and is recording that fact repeatedly.
