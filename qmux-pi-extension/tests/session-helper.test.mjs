import assert from "node:assert/strict";
import test from "node:test";
import { createBranchedSession } from "../session-helper.js";

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
