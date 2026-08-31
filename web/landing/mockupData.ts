// The content the landing-page mockup renders: a complete qmux session drawn
// from this page's own iteration, plus sessions for the open-source projects in
// its sidebar. Kept as data so
// the markup stays structural and the copy can be refreshed without touching
// layout.
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

const explored = (step: number, ...entries: string[]): TerminalBlock => ({
  step,
  lines: [
    { spans: [s("• ", "dim"), s("Explored", "strong")] },
    ...entries.map((entry, index) => ({
      spans: [
        s(index === 0 ? "  └ " : "    ", "dim"),
        s(entry.startsWith("Search ") ? entry : `Read ${entry}`, "command"),
      ],
    })),
  ],
});

const ran = (step: number, command: string, ...output: string[]): TerminalBlock => ({
  step,
  lines: [
    { spans: [s("• ", "dim"), s("Ran ", "strong"), s(command, "command")] },
    ...output.map((line, index) => ({
      spans: [s(index === 0 ? "  └ " : "    ", "dim"), s(line)],
    })),
  ],
});

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
  sessionId: string;
  title: string;
  status: "idle" | "active" | "attention" | "done";
  selected?: boolean;
  badge?: string;
}

// Real projects, with task titles of the shape qmux generates from an agent's
// first turn. Nothing here claims anything about the state of other codebases:
// the titles describe work an agent is doing, and the file paths and commands
// are the ones those repositories actually use.
export const MOCK_GROUPS: MockGroup[] = [
  {
    name: "qmux",
    collapsed: false,
    panes: [
      {
        sessionId: "qmux-landing-transcript",
        title: "landing demo: default qmux session, inline image, and a longer live replay",
        status: "active",
        selected: true,
      },
    ],
  },
  {
    name: "porffor",
    collapsed: false,
    panes: [
      {
        sessionId: "porffor-replace-all",
        title:
          "replaceAll: empty search advances a code unit, RegExp guard before the loop, test262 built-ins/String",
        status: "idle",
      },
      {
        sessionId: "porffor-codegen-casts",
        title: "render: verify unaligned i16 loads sign-extend before widening",
        status: "done",
      },
      {
        sessionId: "porffor-math-hypot",
        title: "builtins: verify Math.hypot abrupt coercion in native output",
        status: "idle",
      },
    ],
  },
  {
    name: "autoresearch",
    collapsed: true,
    statuses: ["active"],
    panes: [
      {
        sessionId: "autoresearch-muon-ramp",
        title: "train.py: tune the Muon momentum ramp in the five-minute budget",
        status: "active",
      },
      {
        sessionId: "autoresearch-qk-norm",
        title: "train.py: test QK norm, keep only if val_bpb drops",
        status: "idle",
      },
      {
        sessionId: "autoresearch-results",
        title: "results.tsv: rank kept experiments by val_bpb",
        status: "idle",
      },
    ],
  },
  {
    name: "nanochat",
    collapsed: true,
    panes: [
      {
        sessionId: "nanochat-tokenizer",
        title: "tokenizer: round-trip special tokens and conversation masks",
        status: "active",
      },
      {
        sessionId: "nanochat-task-mixture",
        title: "eval: verify deterministic TaskMixture shuffle and oversampling",
        status: "idle",
      },
    ],
  },
  {
    name: "llm.c",
    collapsed: true,
    panes: [
      {
        sessionId: "llmc-layernorm",
        title: "cuda: validate vectorized layernorm backward kernel 10",
        status: "idle",
      },
    ],
  },
  {
    name: "tinygrad",
    collapsed: true,
    panes: [
      {
        sessionId: "tinygrad-movement",
        title: "uop: cover adjacent reshape cleanup in the movement matcher",
        status: "idle",
      },
      {
        sessionId: "tinygrad-metal",
        title: "metal: cover invalid and corrupt shader-library paths",
        status: "done",
      },
    ],
  },
  {
    name: "wiki",
    collapsed: true,
    panes: [
      {
        sessionId: "wiki-compiler-index",
        title: "per-file summaries for compiler/, linked from the index",
        status: "active",
      },
      {
        sessionId: "wiki-glossary",
        title: "glossary: define AOT, IR, and native runtime terms",
        status: "idle",
      },
    ],
  },
];

// Card and rail shapes for the terminal map (see MOCK_HOME_RAILS below).
export interface MockHomePastTurn {
  text: string;
  /** Rendered after the receipt check: "3h ago". */
  receipt: string;
}

export interface MockHomeCurrentTurn {
  text: string;
  tone: "active" | "done";
  /** "working · 4m" while running, "Done · 2h" once settled. */
  receipt: string;
}

export interface MockHomeRail {
  past?: MockHomePastTurn[];
  current: MockHomeCurrentTurn;
  queued?: string[];
  paused?: boolean;
}

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

export interface TranscriptImage {
  type: "image";
  step: number;
  src: string;
  alt: string;
}

export interface TranscriptVisualization {
  type: "visualization";
  step: number;
  title: string;
  file: string;
}

export type TranscriptItem =
  | TranscriptParagraph
  | TranscriptActivity
  | TranscriptImage
  | TranscriptVisualization;

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

const image = (
  step: number,
  src: string,
  alt: string,
): TranscriptImage => ({
  type: "image",
  step,
  src,
  alt,
});

const visualization = (
  step: number,
  title: string,
  file: string,
): TranscriptVisualization => ({
  type: "visualization",
  step,
  title,
  file,
});

export interface MockUserTurn {
  tags: string[];
  text: string;
}

export interface MockSession {
  project: string;
  userTurn: MockUserTurn;
  agentTurn: TranscriptItem[];
  terminalBlocks: TerminalBlock[];
}

const user = (text: string): MockUserTurn => ({
  tags: ["<INSTRUCTIONS>", "<environment_context>"],
  text,
});

