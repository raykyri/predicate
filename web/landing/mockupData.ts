// The content the landing-page mockup renders: one session of a Codex agent
// working on Porffor, the ahead-of-time JavaScript engine, plus a sidebar of
// other open-source projects. Kept as data so the markup stays structural and
// the copy can be refreshed without touching layout.
//
// Everything here is either real or shaped exactly like the real thing: the
// repositories exist, the file paths and commands are the ones those projects
// use, and the JavaScript semantics the agent reasons about are the spec's. What
// the agent says is a log of its own work — it makes no claim about the state of
// anyone's codebase, and quotes nobody.

export type TerminalTone =
  | "strong"
  | "command"
  | "argument"
  | "string"
  | "path"
  | "dim"
  | "text";

export interface TerminalSpan {
  text: string;
  tone?: TerminalTone;
}

export interface TerminalLine {
  spans: TerminalSpan[];
}

export interface TerminalBlock {
  // Which replay step reveals this block. Steps are shared with the transcript,
  // so a block streams in exactly when the agent turn that ran it appears.
  step: number;
  lines: TerminalLine[];
}

const s = (text: string, tone?: TerminalTone): TerminalSpan => ({ text, tone });

export const TERMINAL_BLOCKS: TerminalBlock[] = [
  {
    step: 2,
    lines: [
      { spans: [s("• ", "dim"), s("Explored", "strong")] },
      { spans: [s("  └ ", "dim"), s("Read ", "command"), s("compiler/builtins/string.ts", "path")] },
      { spans: [s("    ", "dim"), s("Read ", "command"), s("compiler/codegen.js", "path")] },
      { spans: [s("    ", "dim"), s("Read ", "command"), s("compiler/builtins_precompiled.js", "path")] },
      {
        spans: [
          s("    ", "dim"),
          s("Search ", "command"),
          s("replaceAll|__String_prototype_replace", "argument"),
          s(" in "),
          s("compiler", "path"),
        ],
      },
    ],
  },
  {
    step: 2,
    lines: [
      {
        spans: [
          s("• ", "dim"),
          s("Ran ", "strong"),
          s("rg -n ", "command"),
          s("'replaceAll|__String_prototype_replace'", "string"),
        ],
      },
      { spans: [s("    "), s("compiler/builtins/string.ts", "path")] },
      {
        spans: [
          s("  └ ", "dim"),
          s("1188:", "dim"),
          s("export const __String_prototype_replace = (_this: string,"),
        ],
      },
      { spans: [s("    1246:", "dim"), s("  // searchValue is a string here, not a regex")] },
      { spans: [s("    … +7 lines (ctrl + t to view transcript)", "dim")] },
      {
        spans: [
          s("    1301:", "dim"),
          s("export const __String_prototype_replaceAll = (_this: string,"),
        ],
      },
      { spans: [s("    1338:", "dim"), s("    if (Porffor.wasm.i32.eq(searchLen, 0)) {")] },
    ],
  },
  {
    step: 4,
    lines: [
      {
        spans: [
          s("• ", "dim"),
          s("Ran ", "strong"),
          s("node ", "command"),
          s("runner/index.js", "path"),
          s(" test/replaceall.js ", "argument"),
          s("--valtype=i32", "argument"),
          s(" &&"),
        ],
      },
      {
        spans: [
          s("    "),
          s("node ", "command"),
          s("runner/index.js ", "path"),
          s("-e", "argument"),
          s(" \"console.log('a-b-c'.replaceAll('-', '+'))\"", "string"),
        ],
      },
      { spans: [s("  └ ", "dim"), s("compiled 47 funcs in 91ms, 14.2kb wasm", "dim")] },
      { spans: [s("    a+b+c")] },
      { spans: [s("    … +12 lines (ctrl + t to view transcript)", "dim")] },
      { spans: [s("    a-b-c")] },
      { spans: [s("    ✗ empty search: timed out after 10000ms (replaceall.js:14)")] },
    ],
  },
  {
    step: 6,
    lines: [
      {
        spans: [
          s("• ", "dim"),
          s("Ran ", "strong"),
          s("node ", "command"),
          s("test262/index.js", "path"),
          s(" test/built-ins/String/prototype/replaceAll", "argument"),
        ],
      },
      { spans: [s("  └ ", "dim"), s("62/64 (96.9%) — 2 fail, 0 timeout, 0 skip")] },
      { spans: [s("    ✗ searchValue-regexp-not-global.js", "dim")] },
      { spans: [s("    ✗ searchValue-empty-string.js", "dim")] },
      { spans: [s("    … +9 lines (ctrl + t to view transcript)", "dim")] },
      { spans: [s("    TypeError: replaceAll must be called with a global RegExp")] },
    ],
  },
  {
    step: 6,
    lines: [
      {
        spans: [
          s("• ", "dim"),
          s("Ran ", "strong"),
          s("git diff ", "command"),
          s("--stat", "argument"),
          s(" && "),
          s("git diff ", "command"),
          s("-- compiler/builtins/string.ts", "argument"),
          s(" |"),
        ],
      },
      {
        spans: [
          s("    "),
          s("sed -n ", "command"),
          s("'1,80p'", "string"),
        ],
      },
      { spans: [s("  └ ", "dim"), s(" compiler/builtins/string.ts | 46 ++++++++++++++++++-------")] },
      { spans: [s("     1 file changed, 31 insertions(+), 15 deletions(-)")] },
      { spans: [s("    +  if (Porffor.wasm.i32.eq(searchLen, 0)) {")] },
      { spans: [s("    +    // an empty search advances one code unit, or we never")] },
      { spans: [s("    … +52 lines (ctrl + t to view transcript)", "dim")] },
    ],
  },
  {
    step: 6,
    lines: [
      {
        spans: [
          s("• ", "dim"),
          s("Ran ", "strong"),
          s("node ", "command"),
          s("test262/index.js", "path"),
          s(" test/built-ins/String/prototype", "argument"),
          s("; "),
          s("node", "command"),
        ],
      },
    ],
  },
];

