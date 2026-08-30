import DOMPurify from "dompurify";
import { Globe, QrCode, Wifi } from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { RefObject } from "react";
import { createPortal } from "react-dom";
import {
  beginRemotePairing,
  cancelRemotePairing,
  claimNativeTerminalPointerForWebDrag,
  revokeRemoteDevice,
  setRemoteDeviceReadOnly,
  setRemoteEnabled,
  setRemoteLaunchEnabled,
  setRemoteReach,
} from "../lib/api";
import { clampContextMenuToViewport } from "../lib/appHelpers";
import {
  formatPairingCode,
  formatPairingCountdown,
  pairingRemainingMs,
  remoteDeviceStatusLine,
  remotePopoverSections,
  remoteSessionStatusLine,
  remoteStatusSummary,
  sortRemoteDevices,
} from "../lib/remoteControl";
import type { RemotePairingPanel, RemoteStatus } from "../lib/remoteControl";

interface RemoteControlPopoverProps {
  status: RemoteStatus;
  // The sidebar header button the panel hangs from.
  anchorRef: RefObject<HTMLButtonElement | null>;
  // Every mutation answers with a fresh RemoteStatus; hand it straight up.
  onStatus: (status: RemoteStatus) => void;
  onClose: () => void;
}

const POPOVER_WIDTH = 300;
const ANCHOR_GAP = 6;

// The QR is rendered by the backend (the `qrcode` crate) and injected with
// dangerouslySetInnerHTML into the privileged webview, so it goes through
// DOMPurify's SVG profile first — the same treatment agent-authored diagrams
// get in DiagramBlock. A QR needs no links, scripts, or external references, so
// the plain profile is the whole configuration.
const QR_SANITIZE_CONFIG = { USE_PROFILES: { svg: true } } as const;

function sanitizePairingQr(svg: string): string {
  return DOMPurify.sanitize(svg, QR_SANITIZE_CONFIG);
}

/**
 * The remote-control panel: master switch, reach, pairing, paired devices, live
 * sessions, and the on-at-launch preference.
 *
 * Off is rendered as copy rather than a wall of disabled controls, because off
 * genuinely means absent — no endpoint, no discovery record, no relay. Reach is
 * the one control that does not apply on click: "anywhere" publishes this Mac's
 * address, which is a different consent from "on", so it is confirmed inline
 * before `remote_set_reach` is called.
 */
