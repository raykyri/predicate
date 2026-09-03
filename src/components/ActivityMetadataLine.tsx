import { findAgentUiAdapter } from "../adapters";
import type { ActivityEvent } from "../lib/activity";
import {
  CUSTOM_MODEL,
  formatLauncherModelLabel,
  modelPresetsFor,
} from "../lib/launcherModels";
import { formatRelativeTime } from "../lib/transcriptSessions";

function adapterDisplayLabel(adapter: string): string {
  if (!adapter) return "";
  return findAgentUiAdapter(adapter)?.label ?? formatLauncherModelLabel(adapter, adapter);
}

/** Preset names like Fable/Opus. Product ids (`gpt-5.6-sol`) and custom
 * slugs are omitted so the line can fall back to just the adapter. */
function humanReadableModelName(adapter: string, model?: string | null): string | null {
  if (!model || model === CUSTOM_MODEL) return null;
  if (!modelPresetsFor(adapter).includes(model)) return null;
  const label = formatLauncherModelLabel(adapter, model);
  if (/[\d._-]/.test(label)) return null;
  return label;
}

/** One-line summary for Recent Activity. Research asks name the model when
 * it is a readable preset; follow-ups name the thread instead. */
export function formatActivityMetadataSummary(event: ActivityEvent): string {
  if (event.object.kind === "research-query") {
    if (event.relationship?.kind === "follow-up") {
      return `Follow-up in '${event.context?.label ?? "Research"}'`;
    }
    const adapterLabel = adapterDisplayLabel(event.execution?.adapter ?? "");
    const modelName = humanReadableModelName(
      event.execution?.adapter ?? "",
      event.execution?.model,
    );
    if (adapterLabel && modelName) return `You asked ${adapterLabel} ${modelName}`;
    if (adapterLabel) return `You asked ${adapterLabel}`;
    return "You asked";
  }
  return [event.actor.label, event.action.label, event.object.label].filter(Boolean).join(" ");
}

/** App-wide renderer for activity grammar slots. Metadata stays outside the
 * content surface because it describes the event, not the object payload. */
export default function ActivityMetadataLine({ event }: { event: ActivityEvent }) {
  const finiteTime = Number.isFinite(event.occurredAt);
  return (
    <div
      className="activity-metadata"
      title={finiteTime ? new Date(event.occurredAt).toLocaleString() : undefined}
    >
      <span className="activity-metadata-summary">{formatActivityMetadataSummary(event)}</span>
      {finiteTime ? (
        <time dateTime={new Date(event.occurredAt).toISOString()}>
          {formatRelativeTime(event.occurredAt)}
        </time>
      ) : null}
    </div>
  );
}