export interface MockGroup {
  name: string;
  // Whether the group starts collapsed. Every group carries its panes so the
  // enhanced sidebar can expand one without fetching anything.
  collapsed: boolean;
  // Status dots shown beside a collapsed group's name for agents still working.
  statuses?: ("active" | "attention")[];
  panes: MockPane[];
}

export interface MockPane {
  title: string;
  status: "idle" | "active" | "attention" | "done";
  selected?: boolean;
  badge?: string;
}

// Real open-source projects, with task titles of the shape qmux generates from
// an agent's first turn. Nothing here claims anything about the state of these
// codebases: the titles describe work an agent is doing, and the file paths and
// commands are the ones those repositories actually use.
export const MOCK_GROUPS: MockGroup[] = [
  {
    name: "porffor",
    collapsed: false,
    panes: [
      {
        title:
          "replaceAll: empty search advances a code unit, RegExp guard before the loop, test262 built-ins/String",
        status: "idle",
        selected: true,
      },
      { title: "codegen: fold repeated i32.const before emit", status: "done" },
      { title: "builtins: Math.hypot without the intermediate array", status: "idle" },
    ],
  },
  {
    name: "pi",
    collapsed: true,
    statuses: ["active"],
    panes: [
      {
        title: "pi-ai: provider adapter for an OpenAI-compatible endpoint",
        status: "active",
        badge: "2 queued",
      },
      { title: "coding-agent: /fork a session from a mid-run turn", status: "idle" },
      { title: "pi-tui: differential render across a resize", status: "idle" },
      { title: "pi-skills: wrap youtube-transcript as a skill", status: "done" },
    ],
  },
  {
    name: "autoresearch",
    collapsed: true,
    statuses: ["active"],
    panes: [
      { title: "program.md: a direction for the Muon momentum schedule", status: "active" },
      { title: "train.py: 5-minute budget, keep the run only if val_bpb drops", status: "idle" },
      { title: "analysis.ipynb: plot accepted against discarded runs", status: "idle" },
    ],
  },
  {
    name: "parameter-golf",
    collapsed: true,
    panes: [
      { title: "train_gpt.py: under 16,000,000 bytes with the sp1024 vocab", status: "idle" },
      { title: "submission.json: count code bytes plus compressed weights", status: "done" },
    ],
  },
  {
    name: "modded-nanogpt",
    collapsed: true,
    panes: [{ title: "speedrun: 3.28 FineWeb val loss in fewer steps", status: "idle" }],
  },
  {
    name: "workerd",
    collapsed: true,
    statuses: ["active"],
    panes: [
      {
        title: "actor-state: alarm retry backoff, bazel test //src/workerd/api",
        status: "active",
        badge: "3 queued",
      },
      { title: "jsg: finalizer order for resource types", status: "idle" },
      { title: "server: config schema round-trip test", status: "done" },
    ],
  },
  {
    name: "nanochat",
    collapsed: true,
    panes: [
      { title: "tokenizer: compare rust bpe merges against tiktoken", status: "active" },
      { title: "eval: mid-training checkpoint sweep", status: "idle" },
    ],
  },
  {
    name: "lm-evaluation-harness",
    collapsed: true,
    panes: [
      { title: "task yaml for a held-out multiple-choice set", status: "idle" },
      { title: "fewshot sampling: make the seed actually deterministic", status: "done" },
    ],
  },
  {
    name: "llm.c",
    collapsed: true,
    panes: [{ title: "cuda: fuse layernorm backward into the residual add", status: "idle" }],
  },
  {
    name: "tinygrad",
    collapsed: true,
    panes: [
      { title: "shapetracker: merge adjacent views on reshape", status: "idle" },
      { title: "metal: fewer kernel launches in conv2d", status: "done" },
    ],
  },
  {
    name: "minbpe",
    collapsed: true,
    panes: [{ title: "regex split: contractions the GPT-4 pattern misses", status: "done" }],
  },
  {
    name: "sol-execbench",
    collapsed: true,
    panes: [{ title: "kernel: fused layernorm, scored on a B200", status: "idle" }],
  },
  {
    name: "kryptos-ctf",
    collapsed: true,
    panes: [
      { title: "K1–K3: re-derive the keys before going near K4", status: "done" },
      { title: "solver: score candidate keyword lengths against the crib", status: "idle" },
    ],
  },
  {
    name: "paradigm-ctf",
    collapsed: true,
    panes: [
      { title: "forge test: stand the challenge up against a local fork", status: "idle" },
      { title: "writeup: the one-transaction path, with the trace", status: "idle" },
    ],
  },
  {
    name: "ripgrep",
    collapsed: true,
    panes: [{ title: "bench: -uu against a linux checkout, three runs", status: "idle" }],
  },
  {
    name: "wiki",
    collapsed: true,
    panes: [
      { title: "per-file summaries for compiler/, linked from the index", status: "active" },
      { title: "backfill glossary entries for the wasm terms", status: "idle" },
    ],
  },
];