// Each sidebar tab is a complete, independent agent session. Commands and file
// paths follow the corresponding repositories' current documented workflows;
// the numbers are deliberately mundane rather than marketing-perfect.
export const MOCK_SESSIONS: Record<string, MockSession> = {
  "qmux-landing-transcript": {
    project: "qmux",
    userTurn: user(
      "add a qmux group above porffor, have that open by default, draw the transcript from this conversation, and make sure a visible image embed renders in the right pane",
    ),
    agentTurn: [
      p(
        1,
        "I'll make qmux the default project, excerpt this landing-page iteration into a real session, and add an inline image item to the mock transcript renderer. The active replay will start with existing output and run long enough to visibly scroll.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The current transcript union only handles paragraphs and activity summaries. I'll add a typed image block, render the bundled qmux capture with fixed dimensions, and keep Porffor available as a collapsed secondary group.",
      ),
      activity(4, "Called 5 tools"),
      p(
        5,
        "The new qmux session is now the server-rendered default. Its first completed inspection is visible immediately, while later edits, checks, and the image arrive through the working-session replay.",
      ),
      activity(6, "Called 5 tools"),
      p(
        7,
        "The rendered document now contains fourteen terminal sessions, one inline transcript image, and an active qmux timeline. Completed sessions still bypass replay and show their full history immediately.",
      ),
      activity(8, "Called 2 tools"),
      p(
        9,
        "The capture is a bundled asset rather than a temporary clipboard path, so it remains visible in production and obeys the landing page's self-only image policy.",
      ),
      activity(10, "Called 2 tools"),
      p(
        11,
        "Everything is passing. The static capture remains embedded, and the interactive design reference now renders as a native attachment that opens in qmux's browser:",
      ),
      image(12, "/qmux.png", "The qmux desktop interface"),
      visualization(
        12,
        "Recent activity design",
        "recent-activity-design.fragment.html",
      ),
    ],
    terminalBlocks: [
      explored(
        2,
        "web/landing/mockupData.ts",
        "web/landing/AppMockup.tsx",
        "web/landing/mockupCss.ts",
      ),
      ran(
        2,
        "rg -n 'DEFAULT_SESSION_ID|TranscriptItem|MOCK_GROUPS' web/landing/mockupData.ts web/landing/AppMockup.tsx",
        "web/landing/mockupData.ts:84:export const MOCK_GROUPS: MockGroup[] = [",
        "web/landing/mockupData.ts:218:export type TranscriptItem = TranscriptParagraph | TranscriptActivity;",
        "web/landing/mockupData.ts:1140:export const DEFAULT_SESSION_ID = 'porffor-replace-all';",
      ),
      ran(
        2,
        "sips -g pixelWidth -g pixelHeight site/qmux.png",
        "site/qmux.png",
        "  pixelWidth: 2704",
        "  pixelHeight: 1704",
      ),
      ran(
        4,
        "rg -n 'turn-image|img-src' src/styles/features/transcript.css web/landing/mockupCss.ts web/server.tsx",
        "src/styles/features/transcript.css:736:.turn-image {",
        "web/server.tsx:332:      \"img-src 'self'\",",
      ),
      ran(
        4,
        "git diff --stat -- web/landing",
        "web/landing/AppMockup.tsx | 24 +++++++++++++++++++++---",
        "web/landing/mockupCss.ts  | 31 +++++++++++++++++++++++++++++++",
        "web/landing/mockupData.ts | 96 +++++++++++++++++++++++++++++++++++++++++++++++",
      ),
      ran(
        4,
        "node --import tsx -e \"import {MOCK_GROUPS,DEFAULT_SESSION_ID} from './web/landing/mockupData.ts'; console.log(MOCK_GROUPS[0].name, DEFAULT_SESSION_ID)\"",
        "qmux qmux-landing-transcript",
      ),
      ran(
        4,
        "npm run build:site:server",
        "> qmux@0.3.0 build:site:server",
        "> tsc -p web/tsconfig.json && esbuild web/server.tsx --bundle --packages=external --platform=node --format=esm --target=node20 --outfile=dist-site/server.mjs",
        "dist-site/server.mjs  267.4kb",
        "Done in 7ms",
      ),
      ran(6, "node --check site/mockup.js"),
      ran(
        6,
        "node --import tsx -e \"import {MOCK_GROUPS,MOCK_SESSIONS} from './web/landing/mockupData.ts'; console.log(MOCK_GROUPS.flatMap(g=>g.panes).length, Object.keys(MOCK_SESSIONS).length)\"",
        "14 14",
      ),
      ran(
        6,
        "node --import tsx --test web/server.test.ts",
        "# tests 19",
        "# pass 19",
        "# fail 0",
        "# duration_ms 846.3",
      ),
      ran(6, "git diff --check"),
      ran(
        8,
        "node --import tsx -e \"import {renderLandingPage} from './web/landing/LandingPage.tsx'; const h=renderLandingPage('https://qmux.app'); console.log(/class=\\\"turn-image\\\"/.test(h), /src=\\\"\\/qmux.png\\\"/.test(h))\"",
        "true true",
      ),
      ran(
        8,
        "sips -g format -g pixelWidth -g pixelHeight site/qmux.png",
        "site/qmux.png",
        "  format: png",
        "  pixelWidth: 2704",
        "  pixelHeight: 1704",
      ),
      ran(
        10,
        "npm run build:site:server",
        "> qmux@0.3.0 build:site:server",
        "> tsc -p web/tsconfig.json && esbuild web/server.tsx --bundle --packages=external --platform=node --format=esm --target=node20 --outfile=dist-site/server.mjs",
        "dist-site/server.mjs  267.4kb",
        "Done in 6ms",
      ),
      ran(
        10,
        "git diff --check && git status --short",
        " M site/mockup.js",
        " M web/landing/AppMockup.tsx",
        " M web/landing/mockupCss.ts",
        " M web/landing/mockupData.ts",
        " M web/server.test.ts",
      ),
    ],
  },
  "porffor-replace-all": {
    project: "porffor",
    userTurn: user("get replaceAll passing the two test262 cases we still fail"),
    agentTurn: [
      p(
        1,
        "I'll read the string builtin and run only the 45-test replaceAll directory first, then reproduce the empty-search behavior through Porffor's native compiler.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The two failures are independent: the loop reuses the zero match length as its cursor increment, and the non-global RegExp guard runs too late. I'll split match width from advancement and move the ",
        { text: "RegExp", code: true },
        " check ahead of allocation.",
      ),
      activity(4, "Called 5 tools"),
      p(
        5,
        "All 45 replaceAll tests now pass, and the native Porffor smoke program prints the leading, interstitial, and trailing replacements. The change is confined to ",
        { text: "compiler/builtins/string.ts", code: true },
        ".",
      ),
    ],
    terminalBlocks: [
      explored(
        2,
        "compiler/builtins/string.ts",
        "test262/index.js",
        "Search replaceAll|replace in compiler/builtins/string.ts",
      ),
      ran(
        2,
        "node test262/index.js built-ins/String/prototype/replaceAll --plain-results --threads=1",
        "Using local test262 checkout: test262/",
        "Running built-ins/String/prototype/replaceAll (45 tests, 1 thread)",
        "total: 45 | pass: 43 | fail: 2 | runtime error: 0 | timeout: 0",
        "fail: built-ins/String/prototype/replaceAll/searchValue-empty-string.js",
        "fail: built-ins/String/prototype/replaceAll/searchValue-flags-no-g-throws.js",
      ),
      ran(
        2,
        "rg -n 'replaceAll|non-global RegExp' compiler/builtins/string.ts",
        "1492:export const __Porffor_string_replaceAll = (str: any, searchValue: any, replaceValue: any) => {",
        "1497:      throw new TypeError('String.prototype.replaceAll called with a non-global RegExp argument');",
        "1537:export const __String_prototype_replaceAll = function (this: string, searchValue: any, replaceValue: any) {",
      ),
      ran(
        4,
        "git diff --stat -- compiler/builtins/string.ts",
        "compiler/builtins/string.ts | 9 ++++++---",
        "1 file changed, 6 insertions(+), 3 deletions(-)",
      ),
      ran(
        4,
        "./porf /tmp/replaceall-smoke.js -o /tmp/replaceall-smoke && /tmp/replaceall-smoke",
        "[108ms] compiled /tmp/replaceall-smoke.js -> /tmp/replaceall-smoke (34.0KB)",
        "-a-b-",
        "a+b+c",
        "caught TypeError: String.prototype.replaceAll called with a non-global RegExp argument",
      ),
      ran(
        4,
        "node test262/index.js built-ins/String/prototype/replaceAll --plain-results --threads=1",
        "Using local test262 checkout: test262/",
        "Running built-ins/String/prototype/replaceAll (45 tests, 1 thread)",
        "total: 45 | pass: 45 | fail: 0 | runtime error: 0 | timeout: 0",
      ),
      ran(4, "git diff --check && git status --short", " M compiler/builtins/string.ts"),
    ],
  },
  "porffor-codegen-casts": {
    project: "porffor",
    userTurn: user("verify sign extension for unaligned i16 loads in emitted C"),
    agentTurn: [
      p(
        1,
        "I'll trace the unaligned load case in the C renderer, then compile a DataView smoke program with Porffor so the emitted native path is exercised directly.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The renderer loads an unaligned signed value through its unsigned width, then casts back to the narrow signed type before C widens it. I'll verify that exact expression in emitted C and native output.",
      ),
      activity(4, "Called 5 tools"),
      p(
        5,
        "The emitted C contains the narrow signed cast, and the Porffor-compiled program reads -2 while the unsigned control remains 65534. No renderer change is needed.",
      ),
    ],
    terminalBlocks: [
      explored(2, "compiler/render.js", "compiler/ir.js", "compiler/builtins/dataview.ts"),
      ran(
        2,
        "rg -n 'signed unaligned|load unsigned width' compiler/render.js",
        "488:          // signed unaligned: load unsigned width, cast (u8/i8 are always aligned)",
      ),
      ran(
        2,
        "sed -n '484,493p' compiler/render.js",
        "const ctype = node[N_A];",
        "const [off, unaligned] = node[N_C];",
        "const addr = `MEM + ${rx(node[N_B], P_ADD)}${off ? ` + ${off}u` : ''}`;",
        "if (unaligned) {",
        "  // signed unaligned: load unsigned width, cast (u8/i8 are always aligned)",
        "  if (ctype === 'i16') return [`(int16_t)porf_load_un_u16(${addr})`, P_CAST];",
        "}",
      ),
      ran(
        4,
        "./porf c /tmp/unaligned-i16.js -o /tmp/unaligned-i16.c",
      ),
      ran(
        4,
        "rg -n 'int16_t|porf_load_un_u16' /tmp/unaligned-i16.c | tail -n 2",
        "1848: int32_t value = (int16_t)porf_load_un_u16(MEM + offset);",
        "1852: int32_t aligned = *(int16_t *)(MEM + aligned_offset);",
      ),
      ran(
        4,
        "./porf /tmp/unaligned-i16.js -o /tmp/unaligned-i16 && /tmp/unaligned-i16",
        "[101ms] compiled /tmp/unaligned-i16.js -> /tmp/unaligned-i16 (34.2KB)",
        "unaligned i16 le: -2",
        "aligned i16 le:   -2",
        "unaligned u16 le: 65534",
      ),
      ran(4, "node --check compiler/render.js"),
      ran(
        4,
        "git diff --quiet -- compiler/render.js; echo $?",
        "0",
      ),
    ],
  },
  "porffor-math-hypot": {
    project: "porffor",
    userTurn: user("verify Math.hypot stops coercing after an argument throws"),
    agentTurn: [
      p(
        1,
        "I'll inspect the builtin's argument loop and run the single test262 abrupt-coercion case before adding a native Porffor smoke program that records coercion order.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The implementation calls ",
        { text: "ToNumber", code: true },
        " inside its left-to-right loop, so an abrupt completion should prevent later arguments from being observed. The focused conformance test confirms that path.",
      ),
      activity(4, "Called 4 tools"),
      p(
        5,
        "The native smoke program logs only the first and throwing arguments, then catches the sentinel error. No compiler change is needed; I recorded the verified behavior instead of manufacturing a patch.",
      ),
    ],
    terminalBlocks: [
      explored(2, "compiler/builtins/math.ts", "test262/index.js"),
      ran(
        2,
        "node test262/index.js built-ins/Math/hypot/Math.hypot_ToNumberErr.js --plain-results --threads=1",
        "Using local test262 checkout: test262/",
        "Running built-ins/Math/hypot/Math.hypot_ToNumberErr.js (1 test, 1 thread)",
        "total: 1 | pass: 1 | fail: 0 | runtime error: 0 | timeout: 0",
      ),
      ran(
        2,
        "rg -n 'export const __Math_hypot|ToNumber' compiler/builtins/math.ts | head",
        "3:export const __Math_hypot = (...args: any[]): number => {",
        "8:    const n: number = ecma262.ToNumber(args[i]);",
      ),
      ran(
        2,
        "sed -n '3,13p' compiler/builtins/math.ts",
        "export const __Math_hypot = (...args: any[]): number => {",
        "  const len: i32 = args.length;",
        "  let out: number = 0;",
        "  for (let i: i32 = 0; i < len; i++) {",
        "    const n: number = ecma262.ToNumber(args[i]);",
        "    Porffor.c`out = hypot(out, n);`;",
        "  }",
        "  return out;",
        "};",
      ),
      ran(
        4,
        "./porf /tmp/hypot-order.js -o /tmp/hypot-order && /tmp/hypot-order",
        "[104ms] compiled /tmp/hypot-order.js -> /tmp/hypot-order (34.6KB)",
        "valueOf first",
        "valueOf throws",
        "caught sentinel",
        "coercions: first,throws",
      ),
      ran(
        4,
        "node /tmp/hypot-order.js",
        "valueOf first",
        "valueOf throws",
        "caught sentinel",
        "coercions: first,throws",
      ),
      ran(4, "git diff --quiet -- compiler/builtins/math.ts; echo $?", "0"),
      ran(4, "git status --short --branch", "## main"),
    ],
  },
  "autoresearch-muon-ramp": {
    project: "autoresearch",
    userTurn: user("try a slower Muon momentum ramp; keep it only if val_bpb improves"),
    agentTurn: [
      p(
        1,
        "I'll confirm the experiment rules and current baseline, then change only the Muon momentum schedule in ",
        { text: "train.py", code: true },
        ".",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The baseline is 0.997900 val_bpb at 44.0 GB. I'll extend the ramp without changing batch size or the five-minute budget so the comparison stays attributable.",
      ),
      activity(4, "Called 8 tools"),
      p(
        5,
        "The candidate reaches 0.996841 at essentially unchanged memory, so it clears the keep rule. I've committed the change and recorded the measured result; I'm checking the results row before wrapping up.",
      ),
    ],
    terminalBlocks: [
      explored(2, "program.md", "train.py", "results.tsv"),
      ran(
        2,
        "column -ts $'\\t' results.tsv | tail -n 4",
        "commit  val_bpb  memory_gb  status   description",
        "4e92ac1 0.997900 44.0       keep     baseline",
        "c71fd04 1.001337 44.0       discard  constant Muon momentum",
        "89b07bb 0.998612 44.0       discard  faster Muon ramp",
      ),
      ran(
        2,
        "rg -n 'momentum|TIME_BUDGET' train.py program.md",
        "program.md:31:Each experiment runs on a single GPU. The training script runs for a fixed time budget of 5 minutes",
        "train.py:527:def get_muon_momentum(step):",
        "train.py:528:    frac = min(step / 300, 1)",
        "train.py:529:    return (1 - frac) * 0.85 + frac * 0.95",
      ),
      ran(
        4,
        "git diff --stat && git diff --check",
        "train.py | 6 ++++--",
        "1 file changed, 4 insertions(+), 2 deletions(-)",
      ),
      ran(
        4,
        "git add train.py && git commit -m 'slower Muon momentum ramp'",
        "[autoresearch/aug12 8d13e7a] slower Muon momentum ramp",
        " 1 file changed, 4 insertions(+), 2 deletions(-)",
      ),
      ran(
        4,
        "uv run train.py > run.log 2>&1; grep -E '^val_bpb:|^peak_vram_mb:' run.log",
        "val_bpb:          0.996841",
        "peak_vram_mb:     45121.6",
      ),
      ran(
        4,
        "grep -E '^training_seconds:|^total_tokens_M:|^val_bpb:' run.log",
        "val_bpb:          0.996841",
        "training_seconds: 300.1",
        "total_tokens_M:   499.6",
      ),
      ran(
        4,
        "printf '8d13e7a\\t0.996841\\t44.1\\tkeep\\tslower Muon momentum ramp\\n' >> results.tsv",
      ),
      ran(
        4,
        "tail -n 2 results.tsv",
        "89b07bb\t0.998612\t44.0\tdiscard\tfaster Muon ramp",
        "8d13e7a\t0.996841\t44.1\tkeep\tslower Muon momentum ramp",
      ),
      ran(
        4,
        "git status --short",
        "?? results.tsv",
      ),
    ],
  },
  "autoresearch-qk-norm": {
    project: "autoresearch",
    userTurn: user("test QK normalization and discard it if the fixed-budget metric gets worse"),
    agentTurn: [
      p(
        1,
        "I'll branch from the current kept result, add QK normalization in the attention block, and judge it only on the final validation bits per byte.",
      ),
      activity(2, "Called 3 tools"),
      p(
        3,
        "The implementation is small and the run is healthy, but the experiment still has to beat 0.996841. I'll let the fixed timer and validation finish before deciding.",
      ),
      activity(4, "Called 9 tools"),
      p(
        5,
        "It regressed to 0.999284 with no compensating memory benefit. I logged the discard, reset the branch to the prior kept commit, and left the untracked results log intact.",
      ),
    ],
    terminalBlocks: [
      ran(2, "git status --short --branch", "## autoresearch/aug12", "?? results.tsv"),
      ran(
        2,
        "git log -3 --oneline",
        "8d13e7a slower Muon momentum ramp",
        "4e92ac1 baseline",
        "9276d1b initial setup",
      ),
      ran(
        2,
        "rg -n 'class CausalSelfAttention|self.c_q|self.c_k' train.py",
        "61:class CausalSelfAttention(nn.Module):",
        "69:        self.c_q = nn.Linear(self.n_embd, self.n_head * self.head_dim, bias=False)",
        "80:        q = self.c_q(x).view(B, T, self.n_head, self.head_dim)",
      ),
      ran(
        4,
        "git diff --stat && git diff --check",
        "train.py | 8 ++++++--",
        "1 file changed, 6 insertions(+), 2 deletions(-)",
      ),
      ran(
        4,
        "git add train.py && git commit -m 'add QK normalization'",
        "[autoresearch/aug12 a603bc9] add QK normalization",
        " 1 file changed, 6 insertions(+), 2 deletions(-)",
      ),
      ran(
        4,
        "uv run train.py > run.log 2>&1; grep -E '^val_bpb:|^peak_vram_mb:' run.log",
        "val_bpb:          0.999284",
        "peak_vram_mb:     45118.9",
      ),
      ran(
        4,
        "grep -E '^training_seconds:|^total_tokens_M:|^val_bpb:' run.log",
        "val_bpb:          0.999284",
        "training_seconds: 300.0",
        "total_tokens_M:   499.2",
      ),
      ran(
        4,
        "printf 'a603bc9\\t0.999284\\t44.1\\tdiscard\\tadd QK normalization\\n' >> results.tsv",
      ),
      ran(
        4,
        "tail -n 2 results.tsv",
        "8d13e7a\t0.996841\t44.1\tkeep\tslower Muon momentum ramp",
        "a603bc9\t0.999284\t44.1\tdiscard\tadd QK normalization",
      ),
      ran(4, "git reset --hard 8d13e7a", "HEAD is now at 8d13e7a slower Muon momentum ramp"),
      ran(
        4,
        "git status --short --branch",
        "## autoresearch/aug12",
        "?? results.tsv",
      ),
    ],
  },
  "autoresearch-results": {
    project: "autoresearch",
    userTurn: user("summarize the kept and discarded experiments without rerunning training"),
    agentTurn: [
      p(1, "I'll treat the tab-separated log as the source of truth and check its shape before ranking results."),
      activity(2, "Called 3 tools"),
      p(
        3,
        "All rows have five fields. There are two kept candidates beyond baseline and three discarded runs; the best kept metric is 0.996841.",
      ),
      activity(4, "Called 4 tools"),
      p(
        5,
        "I added a short Markdown summary that separates measured results from hypotheses and links each entry back to its commit.",
      ),
    ],
    terminalBlocks: [
      ran(
        2,
        "awk -F '\\t' 'NF != 5 { bad=1 } END { if (!bad) print \"5 columns on every row\" }' results.tsv",
        "5 columns on every row",
      ),
      ran(
        2,
        "awk -F '\\t' 'NR>1 { count[$4]++ } END { for (s in count) print s, count[s] }' results.tsv | sort",
        "discard 3",
        "keep 3",
      ),
      ran(
        2,
        "{ head -n 1 results.tsv; tail -n +2 results.tsv | sort -t $'\\t' -k2,2n; } | column -ts $'\\t'",
        "commit val_bpb memory_gb status description",
        "8d13e7a 0.996841 44.1 keep slower Muon momentum ramp",
        "61ca08f 0.997213 44.0 keep earlier final-lr decay",
        "4e92ac1 0.997900 44.0 keep baseline",
        "89b07bb 0.998612 44.0 discard faster Muon ramp",
        "a603bc9 0.999284 44.1 discard add QK normalization",
        "c71fd04 1.001337 44.0 discard constant Muon momentum",
      ),
      ran(
        4,
        "sed -n '1,120p' experiment-summary.md",
        "# Experiment summary",
        "",
        "## Kept",
        "- `8d13e7a` — 0.996841 val_bpb — slower Muon momentum ramp",
        "- `61ca08f` — 0.997213 val_bpb — earlier final-lr decay",
        "",
        "## Discarded",
        "Three measured runs were reverted; results.tsv retains their metrics.",
      ),
      ran(
        4,
        "for c in 8d13e7a 61ca08f 4e92ac1 89b07bb a603bc9 c71fd04; do git cat-file -e \"$c^{commit}\" || exit; done",
      ),
      ran(
        4,
        "git add experiment-summary.md && git diff --cached --check && git diff --cached --stat",
        "experiment-summary.md | 24 ++++++++++++++++++++++++",
      ),
    ],
  },
  "nanochat-tokenizer": {
    project: "nanochat",
    userTurn: user("add coverage for tokenizer special tokens and conversation loss masks"),
    agentTurn: [
      p(
        1,
        "I'll read the hermetic tokenizer fixture and the conversation renderer, then add the smallest cases that distinguish supervised assistant tokens from unsupervised tool output.",
      ),
      activity(2, "Called 4 tools"),
      p(
        3,
        "The existing fixture trains a tiny Rust BPE in process, so the new assertions need no cached tokenizer or dataset. I'll extend that fixture rather than invoking the full tokenizer evaluation script.",
      ),
      activity(4, "Called 5 tools"),
      p(
        5,
        "All ten focused tests pass. The coverage now distinguishes assistant supervision from tool-output masking; I'm checking the broader tokenizer tests before wrapping up.",
      ),
    ],
    terminalBlocks: [
      explored(2, "nanochat/tokenizer.py", "tests/test_tokenizer.py"),
      ran(
        2,
        "rg -n 'render_conversation|python_output|SPECIAL_TOKENS' nanochat/tokenizer.py tests/test_tokenizer.py",
        "nanochat/tokenizer.py:140:    def render_conversation(self, conversation, max_tokens=2048):",
        "nanochat/tokenizer.py:209:                        elif part['type'] == 'python_output':",
        "tests/test_tokenizer.py:10:from nanochat.tokenizer import RustBPETokenizer, SPECIAL_TOKENS",
        "tests/test_tokenizer.py:61:def test_render_conversation_masks(tokenizer):",
      ),
      ran(
        2,
        "uv run --extra cpu python -m pytest tests/test_tokenizer.py --collect-only -q | tail -n 6",
        "tests/test_tokenizer.py::test_render_conversation_masks",
        "tests/test_tokenizer.py::test_render_conversation_system_message_merged",
        "tests/test_tokenizer.py::test_render_conversation_tool_parts",
        "tests/test_tokenizer.py::test_render_conversation_truncation",
        "tests/test_tokenizer.py::test_render_for_completion",
        "10 tests collected in 0.18s",
      ),
      ran(
        4,
        "git diff --stat && git diff --check",
        "tests/test_tokenizer.py | 38 ++++++++++++++++++++++++++++++++++++++",
        "1 file changed, 38 insertions(+)",
      ),
      ran(
        4,
        "uv run --extra cpu python -m pytest tests/test_tokenizer.py -q",
        "..........                                                               [100%]",
        "10 passed in 0.42s",
      ),
      ran(
        4,
        "uv run --extra cpu python -m pytest tests/test_tokenizer.py -vv -k 'render_conversation'",
        "tests/test_tokenizer.py::test_render_conversation_masks PASSED           [ 25%]",
        "tests/test_tokenizer.py::test_render_conversation_system_message_merged PASSED [ 50%]",
        "tests/test_tokenizer.py::test_render_conversation_tool_parts PASSED      [ 75%]",
        "tests/test_tokenizer.py::test_render_conversation_truncation PASSED      [100%]",
        "======================= 4 passed, 6 deselected in 0.31s =======================",
      ),
      ran(4, "git status --short", " M tests/test_tokenizer.py"),
    ],
  },
  "nanochat-task-mixture": {
    project: "nanochat",
    userTurn: user("verify TaskMixture stays deterministic when a task is repeated"),
    agentTurn: [
      p(
        1,
        "I'll inspect how the mixture builds and shuffles its index map, then run the small in-memory task tests without touching Hub data or GPU evaluation.",
      ),
      activity(2, "Called 4 tools"),
      p(
        3,
        "The implementation enumerates the configured task list into an index map and shuffles it with a local ",
        { text: "random.Random(42)", code: true },
        ". Repeating a task intentionally duplicates its examples; a second mixture should produce the same order.",
      ),
      activity(4, "Called 4 tools"),
      p(
        5,
        "All seven task tests pass, including the deterministic second-instance comparison and repeated-task oversampling. The behavior is already covered, so no patch is warranted.",
      ),
    ],
    terminalBlocks: [
      explored(2, "tasks/common.py", "tests/test_tasks.py"),
      ran(
        2,
        "rg -n 'class TaskMixture|random.Random' tasks/common.py",
        "129:class TaskMixture(Task):",
        "147:        rng = random.Random(42)",
        "148:        rng.shuffle(self.index_map)",
      ),
      ran(
        2,
        "sed -n '139,149p' tasks/common.py",
        "    self.lengths = [task.num_examples() for task in tasks]",
        "    self.num_conversations = sum(self.lengths)",
        "    self.index_map = []",
        "    for task_idx, task_length in enumerate(self.lengths):",
        "        for local_idx in range(task_length):",
        "            self.index_map.append((task_idx, local_idx))",
        "    rng = random.Random(42)",
        "    rng.shuffle(self.index_map)",
      ),
      ran(
        4,
        "uv run --extra cpu python -m pytest tests/test_tasks.py --collect-only -q | tail -n 4",
        "tests/test_tasks.py::test_hub_dataset_rows",
        "tests/test_tasks.py::test_hub_dataset_shuffle_matches_numpy",
        "tests/test_tasks.py::test_render_mc_letter_binding",
        "7 tests collected in 0.19s",
      ),
      ran(
        4,
        "uv run --extra cpu python -m pytest tests/test_tasks.py -q",
        ".......                                                                  [100%]",
        "7 passed in 0.48s",
      ),
      ran(
        4,
        "uv run --extra cpu python -m pytest tests/test_tasks.py -vv -k 'mixture'",
        "tests/test_tasks.py::test_mixture_covers_all_examples_deterministically PASSED [ 50%]",
        "tests/test_tasks.py::test_mixture_oversampling PASSED                    [100%]",
        "======================= 2 passed, 5 deselected in 0.34s =======================",
      ),
      ran(4, "git status --short --branch", "## main"),
    ],
  },
  "llmc-layernorm": {
    project: "llm.c",
    userTurn: user("validate layernorm backward kernel 10 against the CPU reference"),
    agentTurn: [
      p(
        1,
        "I'll use the standalone CUDA harness in ",
        { text: "dev/cuda/layernorm_backward.cu", code: true },
        " because it dispatches kernel 10 directly and compares every output with its CPU reference.",
      ),
      activity(2, "Called 6 tools"),
      p(
        3,
        "Kernel 10 vectorizes the input-gradient load and store while keeping the parameter-gradient reduction in its scratch buffer. I'll compile the harness exactly as documented at the top of the file.",
      ),
      activity(4, "Called 4 tools"),
      p(
        5,
        "Every tested block size matches for input, weight, and bias gradients. The harness then reports kernel timings, so this validates the edited code path rather than an unrelated production target.",
      ),
    ],
    terminalBlocks: [
      explored(2, "dev/cuda/layernorm_backward.cu", "llmc/cuda_common.h", "llmc/cuda_utils.cuh"),
      ran(
        2,
        "rg -n 'kernel10|All results match|benchmark_kernel' dev/cuda/layernorm_backward.cu",
        "1057:layernorm_backward_kernel10(floatX* dinp, floatX* dweight, floatX* dbias,",
        "1516:        printf(\"All results match for block_size=%d.\\n\\n\", block_size);",
        "1523:        float elapsed_time = benchmark_kernel(repeat_times, layernorm_backward, kernel_num,",
      ),
      ran(
        2,
        "nvidia-smi --query-gpu=name,compute_cap,memory.total --format=csv,noheader",
        "NVIDIA RTX 4090, 8.9, 24564 MiB",
      ),
      ran(
        2,
        "nvcc --version | tail -n 2",
        "Cuda compilation tools, release 12.4, V12.4.131",
        "Build cuda_12.4.r12.4/compiler.34097967_0",
      ),
      ran(
        4,
        "nvcc -O3 --use_fast_math -lcublas -lcublasLt dev/cuda/layernorm_backward.cu -o /tmp/layernorm_backward",
      ),
      ran(4, "stat -c '%s bytes' /tmp/layernorm_backward", "1229472 bytes"),
      ran(
        4,
        "/tmp/layernorm_backward 10",
        "Using kernel 10",
        "Checking correctness...",
        "All results match for block_size=32.",
        "All results match for block_size=64.",
        "All results match for block_size=128.",
        "All results match for block_size=256.",
        "All results match for block_size=512.",
        "All results match for block_size=1024.",
        "",
        "Benchmarking kernel 10...",
        "block_size   32 time 0.0308 ms",
        "block_size   64 time 0.0261 ms",
        "block_size  128 time 0.0234 ms",
        "block_size  256 time 0.0220 ms",
        "block_size  512 time 0.0216 ms",
        "block_size 1024 time 0.0218 ms",
      ),
      ran(4, "git diff --check && git status --short", " M dev/cuda/layernorm_backward.cu"),
    ],
  },
  "tinygrad-movement": {
    project: "tinygrad",
    userTurn: user("add a focused regression for adjacent reshape cleanup"),
    agentTurn: [
      p(
        1,
        "I'll inspect the current movement pattern and test it at the UOp graph-rewrite layer. The old ShapeTracker layout is gone, so a regression should target ",
        { text: "mop_cleanup", code: true },
        " directly.",
      ),
      activity(2, "Called 4 tools"),
      p(
        3,
        "The matcher already replaces nested reshapes with one reshape over the original source. I'll add a focused test that asserts both the resulting op and source identity, without changing the rewrite itself.",
      ),
      activity(4, "Called 6 tools"),
      p(
        5,
        "The new regression passes, followed by the full 56-test schedule suite. This documents current cleanup behavior without adding speculative guards that the matcher does not have.",
      ),
    ],
    terminalBlocks: [
      explored(2, "tinygrad/uop/movement.py", "test/null/test_uop_graph.py"),
      ran(
        2,
        "rg -n 'merge adjacent RESHAPES' tinygrad/uop/movement.py",
        "6:  # merge adjacent RESHAPES",
      ),
      ran(
        2,
        "sed -n '1,18p' tinygrad/uop/movement.py",
        "mop_cleanup = PatternMatcher([",
        "  # merge adjacent RESHAPES",
        "  (UPat(Ops.RESHAPE, src=(UPat(Ops.RESHAPE, name=\"x2\"), UPat()), name=\"x\"),",
        "   lambda x,x2: x.replace(src=(x2.src[0], x.src[1]))),",
        "  # remove noop RESHAPEs",
        "])",
      ),
      ran(
        4,
        "git diff --stat -- test/null/test_uop_graph.py",
        "test/null/test_uop_graph.py | 8 ++++++++",
        "1 file changed, 8 insertions(+)",
      ),
      ran(
        4,
        "python3 -m pytest test/null/test_uop_graph.py::TestMovementOps::test_adjacent_reshape_cleanup -q",
        ".                                                                        [100%]",
        "1 passed in 0.17s",
      ),
      ran(
        4,
        "python3 -m pytest test/null/test_uop_graph.py -q",
        "........................................................................ [ 91%]",
        ".......                                                                  [100%]",
        "79 passed in 0.63s",
      ),
      ran(
        4,
        "python3 -m pytest test/backend/test_schedule.py -q",
        "........................................................                 [100%]",
        "56 passed in 1.84s",
      ),
      ran(4, "git diff --check && git status --short", " M test/null/test_uop_graph.py"),
    ],
  },
  "tinygrad-metal": {
    project: "tinygrad",
    userTurn: user("cover invalid Metal source and corrupt compiled libraries"),
    agentTurn: [
      p(
        1,
        "I'll use the existing Metal-only device tests, which exercise both source compilation failures and runtime rejection of a corrupted compiled library.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The compiler path raises ",
        { text: "CompileError", code: true },
        " from the Metal build service, while the corrupt-library path must make runtime construction fail. I'll keep those failure types distinct in the assertions.",
      ),
      activity(4, "Called 3 tools"),
      p(
        5,
        "All five Metal device tests pass, including OOM, compile failure, compile success, corrupt library rejection, and explicit buffer release.",
      ),
    ],
    terminalBlocks: [
      explored(2, "tinygrad/runtime/ops_metal.py", "test/device/test_metal.py"),
      ran(
        2,
        "rg -n 'test_compile_error|test_failed_newLibraryWithData|CompileError' test/device/test_metal.py tinygrad/runtime/ops_metal.py",
        "test/device/test_metal.py:11:  def test_compile_error(self):",
        "test/device/test_metal.py:35:  def test_failed_newLibraryWithData(self):",
        "tinygrad/runtime/ops_metal.py:74:  def compile(self, src:str) -> bytes:",
      ),
      ran(
        2,
        "sw_vers && system_profiler SPDisplaysDataType | rg 'Chipset Model|Metal Support'",
        "ProductName:\t\tmacOS",
        "ProductVersion:\t\t15.6",
        "BuildVersion:\t\t24G84",
        "      Chipset Model: Apple M3 Max",
        "      Metal Support: Metal 3",
      ),
      ran(
        2,
        "sed -n '1,55p' test/device/test_metal.py",
        "class TestMetal(unittest.TestCase):",
        "  def test_alloc_oom(self):",
        "  def test_compile_error(self):",
        "  def test_compile_success(self):",
        "  def test_failed_newLibraryWithData(self):",
        "  def test_free(self):",
      ),
      ran(
        4,
        "METAL=1 python3 -m pytest test/device/test_metal.py::TestMetal::test_compile_error test/device/test_metal.py::TestMetal::test_failed_newLibraryWithData -vv",
        "test/device/test_metal.py::TestMetal::test_compile_error PASSED          [ 50%]",
        "test/device/test_metal.py::TestMetal::test_failed_newLibraryWithData PASSED [100%]",
        "============================== 2 passed in 0.42s ==============================",
      ),
      ran(
        4,
        "METAL=1 python3 -m pytest test/device/test_metal.py -q",
        ".....                                                                    [100%]",
        "5 passed in 0.71s",
      ),
      ran(4, "git diff --check && git status --short --branch", "## main"),
    ],
  },
  "wiki-compiler-index": {
    project: "wiki",
    userTurn: user("add concise per-file summaries for compiler/ and link them from its index"),
    agentTurn: [
      p(
        1,
        "I'll inventory the existing compiler notes and follow their heading and relative-link conventions before adding missing summaries.",
      ),
      activity(2, "Called 3 tools"),
      p(
        3,
        "The index covers parsing, code generation, and builtins but not the IR or rendering pages. I'll add one factual paragraph per missing page and keep implementation details in the leaf pages.",
      ),
      activity(4, "Called 6 tools"),
      p(
        5,
        "Every new index link resolves to a tracked Markdown file, and the diff contains only the compiler index plus the two missing summaries. I'm doing a final terminology pass now.",
      ),
    ],
    terminalBlocks: [
      ran(
        2,
        "rg --files compiler | sort",
        "compiler/builtins.md",
        "compiler/codegen.md",
        "compiler/index.md",
        "compiler/ir.md",
        "compiler/parse.md",
        "compiler/render.md",
      ),
      ran(
        2,
        "sed -n '1,120p' compiler/index.md",
        "# Compiler",
        "- [Parsing](parse.md)",
        "- [Code generation](codegen.md)",
        "- [Builtins](builtins.md)",
      ),
      ran(
        2,
        "for f in compiler/*.md; do printf '%-24s ' \"$f\"; sed -n '1s/^# //p' \"$f\"; done",
        "compiler/builtins.md     Builtins",
        "compiler/codegen.md      Code generation",
        "compiler/index.md        Compiler",
        "compiler/ir.md           Intermediate representation",
        "compiler/parse.md        Parsing",
        "compiler/render.md       Rendering",
      ),
      ran(
        4,
        "git diff --stat -- compiler/index.md",
        "compiler/index.md | 6 ++++++",
        "1 file changed, 6 insertions(+)",
      ),
      ran(
        4,
        "rg -o '\\([^)]*\\.md\\)' compiler/index.md | tr -d '()' | while read f; do test -f \"compiler/$f\" || echo \"missing: $f\"; done",
      ),
      ran(
        4,
        "rg -n '^# ' compiler/{index,ir,render}.md",
        "compiler/index.md:1:# Compiler",
        "compiler/ir.md:1:# Intermediate representation",
        "compiler/render.md:1:# Rendering",
      ),
      ran(
        4,
        "git diff --check && git diff --stat",
        "compiler/index.md  |  6 ++++++",
        "compiler/ir.md     | 18 ++++++++++++++++++",
        "compiler/render.md | 15 +++++++++++++++",
      ),
      ran(4, "git status --short", " M compiler/index.md", " M compiler/ir.md", " M compiler/render.md"),
    ],
  },
  "wiki-glossary": {
    project: "wiki",
    userTurn: user("define AOT, intermediate representation, and native runtime consistently"),
    agentTurn: [
      p(
        1,
        "I'll find every existing use first so the glossary describes this wiki's terminology instead of importing generic compiler definitions.",
      ),
      activity(2, "Called 5 tools"),
      p(
        3,
        "The pages use IR and intermediate representation interchangeably, while runtime sometimes means the generated support code and sometimes the host process. I'll define both senses explicitly.",
      ),
      activity(4, "Called 6 tools"),
      p(
        5,
        "The glossary now owns the definitions, and the compiler overview links the first occurrence of each term without turning later prose into link noise.",
      ),
    ],
    terminalBlocks: [
      ran(
        2,
        "rg -n '\\b(AOT|IR|intermediate representation|native runtime)\\b' --glob '*.md'",
        "compiler/index.md:7:The AOT pipeline lowers source into an IR.",
        "compiler/render.md:11:The intermediate representation is rendered as C.",
        "compiler/codegen.md:3:AOT compilation starts after parsing.",
        "runtime/index.md:4:The native runtime supplies host-facing operations.",
        "runtime/ffi.md:8:The host process enters the native runtime through generated wrappers.",
      ),
      explored(2, "glossary.md", "compiler/index.md", "runtime/index.md"),
      ran(
        2,
        "sed -n '1,100p' glossary.md",
        "# Glossary",
        "",
        "Existing entries are alphabetical and use sentence-style definitions.",
        "Cross-references use relative Markdown links.",
      ),
      ran(
        4,
        "git diff --stat -- glossary.md",
        "glossary.md | 21 +++++++++++++++++++++",
        "1 file changed, 21 insertions(+)",
      ),
      ran(
        4,
        "rg -n '^## (Ahead-of-time|Intermediate representation|Native runtime)' glossary.md",
        "glossary.md:18:## Ahead-of-time compilation (AOT)",
        "glossary.md:27:## Intermediate representation (IR)",
        "glossary.md:36:## Native runtime",
      ),
      ran(
        4,
        "rg -n '\\[.*\\]\\(.*glossary.md' compiler/index.md runtime/index.md",
        "compiler/index.md:7:The [AOT pipeline](../glossary.md#ahead-of-time-compilation-aot) lowers source into an [IR](../glossary.md#intermediate-representation-ir).",
        "runtime/index.md:4:The [native runtime](../glossary.md#native-runtime) supplies host-facing operations.",
      ),
      ran(
        4,
        "git diff --check && git diff --stat",
        "glossary.md       | 21 +++++++++++++++++++++",
        "compiler/index.md |  4 ++--",
        "runtime/index.md  |  2 +-",
      ),
      ran(4, "git status --short", " M compiler/index.md", " M glossary.md", " M runtime/index.md"),
    ],
  },
};

