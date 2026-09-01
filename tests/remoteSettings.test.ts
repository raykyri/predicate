import assert from "node:assert/strict";
import test from "node:test";
import {
  availableRemoteId,
  remoteDraftFromSshAlias,
  remoteIdFromLabel,
  unconfiguredSshAliases,
} from "../src/lib/remoteSettings";
import type { RemoteChoice } from "../src/types";

function remote(id: string): RemoteChoice {
  return {
    id,
    label: id,
    host: id,
    multiplexer: "tmux",
    source: "preferences",
    usable: true,
  };
}

test("remote ids are stable lowercase slugs", () => {
  assert.equal(remoteIdFromLabel("  Build Box / West  "), "build-box-west");
  assert.equal(remoteIdFromLabel("---"), "");
  assert.equal(remoteIdFromLabel("a".repeat(80)).length, 64);
});

test("copied remote ids avoid collisions without exceeding the backend limit", () => {
  assert.equal(
    availableRemoteId("devbox-copy", [remote("devbox-copy"), remote("devbox-copy-2")]),
    "devbox-copy-3",
  );
  const long = "a".repeat(64);
  const result = availableRemoteId(long, [remote(long)]);
  assert.equal(result, `${"a".repeat(62)}-2`);
  assert.equal(result.length, 64);
});

test("SSH aliases seed an unsaved remote with collision-safe defaults", () => {
  assert.deepEqual(remoteDraftFromSshAlias("Prod-West", [remote("prod-west")]), {
    id: "prod-west-2",
    label: "Prod-West",
    host: "Prod-West",
    workspaceRoot: "",
    qmuxCli: "",
    multiplexer: "tmux",
  });
});

test("configured SSH aliases are omitted case-insensitively, including user overrides", () => {
  const remotes = [
    { ...remote("prod"), host: "Prod-West" },
    { ...remote("staging"), host: "deploy@staging" },
  ];
  assert.deepEqual(
    unconfiguredSshAliases(["devbox", "prod-west", "STAGING"], remotes),
    ["devbox"],
  );
});
