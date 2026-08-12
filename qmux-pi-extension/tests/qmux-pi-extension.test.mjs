import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import test from "node:test";
import {
  QMUX_PI_PROTOCOL_VERSION,
  createQmuxPiExtension,
} from "../index.js";

function fakePi() {
  const handlers = new Map();
  return {
    handlers,
    on(event, handler) {
      assert.equal(typeof handler, "function");
      handlers.set(event, handler);
    },
  };
}

function recordingSpawn(records) {
  return (cli, args, options) => {
    const child = new EventEmitter();
    child.stdin = new EventEmitter();
    child.stdin.end = (payload) => {
      records.push({ cli, args, options, payload: JSON.parse(payload) });
      queueMicrotask(() => child.emit("close", 0));
    };
    child.kill = () => child.emit("close", 1);
    return child;
  };
}

function qmuxEnv() {
  return {
    QMUX_CLI: "/tmp/qmux-cli",
    QMUX_SOCK: "/tmp/qmux.sock",
    QMUX_TOKEN: "pane-token",
    QMUX_PANE_ID: "pane-1",
    QMUX_AGENT_ID: "agent-1",
  };
}

function piContext() {
  return {
    model: { provider: "anthropic", id: "claude-sonnet", name: "Sonnet" },
    thinkingLevel: "high",
    sessionManager: {
      getSessionId: () => "session-1",
      getSessionFile: () => "/tmp/session-1.jsonl",
      getLeafId: () => "leaf-1",
    },
  };
}

test("registers only observer lifecycle handlers", async () => {
  const pi = fakePi();
  const records = [];
  await createQmuxPiExtension({ env: qmuxEnv(), spawn: recordingSpawn(records) })(pi);

  assert.deepEqual([...pi.handlers.keys()], [
    "session_start",
    "session_info_changed",
    "session_tree",
    "session_compact",
    "model_select",
    "thinking_level_select",
    "agent_start",
    "before_agent_start",
    "turn_start",
    "turn_end",
    "agent_end",
    "agent_settled",
    "session_shutdown",
  ]);
  assert.equal(records[0].args.join(" "), "notify PiExtensionReady");
  assert.deepEqual(records[0].payload, { protocol_version: QMUX_PI_PROTOCOL_VERSION });
});

test("reports session identity, active leaf, model, and thinking state", async () => {
  const pi = fakePi();
  const records = [];
  await createQmuxPiExtension({ env: qmuxEnv(), spawn: recordingSpawn(records) })(pi);
  await pi.handlers.get("session_start")(
    { reason: "resume", previousSessionFile: "/tmp/old.jsonl" },
    piContext(),
  );

  assert.equal(records[1].args.join(" "), "notify PiSessionStart");
  assert.deepEqual(records[1].payload, {
    session_id: "session-1",
    session_file: "/tmp/session-1.jsonl",
    leaf_id: "leaf-1",
    provider: "anthropic",
    model: "claude-sonnet",
    model_display_name: "Sonnet",
    thinking_level: "high",
    reason: "resume",
    previous_session_file: "/tmp/old.jsonl",
  });
  assert.equal(records[1].options.env.QMUX_ADAPTER_ID, "pi");
});

test("serializes lifecycle delivery and reports the settled boundary", async () => {
  const pi = fakePi();
  const records = [];
  await createQmuxPiExtension({ env: qmuxEnv(), spawn: recordingSpawn(records) })(pi);
  const ctx = piContext();

  await Promise.all([
    pi.handlers.get("agent_start")({}, ctx),
    pi.handlers.get("turn_start")({}, ctx),
    pi.handlers.get("agent_end")({}, ctx),
    pi.handlers.get("agent_settled")({}, ctx),
  ]);

  assert.deepEqual(
    records.slice(1).map((record) => record.args[1]),
    ["PiAgentStart", "PiTurnStart", "PiAgentEnd", "PiAgentSettled"],
  );
});

test("reports the submitted prompt without transforming it", async () => {
  const pi = fakePi();
  const records = [];
  await createQmuxPiExtension({ env: qmuxEnv(), spawn: recordingSpawn(records) })(pi);
  await pi.handlers.get("before_agent_start")({ prompt: "keep this exact" }, piContext());

  assert.equal(records[1].args.join(" "), "notify PiPromptSubmit");
  assert.equal(records[1].payload.prompt, "keep this exact");
});

test("does nothing when it is not running inside qmux", async () => {
  const pi = fakePi();
  const records = [];
  await createQmuxPiExtension({ env: {}, spawn: recordingSpawn(records) })(pi);
  await pi.handlers.get("agent_start")({}, piContext());
  assert.deepEqual(records, []);
});

test("terminates a notifier helper whose stdin fails", async () => {
  const pi = fakePi();
  let killed = 0;
  const spawn = () => {
    const child = new EventEmitter();
    child.stdin = new EventEmitter();
    child.stdin.end = () => queueMicrotask(() => child.stdin.emit("error", new Error("EPIPE")));
    child.kill = () => {
      killed += 1;
      queueMicrotask(() => child.emit("close", 1));
    };
    return child;
  };

  await createQmuxPiExtension({ env: qmuxEnv(), spawn })(pi);
  assert.equal(killed, 1);
});
