import { useCallback, useEffect, useLayoutEffect, useState } from "react";
import {
  claimNativeTerminalPointerForWebDrag,
  respondToRemotePairRequest,
} from "../lib/api";
import { middleTruncate } from "../lib/remoteControl";
import type { RemotePendingPair, RemoteStatus } from "../lib/remoteControl";
import ConfirmDialogActionButton from "./ConfirmDialogActionButton";

interface RemotePairDialogProps {
  request: RemotePendingPair | null;
  onStatus: (status: RemoteStatus) => void;
  onDismiss: (requestId: string) => void;
}

/**
 * The approval prompt for one pairing request. It is app-level rather than
 * popover-local on purpose: the QR is scanned on the phone, often with the
 * popover already dismissed, and an authorization prompt nobody sees is an
 * authorization prompt nobody made.
 */
export default function RemotePairDialog({
  request,
  onStatus,
  onDismiss,
}: RemotePairDialogProps) {
  const [readOnly, setReadOnly] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestId = request?.requestId ?? null;

  // Each request answers for itself: never inherit the last one's read-only box.
  useEffect(() => {
    setReadOnly(false);
    setBusy(false);
    setError(null);
  }, [requestId]);

  // The backdrop can cover a native Ghostty surface, whose event monitor eats
  // mouseup before a DOM button can produce a click. Same claim useConfirm makes.
  useLayoutEffect(() => {
    if (!requestId) {
      return;
    }
    return claimNativeTerminalPointerForWebDrag();
  }, [requestId]);

  const respond = useCallback(
    async (approved: boolean) => {
      if (!request) {
        return;
      }
      setBusy(true);
      setError(null);
      try {
        onStatus(await respondToRemotePairRequest(request.requestId, approved, readOnly));
        onDismiss(request.requestId);
      } catch (err) {
        setError(String(err));
        setBusy(false);
      }
    },
    [onDismiss, onStatus, readOnly, request],
  );

  if (!request) {
    return null;
  }

  return (
    <div className="confirm-dialog-backdrop" role="presentation">
      <div
        className="confirm-dialog remote-pair-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={`Pair ${request.deviceName}?`}
        onKeyDown={(event) => {
          if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            // Dismissing an authorization prompt is a denial, not a deferral —
            // the connection is waiting on an answer either way.
            void respond(false);
          }
        }}
      >
        <h2>Pair “{request.deviceName}”?</h2>
        <p>
          It will see every terminal in this workspace and be able to send input, queue
          turns, and answer permission prompts.
        </p>
        <div className="remote-pair-key" title={request.endpointId}>
          {middleTruncate(request.endpointId)}
        </div>
        <label className="remote-pair-readonly">
          <input
            type="checkbox"
            className="settings-checkbox"
            checked={readOnly}
            disabled={busy}
            onChange={(event) => setReadOnly(event.target.checked)}
          />
          <span>Read only</span>
        </label>
        {error ? <p className="confirm-dialog-error">{error}</p> : null}
        <div className="confirm-dialog-actions">
          {/* Deny takes the default focus: a prompt that arrives unbidden must
              not be one Return away from handing over the workspace. */}
          <button
            type="button"
            className="control-button"
            autoFocus
            disabled={busy}
            onClick={() => void respond(false)}
          >
            Deny
          </button>
          <ConfirmDialogActionButton
            className="remote-pair-accept"
            pending={busy}
            pendingLabel="Pairing…"
            onClick={() => void respond(true)}
          >
            Pair
          </ConfirmDialogActionButton>
        </div>
      </div>
    </div>
  );
}
