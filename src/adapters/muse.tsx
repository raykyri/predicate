import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, ComposerPolicy, LauncherOptionsProps } from ".";

export const MUSE_ADAPTER_ID = "muse";

// Mirrors MUSE_REASONING_EFFORTS in src-tauri/src/adapters/muse.rs, which
// validates the choice before it reaches the CLI.
const MUSE_REASONING_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Default reasoning" },
  { value: "none", label: "No reasoning", dividerBefore: true },
  { value: "minimal", label: "Minimal reasoning" },
  { value: "low", label: "Low reasoning" },
  { value: "medium", label: "Medium reasoning" },
  { value: "high", label: "High reasoning" },
  { value: "xhigh", label: "Extra high reasoning" },
  { value: "ultra", label: "Ultra reasoning" },
];

// Mirrors MUSE_APPROVAL_MODES. Muse's own default is "on-request"; qmux does not
// offer `--yolo`, which would disable approval *and* the sandbox for the run.
const MUSE_APPROVAL_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Default approvals" },
  {
    value: "untrusted",
    label: "Ask for untrusted commands",
    dividerBefore: true,
  },
  { value: "on-request", label: "Allow approval requests" },
  { value: "never", label: "Block approval requests" },
];

// Mirrors the Rust MuseAdapter::composer_policy so the composer enables/queues/
// steers turns identically to the backend.
const museComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: ["starting", "running"],
  permissionActions: [],
};

export const museUiAdapter: AgentUiAdapter = {
  id: MUSE_ADAPTER_ID,
  label: "Muse",
  LauncherOptions: MuseLauncherOptions,
  composerPolicy: () => museComposerPolicy,
};

function MuseLauncherOptions({ value, onChange }: LauncherOptionsProps) {
  return (
    <>
      <LauncherSelect
        ariaLabel="Reasoning level"
        value={stringOption(value.reasoningEffort)}
        options={MUSE_REASONING_OPTIONS}
        onChange={(next) => setOption(value, onChange, "reasoningEffort", next)}
      />
      <LauncherSelect
        ariaLabel="Approval policy"
        value={stringOption(value.approvalMode)}
        options={MUSE_APPROVAL_OPTIONS}
        onChange={(next) => setOption(value, onChange, "approvalMode", next)}
      />
    </>
  );
}

function stringOption(value: unknown): string {
  return typeof value === "string" ? value : "";
}

function setOption(
  value: Record<string, unknown>,
  onChange: (next: Record<string, unknown>) => void,
  key: string,
  nextValue: string,
) {
  const next = { ...value };
  if (nextValue === "") {
    delete next[key];
  } else {
    next[key] = nextValue;
  }
  onChange(next);
}
