// Copy for the marketing page. Kept apart from the markup so the wording can be
// edited without touching layout.

export const LATEST_VERSION = "0.2.0";
export const GITHUB_URL = "https://github.com/raykyri/qmux";
export const DOWNLOAD_URL = `${GITHUB_URL}/releases/download/v${LATEST_VERSION}/qmux_${LATEST_VERSION}_universal.dmg`;
export const RELEASES_URL = `${GITHUB_URL}/releases`;

export const SITE_TITLE = "qmux — The queueing terminal multiplexer";
export const SITE_DESCRIPTION =
  "qmux is a macOS desktop terminal with a native control plane for AI coding agents.";

export interface Feature {
  name: string;
  copy: string;
}

export const FEATURES: Feature[] = [
  {
    name: "First-class agents",
    copy: "Native support for different coding agents: Codex, Claude, Grok, OpenCode.",
  },
  {
    name: "Vertical splittable tabs",
    copy: "The sidebar shows every terminal and agent, grouped by project. Drag to reorder.",
  },
  {
    name: "Rich text transcripts",
    copy: "Your agent’s transcript renders through a native parser: Markdown, tables, code blocks, graphs.",
  },
  {
    name: "Turn queue",
    copy: "Queue up follow-ups while an agent works; edit, reorder, and drag queued work between panes.",
  },
  {
    name: "Automatic recovery",
    copy: "Terminals automatically respawn on restart, along with tab groups, queued turns, and drafts.",
  },
  {
    name: "Session forking",
    copy: "Fork a running session to a new tab. The fork inherits the transcript, up to your last turn.",
  },
  {
    name: "Git worktrees",
    copy: "Launch agents into their own worktrees, with dirty-state checks and auto-cleanup on close.",
  },
  {
    name: "Prompt library",
    copy: "Save your commonly used prompts. Insert them from the ⌘K command palette.",
  },
  {
    name: "Based on libghostty",
    copy: "Every pane is a real libghostty terminal, rendered natively with Metal.",
  },
  {
    name: "Browser overlay",
    copy: "Preview local files or a localhost dev server in a resizable panel over the terminal.",
  },
];

export interface FaqEntry {
  question: string;
  answers: string[];
}

// `code` runs are wrapped in <code> when the paragraph is rendered.
export const FAQS: FaqEntry[] = [
  {
    question: "What is qmux?",
    answers: ["A Mac desktop app, terminal multiplexer, and agent control plane in one."],
  },
  {
    question: "How is it different from tmux?",
    answers: [
      "tmux runs in your terminal and multiplexes terminals; qmux is a terminal and multiplexes agents. It adds a native GUI and rendering, which a terminal-only multiplexer can’t see into.",
    ],
  },
  {
    question: "How is it different from cmux?",
    answers: [
      "Both are agent-friendly terminals with vertical tabs. cmux is mostly a terminal. qmux is mostly a control plane.",
    ],
  },
  {
    question: "How is it different from Herdr?",
    answers: [
      "Both are agent-friendly terminal multiplexers. qmux is a terminal app, while Herdr runs inside a terminal app.",
    ],
  },
  {
    question: "How is it different from Conductor, T3 Code, etc.?",
    answers: [
      "Those hide the terminal behind a dashboard, and often use Agent SDKs. In qmux you use the native CLIs for your coding agent.",
    ],
  },
  {
    question: "Which agents does it support?",
    answers: [
      "Claude Code, Codex, Grok, OpenCode. Agents are integrated through a pluggable adapter layer, so new ones can be easily added.",
    ],
  },
  {
    question: "How do I launch agents inside qmux?",
    answers: ["Run `claude` or `codex` in any shell pane and qmux routes it automatically."],
  },
  {
    question: "How does qmux know what an agent is doing?",
    answers: [
      "Our shell injects hooks for `claude`, `codex`, and other agents, that report session start, prompts, tool use, and permission requests over a Unix socket. Hooks are only installed in shells in qmux, not globally.",
    ],
  },
  {
    question: "What platforms does it run on?",
    answers: ["macOS 13 (Ventura) or later, either Apple Silicon or Intel. Linux is planned."],
  },
  {
    question: "Is my data sent anywhere?",
    answers: [
      "No. The application is entirely local and sends no telemetry. Tab title generation uses Apple Foundation Models on your local device by default.",
    ],
  },
  {
    question: "What's the business model?",
    answers: [
      "qmux was created as a part of our research efforts, and is now available as a free and open source project under the MIT License.",
    ],
  },
];
