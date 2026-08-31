import type { ActivityEvent } from "../lib/activity";
import { formatRelativeTime } from "../lib/transcriptSessions";

function activityStateClass(state: ActivityEvent["state"]): string {
  if (!state) return "";
  if (state.kind === "failed") return " is-failed";
  if (state.kind === "cancelled") return " is-cancelled";
  if (state.kind === "queued" || state.kind === "starting" || state.kind === "running") {
    return " is-active";
  }
  return "";
}

/** App-wide renderer for activity grammar slots. Metadata stays outside the
 * content surface because it describes the event, not the object payload. */
export default function ActivityMetadataLine({ event }: { event: ActivityEvent }) {
  const finiteTime = Number.isFinite(event.occurredAt);
  const execution = event.execution
    ? [event.execution.adapter, event.execution.model].filter(Boolean).join(" · ")
    : null;
  return (
    <div
      className="activity-metadata"
      title={finiteTime ? new Date(event.occurredAt).toLocaleString() : undefined}
    >
      <span className="activity-metadata-primary">
        {event.actor.label} {event.action.label}
      </span>
      <span>{event.object.label}</span>
      {event.relationship ? <span>{event.relationship.label}</span> : null}
      {event.context ? <span className="activity-metadata-context">{event.context.label}</span> : null}
      {execution ? <span>{execution}</span> : null}
      {finiteTime ? (
        <time dateTime={new Date(event.occurredAt).toISOString()}>
          {formatRelativeTime(event.occurredAt)}
        </time>
      ) : null}
      {event.state ? (
        <span className={`activity-metadata-state${activityStateClass(event.state)}`}>
          {event.state.label}
        </span>
      ) : null}
    </div>
  );
}
