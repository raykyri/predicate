// Observer-only Pi extension for qmux.
//
// This extension deliberately registers no tools, commands, shortcuts, flags,
// providers, UI, input transforms, permission gates, or project-trust handlers.
// It reports Pi's native lifecycle and session identity to the pane-scoped qmux
// control socket; Pi's own JSONL session remains the transcript source of truth.

import { spawn as nodeSpawn } from "node:child_process";

export const QMUX_PI_PROTOCOL_VERSION = 1;

function nonEmptyString(value) {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function callString(target, method) {
  try {
    return nonEmptyString(target?.[method]?.());
  } catch {
    return undefined;
  }
}

function callLeafId(manager) {
  try {
    const value = manager?.getLeafId?.();
    return value === null ? null : nonEmptyString(value);
  } catch {
    return undefined;
  }
}

function sessionPayload(ctx, extra = {}) {
  const manager = ctx?.sessionManager;
  return compact({
    session_id: callString(manager, "getSessionId"),
    session_file: callString(manager, "getSessionFile"),
    // Pi uses null for the tree root before the first entry. Preserve that
    // distinction; omitting it would make qmux fall back to the file's last leaf.
    leaf_id: callLeafId(manager),
    ...modelPayload(ctx?.model),
    thinking_level: nonEmptyString(ctx?.thinkingLevel),
    ...extra,
  });
}

function modelPayload(model) {
  if (!model || typeof model !== "object") return {};
  const provider = nonEmptyString(model.provider);
  const modelId = nonEmptyString(model.id);
  return compact({
    provider,
    model: modelId,
    model_display_name: nonEmptyString(model.name),
  });
}

function compact(value) {
  return Object.fromEntries(Object.entries(value).filter(([, entry]) => entry !== undefined));
}

function createNotifier({ env, spawn, timeoutMs = 2_000 }) {
  const cli = nonEmptyString(env.QMUX_CLI);
  const enabled = Boolean(
    cli &&
      nonEmptyString(env.QMUX_SOCK) &&
      nonEmptyString(env.QMUX_TOKEN) &&
      nonEmptyString(env.QMUX_PANE_ID) &&
      nonEmptyString(env.QMUX_AGENT_ID),
  );
  let tail = Promise.resolve();

  const deliver = (event, payload) => {
    if (!enabled) return Promise.resolve();
    return new Promise((resolve) => {
      let settled = false;
      let child;
      const finish = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(() => {
        try {
          child?.kill();
        } catch {
          // Best effort: lifecycle reporting must never break Pi.
        }
        finish();
      }, timeoutMs);
      const abort = () => {
        try {
          child?.kill();
        } catch {
          // Best effort: the process may already have exited.
        }
        finish();
      };

      try {
        child = spawn(cli, ["notify", event], {
          env: { ...env, QMUX_ADAPTER_ID: "pi" },
          stdio: ["pipe", "ignore", "ignore"],
        });
        child.on("error", finish);
        child.on("close", finish);
        child.stdin.on("error", abort);
        child.stdin.end(JSON.stringify(payload ?? {}));
      } catch {
        finish();
      }
    });
  };

  return {
    send(event, payload = {}) {
      tail = tail.then(() => deliver(event, payload)).catch(() => undefined);
      return tail;
    },
    flush() {
      return tail;
    },
  };
}

export function createQmuxPiExtension({
  env = process.env,
  spawn = nodeSpawn,
  timeoutMs = 2_000,
} = {}) {
  return async function qmuxPiExtension(pi) {
    const notifier = createNotifier({ env, spawn, timeoutMs });
    await notifier.send("PiExtensionReady", { protocol_version: QMUX_PI_PROTOCOL_VERSION });

    pi.on("session_start", (event, ctx) =>
      notifier.send(
        "PiSessionStart",
        sessionPayload(ctx, {
          reason: nonEmptyString(event?.reason),
          previous_session_file: nonEmptyString(event?.previousSessionFile),
        }),
      ),
    );
    pi.on("session_info_changed", (event, ctx) =>
      notifier.send(
        "PiSessionInfoChanged",
        sessionPayload(ctx, { name: nonEmptyString(event?.name) }),
      ),
    );
    pi.on("session_tree", (event, ctx) =>
      notifier.send(
        "PiSessionTree",
        sessionPayload(ctx, {
          leaf_id:
            event?.newLeafId === null
              ? null
              : nonEmptyString(event?.newLeafId) ?? callLeafId(ctx?.sessionManager),
          previous_leaf_id: nonEmptyString(event?.oldLeafId),
        }),
      ),
    );
    pi.on("session_compact", (event, ctx) =>
      notifier.send(
        "PiSessionCompact",
        sessionPayload(ctx, {
          reason: nonEmptyString(event?.reason),
          will_retry: typeof event?.willRetry === "boolean" ? event.willRetry : undefined,
        }),
      ),
    );
    pi.on("model_select", (event, ctx) =>
      notifier.send("PiModelSelect", sessionPayload(ctx, modelPayload(event?.model))),
    );
    pi.on("thinking_level_select", (event, ctx) =>
      notifier.send(
        "PiThinkingLevelSelect",
        sessionPayload(ctx, { thinking_level: nonEmptyString(event?.level) }),
      ),
    );
    pi.on("agent_start", (_event, ctx) =>
      notifier.send("PiAgentStart", sessionPayload(ctx)),
    );
    pi.on("before_agent_start", (event, ctx) =>
      notifier.send(
        "PiPromptSubmit",
        sessionPayload(ctx, { prompt: typeof event?.prompt === "string" ? event.prompt : undefined }),
      ),
    );
    pi.on("turn_start", (_event, ctx) => notifier.send("PiTurnStart", sessionPayload(ctx)));
    pi.on("turn_end", (_event, ctx) => notifier.send("PiTurnEnd", sessionPayload(ctx)));
    pi.on("agent_end", (event, ctx) =>
      notifier.send(
        "PiAgentEnd",
        sessionPayload(ctx, { reason: nonEmptyString(event?.reason) }),
      ),
    );
    pi.on("agent_settled", (_event, ctx) =>
      notifier.send("PiAgentSettled", sessionPayload(ctx)),
    );
    pi.on("session_shutdown", async (event, ctx) => {
      await notifier.send(
        "PiSessionShutdown",
        sessionPayload(ctx, {
          reason: nonEmptyString(event?.reason),
          target_session_file: nonEmptyString(event?.targetSessionFile),
        }),
      );
      await notifier.flush();
    });
  };
}

export default createQmuxPiExtension();
