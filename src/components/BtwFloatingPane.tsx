import { useLayoutEffect, useRef, useState } from "react";
import type {
  KeyboardEvent as ReactKeyboardEvent,
  PointerEvent as ReactPointerEvent,
  ReactNode,
} from "react";

interface BtwFloatingPaneProps {
  title: string;
  children: ReactNode;
  onActivate: () => void;
  offset?: number;
}

interface Point {
  x: number;
  y: number;
}

interface PaneSize {
  width: number;
  height: number;
}

const FRAME_INSET = 8;
const MIN_WIDTH = 280;
const MIN_HEIGHT = 240;
const KEYBOARD_RESIZE_STEP = 16;

function clampPaneSize(width: number, height: number, maxWidth: number, maxHeight: number) {
  return {
    width: Math.min(maxWidth, Math.max(Math.min(MIN_WIDTH, maxWidth), width)),
    height: Math.min(maxHeight, Math.max(Math.min(MIN_HEIGHT, maxHeight), height)),
  };
}

/** A lightweight transcript window constrained to its owning right-pane cell. */
export default function BtwFloatingPane({
  title,
  children,
  onActivate,
  offset = 0,
}: BtwFloatingPaneProps) {
  const frameRef = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState<Point>(() => ({
    x: FRAME_INSET + Math.min(offset, 4) * 18,
    y: FRAME_INSET + Math.min(offset, 4) * 18,
  }));
  const [size, setSize] = useState<PaneSize | null>(null);

  useLayoutEffect(() => {
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    const constrain = () => {
      setPosition((current) => ({
        x: Math.min(
          Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET),
          current.x,
        ),
        y: Math.min(
          Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET),
          current.y,
        ),
      }));
    };
    const observer = new ResizeObserver(constrain);
    observer.observe(parent);
    constrain();
    return () => observer.disconnect();
  }, []);

  function startDrag(event: ReactPointerEvent<HTMLDivElement>) {
    if (event.button !== 0) {
      return;
    }
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    event.preventDefault();
    onActivate();
    event.currentTarget.setPointerCapture(event.pointerId);
    const start = { x: event.clientX, y: event.clientY };
    const origin = position;

    const move = (moveEvent: PointerEvent) => {
      const maxX = Math.max(FRAME_INSET, parent.clientWidth - frame.offsetWidth - FRAME_INSET);
      const maxY = Math.max(FRAME_INSET, parent.clientHeight - frame.offsetHeight - FRAME_INSET);
      setPosition({
        x: Math.min(maxX, Math.max(FRAME_INSET, origin.x + moveEvent.clientX - start.x)),
        y: Math.min(maxY, Math.max(FRAME_INSET, origin.y + moveEvent.clientY - start.y)),
      });
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }

  function startResize(event: ReactPointerEvent<HTMLButtonElement>) {
    if (event.button !== 0) {
      return;
    }
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    onActivate();
    event.currentTarget.setPointerCapture(event.pointerId);
    const start = { x: event.clientX, y: event.clientY };
    const origin = { width: frame.offsetWidth, height: frame.offsetHeight };

    const move = (moveEvent: PointerEvent) => {
      const maxWidth = Math.max(0, parent.clientWidth - position.x - FRAME_INSET);
      const maxHeight = Math.max(0, parent.clientHeight - position.y - FRAME_INSET);
      setSize(
        clampPaneSize(
          origin.width + moveEvent.clientX - start.x,
          origin.height + moveEvent.clientY - start.y,
          maxWidth,
          maxHeight,
        ),
      );
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  }

  function resizeWithKeyboard(event: ReactKeyboardEvent<HTMLButtonElement>) {
    const delta = {
      ArrowLeft: { width: -KEYBOARD_RESIZE_STEP, height: 0 },
      ArrowRight: { width: KEYBOARD_RESIZE_STEP, height: 0 },
      ArrowUp: { width: 0, height: -KEYBOARD_RESIZE_STEP },
      ArrowDown: { width: 0, height: KEYBOARD_RESIZE_STEP },
    }[event.key];
    if (!delta) {
      return;
    }
    const frame = frameRef.current;
    const parent = frame?.parentElement;
    if (!frame || !parent) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    onActivate();
    setSize(
      clampPaneSize(
        frame.offsetWidth + delta.width,
        frame.offsetHeight + delta.height,
        Math.max(0, parent.clientWidth - position.x - FRAME_INSET),
        Math.max(0, parent.clientHeight - position.y - FRAME_INSET),
      ),
    );
  }

  return (
    <div
      ref={frameRef}
      className="btw-floating-pane"
      style={{
        left: position.x,
        top: position.y,
        width: size?.width,
        height: size?.height,
        maxWidth: `calc(100% - ${position.x + FRAME_INSET}px)`,
        maxHeight: `calc(100% - ${position.y + FRAME_INSET}px)`,
      }}
      onPointerDownCapture={onActivate}
    >
      <div className="btw-floating-pane-titlebar" onPointerDown={startDrag}>
        <span className="btw-floating-pane-label">BTW</span>
        <span className="btw-floating-pane-title" title={title}>
          {title}
        </span>
      </div>
      <div className="btw-floating-pane-content">{children}</div>
      <button
        type="button"
        className="btw-floating-pane-resizer"
        aria-label="Resize BTW pane"
        title="Resize BTW pane"
        onPointerDown={startResize}
        onKeyDown={resizeWithKeyboard}
      />
    </div>
  );
}