// The terminal map's rails: one column per sidebar pane, showing what each
// agent settled, what it is working on, and what is queued behind it. The
// current card carries each session's own prompt — the same text its
// transcript opens with — so a rail and its pane never disagree.
const rail = (
  sessionId: string,
  rest: Omit<MockHomeRail, "current"> &
    Partial<Pick<MockHomeCurrentTurn, "tone" | "receipt">>,
): MockHomeRail => ({
  ...rest,
  current: {
    text: MOCK_SESSIONS[sessionId].userTurn.text,
    tone: rest.tone ?? "done",
    receipt: rest.receipt ?? "Done · just now",
  },
});

export const MOCK_HOME_RAILS: Record<string, MockHomeRail> = {
  "qmux-landing-transcript": rail("qmux-landing-transcript", {
    tone: "active",
    receipt: "working · 4m",
    queued: [
      "narrow the replay window once the image step lands",
      "commit the landing copy pass",
    ],
  }),
  "porffor-replace-all": rail("porffor-replace-all", {
    past: [{ text: "reproduce the empty-search hang on main", receipt: "4h ago" }],
    receipt: "Done · 22m",
    queued: ["backport the match-width split to replace"],
  }),
  "porffor-codegen-casts": rail("porffor-codegen-casts", { receipt: "Done · 1h" }),
  "porffor-math-hypot": rail("porffor-math-hypot", { receipt: "Done · 3h" }),
  "autoresearch-muon-ramp": rail("autoresearch-muon-ramp", {
    past: [{ text: "baseline the constant-momentum schedule", receipt: "6h ago" }],
    tone: "active",
    receipt: "working · 12m",
    queued: [
      "hold the ramp only if memory stays under 45 GB",
      "rank tonight's keeps in results.tsv",
    ],
  }),
  "autoresearch-qk-norm": rail("autoresearch-qk-norm", {
    receipt: "Done · 2h",
    queued: ["retry QK norm at half the head dim", "discard unless val_bpb beats 0.996841"],
    paused: true,
  }),
  "autoresearch-results": rail("autoresearch-results", { receipt: "Done · 5h" }),
  "nanochat-tokenizer": rail("nanochat-tokenizer", {
    tone: "active",
    receipt: "working · 2m",
    queued: ["extend coverage to the byte-fallback path"],
  }),
  "nanochat-task-mixture": rail("nanochat-task-mixture", { receipt: "Done · 6h" }),
  "llmc-layernorm": rail("llmc-layernorm", { receipt: "Done · 1d" }),
  "tinygrad-movement": rail("tinygrad-movement", { receipt: "Done · 1d" }),
  "tinygrad-metal": rail("tinygrad-metal", { receipt: "Done · 2d" }),
  "wiki-compiler-index": rail("wiki-compiler-index", {
    tone: "active",
    receipt: "working · 7m",
    queued: ["link the new summaries from the wiki home"],
  }),
  "wiki-glossary": rail("wiki-glossary", { receipt: "Done · 3d" }),
};

