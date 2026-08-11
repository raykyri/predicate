import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

test("scopes transcript events to the root session and deduplicates final text parts", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-plugin-"));
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-1",
    QMUX_AGENT_ID: "agent-1",
    QMUX_CLI: "/usr/bin/true",
    QMUX_WORKSPACE_ROOT: workspace,
  });

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?test=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "root-session" } } },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: { info: { id: "child-session", parentID: "root-session" } },
    },
  });

  await hooks["chat.message"](
    { sessionID: "child-session" },
    {
      message: { id: "child-message", role: "user" },
      parts: [{ type: "text", text: "private child prompt" }],
    },
  );
  await hooks["chat.message"](
    { sessionID: "root-session" },
    {
      message: { id: "user-message", role: "user" },
      parts: [{ type: "text", text: "root prompt" }],
    },
  );
  await hooks.event({
    event: {
      type: "message.updated",
      properties: { info: { id: "assistant-message", role: "assistant" } },
    },
  });
  const finalPart = {
    id: "text-part",
    type: "text",
    messageID: "assistant-message",
    sessionID: "root-session",
    text: "root reply",
    time: { end: Date.now() },
  };
  await hooks.event({
    event: { type: "message.part.updated", properties: { part: finalPart } },
  });
  await hooks.event({
    event: { type: "message.part.updated", properties: { part: finalPart } },
  });

  const transcript = await readFile(
    join(workspace, ".qmux", "opencode", "agent-1", "root-session.jsonl"),
    "utf8",
  );
  const lines = transcript.trim().split("\n").map(JSON.parse);
  assert.equal(lines.length, 2);
  assert.deepEqual(
    lines.map((line) => line.session_id),
    ["root-session", "root-session"],
  );
  assert.equal(lines[0].payload.content[0].text, "root prompt");
  assert.equal(lines[1].payload.content[0].text, "root reply");

  await rm(workspace, { recursive: true, force: true });
});

test("sends lifecycle payloads to qmux over stdin", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-notify-"));
  const cli = join(workspace, "qmux-test-cli");
  const notifications = join(workspace, "notifications.txt");
  await writeFile(
    cli,
    '#!/bin/sh\npayload=$(cat)\nprintf "%s\\t%s\\n" "$*" "$payload" >> "$QMUX_NOTIFY_LOG"\n',
    "utf8",
  );
  await chmod(cli, 0o755);
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-notify",
    QMUX_AGENT_ID: "agent-notify",
    QMUX_CLI: cli,
    QMUX_WORKSPACE_ROOT: workspace,
    QMUX_NOTIFY_LOG: notifications,
  });
  delete process.env.QMUX_FORK_POINT;
  delete process.env.QMUX_ROOT_SESSION_ID;

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?stdin=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "session-stdin" } } },
  });

  const [line] = (await readFile(notifications, "utf8")).trim().split("\n");
  const separator = line.indexOf("\t");
  assert.equal(line.slice(0, separator), "notify SessionStart");
  assert.deepEqual(JSON.parse(line.slice(separator + 1)), { session_id: "session-stdin" });

  await rm(workspace, { recursive: true, force: true });
});

test("writes successive text-part snapshots for live transcript updates", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-stream-"));
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-stream",
    QMUX_AGENT_ID: "agent-stream",
    QMUX_CLI: "/usr/bin/true",
    QMUX_WORKSPACE_ROOT: workspace,
  });
  delete process.env.QMUX_FORK_POINT;
  delete process.env.QMUX_ROOT_SESSION_ID;

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?stream=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "stream-session" } } },
  });
  await hooks.event({
    event: {
      type: "message.updated",
      properties: { info: { id: "assistant-message", role: "assistant" } },
    },
  });

  for (const part of [
    { text: "hel", time: {} },
    { text: "hello", time: {} },
    { text: "hello!", time: { end: Date.now() } },
  ]) {
    await hooks.event({
      event: {
        type: "message.part.updated",
        properties: {
          part: {
            id: "text-part",
            type: "text",
            messageID: "assistant-message",
            sessionID: "stream-session",
            ...part,
          },
        },
      },
    });
  }

  const transcript = await readFile(
    join(workspace, ".qmux", "opencode", "agent-stream", "stream-session.jsonl"),
    "utf8",
  );
  const lines = transcript.trim().split("\n").map(JSON.parse);
  assert.deepEqual(
    lines.map((line) => line.payload.content[0].text),
    ["hel", "hello", "hello!"],
  );
  assert(lines.every((line) => line.payload.id === "text-part"));
  assert(lines.every((line) => line.payload.opencode_text_snapshot));

  await rm(workspace, { recursive: true, force: true });
});

