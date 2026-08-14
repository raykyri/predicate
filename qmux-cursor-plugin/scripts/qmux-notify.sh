#!/bin/sh
# Observer shim for cursor-agent hooks. The copy qmux actually launches is
# generated (see `cursor_hook_shim`): cursor-agent does not inherit QMUX_*
# into plugin hooks, so that overlay calls `qmux cursor-notify` with a baked
# CLI path and bindings directory. This bundled script remains the env-gated
# fallback used by plugin tests and by a standalone `cursor-agent --plugin-dir`
# pointed at the repo copy. Always prints `{}` so Cursor's hook contract is
# satisfied.
event="${1:-}"
payload=$(cat || true)
if [ -n "$event" ] \
  && [ -n "${QMUX_SOCK:-}" ] \
  && [ -n "${QMUX_TOKEN:-}" ] \
  && [ -n "${QMUX_PANE_ID:-}" ] \
  && [ -n "${QMUX_AGENT_ID:-}" ] \
  && [ -n "${QMUX_CLI:-}" ]; then
  printf '%s' "$payload" | "$QMUX_CLI" notify "$event" >/dev/null 2>&1 || true
fi
printf '%s\n' '{}'
