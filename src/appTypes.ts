import type { GroupInfo, PaneInfo } from "./types";

export type CloseGroupContinuation = {
  groupId: string;
  groupName: string;
  remainingPaneIds: string[];
  totalCount: number;
};

type CloseDialogGroupContext = {
  groupClose?: CloseGroupContinuation;
};

// The close-confirmation dialog covers research cancellation, research-folder
// removal, plus four ordinary cases: a worktree agent (offer to
// keep or delete the worktree), a live agent without a worktree (just confirm the
// stop), a tab with child processes, and the explicit tab close button (always
// confirm). These render in-app because window.confirm is a no-op in the Tauri
// webview.
export type CloseDialogState =
  | ({ kind: "researchFolderRemove"; workspace: GroupInfo } & CloseDialogGroupContext)
  | ({ kind: "researchCancel"; pane: PaneInfo } & CloseDialogGroupContext)
  | ({
      kind: "worktree";
      pane: PaneInfo;
      agentId: string;
      worktreeDir: string;
      // null means the git status probe failed or was intentionally skipped.
      hasChanges: boolean | null;
      // True while the git status probe is still in flight: the dialog opens
      // immediately (git status can take seconds on a large worktree) and the
      // verdict patches in when the probe resolves.
      checkingChanges: boolean;
      // Identifies which dialog generation an in-flight probe belongs to, so a
      // probe from a dismissed dialog can't patch a newer dialog for the same
      // pane with its older verdict.
      probeNonce: number;
      busy: boolean;
    } & CloseDialogGroupContext)
  | ({ kind: "stop"; pane: PaneInfo; reason: string } & CloseDialogGroupContext)
  | ({
      kind: "runningProcess";
      pane: PaneInfo;
      processCount: number;
      processSummary?: string | null;
    } & CloseDialogGroupContext)
  | ({ kind: "pane"; pane: PaneInfo } & CloseDialogGroupContext);

export type ExitDialogState = {
  paneCount: number;
};

export type ExitPreflightRequest = {
  paneCount: number;
  nonce: number;
};

export type PaneContextMenuState = {
  paneId: string;
  x: number;
  y: number;
};

export type PaneTabPointerDrag = {
  pointerId: number;
  paneId: string;
  startX: number;
  startY: number;
  active: boolean;
};

export type GroupPointerDrag = {
  pointerId: number;
  groupId: string;
  startX: number;
  startY: number;
  active: boolean;
};

// Where a tab drag will land: a gap between rows (reorder), onto a row (nest), or
// into the visible terminal stack (split above/below the target pane).
export type PaneDropTarget =
  | { kind: "gap"; groupId: string; index: number }
  | { kind: "nest"; groupId: string; paneId: string }
  | {
      kind: "terminal-split";
      groupId: string;
      targetPaneId: string;
      position: "above" | "below";
    };

export type GroupDropTarget = { index: number };

export type BrowserOverlaySize = {
  width: number;
  height: number;
};

// Per-pane browser overlay: the URL it's showing, whether it's visible, a nonce
// bumped on open/refresh to force the iframe to remount (reload), whether the
// iframe should be sandboxed (true for token-bearing file-server URLs), and an
// optional user-resized size that survives tab switches in React state.
export type BrowserOverlayState = {
  url: string | null;
  open: boolean;
  reloadNonce: number;
  sandbox: boolean;
  size?: BrowserOverlaySize | null;
};
