import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type {
  MouseEvent as ReactMouseEvent,
  PointerEvent as ReactPointerEvent,
} from "react";
import { createPortal } from "react-dom";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  Download,
  FileText,
  Folder,
  FolderMinus,
  FolderPlus,
  Import,
  LoaderCircle,
  MoreHorizontal,
  Pencil,
  RefreshCw,
  Star,
  StarOff,
  Terminal,
  Trash2,
} from "lucide-react";
import type { ResearchTreeSummary } from "../../types";
import { moveResearchTreeIdToGap } from "../../lib/researchOrder";
import {
  addTreesToResearchFolder,
  buildResearchSidebarLists,
  flattenResearchSidebarUnits,
  flattenVisibleResearchSidebarUnits,
  isResearchStarred,
  moveResearchFolderMemberToGap,
  moveResearchUnitToGap,
  removeTreesFromResearchFolderMembership,
  researchSidebarUnitId,
  translateResearchGapAfterInsertion,
  type ResearchFolder,
  type ResearchFolderState,
  type ResearchSidebarUnit,
} from "../../lib/researchFolders";

const RESEARCH_MENU_WIDTH = 190;
const RESEARCH_MENU_HEIGHT_ESTIMATE = 132;
const RESEARCH_MENU_GAP = 4;
const VIEWPORT_MARGIN = 8;

export type ResearchVisibilityFilter = "active" | "archived" | "all";

interface ResearchSidebarSectionProps {
  trees: ResearchTreeSummary[];
  archivedTrees: ResearchTreeSummary[];
  workspaceId: string | null;
  visibilityFilter: ResearchVisibilityFilter;
  activeTreeId: string | null;
  multiSelectedIds: string[];
  folderState: ResearchFolderState;
  /** Whether the Cmd-1..9 jump hints are currently visible (Cmd held). */
  shortcutHintsShown: boolean;
  /** Zero-based shortcut position per tree id, for trees that have a Cmd-N jump. */
  shortcutIndexByTreeId: Map<string, number>;
  onMultiSelectChange: (treeIds: string[]) => void;
  onSelect: (treeId: string) => void;
  onRename: (treeId: string, title: string) => Promise<void>;
  onArchive: (treeId: string) => Promise<void>;
  onRegenerateTitle: (treeId: string) => Promise<void>;
  onRestore: (treeId: string) => Promise<void>;
  onRemove: (treeId: string) => Promise<void>;
  onReorder: (archived: boolean, orderedTreeIds: string[]) => void;
  onRequestCreateFolder: (treeIds: string[]) => void;
  onRequestImportConversations: () => void;
  onAddToFolder: (folderId: string, treeIds: string[]) => void;
  onRemoveFromFolder: (treeIds: string[]) => void;
  onFolderCollapsedChange: (folderId: string, collapsed: boolean) => void;
  onRenameFolder: (folderId: string, name: string) => void;
  onDissolveFolder: (folderId: string) => void;
  onArchiveFolder: (folderId: string) => Promise<void>;
  onDeleteFolder: (folderId: string) => Promise<void>;
  onToggleStar: (id: string) => void;
  onReorderStars: (orderedIds: string[]) => void;
}

type ResearchMenu =
  | { kind: "tree"; treeId: string; archived: boolean; left: number; top: number }
  | { kind: "folder"; folderId: string; left: number; top: number }
  | { kind: "multi"; left: number; top: number };

// Where a drag starts: top-level units of the starred or main list (plain
// trees and whole folders), members inside one folder, or the flat archived
// list. Active tree items may cross these scopes to enter or leave folders.
type ResearchDragScope =
  | { kind: "units" }
  | { kind: "starred" }
  | { kind: "folder"; folderId: string }
  | { kind: "archived" };

type ResearchPointerDrag = {
  pointerId: number;
  /** Tree id, or folder id when dragging a folder header. */
  id: string;
  scope: ResearchDragScope;
  startX: number;
  startY: number;
  active: boolean;
};

type ResearchDropTarget =
  | { kind: "gap"; scope: ResearchDragScope; index: number }
  | { kind: "folder"; folderId: string; index: number; onHeader: boolean };

const RESEARCH_DRAG_START_THRESHOLD = 4;
const RESEARCH_DRAG_CLICK_SUPPRESS_MS = 100;

function ResearchSidebarTitle({ tree }: { tree: ResearchTreeSummary }) {
  return (
    <span className="research-sidebar-title">
      {tree.kind === "document" ? (
        <FileText className="research-sidebar-doc-icon" size={12} aria-hidden="true" />
      ) : tree.kind === "conversation" ? (
        tree.origin === "conversationImport" ? (
          <Download className="research-sidebar-doc-icon" size={12} aria-hidden="true" />
        ) : (
          <Terminal className="research-sidebar-doc-icon" size={12} aria-hidden="true" />
        )
      ) : null}
      <span className="research-sidebar-title-text">{tree.title}</span>
    </span>
  );
}