test("treats the native fork child as root while excluding its subagents", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-fork-plugin-"));
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-fork",
    QMUX_AGENT_ID: "agent-fork",
    QMUX_CLI: "/usr/bin/true",
    QMUX_WORKSPACE_ROOT: workspace,
    QMUX_FORK_POINT: "source-session",
  });

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?fork=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "source-session" } } },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: { info: { id: "fork-session", parentID: "source-session" } },
    },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: { info: { id: "subagent-session", parentID: "fork-session" } },
    },
  });
  await hooks["chat.message"](
    { sessionID: "fork-session" },
    {
      message: { id: "fork-message", role: "user" },
      parts: [{ type: "text", text: "fork prompt" }],
    },
  );
  await hooks["chat.message"](
    { sessionID: "subagent-session" },
    {
      message: { id: "subagent-message", role: "user" },
      parts: [{ type: "text", text: "private subagent prompt" }],
    },
  );

  const transcript = await readFile(
    join(workspace, ".qmux", "opencode", "agent-fork", "fork-session.jsonl"),
    "utf8",
  );
  const lines = transcript.trim().split("\n").map(JSON.parse);
  assert.equal(lines.length, 1);
  assert.equal(lines[0].session_id, "fork-session");
  assert.equal(lines[0].payload.content[0].text, "fork prompt");

  delete process.env.QMUX_FORK_POINT;
  await rm(workspace, { recursive: true, force: true });
});

test("deduplicates error and idle completion notifications", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-stop-plugin-"));
  const cli = join(workspace, "qmux-test-cli");
  const notifications = join(workspace, "notifications.txt");
  await writeFile(cli, '#!/bin/sh\nprintf "%s\\n" "$*" >> "$QMUX_NOTIFY_LOG"\n', "utf8");
  await chmod(cli, 0o755);
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-stop",
    QMUX_AGENT_ID: "agent-stop",
    QMUX_CLI: cli,
    QMUX_WORKSPACE_ROOT: workspace,
    QMUX_NOTIFY_LOG: notifications,
  });
  delete process.env.QMUX_FORK_POINT;

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?stop=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "root-session" } } },
  });
  await hooks.event({
    event: {
      type: "session.error",
      properties: { sessionID: "root-session" },
    },
  });
  await hooks.event({
    event: {
      type: "session.idle",
      properties: { sessionID: "root-session" },
    },
  });

  const lines = (await readFile(notifications, "utf8")).trim().split("\n");
  assert.equal(lines.filter((line) => line.includes("StopFailure")).length, 1);
  assert.equal(lines.filter((line) => /notify Stop /.test(line)).length, 0);

  await rm(workspace, { recursive: true, force: true });
});

test("treats an expected resumed fork as root despite its parent lineage", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-resume-plugin-"));
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-resume",
    QMUX_AGENT_ID: "agent-resume",
    QMUX_CLI: "/usr/bin/true",
    QMUX_WORKSPACE_ROOT: workspace,
    QMUX_ROOT_SESSION_ID: "resumed-fork-session",
  });
  delete process.env.QMUX_FORK_POINT;

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?resume=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: {
      type: "session.created",
      properties: {
        info: { id: "resumed-fork-session", parentID: "original-session" },
      },
    },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: {
        info: { id: "subagent-session", parentID: "resumed-fork-session" },
      },
    },
  });
  await hooks["chat.message"](
    { sessionID: "resumed-fork-session" },
    {
      message: { id: "root-message", role: "user" },
      parts: [{ type: "text", text: "resume prompt" }],
    },
  );
  await hooks["chat.message"](
    { sessionID: "subagent-session" },
    {
      message: { id: "child-message", role: "user" },
      parts: [{ type: "text", text: "private child prompt" }],
    },
  );

  const transcript = await readFile(
    join(
      workspace,
      ".qmux",
      "opencode",
      "agent-resume",
      "resumed-fork-session.jsonl",
    ),
    "utf8",
  );
  const lines = transcript.trim().split("\n").map(JSON.parse);
  assert.equal(lines.length, 1);
  assert.equal(lines[0].payload.content[0].text, "resume prompt");

  delete process.env.QMUX_ROOT_SESSION_ID;
  await rm(workspace, { recursive: true, force: true });
});

