import assert from "node:assert/strict";
import { test } from "node:test";
import { rewriteDevinFileRefs } from "../src/lib/devinFileRefs";

const snippetFile =
  "/Users/raymond/Code/multitool/.claude/worktrees/foks-experiment/foks-ui/src/screens/write-workflows.tsx";

test("rewrites Devin snippet tags to file and line-number markdown links", () => {
  assert.equal(
    rewriteDevinFileRefs(
      `<ref_snippet file="${snippetFile}" lines="760-843" />`,
    ),
    `[write-workflows.tsx:760-843](${snippetFile}:760-843)`,
  );
  assert.equal(
    rewriteDevinFileRefs(
      `see <ref_snippet lines="710" file="/tmp/services/process.ts" /> here`,
    ),
    "see [process.ts:710](/tmp/services/process.ts:710) here",
  );
});

test("rewrites Devin file tags to basename markdown links", () => {
  assert.equal(
    rewriteDevinFileRefs(
      `Here's the configuration file: <ref_file file="/home/ubuntu/repos/project/config.json" />`,
    ),
    "Here's the configuration file: [config.json](/home/ubuntu/repos/project/config.json)",
  );
  assert.equal(
    rewriteDevinFileRefs(`<ref_file file='/tmp/notes.md' />`),
    "[notes.md](/tmp/notes.md)",
  );
});

test("leaves malformed or non-local Devin ref tags unchanged", () => {
  assert.equal(
    rewriteDevinFileRefs(`<ref_snippet file="${snippetFile}" />`),
    `<ref_snippet file="${snippetFile}" />`,
  );
  assert.equal(
    rewriteDevinFileRefs(`<ref_snippet file="${snippetFile}" lines="start-end" />`),
    `<ref_snippet file="${snippetFile}" lines="start-end" />`,
  );
  assert.equal(
    rewriteDevinFileRefs(`<ref_file file="relative/config.json" />`),
    `<ref_file file="relative/config.json" />`,
  );
  assert.equal(rewriteDevinFileRefs("no tags here"), "no tags here");
});
