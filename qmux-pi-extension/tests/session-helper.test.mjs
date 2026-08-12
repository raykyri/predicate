import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createBranchedSession } from "../session-helper.js";

test("declares an explicit ESM boundary for packaged app resources", () => {
  const manifest = JSON.parse(
    readFileSync(new URL("../package.json", import.meta.url), "utf8"),
  );
  assert.equal(manifest.type, "module");
});

test("bundles the ESM boundary beside both Pi integration scripts", () => {
  const tauriConfig = JSON.parse(
    readFileSync(new URL("../../src-tauri/tauri.conf.json", import.meta.url), "utf8"),
  );
  const resources = tauriConfig.bundle.resources;
  assert.equal(resources["../qmux-pi-extension/index.js"], "qmux-pi-extension/index.js");
  assert.equal(
    resources["../qmux-pi-extension/session-helper.js"],
    "qmux-pi-extension/session-helper.js",
  );
  assert.equal(
    resources["../qmux-pi-extension/package.json"],
    "qmux-pi-extension/package.json",
  );
});

test("delegates branch creation and target-directory selection to Pi", () => {
  const calls = [];
  const manager = {
    createBranchedSession(leafId) {
      calls.push(["branch", leafId]);
      return "/sessions/target/fork.jsonl";
    },
    getSessionId: () => "fork-session",
    getLeafId: () => "leaf-1",
  };
  const api = {
    SessionManager: {
      create(cwd) {
        calls.push(["create", cwd]);
        return { getSessionDir: () => "/sessions/target" };
      },
      open(source, sessionDir, cwd) {
        calls.push(["open", source, sessionDir, cwd]);
        return manager;
      },
    },
  };

  assert.deepEqual(
    createBranchedSession(api, "/sessions/source.jsonl", "leaf-1", "/worktree", {
      fileExists: () => true,
    }),
    {
      session_file: "/sessions/target/fork.jsonl",
      session_id: "fork-session",
      leaf_id: "leaf-1",
    },
  );
  assert.deepEqual(calls, [
    ["create", "/worktree"],
    ["open", "/sessions/source.jsonl", "/sessions/target", "/worktree"],
    ["branch", "leaf-1"],
  ]);
});

test("rejects a non-persisted branch", () => {
  const api = {
    SessionManager: {
      create: () => ({ getSessionDir: () => "/sessions/target" }),
      open: () => ({
        createBranchedSession: () => undefined,
      }),
    },
  };
  assert.throws(
    () => createBranchedSession(api, "/source.jsonl", "leaf-1", "/worktree"),
    /did not persist/,
  );
});

test("rejects Pi's deferred path when the selected turn has no assistant response", () => {
  const api = {
    SessionManager: {
      create: () => ({ getSessionDir: () => "/sessions/target" }),
      open: () => ({
        createBranchedSession: () => "/sessions/target/deferred.jsonl",
      }),
    },
  };
  assert.throws(
    () =>
      createBranchedSession(api, "/source.jsonl", "user-only", "/worktree", {
        fileExists: () => false,
      }),
    /wait for the turn to finish/,
  );
});