test("reports descendant sessions and keeps them on root idle snapshots", async () => {
  const workspace = await mkdtemp(join(tmpdir(), "qmux-opencode-subagents-"));
  const cli = join(workspace, "qmux-test-cli");
  const notifications = join(workspace, "notifications.txt");
  await writeFile(
    cli,
    '#!/bin/sh\npayload=$(cat)\nprintf "%s\\t%s\\n" "$*" "$payload" >> "$QMUX_NOTIFY_LOG"\n',
    "utf8",
  );
  await chmod(cli, 0o755);
  Object.assign(process.env, {
    QMUX_SOCK: join(workspace, "qmux.sock"),
    QMUX_TOKEN: "test-token",
    QMUX_PANE_ID: "pane-subagents",
    QMUX_AGENT_ID: "agent-subagents",
    QMUX_CLI: cli,
    QMUX_WORKSPACE_ROOT: workspace,
    QMUX_NOTIFY_LOG: notifications,
  });
  delete process.env.QMUX_FORK_POINT;
  delete process.env.QMUX_ROOT_SESSION_ID;

  const { QmuxNotifyPlugin } = await import(`../plugins/qmux-notify.js?subagents=${Date.now()}`);
  const hooks = await QmuxNotifyPlugin();
  await hooks.event({
    event: { type: "session.created", properties: { info: { id: "root-session" } } },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: { info: { id: "child-session", parentID: "root-session" } },
    },
  });
  await hooks.event({
    event: {
      type: "session.created",
      properties: { info: { id: "grandchild-session", parentID: "child-session" } },
    },
  });
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "root-session" } },
  });
  // A native root prompt can arrive while descendants still run. It resets
  // per-turn deduplication but must not erase descendant tracking.
  await hooks["chat.message"](
    { sessionID: "root-session" },
    {
      message: { id: "next-message", role: "user" },
      parts: [{ type: "text", text: "next turn" }],
    },
  );
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "root-session" } },
  });
  // Child completion is lifecycle bookkeeping only: it must not masquerade as
  // completion of the root turn, and the grandchild remains active.
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "child-session" } },
  });
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "grandchild-session" } },
  });
  // This lands inside the ordinary one-second duplicate window. The earlier
  // non-terminal root idle must not suppress this real completion edge.
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "root-session" } },
  });
  // A new short turn after real completion must get its own completion edge
  // without waiting out the previous turn's one-second dedup window.
  await hooks["chat.message"](
    { sessionID: "root-session" },
    {
      message: { id: "third-message", role: "user" },
      parts: [{ type: "text", text: "third turn" }],
    },
  );
  await hooks.event({
    event: { type: "session.idle", properties: { sessionID: "root-session" } },
  });

  const lines = (await readFile(notifications, "utf8"))
    .trim()
    .split("\n")
    .map((line) => {
      const separator = line.indexOf("\t");
      return {
        command: line.slice(0, separator),
        payload: JSON.parse(line.slice(separator + 1)),
      };
    });
  assert.deepEqual(
    lines.filter((line) => line.command === "notify SubagentStart").map((line) => line.payload),
    [
      {
        session_id: "root-session",
        subagent_id: "child-session",
        parent_session_id: "root-session",
      },
      {
        session_id: "root-session",
        subagent_id: "grandchild-session",
        parent_session_id: "child-session",
      },
    ],
  );
  const rootStops = lines.filter(
    (line) => line.command === "notify Stop" && line.payload.session_id === "root-session",
  );
  assert.deepEqual(
    rootStops.map((line) => line.payload.active_subagents),
    [
      ["child-session", "grandchild-session"],
      ["child-session", "grandchild-session"],
      [],
      [],
    ],
  );
  assert.deepEqual(
    lines.filter((line) => line.command === "notify SubagentStop").map((line) => line.payload),
    [
      { session_id: "root-session", subagent_id: "child-session" },
      { session_id: "root-session", subagent_id: "grandchild-session" },
    ],
  );
  assert.equal(
    lines.filter(
      (line) => line.command === "notify Stop" && line.payload.session_id === "child-session",
    ).length,
    0,
  );

  await rm(workspace, { recursive: true, force: true });
});
