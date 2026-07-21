import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, ComposerPolicy, LauncherOptionsProps } from ".";
import { normalizeClaudeTurns } from "./claudeTurns";

const CLAUDE_PERMISSION_OPTIONS: LauncherSelectOption[] = [
  { value: "auto", label: "Auto mode" },
  { value: "manual", label: "Ask for approval" },
  { value: "acceptEdits", label: "Only accept edits" },
  { value: "plan", label: "Plan mode" },
  { value: "dontAsk", label: "Block approval requests" },
  { value: "bypassPermissions", label: "Bypass permissions", tone: "danger" },
];

// Mirrors CLAUDE_EFFORT_LEVELS in src-tauri/src/adapters/claude.rs. Every
// current Claude model (Opus, Fable, Sonnet) supports the full range.
export const CLAUDE_EFFORT_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Default effort" },
  { value: "low", label: "Low effort", dividerBefore: true },
  { value: "medium", label: "Medium effort" },
  { value: "high", label: "High effort" },
  { value: "xhigh", label: "xHigh effort" },
  { value: "max", label: "Max effort" },
  { value: "ultracode", label: "Ultracode effort" },
];

export const CLAUDE_ADAPTER_ID = "claude";

const claudeComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [
    { id: "approve", label: "Approve", input: "y" },
    { id: "deny", label: "Deny", input: "n" },
  ],
};

export const claudeUiAdapter: AgentUiAdapter = {
  id: CLAUDE_ADAPTER_ID,
  label: "Claude",
  LauncherOptions: ClaudeLauncherOptions,
  normalizeTurns: normalizeClaudeTurns,
  composerPolicy: () => claudeComposerPolicy,
};

function ClaudeLauncherOptions({ value, onChange }: LauncherOptionsProps) {
  const permissionMode = typeof value.permissionMode === "string" ? value.permissionMode : "auto";
  const effort = typeof value.effort === "string" ? value.effort : "";

  return (
    <>
      <LauncherSelect
        ariaLabel="Permission mode"
        value={permissionMode}
        options={CLAUDE_PERMISSION_OPTIONS}
        onChange={(next) => {
          const updated = { ...value };
          if (next) {
            updated.permissionMode = next;
          } else {
            delete updated.permissionMode;
          }
          onChange(updated);
        }}
      />
      <LauncherSelect
        ariaLabel="Effort level"
        value={effort}
        options={CLAUDE_EFFORT_OPTIONS}
        onChange={(next) => {
          const updated = { ...value };
          if (next) {
            updated.effort = next;
          } else {
            delete updated.effort;
          }
          onChange(updated);
        }}
      />
    </>
  );
}