// The application-global drafts rail, shown left of the agent columns.
export const MOCK_HOME_DRAFTS = [
  "profile the cold-start path before touching the bundler",
  "ask upstream how the test262 harness flags stale results",
];

// Extra rows for a tab's right-click details menu. The app omits any row it
// has no data for, so only branches are called out here — the directory comes
// from the session's project and the status from its pane.
export interface MockTabDetails {
  branch?: string;
}

export const MOCK_TAB_DETAILS: Record<string, MockTabDetails> = {
  "qmux-landing-transcript": { branch: "main" },
  "porffor-replace-all": { branch: "main" },
  "porffor-codegen-casts": { branch: "main" },
  "autoresearch-muon-ramp": { branch: "autoresearch/aug12" },
  "autoresearch-qk-norm": { branch: "autoresearch/aug12" },
  "nanochat-tokenizer": { branch: "main" },
  "tinygrad-movement": { branch: "main" },
  "tinygrad-metal": { branch: "main" },
};

export const DEFAULT_SESSION_ID = "qmux-landing-transcript";

// Backwards-compatible names keep the selected server-rendered session easy to
// inspect in tests and make the default static state explicit.
export const TERMINAL_BLOCKS = MOCK_SESSIONS[DEFAULT_SESSION_ID].terminalBlocks;
export const AGENT_TURN = MOCK_SESSIONS[DEFAULT_SESSION_ID].agentTurn;
export const USER_TURN = MOCK_SESSIONS[DEFAULT_SESSION_ID].userTurn;