export interface TranscriptParagraph {
  type: "paragraph";
  step: number;
  // Inline runs; `code: true` renders the transcript's inline-code style.
  runs: { text: string; code?: boolean }[];
}

export interface TranscriptActivity {
  type: "activity";
  step: number;
  label: string;
}

export type TranscriptItem = TranscriptParagraph | TranscriptActivity;

const p = (
  step: number,
  ...runs: (string | { text: string; code?: boolean })[]
): TranscriptParagraph => ({
  type: "paragraph",
  step,
  runs: runs.map((run) => (typeof run === "string" ? { text: run } : run)),
});

const activity = (step: number, label: string): TranscriptActivity => ({
  type: "activity",
  step,
  label,
});

export const AGENT_TURN: TranscriptItem[] = [
  p(
    1,
    "I'll read the existing ",
    { text: "replace", code: true },
    " implementation first, since ",
    { text: "replaceAll", code: true },
    " shares its search loop, then run just the replaceAll directory of test262 to see exactly what fails.",
  ),
  activity(2, "Called exec"),
  p(
    3,
    "Two failures. An empty search string has to splice the replacement between every code unit, and the loop never advances when the match length is zero. A ",
    { text: "RegExp", code: true },
    " search value has to throw a ",
    { text: "TypeError", code: true },
    " unless it carries the ",
    { text: "g", code: true },
    " flag.",
  ),
  activity(4, "Called 2 tools"),
  p(
    5,
    "I'm advancing the cursor by one code unit when the match length is zero, and putting the ",
    { text: "TypeError", code: true },
    " guard before the loop rather than inside it, so the throw happens before anything is allocated.",
  ),
  activity(6, "Called 14 tools"),
];

// The user's prompt is step 0; the transcript and terminal share one step
// timeline so the replay can interleave them.
export const USER_TURN = {
  tags: ["<INSTRUCTIONS>", "<environment_context>"],
  text: "get replaceAll passing the two test262 cases we still fail",
};

// The prompt library's saved prompts, the artifacts this pane's agent opened,
// and the address the browser overlay is previewing.
export const SAVED_PROMPTS = [
  "Review the diff for correctness bugs only. Skip style.",
  "Read the existing implementation first, and say what it already handles.",
  "Write the failing test first, then make it pass.",
  "Summarise what changed and what is still unmerged.",
  "Run the failing directory only, and report each failure with the command to reproduce it.",
];

export interface MockArtifact {
  name: string;
  meta: string;
}

export const ARTIFACTS: MockArtifact[] = [
  { name: "test262-report.html", meta: "html" },
  { name: "compiler/builtins/string.ts", meta: "31k" },
  { name: "test262-replaceall.log", meta: "log" },
  { name: "replaceall.patch", meta: "patch" },
  { name: "compiler/codegen.js", meta: "js" },
  { name: "out.wasm", meta: "14.2k" },
  { name: "wasm-disas.txt", meta: "txt" },
  { name: "bench-startup.json", meta: "json" },
  { name: "flamegraph.svg", meta: "svg" },
  { name: "builtins_precompiled.js", meta: "js" },
  { name: "notes-replaceall.md", meta: "md" },
];

export const BROWSER_URL = "localhost:8080/test262-report.html";

export const SESSION_LABEL = "Session: 019ff405-b4fe-70d2-9…";
export const COMPOSER_PLACEHOLDER = "What should we investigate next?";
export const ARTIFACT_COUNT = 11;
