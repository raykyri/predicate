import { useEffect, useState } from "react";
import type { RemoteConnectionInfo } from "../types";
import { remoteConnectionPresentation } from "../lib/remoteConnection";

/** Refresh time labels without rerendering the terminal or the entire app. */
export default function RemoteConnectionDetailsText({ connection, active = true, className }: {
  connection?: RemoteConnectionInfo | null;
  active?: boolean;
  className?: string;
}) {
  const [, refresh] = useState(0);
  const view = remoteConnectionPresentation(connection);
  const interval = view.refreshEveryMs;
  useEffect(() => {
    if (!active || interval == null) return;
    const timer = window.setInterval(() => refresh(value => value + 1), interval);
    return () => window.clearInterval(timer);
  }, [active, interval]);
  const description = view.lines.join("\n");
  return <span className={["remote-connection-details-text", className].filter(Boolean).join(" ")}>
    {description ? <span>{description}</span> : null}
    {view.lastConnection ? <span className="remote-connection-last-connection">{view.lastConnection}</span> : null}
  </span>;
}
