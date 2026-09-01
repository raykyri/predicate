# Hooks

Hooks tell qmux when an agent starts a session, when a prompt was
submitted, when tools run, when permission is needed, and when the
agent is idle enough for queued turns to advance.

Hooks are only installed for agents launched by qmux or by qmux's
shell wrapper functions inside a qmux shell pane, so a Claude, Codex,
or Devin process started outside qmux's setup will not have these hooks.

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

For Devin, `--config` replaces `~/.config/devin/config.json` instead of
merging, so qmux copies that user file (or a stub with
`shell.setup_complete` when none exists), injects Claude-shaped
`qmux notify` hooks, and starts Devin with `--config` pointing at a
per-pane, per-spawn file under
<qmux workspace root>/.qmux/hooks/devin-<pane-id>-<nonce>.json (created
0600 in a 0700 dir with O_EXCL, previous file for that pane pruned). The
user's original config is never written. Project `.devin/hooks.v1.json`
is left alone. For the exact hooks, see
src-tauri/src/adapters/devin.rs:44.

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

For Muse (Meta's Muse Code), hooks are neither a settings file nor a
hooks directory — they are capabilities of a *plugin*. qmux generates
one and installs it:

```
$QMUX_MUSE_HOME/qmux-muse-hook       (default $QMUX_MUSE_HOME =
$QMUX_MUSE_HOME/plugin/               $XDG_DATA_HOME/qmux/muse)
$QMUX_MUSE_HOME/bindings/<pane>.json
$QMUX_MUSE_HOME/installed.stamp
```

Every alternative was probed and does not fire: `settings.json` hooks,
`settings.managed_hooks_path`, the `TBH_MANAGED_HOOKS_PATH` env var, and
a project `.musehooks.json`. Only a native plugin works, it requires
`MUSE_EXPERIMENTAL_PLUGINS=1` (which qmux sets on the pane), and its
capabilities must be approved once — so launching runs `muse plugins
install` + `muse plugins approve`, gated on a fingerprint stamp so it
happens only when the generated sources change. Muse also rejects a
plugin whose hooks share one script file, hence one script per event.
For exact hooks, see src-tauri/src/adapters/muse.rs:32.

The Claude, Codex, Grok, and Devin shims call back into qmux via
`qmux notify <event>`, which sends a token-scoped hook.notify request
back to the app.

Muse cannot use that path. It runs hooks with a sanitized environment
that strips every `QMUX_*` variable, so its shim can never see which
pane it belongs to — the "no-op unless the qmux env is set" guard the
other shims rely on would disable every hook instead. Its shim calls
`qmux muse-notify <event>` instead, which resolves the pane from a
binding file qmux writes before launch, keyed on the two identifiers
every Muse payload carries: `session_id` first (exact), then `cwd`. A
payload matching no binding exits quietly, which is what keeps a
standalone `muse` run — which still inherits the globally installed
plugin — unaffected.

The `cwd` fallback only matches a binding no session has claimed yet.
Once a pane's own `SessionStart` stamps its session id onto its binding,
the directory stops being a way in, and the app additionally refuses to
re-point an agent that already has a session id. Together those keep a
`muse` started outside qmux, in a directory a qmux pane is already
working in, from posting hooks to that pane. One residual race has no
fix while the environment is stripped: if the outside process starts
first and claims the unclaimed binding, the qmux pane's own session is
the one that then looks foreign.

Because a binding carries the pane's control-socket token, the bindings
directory is `0700` and each file `0600`. All bindings are dropped at
app startup — tokens are minted per process and never persisted, so
every binding from a previous run is already useless — and pruned again
on each Muse launch, with a grace period so a launch in flight (the
binding is written before the pane exists, because `SessionStart` fires
that early) is not swept by a concurrent one.


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
  `agent.done`. A queue paused by a failed/interrupted/disconnected turn
  is not drained.
- `Notification.elicitation_dialog`: marks the agent `AwaitingInput`
  and emits `agent.awaiting_input`.
- Other `Notification` events: mark the agent `AwaitingInput` and emit
  `agent.notification`.
- `Stop`: uses the same idle handling as `Notification.idle_prompt`,
  including queue draining and the `agent.running` or `agent.done`
  result. This is the only automatic "send the next queued message"
  signal.
- `StopFailure`: the current turn failed. qmux pauses a non-empty queue
  and settles the agent to `Done` without draining, so a recovering TUI
  cannot auto-send follow-ups written for the failed turn. Unpause (or
  send the top queued turn) to continue.
- `SessionEnd` while the agent is still `Starting` or `Running`: same
  hold as `StopFailure`. A session that already settled does not change
  the queue.
- Transcript interrupt (`[Request interrupted by user]`) and Claude's
  lone-Esc watch: settle to `AwaitingInput` and hold the queue the same
  way. Dependents waiting on actual completion stay blocked.
- `SubagentStop`: emits `agent.subagent_stopped` without changing the
  main agent status.
- Unknown Claude hook events: forwarded as `agent.hook.<event>` with
  the raw hook payload.


## Devin

- `SessionStart`: records `session_id` (or `sessionId`) from the hook
  payload. Devin does not report a `transcript_path`; qmux binds
  `--export` to `<workspaceRoot>/.qmux/devin/<agent>.json` at launch and
  tails that ATIF document. This does not mark the agent as running; a
  prompt or tool hook does that.
- `UserPromptSubmit`: marks the agent `Running` and emits
  `agent.prompt_submitted`. qmux matches the payload's `prompt` against
  outstanding send tracking.
- `PreToolUse`: marks the agent `Running` and emits `agent.tool_use`.
- `PostToolUse`: marks the agent `Running` and emits
  `agent.tool_result`.
- `PermissionRequest`: marks the agent `AwaitingPermission` and emits
  `agent.awaiting_permission`. Approvals stay in Devin's TUI; qmux does
  not send permission actions.
- `PostCompaction`: marks the agent `Running` and emits
  `agent.compacted`.
- `Stop`: treats the agent as idle. qmux clears outstanding send
  tracking, respects pause and typing state, drains the next queued turn
  if allowed, and emits either `agent.running` or `agent.done`.
- `SessionEnd`: emits `agent.session_end` without changing status.
- Unknown Devin hook events: forwarded as `agent.hook.<event>` with the
  raw hook payload.


## Codex

- `SessionStart`: treats the session id from `session_id`, `sessionId`,
  `resource_id`, or `resourceId` as provisional and starts transcript
  validation. If Codex provides `transcript_path` or `transcriptPath`,
  qmux polls that explicit `.jsonl` path until its `session_meta` is
  ready. Otherwise qmux searches `$CODEX_HOME/sessions` for a rollout
  whose `session_meta.id` matches the candidate id. This does not mark
  the agent as running.
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

### Codex session identity binding

For a bound Codex agent, qmux maintains this invariant:

```text
agent.session_id == session_meta.id(agent.transcript_path)
```

Hook delivery alone cannot establish that identity. Codex can create a
transient side conversation inside an existing TUI and route its
`SessionStart` through the same pane-scoped hook token even when that
conversation has no durable rollout. Consequently, a hook-reported id
and path form a process-local binding candidate; they do not mutate the
persisted agent yet.

Each candidate gets a generation. Validation may poll an acceptable
explicit path, or discover a rollout below `$CODEX_HOME/sessions` by
reading `session_meta.id`. Once a rollout proves the candidate id, qmux
commits `session_id` and `transcript_path` together and starts the new
tail. A newer candidate invalidates older generations, preventing a slow
validator from committing after it has been superseded.

If validation fails or no rollout appears, qmux discards the candidate.
An existing canonical binding remains unchanged and the failed
candidate is silent; an initially unbound agent receives the normal
`Transcript unavailable` notice. This allows real qmux-created forks and
resumes to take ownership once their rollouts exist while ignoring
rollout-less TUI side conversations. On recovery and hook ingestion,
qmux also repairs legacy hybrid state by deriving the session id from an
already-bound rollout before selecting a session to resume.


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
- `StopFailure`: pauses a non-empty queue and settles to `Done` without
  draining, matching Claude.
- `SessionEnd` while the agent is still `Starting` or `Running`: same
  hold as `StopFailure`.
- Grok does not fire Claude's `PermissionRequest` event (its closest
  event is `PermissionDenied`, after a denial). The adapter still
  understands `PermissionRequest` if it arrives, but does not install
  a hook for it.