// The prompt library's saved prompts, the notification journal's feed, the
// artifacts the default qmux agent opened, and the address the browser overlay
// is previewing.
export const SAVED_PROMPTS = [
  "Review the diff for correctness bugs only. Skip style.",
  "Read the existing implementation first, and say what it already handles.",
  "Write the failing test first, then make it pass.",
  "Summarise what changed and what is still unmerged.",
  "Run the failing directory only, and report each failure with the command to reproduce it.",
];

export interface MockNotification {
  id: string;
  title: string;
  body: string;
  age: string;
  unread: boolean;
}

export const MOCK_NOTIFICATIONS: MockNotification[] = [
  {
    id: "ci-main",
    title: "qmux",
    body: "CI finished on main.",
    age: "2s",
    unread: true,
  },
  {
    id: "draft-published",
    title: "research-agent",
    body: "Published the draft.",
    age: "1h",
    unread: false,
  },
];

export interface MockArtifact {
  name: string;
  meta: string;
}

export const ARTIFACTS: MockArtifact[] = [
  { name: "web/landing/mockupData.ts", meta: "42k" },
  { name: "web/landing/AppMockup.tsx", meta: "tsx" },
  { name: "web/landing/mockupCss.ts", meta: "css" },
  { name: "site/mockup.js", meta: "js" },
  { name: "site/qmux.png", meta: "png" },
  { name: "web/server.test.ts", meta: "test" },
  { name: "dist-site/server.mjs", meta: "267k" },
  { name: "landing-preview.html", meta: "html" },
  { name: "session-replay.log", meta: "log" },
  { name: "mockup.patch", meta: "patch" },
  { name: "notes-landing-demo.md", meta: "md" },
];

