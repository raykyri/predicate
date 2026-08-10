---
name: qmux
description: "Inspect and control qmux workspaces, panes, splits, and coding agents. Use only when the user explicitly mentions qmux or asks to use qmux to inspect or control terminals or agents. Requires QMUX_ENV=1."
---

# qmux

qmux is a terminal for coding and research agents. Its CLI controls the qmux session containing the caller. It returns a versioned JSON envelope so callers can use returned IDs instead of guessing them.

Before issuing a control command, verify that the caller is running in a qmux-managed pane:

```bash
test "${QMUX_ENV:-}" = 1
```

If that fails, say that this process is not running inside qmux and stop. Do not attempt to control a separately focused qmux window from outside its authenticated pane.

## Learn the installed CLI

The installed binary is the authority for syntax:

```bash
qmux --help
qmux workspace --help
qmux pane --help
qmux agent --help
qmux split --help
qmux artifact --help
```

Most public commands print one compact JSON object. Successful responses have `ok: true`, `apiVersion`, and `result`; server failures have `ok: false` and an `error` with a stable `code`, `message`, and optional `details`. Add `--human` for indented output. Parse IDs from responses rather than deriving them from titles or sidebar order.

## Understand caller authority

Every managed pane receives `QMUX_PANE_ID`, `QMUX_SOCK`, and an authenticated token. An interactive shell also receives a private user credential. Agent processes do not inherit that user credential.

This distinction is intentional:

- From a shell pane, the user CLI may inspect and change panes, splits, and agents in its current workspace.
- From an agent pane, reads are limited to the caller and its live descendants. Agent mutations are limited to explicitly supported lifecycle operations, such as starting a child, prompting or releasing a direct child, and forking itself.
- Raw pane writes, focus changes, workspace mutations, and split mutations require the interactive user credential.

If an operation is denied from an agent pane, do not search the environment for a stronger credential. Report the restriction or ask the user to run it from an interactive shell pane.

## Discover context and state

Start with the caller context, then list before targeting opaque IDs:

```bash
qmux context
qmux workspace list
qmux pane list
qmux pane current
qmux agent list
qmux artifact list
qmux split list
```

Use `workspace get <id>`, `pane get <id>`, and `agent get <id>` for individual records. `pane current` intentionally follows `pane list` in the command surface and resolves from the caller credential, not UI focus.

Read terminal output with:

```bash
qmux pane read <pane-id> --source terminal --lines 120
qmux pane read <pane-id> --source viewport
```

`terminal` reads sanitized scrollback and is the default. `viewport` reads the currently rendered native terminal. Agent reads default to structured transcript turns:

```bash
qmux agent read <agent-id> --source transcript --turns 4
qmux agent read <agent-id> --source terminal --lines 120
```

CLI reads never change user focus.

## Run ordinary terminal work

Create a background shell pane in the current workspace, optionally preserving an explicit working directory:

```bash
qmux pane create --current-workspace --cwd "$PWD" --no-focus
```

Read the pane ID from `.result.pane.id`, then send text or atomically submit a command:

```bash
qmux pane send <pane-id> "text without Enter"
qmux pane send <pane-id> "text with Enter" --submit
qmux pane run <pane-id> "just test"
```

Wait for output already present or produced later. Exactly one matcher is required; timeouts accept `ms`, `s`, or `m` and default to seconds when no suffix is present:

```bash
qmux pane wait-output <pane-id> --match "test result" --timeout 2m
qmux pane wait-output <pane-id> --regex "tests? passed" --timeout 120s
```

The wait searches bounded sanitized scrollback immediately and returns a recent output tail. A timeout is a normal JSON result with `complete: false` and `timedOut: true`.

Rename, focus, or close only panes the user intended to change:

```bash
qmux pane rename <pane-id> "tests"
qmux pane focus <pane-id>
qmux pane close <pane-id>
```

Prefer background creation. Do not close panes you did not create unless the user explicitly asked.

## Start and coordinate agents

Start a supported adapter in the current workspace. An optional leading name becomes the pane title; the returned agent ID remains the authoritative target:

```bash
qmux agent start reviewer --adapter codex --prompt "Review the current diff." --worktree --effort high --no-focus
```

Available launch options are `--adapter`, `--prompt`, `--worktree`/`-w`, `--model`, `--effort`, and `--cwd`. Do not request a worktree or different cwd unless the task needs isolation or the user asked for it.

Coordinate the returned agent with:

```bash
qmux agent prompt <agent-id> "Report only actionable findings."
qmux agent wait <agent-id> --until settled --timeout 2m
qmux agent read <agent-id> --source transcript --turns 6
```

Wait states are `settled`, `input`, `permission`, `done`, `failed`, and `exited`. Inspect `agent get` and `agent read` after a timeout or a request for input before choosing the next action.

Fork an existing live agent, optionally with a launch prompt and isolated worktree:

```bash
qmux agent fork <agent-id> --prompt "Try the alternative approach." --worktree --no-focus
```

Use `qmux agent focus <agent-id>` only when the user wants to switch context. Release a live agent only after its work is no longer needed:

```bash
qmux agent release <agent-id>
```

Release refuses to close an agent that still has live descendants. Handle `blockedByLiveDescendants` by coordinating or releasing those descendants first; do not bypass the lineage check with raw pane closure.

## Manage split layout

Splits contain adjacent panes in one workspace. Inspect the existing layout before changing it:

```bash
qmux split list
qmux split join <pane-id> <adjacent-pane-id>
qmux split resize <split-id> <pane-id> 0.6
qmux split leave <pane-id>
```

`resize` sets that pane's absolute fraction and proportionally redistributes the remainder. Fractions preserve a minimum usable size for every member. Split changes are persisted and applied to the live UI.

## Workspaces and artifacts

Shell callers can create a workspace or rename their current one:

```bash
qmux workspace create --name "review" --dir "$PWD"
qmux workspace rename <current-workspace-id> "new name"
```

Artifacts are files or loopback URLs previously recorded by qmux agents. List them, then request one in qmux's browser overlay:

```bash
qmux artifact list
qmux artifact open <artifact-id>
```

Artifact paths remain confined to the owning pane's allowed file roots, and URLs must be loopback HTTP(S).

## Safety rules

- Verify `QMUX_ENV=1` before control commands.
- Use IDs from JSON responses and caller context; never infer a target from UI order.
- Prefer `--no-focus` for background work and use focus commands only on request.
- Inspect before prompting, writing, releasing, closing, or rearranging existing work.
- Never expose or copy `QMUX_TOKEN` or `QMUX_USER_TOKEN`.
- Do not try to turn an agent token into a user credential.
- Server errors exit with status 1. CLI syntax errors exit with status 2.