- Unknown Grok hook events: forwarded as `agent.hook.<event>` with the
  raw hook payload.


## Muse

Muse's payloads are Claude-shaped, but two of its behaviours change how
they are read.

First, **every hook fired inside a subagent reports the child's
`session_id`** and carries no pointer back to the parent. Muse's
built-in `tbh-reminders` plugin runs subagents on *every* turn — and
keeps running them after the main `Stop` — so a pane that let them drive
its status would never settle.

In practice they stop at the shim: once a binding is claimed by the main
session, a payload carrying a child's session id matches nothing and
exits quietly. The adapter still compares each payload's `session_id`
against the agent's recorded main session and forwards a mismatch as a
passive `agent.subagent_activity` event, but that is now a backstop for
the window before the claim rather than the usual route.

That comparison needs a main session to compare against, which a pane
does not have before its first hook. Two rules cover the gap. Muse
reports `SubagentStart` / `SubagentStop` from the child's point of view —
`session_id` equals `child_session_id`, and `subagent_id` names the agent
— so those are recognized on their own terms. And only the events a
subagent cannot produce (`UserPromptSubmit`, `Stop`; `SessionStart` binds
in its own arm) are allowed to name an unbound pane's session, so a
subagent's tool hook can never bind the pane to a child. Either way, only
main-session hooks move the pane's status.

