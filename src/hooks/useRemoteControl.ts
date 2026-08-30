import { useCallback, useEffect, useRef, useState } from "react";
import { listenToRemoteEvents, remoteStatusGet } from "../lib/api";
import { EMPTY_REMOTE_STATUS } from "../lib/remoteControl";
import type { RemotePendingPair, RemoteStatus } from "../lib/remoteControl";

export interface RemoteControlState {
  status: RemoteStatus;
  /** The pairing request awaiting an answer, from an event or the boot status. */
  pendingPair: RemotePendingPair | null;
  /** Applies the snapshot a mutation returned, without waiting for its event. */
  applyStatus: (status: RemoteStatus) => void;
  /** Re-reads the truth; used after events that carry no snapshot. */
  refresh: () => void;
  /** Forgets a request the dialog has answered (or that resolved elsewhere). */
  dismissPendingPair: (requestId?: string) => void;
}

const asPendingPair = (payload: Record<string, unknown>): RemotePendingPair | null => {
  const { requestId, deviceName, endpointId } = payload;
  if (
    typeof requestId !== "string" ||
    typeof deviceName !== "string" ||
    typeof endpointId !== "string"
  ) {
    return null;
  }
  return { requestId, deviceName, endpointId };
};

/**
 * The app-level half of remote control: one status snapshot, kept current by a
 * filtered `remote.*` listener that is independent of the main event reducer.
 * It lives above the popover because the approval dialog must appear whether or
 * not the popover is open, and because a pairing request can be waiting already
 * when the webview loads (a reload during pairing, say).
 *
 * `remote.status_changed` carries a whole RemoteStatus; the session and
 * pair-resolved events carry only names, so those re-read the status instead of
 * patching a guess into it.
 */
export function useRemoteControl(): RemoteControlState {
  const [status, setStatus] = useState<RemoteStatus>(EMPTY_REMOTE_STATUS);
  const [pendingPair, setPendingPair] = useState<RemotePendingPair | null>(null);
  // Bumped by every applied snapshot so a slow refresh can't overwrite a newer
  // one that a mutation or an event already delivered.
  const revisionRef = useRef(0);
  const mountedRef = useRef(true);

  const applyStatus = useCallback((next: RemoteStatus) => {
    if (!mountedRef.current) {
      return;
    }
    revisionRef.current += 1;
    setStatus(next);
    setPendingPair((current) => next.pendingPair ?? current);
  }, []);

  const refresh = useCallback(() => {
    const revision = revisionRef.current;
    void remoteStatusGet()
      .then((next) => {
        if (mountedRef.current && revisionRef.current === revision) {
          applyStatus(next);
        }
      })
      .catch(() => undefined);
  }, [applyStatus]);

  const dismissPendingPair = useCallback((requestId?: string) => {
    setPendingPair((current) =>
      current && (requestId === undefined || current.requestId === requestId) ? null : current,
    );
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    refresh();
    const unlisten = listenToRemoteEvents((event) => {
      switch (event.type) {
        case "remote.status_changed":
          applyStatus(event.payload as unknown as RemoteStatus);
          break;
        case "remote.pair_request": {
          const request = asPendingPair(event.payload);
          if (request) {
            setPendingPair(request);
          }
          break;
        }
        case "remote.pair_resolved": {
          const request = asPendingPair(event.payload);
          dismissPendingPair(request?.requestId);
          refresh();
          break;
        }
        case "remote.session_connected":
        case "remote.session_disconnected":
          refresh();
          break;
        default:
          break;
      }
    });
    return () => {
      mountedRef.current = false;
      void unlisten.then((off) => off()).catch(() => undefined);
    };
  }, [applyStatus, dismissPendingPair, refresh]);

  return { status, pendingPair, applyStatus, refresh, dismissPendingPair };
}
