// Copy for the marketing page. Kept apart from the markup so the wording can be
// edited without touching layout.

export const GITHUB_URL = "https://github.com/raykyri/qmux";
export const RELEASES_URL = `${GITHUB_URL}/releases`;

export const SITE_TITLE = "qmux — The queueing terminal multiplexer";
export const SITE_DESCRIPTION =
  "qmux is a macOS desktop terminal with a native control plane for AI coding agents.";

export interface Feature {
  name: string;
  copy: string;
}

export interface SupportedAgent {
  id: "claude" | "codex" | "opencode" | "grok" | "muse" | "pi" | "cursor";
  label: string;
}

// First-party adapters with a native TUI. ACP is omitted: it is a wire
// protocol for other agents, not a product with its own mark.
export const SUPPORTED_AGENTS: SupportedAgent[] = [
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "opencode", label: "OpenCode" },
  { id: "grok", label: "Grok" },
  { id: "muse", label: "Muse" },
  { id: "pi", label: "Pi" },
  { id: "cursor", label: "Cursor" },
];

export const FEATURES: Feature[] = [
  {
    name: "Rich text transcripts",
    copy: "Your agent’s transcript renders, just like in the native app. Markdown, tables, code blocks, diagrams.",
  },
  {
    name: "Cross-agent queueing",
    copy: "Queue up follow-ups while an agent works. Edit, reorder, remix work between panes.",
  },
  {
    name: "Automatic recovery",
    copy: "Agents automatically respawn on restart, along with tab groups, queued turns, and full history.",
  },
  {
    name: "Session forking and worktrees",
    copy: "First-class support for forking and queue-forking sessions. Schedule multiple branches of work.",
  },
  {
    name: "Based on libghostty",
    copy: "Every pane is a real libghostty terminal, rendered natively with Metal.",
  },
  {
    name: "Open source",
    copy: "Fully open-source, local-first, free forever.",
  },
];