function ResearchSidebarSection({
  trees,
  archivedTrees,
  workspaceId,
  visibilityFilter,
  activeTreeId,
  multiSelectedIds,
  folderState,
  shortcutHintsShown,
  shortcutIndexByTreeId,
  onMultiSelectChange,
  onSelect,
  onRename,
  onArchive,
  onRegenerateTitle,
  onRestore,
  onRemove,
  onReorder,
  onRequestCreateFolder,
  onRequestImportConversations,
  onAddToFolder,
  onRemoveFromFolder,
  onFolderCollapsedChange,
  onRenameFolder,
  onDissolveFolder,
  onArchiveFolder,
  onDeleteFolder,
  onToggleStar,
  onReorderStars,
}: ResearchSidebarSectionProps) {
  const [menu, setMenu] = useState<ResearchMenu | null>(null);
  const [renamingTree, setRenamingTree] = useState<ResearchTreeSummary | null>(null);
  const [renamingFolder, setRenamingFolder] = useState<ResearchFolder | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [deletingTree, setDeletingTree] = useState<ResearchTreeSummary | null>(null);
  const [removingTreeId, setRemovingTreeId] = useState<string | null>(null);
  const [treeRemovalError, setTreeRemovalError] = useState<string | null>(null);
  const [deletingFolder, setDeletingFolder] = useState<ResearchFolder | null>(null);
  const [folderRemovalBusy, setFolderRemovalBusy] = useState(false);
  const [folderRemovalError, setFolderRemovalError] = useState<string | null>(null);
  const [dissolvingFolder, setDissolvingFolder] = useState<ResearchFolder | null>(null);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const renameInputRef = useRef<HTMLInputElement | null>(null);
  const sectionRef = useRef<HTMLElement | null>(null);
  // Range-select anchor: the last plainly clicked (or toggled) row, so a
  // shift-click extends from where the user last acted, like a file list.
  const multiSelectAnchorRef = useRef<string | null>(null);
  const pointerDragRef = useRef<ResearchPointerDrag | null>(null);
  const dropTargetRef = useRef<ResearchDropTarget | null>(null);
  const suppressClickRef = useRef(false);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<ResearchDropTarget | null>(null);
  const activeListVisible = visibilityFilter !== "archived";
  const visibleArchivedTrees = visibilityFilter === "active" ? [] : archivedTrees;
  const lists = useMemo(
    () => buildResearchSidebarLists(trees, folderState, workspaceId),
    [folderState, trees, workspaceId],
  );
  const starredUnits = lists.starred;
  const mainUnits = lists.main;
  const collapsedFolderIds = useMemo(
    () => new Set(folderState.collapsed),
    [folderState.collapsed],
  );
  // Flat display order of the active list (starred first) — what shift-ranges
  // walk, and the full-order basis every unit reorder maps back onto.
  const displayOrderIds = useMemo(
    () => [
      ...flattenVisibleResearchSidebarUnits(lists.starred, folderState),
      ...flattenVisibleResearchSidebarUnits(lists.main, folderState),
    ],
    [folderState, lists],
  );
  const menuTree =
    menu?.kind === "tree"
      ? (menu.archived ? archivedTrees : trees).find((tree) => tree.id === menu.treeId) ??
        null
      : null;
  const menuFolder =
    menu?.kind === "folder"
      ? folderState.folders.find((folder) => folder.id === menu.folderId) ?? null
      : null;
  const menuFolderTrees = useMemo(
    () =>
      menuFolder
        ? [...trees, ...archivedTrees].filter(
            (tree) => folderState.membership[tree.id] === menuFolder.id,
          )
        : [],
    [archivedTrees, folderState.membership, menuFolder, trees],
  );
  const menuFolderHasRunning = menuFolderTrees.some((tree) => tree.runningCount > 0);
  const multiSelectionHasFolderMembers = multiSelectedIds.some(
    (treeId) => Boolean(folderState.membership[treeId]),
  );

  useEffect(() => {
    const visible = new Set(displayOrderIds);
    const next = multiSelectedIds.filter((id) => visible.has(id));
    if (next.length !== multiSelectedIds.length) {
      onMultiSelectChange(next);
    }
  }, [displayOrderIds, multiSelectedIds, onMultiSelectChange]);

  useEffect(() => {
    if (!menu) {
      return;
    }
    const closeMenu = (event: MouseEvent) => {
      const target = event.target as Node;
      if (
        !menuRef.current?.contains(target) &&
        !(target instanceof Element && target.closest("[data-research-menu-trigger]"))
      ) {
        setMenu(null);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setMenu(null);
        return;
      }
      if (
        menu.kind !== "tree" ||
        !menuTree ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey
      ) {
        return;
      }

      const key = event.key.toLowerCase();
      if (key !== "d" && (key !== "a" || menu.archived)) {
        return;
      }

      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      if (menuTree.runningCount > 0) {
        return;
      }
      if (key === "d") {
        openDeleteDialog(menuTree);
        return;
      }

      setMenu(null);
      void onArchive(menuTree.id);
    };
    const closeOnReflow = () => setMenu(null);
    document.addEventListener("mousedown", closeMenu);
    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", closeOnReflow);
    window.addEventListener("scroll", closeOnReflow, true);
    return () => {
      document.removeEventListener("mousedown", closeMenu);
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", closeOnReflow);
      window.removeEventListener("scroll", closeOnReflow, true);
    };
  }, [menu, menuTree, onArchive]);

  useEffect(() => {
    if (renamingTree || renamingFolder) {
      renameInputRef.current?.focus();
      renameInputRef.current?.select();
    }
  }, [renamingFolder, renamingTree]);

  // The height estimate that positioned the menu is only a guess — the tree
  // and folder menus vary with their optional items. Once the real menu has
  // rendered, clamp it back inside the viewport so its bottom items (Delete)
  // stay reachable from triggers near the window's bottom edge.
  useLayoutEffect(() => {
    const element = menuRef.current;
    if (!menu || !element) {
      return;
    }
    const height = element.getBoundingClientRect().height;
    const top = Math.max(
      VIEWPORT_MARGIN,
      Math.min(menu.top, window.innerHeight - VIEWPORT_MARGIN - height),
    );
    if (top !== menu.top) {
      element.style.top = `${top}px`;
    }
  }, [menu]);

  function menuPositionFromTrigger(trigger: HTMLButtonElement) {
    const rect = trigger.getBoundingClientRect();
    const left = Math.max(
      VIEWPORT_MARGIN,
      Math.min(
        rect.right - RESEARCH_MENU_WIDTH,
        window.innerWidth - RESEARCH_MENU_WIDTH - VIEWPORT_MARGIN,
      ),
    );
    const below = rect.bottom + RESEARCH_MENU_GAP;
    const top =
      below + RESEARCH_MENU_HEIGHT_ESTIMATE <= window.innerHeight - VIEWPORT_MARGIN
        ? below
        : Math.max(
            VIEWPORT_MARGIN,
            rect.top - RESEARCH_MENU_HEIGHT_ESTIMATE - RESEARCH_MENU_GAP,
          );
    return { left, top };
  }

  function menuPositionFromPoint(clientX: number, clientY: number) {
    return {
      left: Math.max(
        VIEWPORT_MARGIN,
        Math.min(clientX, window.innerWidth - RESEARCH_MENU_WIDTH - VIEWPORT_MARGIN),
      ),
      top: Math.max(
        VIEWPORT_MARGIN,
        Math.min(
          clientY,
          window.innerHeight - RESEARCH_MENU_HEIGHT_ESTIMATE - VIEWPORT_MARGIN,
        ),
      ),
    };
  }

  function treeMenuForPosition(
    treeId: string,
    archived: boolean,
    position: { left: number; top: number },
  ): ResearchMenu {
    // Right-clicking inside an active multi-selection targets the whole
    // selection; anywhere else the menu is the ordinary per-item one.
    if (!archived && multiSelectedIds.length > 1 && multiSelectedIds.includes(treeId)) {
      return { kind: "multi", ...position };
    }
    return { kind: "tree", treeId, archived, ...position };
  }

  function openMenu(trigger: HTMLButtonElement, treeId: string, archived: boolean) {
    if (menu?.kind === "tree" && menu.treeId === treeId && menu.archived === archived) {
      setMenu(null);
      return;
    }
    setMenu(treeMenuForPosition(treeId, archived, menuPositionFromTrigger(trigger)));
  }

  function openFolderMenu(trigger: HTMLButtonElement, folderId: string) {
    if (menu?.kind === "folder" && menu.folderId === folderId) {
      setMenu(null);
      return;
    }
    setMenu({ kind: "folder", folderId, ...menuPositionFromTrigger(trigger) });
  }

  function openContextMenu(
    treeId: string,
    archived: boolean,
    clientX: number,
    clientY: number,
  ) {
    setMenu(treeMenuForPosition(treeId, archived, menuPositionFromPoint(clientX, clientY)));
  }

  function openFolderContextMenu(folderId: string, clientX: number, clientY: number) {
    setMenu({ kind: "folder", folderId, ...menuPositionFromPoint(clientX, clientY) });
  }

  function openDeleteDialog(tree: ResearchTreeSummary) {
    setMenu(null);
    setTreeRemovalError(null);
    setDeletingTree(tree);
  }

  function openRenameDialog(tree: ResearchTreeSummary) {
    setMenu(null);
    setRenamingFolder(null);
    setRenameDraft(tree.title);
    setRenamingTree(tree);
  }

  function openFolderRenameDialog(folder: ResearchFolder) {
    setMenu(null);
    setRenamingTree(null);
    setRenameDraft(folder.name);
    setRenamingFolder(folder);
  }

  function folderMemberTrees(folderId: string) {
    return [...trees, ...archivedTrees].filter(
      (tree) => folderState.membership[tree.id] === folderId,
    );
  }

  async function confirmTreeRemoval() {
    if (!deletingTree || removingTreeId) {
      return;
    }
    const treeId = deletingTree.id;
    setTreeRemovalError(null);
    setRemovingTreeId(treeId);
    try {
      await onRemove(treeId);
      setDeletingTree(null);
    } catch (err) {
      // The app shell surfaces the backend error. Keep the confirmation open so
      // the user does not have to reopen the menu after a transient rejection.
      setTreeRemovalError(err instanceof Error ? err.message : String(err));
    } finally {
      setRemovingTreeId(null);
    }
  }

  async function confirmFolderRemoval() {
    if (!deletingFolder || folderRemovalBusy) {
      return;
    }
    setFolderRemovalError(null);
    setFolderRemovalBusy(true);
    try {
      await onDeleteFolder(deletingFolder.id);
      setDeletingFolder(null);
    } catch (err) {
      setFolderRemovalError(err instanceof Error ? err.message : String(err));
    } finally {
      setFolderRemovalBusy(false);
    }
  }

  function submitRename() {
    if (renamingFolder) {
      const folder = renamingFolder;
      const name = renameDraft.trim();
      setRenamingFolder(null);
      if (name && name !== folder.name) {
        onRenameFolder(folder.id, name);
      }
      return;
    }
    if (!renamingTree) {
      return;
    }
    const tree = renamingTree;
    const title = renameDraft.trim();
    setRenamingTree(null);
    if (!title || title === tree.title) {
      return;
    }
    void onRename(tree.id, title);
  }

  // Multi-select gestures. Shift extends a contiguous range from the anchor,
  // meta/ctrl toggles a single row in or out. Both operate on the visible
  // active list only — archived rows cannot be foldered or bulk-acted upon.
  function updateMultiSelection(treeId: string, range: boolean) {
    const ids = displayOrderIds;
    if (!ids.includes(treeId)) {
      return;
    }
    if (range) {
      const anchorCandidate =
        multiSelectAnchorRef.current && ids.includes(multiSelectAnchorRef.current)
          ? multiSelectAnchorRef.current
          : activeTreeId && ids.includes(activeTreeId)
            ? activeTreeId
            : treeId;
      multiSelectAnchorRef.current = anchorCandidate;
      const from = ids.indexOf(anchorCandidate);
      const to = ids.indexOf(treeId);
      const [start, end] = from <= to ? [from, to] : [to, from];
      onMultiSelectChange(ids.slice(start, end + 1));
      return;
    }
    // A toggle that starts a fresh multi-selection folds the currently active
    // row in, so ctrl-clicking a second row reads as "these two".
    const base =
      multiSelectedIds.length > 0
        ? multiSelectedIds.filter((id) => ids.includes(id))
        : activeTreeId && ids.includes(activeTreeId) && activeTreeId !== treeId
          ? [activeTreeId]
          : [];
    const next = base.includes(treeId)
      ? base.filter((id) => id !== treeId)
      : [...base, treeId];
    multiSelectAnchorRef.current = treeId;
    onMultiSelectChange(next);
  }

  function clearPointerDrag() {
    pointerDragRef.current = null;
    dropTargetRef.current = null;
    setDraggingId(null);
    setDropTarget(null);
  }

  function computeDropTarget(
    clientX: number,
    clientY: number,
    drag: ResearchPointerDrag,
  ): ResearchDropTarget | null {
    const section = sectionRef.current;
    if (!section) {
      return null;
    }
    if (drag.scope.kind === "archived") {
      const dragIndex = visibleArchivedTrees.findIndex((tree) => tree.id === drag.id);
      if (dragIndex < 0) {
        return null;
      }
      const rows = Array.from(
        section.querySelectorAll<HTMLElement>(
          '.research-sidebar-row[data-research-archived="true"]',
        ),
      );
      for (const [index, row] of rows.entries()) {
        const rect = row.getBoundingClientRect();
        if (clientY < rect.top + rect.height / 2) {
          return index === dragIndex || index === dragIndex + 1
            ? null
            : { kind: "gap", scope: drag.scope, index };
        }
      }
      return rows.length === dragIndex || rows.length === dragIndex + 1
        ? null
        : { kind: "gap", scope: drag.scope, index: rows.length };
    }

    const draggedTree = trees.find((tree) => tree.id === drag.id) ?? null;
    const unitGap = (
      list: "units" | "starred",
      index: number,
    ): ResearchDropTarget | null => {
      const units = list === "starred" ? starredUnits : mainUnits;
      const dragIndex = units.findIndex(
        (unit) => researchSidebarUnitId(unit) === drag.id,
      );
      if (
        drag.scope.kind === list &&
        (index === dragIndex || index === dragIndex + 1)
      ) {
        return null;
      }
      return { kind: "gap", scope: { kind: list }, index };
    };
    const unitGapAtY = (list: "units" | "starred") => {
      const units = list === "starred" ? starredUnits : mainUnits;
      const attribute =
        list === "starred" ? "data-research-star-index" : "data-research-unit-index";
      const blocks = new Map<number, { top: number; bottom: number }>();
      for (const row of section.querySelectorAll<HTMLElement>(
        `.research-sidebar-row[${attribute}]`,
      )) {
        const index = Number(row.getAttribute(attribute));
        if (!Number.isInteger(index)) {
          continue;
        }
        const rect = row.getBoundingClientRect();
        const block = blocks.get(index);
        blocks.set(index, {
          top: block ? Math.min(block.top, rect.top) : rect.top,
          bottom: block ? Math.max(block.bottom, rect.bottom) : rect.bottom,
        });
      }
      for (let index = 0; index < units.length; index += 1) {
        const block = blocks.get(index);
        if (block && clientY < (block.top + block.bottom) / 2) {
          return unitGap(list, index);
        }
      }
      return unitGap(list, units.length);
    };

    const hit = document.elementFromPoint(clientX, clientY);
    const row = hit?.closest<HTMLElement>(".research-sidebar-row") ?? null;
    const hitRow = row && section.contains(row) ? row : null;
    const memberFolderId = hitRow?.dataset.researchFolderMember;
    if (draggedTree && memberFolderId) {
      const unit = [...starredUnits, ...mainUnits].find(
        (candidate) =>
          candidate.kind === "folder" && candidate.folder.id === memberFolderId,
      );
      if (unit?.kind === "folder") {
        const memberIndex = unit.trees.findIndex(
          (tree) => tree.id === hitRow.dataset.researchTreeId,
        );
        const rect = hitRow.getBoundingClientRect();
        const index = memberIndex + (clientY >= rect.top + rect.height / 2 ? 1 : 0);
        const dragIndex = unit.trees.findIndex((tree) => tree.id === drag.id);
        if (
          drag.scope.kind === "folder" &&
          memberFolderId === drag.scope.folderId &&
          (index === dragIndex || index === dragIndex + 1)
        ) {
          return null;
        }
        return {
          kind: "folder",
          folderId: memberFolderId,
          index,
          onHeader: false,
        };
      }
    }

    const headerFolderId = hitRow?.dataset.researchFolderId;
    if (headerFolderId) {
      const list = hitRow.dataset.researchStarIndex === undefined ? "units" : "starred";
      const unitIndex = Number(
        list === "starred"
          ? hitRow.dataset.researchStarIndex
          : hitRow.dataset.researchUnitIndex,
      );
      const rect = hitRow.getBoundingClientRect();
      const edge = Math.min(6, rect.height * 0.25);
      if (!draggedTree) {
        return unitGap(
          list,
          unitIndex + (clientY >= rect.top + rect.height / 2 ? 1 : 0),
        );
      }
      if (clientY < rect.top + edge || clientY > rect.bottom - edge) {
        return unitGap(
          list,
          unitIndex + (clientY > rect.bottom - edge ? 1 : 0),
        );
      }
      const target = [...starredUnits, ...mainUnits].find(
        (unit) => unit.kind === "folder" && unit.folder.id === headerFolderId,
      );
      if (target?.kind === "folder") {
        return {
          kind: "folder",
          folderId: headerFolderId,
          index: target.trees.length,
          onHeader: true,
        };
      }
    }

    if (hitRow && !memberFolderId) {
      const list =
        hitRow.dataset.researchStarIndex !== undefined ? "starred" : "units";
      if (draggedTree && list === "starred" && drag.scope.kind !== "starred") {
        return null;
      }
      const index = Number(
        list === "starred"
          ? hitRow.dataset.researchStarIndex
          : hitRow.dataset.researchUnitIndex,
      );
      const rect = hitRow.getBoundingClientRect();
      return unitGap(list, index + (clientY >= rect.top + rect.height / 2 ? 1 : 0));
    }

    return unitGapAtY(drag.scope.kind === "starred" ? "starred" : "units");
  }

  function handlePointerDown(
    event: ReactPointerEvent<HTMLDivElement>,
    id: string,
    scope: ResearchDragScope,
  ) {
    if (
      event.button !== 0 ||
      // Modified clicks are selection gestures, never the start of a drag.
      event.shiftKey ||
      event.metaKey ||
      event.ctrlKey ||
      (event.target instanceof Element && event.target.closest("[data-research-menu-trigger]"))
    ) {
      return;
    }
    pointerDragRef.current = {
      pointerId: event.pointerId,
      id,
      scope,
      startX: event.clientX,
      startY: event.clientY,
      active: false,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function handlePointerMove(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = pointerDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    if (!drag.active) {
      const distance = Math.hypot(event.clientX - drag.startX, event.clientY - drag.startY);
      if (distance < RESEARCH_DRAG_START_THRESHOLD) {
        return;
      }
      drag.active = true;
      setMenu(null);
      setDraggingId(drag.id);
    }
    event.preventDefault();
    const target = computeDropTarget(event.clientX, event.clientY, drag);
    dropTargetRef.current = target;
    setDropTarget(target);
  }

  function handlePointerUp(event: ReactPointerEvent<HTMLDivElement>) {
    const drag = pointerDragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    try {
      event.currentTarget.releasePointerCapture(event.pointerId);
    } catch {
      // The platform may already have released capture.
    }
    if (!drag.active) {
      pointerDragRef.current = null;
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    suppressClickRef.current = true;
    window.setTimeout(() => {
      suppressClickRef.current = false;
    }, RESEARCH_DRAG_CLICK_SUPPRESS_MS);
    const target =
      dropTargetRef.current ?? computeDropTarget(event.clientX, event.clientY, drag);
    clearPointerDrag();
    if (!target) {
      return;
    }
    if (drag.scope.kind === "archived" && target.kind === "gap") {
      const currentIds = visibleArchivedTrees.map((tree) => tree.id);
      const nextIds = moveResearchTreeIdToGap(currentIds, drag.id, target.index);
      if (nextIds !== currentIds) {
        onReorder(true, nextIds);
      }
      return;
    }
    const draggedTree = trees.find((tree) => tree.id === drag.id) ?? null;
    if (target.kind === "folder" && draggedTree) {
      const proposedState = addTreesToResearchFolder(
        folderState,
        target.folderId,
        [drag.id],
      );
      const wasStarred = isResearchStarred(folderState, drag.id);
      const proposedLists = buildResearchSidebarLists(trees, proposedState, workspaceId);
      const targetIsStarred = proposedLists.starred.some(
        (unit) => unit.kind === "folder" && unit.folder.id === target.folderId,
      );
      const currentTarget = [...starredUnits, ...mainUnits].find(
        (unit) => unit.kind === "folder" && unit.folder.id === target.folderId,
      );
      const proposedTarget = [
        ...proposedLists.starred,
        ...proposedLists.main,
      ].find(
        (unit) => unit.kind === "folder" && unit.folder.id === target.folderId,
      );
      const insertedIntoTargetProjection =
        currentTarget?.kind === "folder" &&
        !currentTarget.trees.some((tree) => tree.id === drag.id);
      const proposedMemberIndex =
        proposedTarget?.kind === "folder"
          ? proposedTarget.trees.findIndex((tree) => tree.id === drag.id)
          : -1;
      const targetIndex = insertedIntoTargetProjection
        ? translateResearchGapAfterInsertion(target.index, proposedMemberIndex)
        : target.index;
      const moved = moveResearchFolderMemberToGap(
        targetIsStarred ? proposedLists.starred : proposedLists.main,
        target.folderId,
        drag.id,
        targetIndex,
      );
      if (wasStarred) {
        onToggleStar(drag.id);
      }
      if (folderState.membership[drag.id] !== target.folderId) {
        onAddToFolder(target.folderId, [drag.id]);
      }
      if (moved) {
        onReorder(false, [
          ...flattenResearchSidebarUnits(
            targetIsStarred ? moved : proposedLists.starred,
          ),
          ...flattenResearchSidebarUnits(
            targetIsStarred ? proposedLists.main : moved,
          ),
        ]);
      }
      return;
    }
    if (target.kind !== "gap") {
      return;
    }
    if (drag.scope.kind === "starred" && target.scope.kind === "starred") {
      // The starred list orders itself client-side; reuse the same gap-move
      // mechanic the backend list order uses.
      const currentIds = starredUnits.map(researchSidebarUnitId);
      const nextIds = moveResearchTreeIdToGap(currentIds, drag.id, target.index);
      if (nextIds !== currentIds) {
        onReorderStars(nextIds);
      }
      return;
    }
    if (target.scope.kind !== "units") {
      return;
    }
    const proposedState = draggedTree
      ? removeTreesFromResearchFolderMembership(folderState, [drag.id])
      : folderState;
    const proposedLists = buildResearchSidebarLists(trees, proposedState, workspaceId);
    const sourceFolderId =
      drag.scope.kind === "folder" ? drag.scope.folderId : null;
    const sourceFolder =
      sourceFolderId
        ? [...starredUnits, ...mainUnits].find(
            (unit) =>
              unit.kind === "folder" && unit.folder.id === sourceFolderId,
          )
        : null;
    const extractionAddsMainUnit =
      sourceFolder?.kind === "folder" &&
      (sourceFolder.trees.length > 1 || !mainUnits.includes(sourceFolder));
    const proposedDragIndex = proposedLists.main.findIndex(
      (unit) => researchSidebarUnitId(unit) === drag.id,
    );
    // Extracting a member adds a new top-level unit without removing its
    // still-populated source folder. Translate the pre-extraction gap into the
    // expanded unit list before applying the ordinary gap move.
    const targetIndex = extractionAddsMainUnit
      ? translateResearchGapAfterInsertion(target.index, proposedDragIndex)
      : target.index;
    const moved = moveResearchUnitToGap(proposedLists.main, drag.id, targetIndex);
    if (draggedTree && folderState.membership[drag.id]) {
      onRemoveFromFolder([drag.id]);
    }
    if (moved) {
      onReorder(false, [
        ...flattenResearchSidebarUnits(proposedLists.starred),
        ...flattenResearchSidebarUnits(moved),
      ]);
    }
  }

  function handlePointerCancel(event: ReactPointerEvent<HTMLDivElement>) {
    if (pointerDragRef.current?.pointerId === event.pointerId) {
      clearPointerDrag();
    }
  }

  // Row-level click handling, shared by the row and its select button. The row
  // takes pointer capture on pointerdown (for drag reordering), and a captured
  // pointer retargets the gesture's mouseup — and therefore its click — to the
  // capturing row, so a handler on the inner button alone never fires. The
  // terminal pane tabs handle clicks on their row for the same reason.
  function selectTreeFromClick(
    event: ReactMouseEvent<HTMLElement>,
    treeId: string,
    archived = false,
  ) {
    if (suppressClickRef.current) {
      return;
    }
    // A double-click still selects the research on its first click, but does
    // not start a second redundant detail fetch before the rename dialog opens.
    if (event.detail > 1) {
      return;
    }
    // An uncaptured click on the menu trigger bubbles here; opening the menu
    // must not also switch the selection.
    if (
      event.target instanceof Element &&
      event.target.closest("[data-research-menu-trigger]")
    ) {
      return;
    }
    if (!archived && (event.shiftKey || event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      updateMultiSelection(treeId, event.shiftKey);
      return;
    }
    if (multiSelectedIds.length > 0) {
      onMultiSelectChange([]);
    }
    multiSelectAnchorRef.current = archived ? null : treeId;
    onSelect(treeId);
  }

  function unitDropClasses(
    list: "units" | "starred",
    unitIndex: number,
    length: number,
    rowRole: "first" | "last" | "only",
  ) {
    if (dropTarget?.kind !== "gap" || dropTarget.scope.kind !== list) {
      return "";
    }
    const before =
      dropTarget.index === unitIndex && (rowRole === "first" || rowRole === "only");
    const after =
      dropTarget.index === length &&
      unitIndex === length - 1 &&
      (rowRole === "last" || rowRole === "only");
    return `${before ? " is-drop-before" : ""}${after ? " is-drop-after" : ""}`;
  }

  function folderMemberDropClasses(folderId: string, index: number, length: number) {
    if (
      dropTarget?.kind !== "folder" ||
      dropTarget.folderId !== folderId ||
      dropTarget.onHeader
    ) {
      return "";
    }
    return `${dropTarget.index === index ? " is-drop-before" : ""}${
      dropTarget.index === length && index === length - 1 ? " is-drop-after" : ""
    }`;
  }

  function archivedDropClasses(index: number, length: number) {
    if (dropTarget?.kind !== "gap" || dropTarget.scope.kind !== "archived") {
      return "";
    }
    return `${dropTarget.index === index ? " is-drop-before" : ""}${
      dropTarget.index === length && index === length - 1 ? " is-drop-after" : ""
    }`;
  }

  function renderTreeRow(
    tree: ResearchTreeSummary,
    options: {
      archived: boolean;
      dragId: string;
      dragScope: ResearchDragScope;
      unitIndex?: number;
      unitList?: "units" | "starred";
      folderId?: string;
      extraClasses: string;
    },
  ) {
    const { archived } = options;
    const shortcutIndex = archived ? undefined : shortcutIndexByTreeId.get(tree.id);
    return (
      <div
        key={tree.id}
        className={`research-sidebar-row${archived ? " is-archived" : ""}${
          activeTreeId === tree.id ? " is-selected" : ""
        }${!archived && multiSelectedIds.includes(tree.id) ? " is-multi-selected" : ""}${
          menu?.kind === "tree" && menu.treeId === tree.id && menu.archived === archived
            ? " has-open-menu"
            : ""
        }${draggingId === tree.id ? " is-dragging" : ""}${options.extraClasses}`}
        data-research-tree-id={tree.id}
        data-research-archived={archived ? "true" : "false"}
        data-research-unit-index={
          options.unitList === "units" ? options.unitIndex : undefined
        }
        data-research-star-index={
          options.unitList === "starred" ? options.unitIndex : undefined
        }
        data-research-folder-member={options.folderId}
        onPointerDown={(event) => handlePointerDown(event, options.dragId, options.dragScope)}
        onPointerMove={handlePointerMove}
        onPointerUp={handlePointerUp}
        onPointerCancel={handlePointerCancel}
        onClick={(event) => selectTreeFromClick(event, tree.id, archived)}
        onContextMenu={(event) => {
          event.preventDefault();
          event.stopPropagation();
          openContextMenu(tree.id, archived, event.clientX, event.clientY);
        }}
        onDoubleClick={archived ? undefined : () => openRenameDialog(tree)}
      >
        <button
          type="button"
          className="control-button research-sidebar-select"
          aria-current={activeTreeId === tree.id ? "page" : undefined}
          title={tree.title}
          onClick={(event) => {
            event.stopPropagation();
            selectTreeFromClick(event, tree.id, archived);
          }}
          onDoubleClick={
            archived
              ? undefined
              : (event) => {
                  event.stopPropagation();
                  openRenameDialog(tree);
                }
          }
        >
          <span className="research-sidebar-copy">
            <ResearchSidebarTitle tree={tree} />
          </span>
          {!archived && tree.runningCount > 0 ? (
            <span
              className="research-sidebar-spinner"
              title={`${tree.runningCount} running`}
            >
              <LoaderCircle size={14} aria-hidden="true" />
            </span>
          ) : !archived && tree.hasUnseenFailure ? (
            <span
              className="research-sidebar-failed"
              title="Failed since last viewed — open to acknowledge"
            >
              !
            </span>
          ) : !archived && tree.hasUnseenUpdate ? (
            <span className="research-sidebar-unseen" title="Updated since last viewed">
              New
            </span>
          ) : null}
        </button>
        <button
          type="button"
          className="control-button research-sidebar-menu-trigger"
          title="Research actions"
          aria-label={`Actions for ${tree.title}`}
          aria-haspopup="menu"
          aria-expanded={
            menu?.kind === "tree" && menu.treeId === tree.id && menu.archived === archived
          }
          data-research-menu-trigger
          onClick={(event) => openMenu(event.currentTarget, tree.id, archived)}
          onDoubleClick={(event) => event.stopPropagation()}
        >
          <MoreHorizontal size={14} aria-hidden="true" />
        </button>
        {shortcutHintsShown && shortcutIndex !== undefined ? (
          <span className="pane-tab-shortcut-hint" aria-hidden="true">
            ⌘{shortcutIndex + 1}
          </span>
        ) : null}
      </div>
    );
  }

  function renderUnit(
    unit: ResearchSidebarUnit,
    unitIndex: number,
    list: "units" | "starred",
  ) {
    const listUnits = list === "starred" ? starredUnits : mainUnits;
    const listScope: ResearchDragScope = { kind: list };
    if (unit.kind === "tree") {
      return renderTreeRow(unit.tree, {
        archived: false,
        dragId: unit.tree.id,
        dragScope: listScope,
        unitIndex,
        unitList: list,
        extraClasses: unitDropClasses(list, unitIndex, listUnits.length, "only"),
      });
    }
    const { folder } = unit;
    const collapsed = collapsedFolderIds.has(folder.id);
    return (
      <div
        key={folder.id}
        className={`research-sidebar-folder${collapsed ? " is-collapsed" : ""}`}
        role="group"
        aria-label={folder.name}
      >
        <div
          className={`research-sidebar-row research-sidebar-folder-row${
            menu?.kind === "folder" && menu.folderId === folder.id ? " has-open-menu" : ""
          }${
            dropTarget?.kind === "folder" &&
            dropTarget.folderId === folder.id &&
            dropTarget.onHeader
              ? " is-folder-drop-target"
              : ""
          }${draggingId === folder.id ? " is-dragging" : ""}${unitDropClasses(
            list,
            unitIndex,
            listUnits.length,
            collapsed || unit.trees.length === 0 ? "only" : "first",
          )}`}
          data-research-unit-index={list === "units" ? unitIndex : undefined}
          data-research-star-index={list === "starred" ? unitIndex : undefined}
          data-research-folder-id={folder.id}
          onPointerDown={(event) => handlePointerDown(event, folder.id, listScope)}
          onPointerMove={handlePointerMove}
          onPointerUp={handlePointerUp}
          onPointerCancel={handlePointerCancel}
          onContextMenu={(event) => {
            event.preventDefault();
            event.stopPropagation();
            openFolderContextMenu(folder.id, event.clientX, event.clientY);
          }}
          onClick={(event) => {
            if (
              suppressClickRef.current ||
              (event.target instanceof Element &&
                event.target.closest("[data-research-menu-trigger]"))
            ) {
              return;
            }
            onFolderCollapsedChange(folder.id, !collapsed);
          }}
        >
          <button
            type="button"
            className="control-button research-sidebar-folder-collapse"
            aria-label={`${collapsed ? "Expand" : "Collapse"} ${folder.name}`}
            aria-expanded={!collapsed}
            onPointerDown={(event) => event.stopPropagation()}
            onClick={(event) => {
              event.stopPropagation();
              onFolderCollapsedChange(folder.id, !collapsed);
            }}
          >
            <ChevronRight size={12} aria-hidden="true" />
          </button>
          <span className="research-sidebar-select research-sidebar-folder-heading">
            <span className="research-sidebar-copy">
              <span className="research-sidebar-title">
                <Folder
                  className="research-sidebar-folder-icon"
                  size={12}
                  aria-hidden="true"
                />
                <span className="research-sidebar-title-text">{folder.name}</span>
              </span>
            </span>
          </span>
          <button
            type="button"
            className="control-button research-sidebar-menu-trigger"
            title="Folder actions"
            aria-label={`Actions for ${folder.name}`}
            aria-haspopup="menu"
            aria-expanded={menu?.kind === "folder" && menu.folderId === folder.id}
            data-research-menu-trigger
            onClick={(event) => openFolderMenu(event.currentTarget, folder.id)}
            onDoubleClick={(event) => event.stopPropagation()}
          >
            <MoreHorizontal size={14} aria-hidden="true" />
          </button>
        </div>
        {collapsed
          ? null
          : unit.trees.map((tree, memberIndex) =>
              renderTreeRow(tree, {
                archived: false,
                dragId: tree.id,
                dragScope: { kind: "folder", folderId: folder.id },
                unitIndex,
                unitList: list,
                folderId: folder.id,
                extraClasses: ` is-folder-member${folderMemberDropClasses(
                  folder.id,
                  memberIndex,
                  unit.trees.length,
                )}${
                  memberIndex === unit.trees.length - 1
                    ? unitDropClasses(list, unitIndex, listUnits.length, "last")
                    : ""
                }`,
              }),
            )}
      </div>
    );
  }

  return (
    <>
      <section
        ref={sectionRef}
        className={`research-sidebar-section${draggingId ? " is-dragging" : ""}`}
        aria-label="Research"
      >
        <div className="research-sidebar-heading">
          <span>Research</span>
          <div className="research-sidebar-heading-actions">
            <button
              className="control-button"
              type="button"
              title="Import conversations"
              aria-label="Import conversations"
              onClick={() => onRequestImportConversations()}
            >
              <Import size={13} aria-hidden="true" />
            </button>
            <button
              className="control-button"
              type="button"
              title="New folder"
              aria-label="New research folder"
              disabled={!workspaceId}
              onClick={() => onRequestCreateFolder([])}
            >
              <FolderPlus size={13} aria-hidden="true" />
            </button>
          </div>
        </div>
        {activeListVisible && starredUnits.length > 0 ? (
          <div className="research-sidebar-starred" role="group" aria-label="Starred research">
            {starredUnits.map((unit, index) => renderUnit(unit, index, "starred"))}
          </div>
        ) : null}
        {activeListVisible
          ? mainUnits.map((unit, index) => renderUnit(unit, index, "units"))
          : null}
        {visibilityFilter !== "active"
          ? visibleArchivedTrees.map((tree, index) =>
              renderTreeRow(tree, {
                archived: true,
                dragId: tree.id,
                dragScope: { kind: "archived" },
                extraClasses: archivedDropClasses(index, visibleArchivedTrees.length),
              }),
            )
          : null}
      </section>
      {menu?.kind === "multi"
        ? createPortal(
            <div
              ref={menuRef}
              className="popover-surface popover-surface--context pane-context-menu research-sidebar-menu"
              role="menu"
              aria-label={`Actions for ${multiSelectedIds.length} selected items`}
              style={{ left: menu.left, top: menu.top }}
              onMouseDown={(event) => event.stopPropagation()}
              onContextMenu={(event) => event.preventDefault()}
            >
              <div className="group-context-actions">
                <button
                  className="control-button"
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setMenu(null);
                    onRequestCreateFolder(multiSelectedIds);
                  }}
                >
                  <FolderPlus size={13} aria-hidden="true" />
                  <span>New folder with {multiSelectedIds.length} items</span>
                </button>
                {multiSelectionHasFolderMembers ? (
                  <div className="context-menu-divider" role="separator" />
                ) : null}
                {multiSelectionHasFolderMembers ? (
                  <button
                    className="control-button"
                    type="button"
                    role="menuitem"
                    onClick={() => {
                      setMenu(null);
                      onRemoveFromFolder(multiSelectedIds);
                    }}
                  >
                    <FolderMinus size={13} aria-hidden="true" />
                    <span>Remove from folders</span>
                  </button>
                ) : null}
              </div>
            </div>,
            document.body,
          )
        : null}
      {menu?.kind === "folder" && menuFolder
        ? createPortal(
            <div
              ref={menuRef}
              className="popover-surface popover-surface--context pane-context-menu research-sidebar-menu"
              role="menu"
              aria-label={`Actions for ${menuFolder.name}`}
              style={{ left: menu.left, top: menu.top }}
              onMouseDown={(event) => event.stopPropagation()}
              onContextMenu={(event) => event.preventDefault()}
            >
              <div className="group-context-actions">
                <button
                  className="control-button"
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setMenu(null);
                    onToggleStar(menuFolder.id);
                  }}
                >
                  {isResearchStarred(folderState, menuFolder.id) ? (
                    <StarOff size={13} aria-hidden="true" />
                  ) : (
                    <Star size={13} aria-hidden="true" />
                  )}
                  <span>
                    {isResearchStarred(folderState, menuFolder.id) ? "Unstar" : "Star"}
                  </span>
                </button>
                <button
                  className="control-button"
                  type="button"
                  role="menuitem"
                  onClick={() => openFolderRenameDialog(menuFolder)}
                >
                  <Pencil size={13} aria-hidden="true" />
                  <span>Rename</span>
                </button>
                <div className="context-menu-divider" role="separator" />
                <button
                  className="control-button"
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setMenu(null);
                    setDissolvingFolder(menuFolder);
                  }}
                >
                  <FolderMinus size={13} aria-hidden="true" />
                  <span>
                    Remove {menuFolderTrees.length}{" "}
                    {menuFolderTrees.length === 1 ? "item" : "items"}…
                  </span>
                </button>
                <div className="context-menu-divider" role="separator" />
                <button
                  className="control-button"
                  type="button"
                  role="menuitem"
                  disabled={menuFolderHasRunning}
                  title={
                    menuFolderHasRunning
                      ? "Folders with active runs cannot be archived"
                      : undefined
                  }
                  onClick={() => {
                    setMenu(null);
                    void onArchiveFolder(menuFolder.id);
                  }}
                >
                  <Archive size={13} aria-hidden="true" />
                  <span>Archive</span>
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="control-button context-menu-danger"
                  disabled={menuFolderHasRunning}
                  title={
                    menuFolderHasRunning
                      ? "Folders with active runs cannot be deleted"
                      : undefined
                  }
                  onClick={() => {
                    setMenu(null);
                    setFolderRemovalError(null);
                    setDeletingFolder(menuFolder);
                  }}
                >
                  <Trash2 size={13} aria-hidden="true" />
                  <span>Delete</span>
                </button>
              </div>
            </div>,
            document.body,
          )
        : null}
      {menu?.kind === "tree" && menuTree
        ? createPortal(
            <div
              ref={menuRef}
              className="popover-surface popover-surface--context pane-context-menu research-sidebar-menu"
              role="menu"
              aria-label={`Actions for ${menuTree.title}`}
              style={{ left: menu.left, top: menu.top }}
              onMouseDown={(event) => event.stopPropagation()}
              onContextMenu={(event) => event.preventDefault()}
            >
              <div className="group-context-actions">
                {menu.archived ? (
                  <button className="control-button"
                    type="button"
                    role="menuitem"
                    onClick={() => {
                      setMenu(null);
                      void onRestore(menuTree.id);
                    }}
                  >
                    <ArchiveRestore size={13} aria-hidden="true" />
                    <span>Unarchive research</span>
                  </button>
                ) : (
                  <>
                    <button
                      className="control-button"
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setMenu(null);
                        onToggleStar(menuTree.id);
                      }}
                    >
                      {isResearchStarred(folderState, menuTree.id) ? (
                        <StarOff size={13} aria-hidden="true" />
                      ) : (
                        <Star size={13} aria-hidden="true" />
                      )}
                      <span>
                        {isResearchStarred(folderState, menuTree.id) ? "Unstar" : "Star"}
                      </span>
                    </button>
                    <button className="control-button"
                      type="button"
                      role="menuitem"
                      onClick={() => openRenameDialog(menuTree)}
                    >
                      <Pencil size={13} aria-hidden="true" />
                      <span>Rename</span>
                    </button>
                    {menuTree.kind !== "document" ? (
                      // Title regeneration reruns the root prompt through the
                      // title model; a document has no prompt to rerun.
                      <button className="control-button"
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          setMenu(null);
                          void onRegenerateTitle(menuTree.id);
                        }}
                      >
                        <RefreshCw size={13} aria-hidden="true" />
                        <span>Regenerate title</span>
                      </button>
                    ) : null}
                    {folderState.membership[menuTree.id] ? (
                      <button
                        className="control-button"
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          setMenu(null);
                          onRemoveFromFolder([menuTree.id]);
                        }}
                      >
                        <FolderMinus size={13} aria-hidden="true" />
                        <span>Remove from folder</span>
                      </button>
                    ) : null}
                    <button
                      className="control-button"
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setMenu(null);
                        onRequestCreateFolder([menuTree.id]);
                      }}
                    >
                      <FolderPlus size={13} aria-hidden="true" />
                      <span>New folder with item</span>
                    </button>
                  </>
                )}
                {!menu.archived ? (
                  <>
                    <div className="context-menu-divider" role="separator" />
                    <button
                      type="button"
                      role="menuitem"
                      className="control-button context-menu-has-shortcut"
                      disabled={menuTree.runningCount > 0}
                      title={
                        menuTree.runningCount > 0
                          ? "Research with active runs cannot be archived"
                          : undefined
                      }
                      onClick={() => {
                        setMenu(null);
                        void onArchive(menuTree.id);
                      }}
                    >
                      <Archive size={13} aria-hidden="true" />
                      <span>Archive</span>
                      <kbd className="context-menu-shortcut is-keycap">A</kbd>
                    </button>
                  </>
                ) : null}
                <button
                  type="button"
                  role="menuitem"
                  className="control-button context-menu-danger context-menu-has-shortcut"
                  disabled={menuTree.runningCount > 0}
                  title={
                    menuTree.runningCount > 0
                      ? "Research with active runs cannot be deleted"
                      : undefined
                  }
                  onClick={() => openDeleteDialog(menuTree)}
                >
                  <Trash2 size={13} aria-hidden="true" />
                  <span>Delete</span>
                  <kbd className="context-menu-shortcut is-keycap">D</kbd>
                </button>
              </div>
            </div>,
            document.body,
          )
        : null}
      {deletingTree
        ? createPortal(
            <div
              className="confirm-dialog-backdrop"
              role="presentation"
              onMouseDown={(event) => {
                if (event.target === event.currentTarget && !removingTreeId) {
                  setDeletingTree(null);
                }
              }}
            >
              <div
                className="confirm-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="delete-research-dialog-title"
                aria-busy={removingTreeId === deletingTree.id}
                onKeyDown={(event) => {
                  if (event.key === "Escape" && !removingTreeId) {
                    event.preventDefault();
                    setDeletingTree(null);
                  }
                }}
              >
                <h2 id="delete-research-dialog-title">Delete “{deletingTree.title}”?</h2>
                <p>
                  This permanently deletes this research and its completed work and follow-up
                  history. This can’t be undone.
                </p>
                {treeRemovalError ? (
                  <p className="confirm-dialog-error" role="alert">
                    {treeRemovalError}
                  </p>
                ) : null}
                <div className="confirm-dialog-actions">
                  <button className="control-button"
                    type="button"
                    disabled={removingTreeId !== null}
                    onClick={() => setDeletingTree(null)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="control-button danger"
                    autoFocus
                    disabled={removingTreeId !== null}
                    onClick={() => void confirmTreeRemoval()}
                  >
                    {removingTreeId === deletingTree.id ? "Deleting…" : "Delete research"}
                  </button>
                </div>
              </div>
            </div>,
            document.body,
          )
        : null}
      {deletingFolder
        ? createPortal(
            <div
              className="confirm-dialog-backdrop"
              role="presentation"
              onMouseDown={(event) => {
                if (event.target === event.currentTarget && !folderRemovalBusy) {
                  setDeletingFolder(null);
                }
              }}
            >
              <div
                className="confirm-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="delete-research-folder-dialog-title"
                aria-busy={folderRemovalBusy}
                onKeyDown={(event) => {
                  if (event.key === "Escape" && !folderRemovalBusy) {
                    event.preventDefault();
                    setDeletingFolder(null);
                  }
                }}
              >
                <h2 id="delete-research-folder-dialog-title">
                  Delete “{deletingFolder.name}”?
                </h2>
                <p>
                  This permanently deletes the folder and all{" "}
                  {folderMemberTrees(deletingFolder.id).length} research items inside it,
                  including their completed work and follow-up history. This can’t be
                  undone.
                </p>
                {folderRemovalError ? (
                  <p className="confirm-dialog-error" role="alert">
                    {folderRemovalError}
                  </p>
                ) : null}
                <div className="confirm-dialog-actions">
                  <button
                    className="control-button"
                    type="button"
                    disabled={folderRemovalBusy}
                    onClick={() => setDeletingFolder(null)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="control-button danger"
                    autoFocus
                    disabled={folderRemovalBusy}
                    onClick={() => void confirmFolderRemoval()}
                  >
                    {folderRemovalBusy ? "Deleting…" : "Delete folder and items"}
                  </button>
                </div>
              </div>
            </div>,
            document.body,
          )
        : null}
      {dissolvingFolder
        ? createPortal(
            <div
              className="confirm-dialog-backdrop"
              role="presentation"
              onMouseDown={(event) => {
                if (event.target === event.currentTarget) {
                  setDissolvingFolder(null);
                }
              }}
            >
              <div
                className="confirm-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="dissolve-research-folder-dialog-title"
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    event.preventDefault();
                    setDissolvingFolder(null);
                  }
                }}
              >
                <h2 id="dissolve-research-folder-dialog-title">
                  Remove {folderMemberTrees(dissolvingFolder.id).length}{" "}
                  {folderMemberTrees(dissolvingFolder.id).length === 1 ? "item" : "items"}{" "}
                  from “{dissolvingFolder.name}”?
                </h2>
                <p>
                  The items return to the research list and the folder is removed. No
                  research is deleted.
                </p>
                <div className="confirm-dialog-actions">
                  <button
                    className="control-button"
                    type="button"
                    onClick={() => setDissolvingFolder(null)}
                  >
                    Cancel
                  </button>
                  <button
                    type="button"
                    className="control-button"
                    autoFocus
                    onClick={() => {
                      const folder = dissolvingFolder;
                      setDissolvingFolder(null);
                      onDissolveFolder(folder.id);
                    }}
                  >
                    Remove items
                  </button>
                </div>
              </div>
            </div>,
            document.body,
          )
        : null}
      {renamingTree || renamingFolder
        ? createPortal(
            <div
              className="confirm-dialog-backdrop"
              role="presentation"
              onMouseDown={(event) => {
                if (event.target === event.currentTarget) {
                  setRenamingTree(null);
                  setRenamingFolder(null);
                }
              }}
            >
              <form
                className="confirm-dialog rename-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="rename-research-dialog-title"
                onSubmit={(event) => {
                  event.preventDefault();
                  submitRename();
                }}
              >
                <h2 id="rename-research-dialog-title">
                  {renamingFolder ? "Rename folder" : "Rename research"}
                </h2>
                <input
                  ref={renameInputRef}
                  className="rename-dialog-input"
                  value={renameDraft}
                  aria-label={renamingFolder ? "Folder name" : "Research title"}
                  onChange={(event) => setRenameDraft(event.currentTarget.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Escape") {
                      event.preventDefault();
                      setRenamingTree(null);
                      setRenamingFolder(null);
                    }
                  }}
                />
                <div className="confirm-dialog-actions">
                  <button
                    className="control-button"
                    type="button"
                    onClick={() => {
                      setRenamingTree(null);
                      setRenamingFolder(null);
                    }}
                  >
                    Cancel
                  </button>
                  <button className="control-button" type="submit">Rename</button>
                </div>
              </form>
            </div>,
            document.body,
          )
        : null}
    </>
  );
}

export default memo(ResearchSidebarSection);