export default function RemoteControlPopover({
  status,
  anchorRef,
  onStatus,
  onClose,
}: RemoteControlPopoverProps) {
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{ x: number; y: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pairing, setPairing] = useState<{
    panel: RemotePairingPanel;
    expiresAt: number;
  } | null>(null);
  const [pairingBusy, setPairingBusy] = useState(false);
  const [reachConfirmOpen, setReachConfirmOpen] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  const sections = remotePopoverSections(status, {
    pairingOpen: pairing !== null,
    reachConfirmOpen,
  });
  const devices = useMemo(() => sortRemoteDevices(status.devices), [status.devices]);
  const qrMarkup = useMemo(
    () => (pairing ? sanitizePairingQr(pairing.panel.qrSvg) : null),
    [pairing],
  );

  const run = useCallback(
    async (action: () => Promise<RemoteStatus>) => {
      setError(null);
      try {
        onStatus(await action());
      } catch (err) {
        setError(String(err));
      }
    },
    [onStatus],
  );

  // Turning the endpoint off invalidates anything that only exists while it is
  // listening, so clear the local panels rather than leaving a dead QR behind.
  const toggleEnabled = useCallback(() => {
    const next = !status.enabled;
    if (!next) {
      setPairing(null);
      setReachConfirmOpen(false);
    }
    void run(() => setRemoteEnabled(next));
  }, [run, status.enabled]);

  const beginPairing = useCallback(async () => {
    setError(null);
    setPairingBusy(true);
    try {
      const panel = await beginRemotePairing();
      setPairing({ panel, expiresAt: Date.now() + panel.expiresInMs });
    } catch (err) {
      setError(String(err));
    } finally {
      setPairingBusy(false);
    }
  }, []);

  const stopPairing = useCallback(() => {
    setPairing(null);
    void run(cancelRemotePairing);
  }, [run]);

  // One clock for the whole panel: the pairing countdown needs a second, and the
  // "last seen" lines get to be fresh for free. It only runs while the popover
  // is mounted, which is only while it is open.
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  const remainingMs = pairing ? pairingRemainingMs(pairing.expiresAt, now) : 0;
  useEffect(() => {
    if (pairing && remainingMs <= 0) {
      // The backend burns the window on its own schedule; drop the dead QR so
      // nobody photographs a code that can no longer be presented.
      setPairing(null);
    }
  }, [pairing, remainingMs]);

  const position = useCallback(() => {
    const anchor = anchorRef.current;
    if (!anchor) {
      return;
    }
    const rect = anchor.getBoundingClientRect();
    const height = popoverRef.current?.getBoundingClientRect().height ?? 0;
    setPos(
      clampContextMenuToViewport({
        // Trailing-aligned with the header controls, opening downward.
        x: rect.right - POPOVER_WIDTH,
        y: rect.bottom + ANCHOR_GAP,
        width: POPOVER_WIDTH,
        height,
      }),
    );
  }, [anchorRef]);

  // Re-measure whenever the content that drives the height changes: the panel
  // grows by a QR, a confirm block, or a device row.
  useLayoutEffect(() => {
    position();
    const onReflow = () => position();
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [position, status, pairing, reachConfirmOpen, error]);

  useEffect(() => {
    const handlePointerDown = (event: MouseEvent) => {
      const target = event.target as Node;
      if (!popoverRef.current?.contains(target) && !anchorRef.current?.contains(target)) {
        onClose();
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
      }
    };
    document.addEventListener("mousedown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [anchorRef, onClose]);

  // The panel can overhang a native Ghostty surface, whose event monitor would
  // otherwise consume mouseup before a DOM button sees a click. The claim is
  // reference-counted and releases when the popover closes.
  useLayoutEffect(() => claimNativeTerminalPointerForWebDrag(), []);

  return createPortal(
    <div
      ref={popoverRef}
      className="popover-surface remote-control-popover"
      role="dialog"
      aria-label="Remote control"
      style={
        pos
          ? { left: pos.x, top: pos.y, width: POPOVER_WIDTH }
          : { left: -9999, top: -9999, width: POPOVER_WIDTH }
      }
    >
      <header className="remote-control-header">
        <div className="remote-control-heading">
          <span className="remote-control-title">Remote control</span>
          <span className="remote-control-subtitle">{remoteStatusSummary(status)}</span>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={status.enabled}
          aria-label="Remote control"
          className={`remote-control-switch${status.enabled ? " is-on" : ""}`}
          onClick={toggleEnabled}
        >
          <span className="remote-control-switch-knob" aria-hidden="true" />
        </button>
      </header>

      {error ? (
        <p className="remote-control-error" role="alert">
          {error}
        </p>
      ) : null}

      {sections.offExplainer ? (
        <p className="remote-control-explainer">
          Nothing is listening. No endpoint is bound, no discovery record is published,
          and no relay connection is open.
        </p>
      ) : null}

      {sections.modeToggle ? (
        <div
          className="sidebar-mode-toggle remote-control-mode-toggle"
          role="group"
          aria-label="Reach"
        >
          <button
            type="button"
            className={status.reach === "local" ? "is-selected" : ""}
            aria-pressed={status.reach === "local"}
            onClick={() => {
              setReachConfirmOpen(false);
              if (status.reach !== "local") {
                void run(() => setRemoteReach("local"));
              }
            }}
          >
            <Wifi size={13} aria-hidden="true" />
            <span>This network</span>
          </button>
          <button
            type="button"
            className={`${status.reach === "anywhere" ? "is-selected" : ""}${
              reachConfirmOpen ? " is-pending" : ""
            }`}
            aria-pressed={status.reach === "anywhere"}
            onClick={() => {
              if (status.reach !== "anywhere") {
                setReachConfirmOpen(true);
              }
            }}
          >
            <Globe size={13} aria-hidden="true" />
            <span>Anywhere</span>
          </button>
        </div>
      ) : null}

      {sections.reachConfirm ? (
        <div className="remote-control-confirm">
          <p>
            qmux will hold a connection to n0&apos;s relay servers and publish this
            Mac&apos;s address so paired devices can find it.
          </p>
          <div className="remote-control-confirm-actions">
            <button
              type="button"
              className="control-button"
              onClick={() => setReachConfirmOpen(false)}
            >
              Keep local
            </button>
            <button
              type="button"
              className="control-button remote-control-confirm-accept"
              onClick={() => {
                setReachConfirmOpen(false);
                void run(() => setRemoteReach("anywhere"));
              }}
            >
              Enable
            </button>
          </div>
        </div>
      ) : null}

      {sections.pairButton ? (
        <button
          type="button"
          className="control-button remote-control-pair-button"
          disabled={pairingBusy}
          onClick={() => void beginPairing()}
        >
          <QrCode size={14} aria-hidden="true" />
          <span>Pair a new device</span>
        </button>
      ) : null}

      {sections.pairingPanel && pairing ? (
        <div className="remote-control-pairing">
          {qrMarkup ? (
            <div
              className="remote-control-qr"
              role="img"
              aria-label="Pairing QR code"
              // Sanitized above with DOMPurify's SVG profile.
              dangerouslySetInnerHTML={{ __html: qrMarkup }}
            />
          ) : null}
          <div className="remote-control-code">{formatPairingCode(pairing.panel.code)}</div>
          <p className="remote-control-pairing-hint">
            Scan it in qmux on the phone. Single use, expires in{" "}
            <span className="remote-control-countdown">
              {formatPairingCountdown(remainingMs)}
            </span>
            .
          </p>
          <button type="button" className="control-button" onClick={stopPairing}>
            Cancel
          </button>
        </div>
      ) : null}

      {sections.devicesEmpty ? (
        <p className="remote-control-empty">
          No devices paired yet. A paired device sees every terminal in this workspace.
        </p>
      ) : null}

      {sections.devices ? (
        <section className="remote-control-section" aria-label="Paired devices">
          <h3 className="remote-control-section-title">Paired devices</h3>
          <ul className="remote-control-list">
            {devices.map((device) => (
              <li key={device.endpointId} className="remote-control-device">
                <div className="remote-control-device-main">
                  <span className="remote-control-device-name">
                    <span className="remote-control-device-label">{device.name}</span>
                    {device.readOnly ? (
                      <span className="remote-control-badge">Read only</span>
                    ) : null}
                  </span>
                  <span
                    className={`remote-control-device-status${
                      device.connected ? " is-connected" : ""
                    }`}
                  >
                    {remoteDeviceStatusLine(device, now)}
                  </span>
                </div>
                <div className="remote-control-device-actions">
                  <button
                    type="button"
                    className="link-button remote-control-device-action"
                    onClick={() =>
                      void run(() =>
                        setRemoteDeviceReadOnly(device.endpointId, !device.readOnly),
                      )
                    }
                  >
                    {device.readOnly ? "Allow input" : "Read only"}
                  </button>
                  <button
                    type="button"
                    className="link-button remote-control-device-action is-danger"
                    onClick={() => void run(() => revokeRemoteDevice(device.endpointId))}
                  >
                    Revoke
                  </button>
                </div>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {sections.sessions ? (
        <section className="remote-control-section" aria-label="Live sessions">
          <h3 className="remote-control-section-title">Live sessions</h3>
          <ul className="remote-control-list">
            {status.sessions.map((session) => (
              <li key={session.endpointId} className="remote-control-session">
                <span className="remote-control-session-dot" aria-hidden="true" />
                <span className="remote-control-device-label">{session.deviceName}</span>
                <span className="remote-control-session-status">
                  {remoteSessionStatusLine(session, now)}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {sections.launchToggle ? (
        <label className="remote-control-launch">
          <input
            type="checkbox"
            className="settings-checkbox"
            checked={status.launchEnabled}
            onChange={(event) => {
              const next = event.target.checked;
              void run(() => setRemoteLaunchEnabled(next));
            }}
          />
          <span>Turn on when qmux launches</span>
        </label>
      ) : null}
    </div>,
    document.body,
  );
}