export const BROWSER_URL = "localhost:8787/";

export const SESSION_LABELS: Record<string, string> = {
  "qmux-landing-transcript": "Session: 019ff401-839a-7a51-b…",
  "porffor-replace-all": "Session: 019ff405-b4fe-70d2-9…",
  "porffor-codegen-casts": "Session: 019ff40a-1872-7586-a…",
  "porffor-math-hypot": "Session: 019ff40d-922f-77b0-b…",
  "autoresearch-muon-ramp": "Session: 019ff411-04ac-7f33-8…",
  "autoresearch-qk-norm": "Session: 019ff414-cc09-7a61-a…",
  "autoresearch-results": "Session: 019ff418-2df8-750d-b…",
  "nanochat-tokenizer": "Session: 019ff41b-a4c7-79e2-9…",
  "nanochat-task-mixture": "Session: 019ff41f-137b-72c8-a…",
  "llmc-layernorm": "Session: 019ff422-8cb4-7d15-b…",
  "tinygrad-movement": "Session: 019ff426-061e-766f-8…",
  "tinygrad-metal": "Session: 019ff429-7f60-7b92-a…",
  "wiki-compiler-index": "Session: 019ff42d-18a5-74eb-b…",
  "wiki-glossary": "Session: 019ff430-91ed-7e44-9…",
};
export const SESSION_LABEL = SESSION_LABELS[DEFAULT_SESSION_ID];
export const COMPOSER_PLACEHOLDER = "What should we investigate next?";
export const ARTIFACT_COUNT = 11;

