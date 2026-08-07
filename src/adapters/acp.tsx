import { LauncherSelect } from "../components/LauncherSelect";
import type { LauncherSelectOption } from "../components/LauncherSelect";
import type { AgentUiAdapter, ComposerPolicy, LauncherOptionsProps } from ".";

export const ACP_ADAPTER_ID = "acp";

// Mirrors AcpAdapter::composer_policy in src-tauri/src/adapters/acp.rs, and the
// two empty lists are the load-bearing part. ACP has one `session/prompt` per
// turn and no mid-turn steer — text sent mid-turn would land in the bridge's
// stdin and surface as the *next* prompt — and a permission request is answered
// by typing a number into the pane, not by a button that writes "y". Falling
// back to Claude's policy (which is what happened while this adapter didn't
// exist) offers both.
const acpComposerPolicy: ComposerPolicy = {
  readyStatuses: ["awaitingInput", "done", "idle"],
  queueStatuses: ["starting", "running", "awaitingPermission"],
  steerStatuses: [],
  permissionActions: [],
};

export const acpUiAdapter: AgentUiAdapter = {
  id: ACP_ADAPTER_ID,
  label: "ACP",
  LauncherOptions: AcpLauncherOptions,
  composerPolicy: () => acpComposerPolicy,
};

/** Picks which configured ACP agent to launch.
 *
 * The choice is sent as the adapter's `agent` launch option, which is the key
 * into `adapters.acp.agents` (or the registry store). With a single agent
 * configured there is nothing to choose, so the control is omitted and the
 * backend resolves it. */
function AcpLauncherOptions({ value, onChange, config }: LauncherOptionsProps) {
  const agents = config?.acpAgents ?? [];
  if (agents.length < 2) {
    return null;
  }
  // `defaultAgent` first, so LauncherSelect's reconcile-to-options[0] both
  // displays it and writes it into the launch options — without that the
  // header would show an agent the request never actually names.
  const options: LauncherSelectOption[] = [...agents]
    .sort((left, right) => Number(right.default) - Number(left.default))
    .map((agent) => ({ value: agent.id, label: agent.label }));

  return (
    <LauncherSelect
      ariaLabel="ACP agent"
      value={typeof value.agent === "string" ? value.agent : ""}
      options={options}
      onChange={(next) => {
        const updated = { ...value };
        if (next) {
          updated.agent = next;
        } else {
          delete updated.agent;
        }
        onChange(updated);
      }}
    />
  );
}
