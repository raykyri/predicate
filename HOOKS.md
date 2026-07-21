# Hooks

Hooks tell qmux when an agent starts a session, when a prompt was
submitted, when tools run, when permission is needed, and when the
agent is idle enough for queued turns to advance.

Hooks are only installed for agents launched by qmux or by qmux's
shell wrapper functions inside a qmux shell pane, so a Claude or Codex
process started outside qmux's setup will not have these hooks.

Because status is entirely hook-driven, a CLI blocked on startup UI
that predates its session — a workspace-trust dialog, a login prompt,
an update gate — is nearly invisible to qmux: the agent sits
`Starting`, or `Running` with no session id bound (Claude fires
`UserPromptSubmit` for a launch-argument prompt even while the trust
dialog still blocks the session). For research runs, whose panes are
read-only outside `AwaitingPermission`/`AwaitingInput`, that would
lock the user out of the very prompt the run is stuck on. A startup
watchdog (`schedule_research_startup_watchdog` in
src-tauri/src/state.rs) covers this for every adapter: a research
agent that still has either signature 10 seconds after launch is
flagged `AwaitingInput`, which unlocks its pane and keeps the node
live; the first real hook moves the status on as usual.


## Agent Integrations

Different agents have different hook configuration formats and
payloads, so each integration is contained in an adapter:

For Claude, we write a per-pane, per-spawn settings file under
<qmux workspace root>/.qmux/hooks/<pane-id>-<nonce>.json (created 0600
in a 0700 dir with O_EXCL, and the pane's previous file pruned), then
start Claude with --settings <that file>. This applies to
launcher-created agents, resumes/forks, and claude run inside a qmux
shell wrapper. Using a fresh, unpredictable path per spawn — rather than
one shared, same-user-writable qmux-hooks.json — keeps a process in one
pane from tampering with the hook commands another pane's Claude loads.
For the exact hooks, see src-tauri/src/adapters/claude.rs:23.

For Codex, we write a qmux-managed profile under `CODEX_HOME`:

```
$CODEX_HOME/qmux/qmux-codex-hook
$CODEX_HOME/qmux-codex.config.toml
```

And then we start Codex with --profile qmux-codex, and the profile
points each Codex hook at the shim. For exact hooks,
see src-tauri/src/adapters/codex.rs:29.

For Grok (xAI Grok Build), whose hook system is Claude-compatible, we
write a shim and a qmux-owned global hook file under Grok's discovered
hooks directory:

```
$GROK_HOME/qmux/qmux-grok-hook    (default $GROK_HOME = ~/.grok)
$GROK_HOME/hooks/qmux.json        (qmux-owned; Grok merges hooks/*.json)
```

Grok discovers global hooks from `~/.grok/hooks/*.json` (not
`user-settings.json`). It has no per-launch settings flag, so the hooks
are installed globally and the shim no-ops unless the qmux env vars are
present, the same way the Codex shim does. Other files in
`~/.grok/hooks/` are left alone. For exact hooks,
see src-tauri/src/adapters/grok.rs:24.

All three hook systems call back into qmux via qmux notify <event>,
which sends a token-scoped hook.notify request back to the app.


## Claude

- `SessionStart`: records `session_id` and `transcript_path` from the
  hook payload when Claude provides them. If a transcript path is known,
  qmux starts tailing it for the agent timeline. This does not mark the
  agent as running; a prompt or tool hook does that.
- `UserPromptSubmit`: marks the agent `Running` and emits
  `agent.prompt_submitted`. For main-agent prompts, qmux matches the
  payload's `prompt` against outstanding send tracking. Subagent
  payloads still mark the agent running, but skip the send-tracking
  match.
- `PreToolUse`: marks the agent `Running` and emits `agent.tool_use`.
- `PostToolUse`: marks the agent `Running` and emits
  `agent.tool_result`.
- `PermissionRequest`: marks the agent `AwaitingPermission` and emits
  `agent.awaiting_permission`.
- `Notification.permission_prompt`: marks the agent
  `AwaitingPermission` and emits `agent.awaiting_permission`.
- `Notification.idle_prompt`: treats the agent as idle. qmux clears
  outstanding send tracking, respects pause and typing state, drains the
  next queued turn if allowed, and emits either `agent.running` or
  `agent.done`.
- `Notification.elicitation_dialog`: marks the agent `AwaitingInput`
  and emits `agent.awaiting_input`.
- Other `Notification` events: mark the agent `AwaitingInput` and emit
  `agent.notification`.
- `Stop`: uses the same idle handling as `Notification.idle_prompt`,
  including queue draining and the `agent.running` or `agent.done`
  result.
- `SubagentStop`: emits `agent.subagent_stopped` without changing the
  main agent status.
- Unknown Claude hook events: forwarded as `agent.hook.<event>` with
  the raw hook payload.


## Codex

- `SessionStart`: records the session id from `session_id`,
  `sessionId`, `resource_id`, or `resourceId` when Codex provides one,
  then starts transcript binding. If Codex provides `transcript_path` or
  `transcriptPath`, qmux polls that explicit `.jsonl` path until it is
  ready. Otherwise qmux searches `$CODEX_HOME/sessions` for a matching
  transcript. This does not mark the agent as running.
- `UserPromptSubmit`: marks the agent `Running` and emits
  `agent.prompt_submitted`. qmux reads `prompt` or `input` from the
  payload and matches it against outstanding send tracking.
- `PreToolUse`: marks the agent `Running` and emits `agent.tool_use`.
- `PostToolUse`: marks the agent `Running` and emits
  `agent.tool_result`.
- `PermissionRequest`: marks the agent `AwaitingPermission` and emits
  `agent.awaiting_permission`.
- `Stop`: treats the agent as idle. qmux clears outstanding send
  tracking, respects pause and typing state, drains the next queued turn
  if allowed, and emits either `agent.running` or `agent.done`.
- Unknown Codex hook events: forwarded as `agent.hook.<event>` with the
  raw hook payload.


## Grok

- `SessionStart`: records the session id from `session_id` or
  `sessionId`. Binds the transcript path Grok reports in `transcript_path`
  / `transcriptPath` and tails it; if none is reported, falls back to a
  qmux-managed JSONL path under `<workspaceRoot>/.qmux/grok`. This does
  not mark the agent as running.
- `UserPromptSubmit`: marks the agent `Running` and emits
  `agent.prompt_submitted`. qmux reads `prompt` or `input` from the
  payload and matches it against outstanding send tracking.
- `PreToolUse`: marks the agent `Running` and emits `agent.tool_use`.
- `PostToolUse`: marks the agent `Running` and emits
  `agent.tool_result`.
- `Stop`: treats the agent as idle. qmux clears outstanding send
  tracking, respects pause and typing state, drains the next queued turn
  if allowed, and emits either `agent.running` or `agent.done`.
- Grok does not fire Claude's `PermissionRequest` event (its closest
  event is `PermissionDenied`, after a denial). The adapter still
  understands `PermissionRequest` if it arrives, but does not install
  a hook for it.
- Unknown Grok hook events: forwarded as `agent.hook.<event>` with the
  raw hook payload.
