import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, LauncherOptionsProps } from ".";

export const CURSOR_ADAPTER_ID = "cursor";

// Mirrors CursorLaunchOptions in src-tauri/src/adapters/cursor.rs. Empty is
// Cursor's default agent mode; plan/ask are the only extra modes qmux offers.
const CURSOR_MODE_OPTIONS: LauncherSelectOption[] = [
  { value: "", label: "Agent mode" },
  { value: "plan", label: "Plan mode", dividerBefore: true },
  { value: "ask", label: "Ask mode" },
];

export const cursorUiAdapter: AgentUiAdapter = {
  id: CURSOR_ADAPTER_ID,
  label: "Cursor",
  LauncherOptions: CursorLauncherOptions,
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
