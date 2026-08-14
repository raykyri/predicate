import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const script = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "scripts",
  "qmux-notify.sh",
);

function runNotify(env, stdin, extraPath) {
  const result = spawnSync(script, ["sessionStart"], {
    input: stdin,
    encoding: "utf8",
    env: { ...process.env, ...env, PATH: extraPath ?? process.env.PATH },
  });
  return result;
}

test("shim prints {} and does not require qmux outside a pane", () => {
  const result = runNotify({}, '{"conversation_id":"abc"}');
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), "{}");
});

test("shim forwards stdin to qmux notify when pane env is set", () => {
  const dir = mkdtempSync(join(tmpdir(), "qmux-cursor-plugin-"));
  const recorder = join(dir, "qmux");
  const log = join(dir, "notify.log");
  writeFileSync(
    recorder,
    `#!/bin/sh
echo "$1" > ${JSON.stringify(log)}
echo "$2" >> ${JSON.stringify(log)}
cat >> ${JSON.stringify(log)}
`,
  );
  chmodSync(recorder, 0o755);
  const result = runNotify(
    {
      QMUX_CLI: recorder,
      QMUX_SOCK: "/tmp/qmux.sock",
      QMUX_TOKEN: "pane-token",
      QMUX_PANE_ID: "pane-1",
      QMUX_AGENT_ID: "agent-1",
    },
    '{"conversation_id":"abc"}',
  );
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout.trim(), "{}");
  const recorded = spawnSync("cat", [log], { encoding: "utf8" });
  assert.equal(recorded.stdout, 'notify\nsessionStart\n{"conversation_id":"abc"}');
});
