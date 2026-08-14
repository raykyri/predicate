#!/bin/sh
# Observer shim for cursor-agent hooks. Forwards hook JSON from stdin to
# `qmux notify <event>` when running inside a qmux pane. Always prints `{}` so
# Cursor's hook contract is satisfied and standalone `cursor-agent` runs that
# happen to load this plugin (via --plugin-dir) are unaffected.
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
