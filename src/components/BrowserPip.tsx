import { Bot, Maximize2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useNativeWebOverlayRegion } from "../hooks/useNativeWebOverlayRegion";
import {
  getBrowserAutomationPreview,
  listBrowserAutomationTargets,
  type BrowserAutomationPreview,
  type BrowserAutomationTarget,
} from "../lib/api";
import { browserPipPageLabel, selectBrowserPipTargets } from "../lib/browserPip";

const TARGET_POLL_MS = 1_000;
const TARGET_ERROR_RETRY_MS = 3_000;
const PREVIEW_POLL_MS = 650;
const PREVIEW_MAX_WIDTH = 320;
const PREVIEW_MAX_HEIGHT = 180;

export type BrowserPipPane = {
  id: string;
  title: string;
};

interface BrowserPipProps {
  target: BrowserAutomationTarget;
  paneTitle: string;
  onOpen: (target: BrowserAutomationTarget) => void;
}

function BrowserPip({ target, paneTitle, onOpen }: BrowserPipProps) {
  const [preview, setPreview] = useState<BrowserAutomationPreview | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: number | null = null;
    let requestInFlight = false;
    setPreview(null);

    const schedule = (delay: number) => {
      if (!cancelled) {
        timer = window.setTimeout(() => void poll(), delay);
      }
    };
    const poll = async () => {
      if (requestInFlight) {
        return;
      }
      if (document.hidden) {
        schedule(TARGET_POLL_MS);
        return;
      }
      requestInFlight = true;
      try {
        const next = await getBrowserAutomationPreview(
          target.paneId,
          target.tabId,
          PREVIEW_MAX_WIDTH,
          PREVIEW_MAX_HEIGHT,
        );
        if (!cancelled) {
          setPreview(next);
        }
      } catch {
        // Keep the last good frame. Target discovery removes a card once the
        // target is truly gone; transient capture errors should not flash it.
      } finally {
        requestInFlight = false;
        schedule(PREVIEW_POLL_MS);
      }
    };
    const handleVisibility = () => {
      if (!document.hidden) {
        if (timer !== null) {
          window.clearTimeout(timer);
          timer = null;
        }
        void poll();
      }
    };

    document.addEventListener("visibilitychange", handleVisibility);
    void poll();
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", handleVisibility);
      if (timer !== null) {
        window.clearTimeout(timer);
      }
    };
  }, [target.paneId, target.tabId]);

  const pageLabel = browserPipPageLabel(target);
  return (
    <button
      type="button"
      className="browser-pip-card"
      title={`Open agent browser from ${paneTitle}`}
      aria-label={`Open agent browser from ${paneTitle}: ${pageLabel}`}
      onClick={() => onOpen(target)}
    >
      <span className="browser-pip-chrome">
        <Bot size={12} aria-hidden="true" />
        <span className="browser-pip-pane-title">{paneTitle}</span>
        <Maximize2 className="browser-pip-open-icon" size={12} aria-hidden="true" />
      </span>
      <span className="browser-pip-viewport">
        {preview ? (
          <img
            className="browser-pip-image"
            src={preview.imageDataUrl}
            alt=""
            draggable={false}
            aria-hidden="true"
          />
        ) : (
          <span className="browser-pip-loading">Connecting…</span>
        )}
      </span>
      <span className="browser-pip-page-title">{pageLabel}</span>
    </button>
  );
}

function sameTargets(
  left: BrowserAutomationTarget[],
  right: BrowserAutomationTarget[],
): boolean {
  return (
    left.length === right.length &&
    left.every((target, index) => {
      const other = right[index];
      return (
        other !== undefined &&
        target.paneId === other.paneId &&
        target.tabId === other.tabId &&
        target.url === other.url &&
        target.title === other.title
      );
    })
  );
}

interface BrowserPipRailProps {
  panes: BrowserPipPane[];
  expandedAgentPaneId: string | null;
  belowBrowserOverlay: boolean;
  onOpen: (target: BrowserAutomationTarget) => void;
}

export default function BrowserPipRail({
  panes,
  expandedAgentPaneId,
  belowBrowserOverlay,
  onOpen,
}: BrowserPipRailProps) {
  const [targets, setTargets] = useState<BrowserAutomationTarget[]>([]);
  const [showAll, setShowAll] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let timer: number | null = null;
    let requestInFlight = false;
    const poll = async () => {
      if (requestInFlight) {
        return;
      }
      let delay = TARGET_POLL_MS;
      if (!document.hidden) {
        requestInFlight = true;
        try {
          const next = await listBrowserAutomationTargets();
          if (!cancelled) {
            setTargets((current) => (sameTargets(current, next) ? current : next));
          }
        } catch {
          // Browser support may be unavailable on this machine. Retry slowly
          // and preserve any targets already visible through a transient miss.
          delay = TARGET_ERROR_RETRY_MS;
        } finally {
          requestInFlight = false;
        }
      }
      if (!cancelled) {
        timer = window.setTimeout(() => void poll(), delay);
      }
    };
    const handleVisibility = () => {
      if (!document.hidden) {
        if (timer !== null) {
          window.clearTimeout(timer);
          timer = null;
        }
        void poll();
      }
    };

    document.addEventListener("visibilitychange", handleVisibility);
    void poll();
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", handleVisibility);
      if (timer !== null) {
        window.clearTimeout(timer);
      }
    };
  }, []);

  useEffect(() => {
    if (targets.length <= 3) {
      setShowAll(false);
    }
  }, [targets.length]);

  const paneOrder = useMemo(() => panes.map((pane) => pane.id), [panes]);
  const paneTitleById = useMemo(
    () => new Map(panes.map((pane) => [pane.id, pane.title])),
    [panes],
  );
  const selection = useMemo(
    () =>
      selectBrowserPipTargets(
        targets,
        paneOrder,
        expandedAgentPaneId,
        showAll ? Number.MAX_SAFE_INTEGER : undefined,
      ),
    [expandedAgentPaneId, paneOrder, showAll, targets],
  );
  const layoutKey = `${belowBrowserOverlay ? "below" : "top"}:${selection.visible
    .map((target) => `${target.paneId}:${target.tabId}`)
    .join("|")}:${selection.overflow}`;
  const railRef = useNativeWebOverlayRegion<HTMLDivElement>(
    selection.visible.length > 0,
    layoutKey,
  );

  if (selection.visible.length === 0) {
    return null;
  }
  return (
    <div
      ref={railRef}
      className={`browser-pip-rail${belowBrowserOverlay ? " is-below-browser-overlay" : ""}`}
      onPointerDown={(event) => event.stopPropagation()}
      role="region"
      aria-label="Running agent browsers"
    >
      {selection.visible.map((target) => (
        <BrowserPip
          key={`${target.paneId}:${target.tabId}`}
          target={target}
          paneTitle={paneTitleById.get(target.paneId) ?? "Terminal"}
          onOpen={onOpen}
        />
      ))}
      {selection.overflow > 0 ? (
        <button
          type="button"
          className="browser-pip-overflow"
          title={`${selection.overflow} more running agent browser${selection.overflow === 1 ? "" : "s"}`}
          onClick={() => setShowAll(true)}
        >
          +{selection.overflow} more
        </button>
      ) : showAll && selection.visible.length > 3 ? (
        <button
          type="button"
          className="browser-pip-overflow"
          title="Show fewer running agent browsers"
          onClick={() => setShowAll(false)}
        >
          Show fewer
        </button>
      ) : null}
    </div>
  );
}
