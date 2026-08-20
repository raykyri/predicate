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
  id: "claude" | "codex" | "opencode" | "grok" | "muse" | "pi" | "cursor" | "devin";
  label: string;
}

// Product adapters represented in the marketing-page icon row.
export const SUPPORTED_AGENTS: SupportedAgent[] = [
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "opencode", label: "OpenCode" },
  { id: "grok", label: "Grok" },
  { id: "muse", label: "Muse" },
  { id: "pi", label: "Pi" },
  { id: "cursor", label: "Cursor" },
  { id: "devin", label: "Devin" },
];

export const FEATURES: Feature[] = [
  {
    name: "Rich text transcripts",
    copy: "Easy-to-read transcripts with Markdown, tables, code blocks, and diagrams.",
  },
  {
    name: "Cross-agent queueing",
    copy: "Queue up follow-ups while an agent works. Edit, reorder, remix work between panes.",
  },
  {
    name: "Automatic recovery",
    copy: "Agents and tab groups automatically respawn on restart.",
  },
  {
    name: "Session forking",
    copy: "Fork sessions while they're working. Easily create worktrees with one click.",
  },
  {
    name: "Based on libghostty",
    copy: "Every pane is a real libghostty terminal, rendered natively with Metal.",
  },
  {
    name: "Open source",
    copy: "Fully open-source, local-first, free forever.",
  },
  {
    name: "Artifacts and previews",
    copy: "Open Markdown, images, and local files without leaving qmux.",
  },
  {
    name: "Vertical tabs and splits",
    copy: "Organize terminal and agent panes with vertical tabs and flexible split layouts.",
  },
  {
    name: "Saved prompt library",
    copy: "Reuse Markdown prompts globally or per project, with placeholders for common inputs.",
  },
  {
    name: "Research trees",
    copy: "Explore questions in branching research trees and publish browsable results.",
  },
  {
    name: "Built-in browser",
    copy: "Keep local previews and web pages beside the agent working with them.",
  },
  {
    name: "Keyboard-first workflow",
    copy: "Launch agents, navigate tabs, and manage panes without reaching for the mouse.",
  },
];
