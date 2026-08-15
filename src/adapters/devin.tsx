import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, ComposerPolicy, LauncherOptionsProps } from ".";

export const DEVIN_ADAPTER_ID = "devin";

// Mirrors DEVIN_PERMISSION_MODES in src-tauri/src/adapters/devin.rs. Empty is
// Devin's default (`auto`). `autonomous` requires `--sandbox` and is omitted.
const DEVIN_PERMISSION_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Default permissions" },
  { value: "auto", label: "Auto (ask for writes)", dividerBefore: true },
  { value: "accept-edits", label: "Accept edits" },
  { value: "smart", label: "Smart" },
  { value: "dangerous", label: "Bypass permissions", tone: "danger" },
];

const devinComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const devinUiAdapter: AgentUiAdapter = {
  id: DEVIN_ADAPTER_ID,
  label: "Devin",
  LauncherOptions: DevinLauncherOptions,
  composerPolicy: () => devinComposerPolicy,
  supportsFork: false,
};

function DevinLauncherOptions({ value, onChange }: LauncherOptionsProps) {
  const permissionMode = typeof value.permissionMode === "string" ? value.permissionMode : "";

  return (
    <LauncherSelect
      ariaLabel="Permission mode"
      value={permissionMode}
      options={DEVIN_PERMISSION_OPTIONS}
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
  );
}
