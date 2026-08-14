import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, ComposerPolicy, LauncherOptionsProps } from ".";

export const CURSOR_ADAPTER_ID = "cursor";

// Mirrors CursorLaunchOptions in src-tauri/src/adapters/cursor.rs. Empty is
// Cursor's default agent mode; plan/ask are the only extra modes qmux offers.
const CURSOR_MODE_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Agent mode" },
  { value: "plan", label: "Plan mode", dividerBefore: true },
  { value: "ask", label: "Ask mode" },
];

// Mirrors CursorAdapter::composer_policy. Cursor owns authentication, model
// selection, and shell approvals inside its native TUI, so the qmux composer
// does not surface permission actions.
const cursorComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const cursorUiAdapter: AgentUiAdapter = {
  id: CURSOR_ADAPTER_ID,
  label: "Cursor",
  LauncherOptions: CursorLauncherOptions,
  composerPolicy: () => cursorComposerPolicy,
  supportsFork: false,
};

function CursorLauncherOptions({ value, onChange }: LauncherOptionsProps) {
  const mode = typeof value.mode === "string" ? value.mode : "";

  return (
    <LauncherSelect
      ariaLabel="Cursor mode"
      value={mode}
      options={CURSOR_MODE_OPTIONS}
      onChange={(next) => {
        const updated = { ...value };
        if (next) {
          updated.mode = next;
        } else {
          delete updated.mode;
        }
        onChange(updated);
      }}
    />
  );
}