// ------------------------------------------------------------------ research
//
// The Research sidebar mode's fixture: the same landing-page iteration seen
// from the other half of the window. The questions are ones this work actually
// raised — how scrollback survives a resize, which test262 failures went stale,
// what the replica has to keep honest — so the research replica reads as the
// terminal replica's other half rather than as a separate demo.
//
// The Journal's tweet is a fabricated post from an invented account. Nothing
// here quotes a real person or reproduces a real post; the handles exist only
// so the embed has a shape to render.

export interface MockResearchRun {
  text: string;
  /** Renders the transcript's inline-code style. */
  code?: boolean;
  /**
   * A saved highlight ("saved"), or the passage a targeted follow-up was asked
   * about ("anchor"). The app paints both with the Custom Highlight API, which
   * needs live ranges; the replica marks the runs in the markup instead.
   */
  mark?: "saved" | "anchor";
}

export type MockResearchBlock =
  | { type: "paragraph"; runs: MockResearchRun[] }
  | { type: "list"; items: string[] };

export interface MockResearchFollowup {
  prompt: string;
  preview?: string;
  status?: "running" | "queued";
  /** An answer that settled while the reader was elsewhere. */
  unread?: boolean;
  /**
   * Anchored cards sit beside the passage they were asked about. The app
   * resolves that offset from the live range; the replica does no layout, so
   * the settled position is part of the fixture.
   */
  anchorTop?: number;
}

export interface MockResearchDoc {
  /** Sidebar title, breadcrumb, and the document's identity in the markup. */
  title: string;
  /** Drives the row's icon, exactly as the app's tree kinds do. */
  kind: "run" | "document" | "conversation";
  /** A run still streaming: spinner in the sidebar, Cancel in the header. */
  running?: boolean;
  /** Answers that landed unseen carry the sidebar's "New" tag. */
  unseen?: boolean;
  /** Documents have no question bubble; runs and conversations do. */
  question?: string;
  /** The passage a targeted follow-up was asked about, above its question. */
  quote?: string;
  answer: MockResearchBlock[];
  /** The answer meta line: word count, then the run's wall time. */
  words?: string;
  duration?: string;
  /** Header count: "4 in thread · 1 branch". */
  thread?: string;
  followups?: MockResearchFollowup[];
}

const run = (...runs: (string | MockResearchRun)[]): MockResearchBlock => ({
  type: "paragraph",
  runs: runs.map((item) => (typeof item === "string" ? { text: item } : item)),
});

const bullets = (...items: string[]): MockResearchBlock => ({ type: "list", items });