Second, **a resumed session fires no `SessionStart`**. `muse resume
<id>` emits only `UserPromptSubmit` and `Stop`, so a resumed pane's
session identity can never arrive from a hook. qmux writes it into the
binding at launch (it knows the id it is resuming), and the first
main-session hook adopts it for an agent that somehow has none.

- `SessionStart`: records `session_id`, stamps it onto the pane binding
  so later hooks resolve by session rather than by directory, and starts
  discovering the transcript. This does not mark the agent as running.
  The first `SessionStart` wins — a Muse session id never changes, so a
  second one belongs to some other process that matched by directory.
- `UserPromptSubmit`: marks the agent `Running`, matches the payload's
  `prompt` against outstanding send tracking, and emits
  `agent.prompt_submitted`.
- `PreToolUse`: marks the agent `Running` and emits `agent.tool_use`.
- `PostToolUse`: marks the agent `Running` and emits
  `agent.tool_result`.
- `PermissionRequest`: marks the agent `Running` and emits
  `agent.permission_request` — deliberately *not* `AwaitingPermission`.
  Muse has no matching resolution event, so nothing would ever clear
  that state, and in practice its policy layer and approval judge allow
  most calls without ever showing a dialog.
- `SubagentStart` / `SubagentStop`: passive. Emitted for visibility but
  never gate idle, for the reminder-agent reason above.
- `Stop`: treats the agent as idle. qmux clears outstanding send
  tracking, respects pause and typing state, drains the next queued turn
  if allowed, and emits either `agent.running` or `agent.done`. It does
  not wait for subagent quiescence.
- Unknown Muse hook events: forwarded as `agent.hook.<event>` with the
  raw hook payload.

Muse always reports `transcript_path: null`, so qmux discovers the log
itself by scanning
`$XDG_DATA_HOME/muse/sessions/<year>/<month>/<day>/<session-id>/session.jsonl`
newest-first, on a background thread that retries — `SessionStart` can
beat the directory into existence. Subagents write their own nested logs
under `<session-id>/subagent/<child-id>/`, so the main log needs no
filtering.