export const MOCK_RESEARCH_DOCS: Record<string, MockResearchDoc> = {
  "scrollback-reflow": {
    title: "How libghostty reflows scrollback when a pane is resized",
    kind: "run",
    question:
      "When a qmux pane is resized, how does libghostty reflow its scrollback without losing the cursor or an active selection — and what does a reflow cost on a 50k-line buffer?",
    answer: [
      run(
        "Reflow happens on the terminal's own page list rather than on a flat line array. Each page holds a fixed-capacity block of rows, so a width change rewrites pages in place and only reallocates when a page can no longer hold its rewrapped rows. ",
        {
          text: "The cursor is tracked as an offset into the row it sits on, not as a column, which is why it survives a rewrap that moves it onto a different visual line.",
          mark: "anchor",
        },
      ),
      run(
        "Soft-wrapped rows carry a continuation flag. Widening joins a run of continuations back into one logical row and re-splits it at the new width; narrowing does the reverse. Rows that were hard-wrapped by the program — anything that ended in a newline — are never joined, which is what keeps command output from collapsing into a paragraph when you drag the divider.",
      ),
      bullets(
        "Pages are rewritten in place; only an overflowing page allocates.",
        "Cursor and viewport are pins into a row, so both follow the rewrap.",
        "Hard-wrapped rows are boundaries and never merge.",
      ),
      run(
        {
          text: "A 50k-line buffer reflows in roughly 8–11ms on an M-series machine, which is under one frame at 60Hz and well under the drag's own coalescing window.",
          mark: "saved",
        },
        " The cost is dominated by the memcpy per page rather than by the rewrap arithmetic, so it scales with scrollback size and not with the width delta.",
      ),
      run(
        "Practically: resizing is cheap enough that qmux does not need to debounce it, and the split-drag can stay live rather than snapping at the end of the gesture.",
      ),
    ],
    words: "1,184 words",
    duration: "3m 12s",
    thread: "4 in thread · 1 branch",
    followups: [
      {
        prompt: "What happens to a selection that spans a row the rewrap moved?",
        preview:
          "Selections are stored as pins too, so the anchor and head follow their rows. A selection that spanned a soft wrap comes back as one range at the new width.",
        anchorTop: 96,
      },
      {
        prompt: "Does the same page list back the search index, or is that separate?",
        preview: "Separate. The search walks pages directly and holds no index of its own…",
        unread: true,
      },
    ],
  },
  "test262-stale": {
    title: "Which test262 failures went stale after the replaceAll fix",
    kind: "run",
    unseen: true,
    question:
      "After the empty-search advance landed, which built-ins/String failures in the last test262 run are stale results rather than real regressions?",
    answer: [
      run(
        "Eleven of the fourteen reported failures were recorded before the guard moved above the loop and never re-ran. Re-running ",
        { text: "built-ins/String/prototype/replaceAll", code: true },
        " alone clears all eleven; the harness keeps the previous result file when a directory is filtered, which is why they persisted in the summary.",
      ),
      run(
        "The remaining three are real and share one cause: a non-callable replacer is coerced before the search argument is validated, so the thrown error is the wrong type.",
      ),
    ],
    words: "612 words",
    duration: "1m 40s",
    followups: [
      {
        prompt: "Write the smallest repro for the coercion-order failure.",
        preview: "Two lines — a Symbol search with a non-callable replacer is enough.",
      },
    ],
  },
  "codex-export": {
    title: "replaceAll: empty search advances a code unit (exported)",
    kind: "conversation",
    question: "Exported from the porffor pane after the fix landed.",
    answer: [
      run(
        "This conversation was exported from a terminal session, so it is a point-in-time copy: the turns are the pane's own, and nothing in it re-runs.",
      ),
      run(
        "The final state of the pane was a passing ",
        { text: "./porf /tmp/replaceall-smoke.js", code: true },
        " and a clean run of the filtered directory.",
      ),
    ],
    words: "2,940 words",
  },
  "muon-ramp-sweep": {
    title: "Which Muon momentum ramps survive the five-minute budget",
    kind: "run",
    running: true,
    question:
      "Sweep the Muon momentum ramp between 0.85 and 0.98 under the five-minute budget and keep only the settings where val_bpb actually drops.",
    answer: [
      run(
        "Six of the nine configurations have reported. The ramp that ends at 0.95 is ahead on val_bpb at every checkpoint so far, but the gap is inside the seed-to-seed spread and three runs are still going.",
      ),
    ],
    words: "318 words",
  },
  "qk-norm-eval": {
    title: "QK norm: keep it only if val_bpb drops",
    kind: "run",
    question:
      "Does QK norm earn its place in the five-minute budget, or is the win inside the seed noise?",
    answer: [
      run(
        "Across five seeds the mean val_bpb difference is smaller than the standard deviation of either arm, so on this budget QK norm is not distinguishable from the baseline.",
      ),
      run(
        "It does make the loss curve visibly smoother in the first 400 steps, which is worth keeping only if the ramp sweep above ends up depending on early-step stability.",
      ),
    ],
    words: "744 words",
    duration: "2m 05s",
  },
  "landing-replica-notes": {
    title: "Landing replica: what the mock has to keep honest",
    kind: "document",
    answer: [
      run(
        "The replica is not a screenshot, so every claim it makes is a claim the app has to be able to keep. Three rules have held up so far:",
      ),
      bullets(
        "Nothing in the markup is required. With the script gone the page is still the finished state of a real session.",
        "A control is either inert or real. A span that looks like a button and does nothing is worse than no button.",
        "Copy is either true or clearly a fixture. No invented benchmarks, no quotes from anyone.",
      ),
      run(
        "The third one is the one that keeps needing enforcement, because a plausible number is always easier to write than a real one.",
      ),
    ],
    words: "480 words",
  },
  "terminal-map-layout": {
    title: "Terminal map: one column per pane, or one per group?",
    kind: "run",
    question:
      "The Home board can lay out one rail per pane or one per group. Which reads better when fourteen agents are open?",
    answer: [
      run(
        "One rail per pane. Grouping collapses exactly the information the board exists to show — which agent is waiting on you — and the group is already recoverable from the chip row above the rails.",
      ),
      run(
        "The cost is horizontal scroll at fourteen panes, which the chips make cheap to narrow: hiding a stream is one click and the rails re-flow immediately.",
      ),
    ],
    words: "521 words",
    duration: "58s",
  },
  "mockup-fonts": {
    title: "Which webfont subsets does the replica actually need?",
    kind: "run",
    question: "Which glyphs does the replica's terminal pane actually use?",
    answer: [
      run(
        "Latin plus the box-drawing and bullet glyphs the agent's output uses. Subsetting to that range takes the monospace file under 40KB, which is small enough to preload without competing with the page's own type.",
      ),
    ],
    words: "295 words",
    duration: "41s",
  },
};

export type MockResearchUnit =
  | { kind: "doc"; id: string }
  | { kind: "folder"; name: string; collapsed: boolean; ids: string[] };

/** Starred research sits above the rest of the list as its own group. */
export const MOCK_RESEARCH_STARRED = ["scrollback-reflow"];

export const MOCK_RESEARCH_UNITS: MockResearchUnit[] = [
  { kind: "folder", name: "porffor", collapsed: false, ids: ["test262-stale", "codex-export"] },
  {
    kind: "folder",
    name: "autoresearch",
    collapsed: true,
    ids: ["muon-ramp-sweep", "qk-norm-eval"],
  },
  { kind: "doc", id: "landing-replica-notes" },
  { kind: "doc", id: "terminal-map-layout" },
];

export const MOCK_RESEARCH_ARCHIVED = ["mockup-fonts"];

export const DEFAULT_RESEARCH_DOC_ID = "scrollback-reflow";

/** The Journal tab's own view id, alongside the document ids above. */
export const JOURNAL_VIEW_ID = "journal";

export const RESEARCH_COMPOSER_PLACEHOLDER = "Ask a follow-up…";
export const JOURNAL_COMPOSER_PLACEHOLDER = "Add a note or paste a URL…";

export interface MockTweetRun {
  text: string;
  /** Set on the runs X renders as links: mentions, hashtags, expanded URLs. */
  link?: boolean;
}

export interface MockJournalTweet {
  name: string;
  handle: string;
  verified?: boolean;
  runs: MockTweetRun[];
  /** Media is bundled with the site; the page loads nothing from elsewhere. */
  media?: { src: string; alt: string; width: number; height: number };
  quoted?: {
    name: string;
    handle: string;
    verified?: boolean;
    runs: MockTweetRun[];
  };
  /** The timeline's age stamp, frozen rather than computed off the clock. */
  age: string;
  /** Engagement as captured. The card renders these as metadata, never as
      controls: nothing in the replica reaches X. */
  replies: string;
  likes: string;
}

export type MockJournalEntry =
  | { id: string; kind: "note"; text: string }
  | { id: string; kind: "link"; url: string }
  | { id: string; kind: "tweet"; url: string; tweet: MockJournalTweet };

// Oldest first, matching the app's storage order; the feed renders newest first.
export const MOCK_JOURNAL_ENTRIES: MockJournalEntry[] = [
  {
    id: "journal-reflow-note",
    kind: "note",
    text: "Reflow only shows up in a profile above ~50k scrollback lines. Measure before touching the page allocator — the drag already feels live.",
  },
  {
    id: "journal-page-list-link",
    kind: "link",
    url: "https://github.com/ghostty-org/ghostty/blob/main/src/terminal/PageList.zig",
  },
  {
    id: "journal-embed-tweet",
    kind: "tweet",
    url: "https://x.com/terminalnotes/status/1892740155039820000",
    tweet: {
      name: "Terminal Notes",
      handle: "terminalnotes",
      verified: true,
      runs: [
        {
          text: "the thing nobody tells you about scrollback is that the hard part isn't storing it, it's keeping the cursor and the selection pinned to the right row after a resize rewraps everything under them",
        },
      ],
      media: {
        src: "/qmux.png",
        alt: "A terminal window with its scrollback reflowed after a resize",
        width: 2704,
        height: 1704,
      },
      quoted: {
        name: "Scrollback Weekly",
        handle: "scrollbackweekly",
        runs: [
          { text: "new issue: page lists, soft wraps, and why your selection survives a drag " },
          { text: "scrollbackweekly.example/12", link: true },
        ],
      },
      age: "Aug 24",
      replies: "37",
      likes: "1,204",
    },
  },
];
