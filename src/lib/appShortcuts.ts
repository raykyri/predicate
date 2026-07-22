export type AppShortcutCommand =
  | { type: "fontZoomIn" }
  | { type: "fontZoomOut" }
  | { type: "fontZoomReset" }
  | { type: "focusTab"; tabIndex: number }
  | { type: "focusResearchTab"; tabIndex: number }
  | { type: "homeOrCycleAdapter" }
  | { type: "openNewResearch" }
  | { type: "focusHome" }
  | { type: "focusResearchHome" }
  | { type: "focusTerminalMode" }
  | { type: "focusResearchMode" }
  | { type: "toggleSidebarMode" }
  | { type: "cyclePaneTab"; direction: -1 | 1 }
  | { type: "cycleAllTab"; direction: -1 | 1 }
  | { type: "moveSidebarItem"; direction: -1 | 1 }
  | { type: "openSettings" }
  | { type: "openCommandPalette" }
  | { type: "newDocument" }
  | { type: "focusFollowups" }
  | { type: "openFolderMenu" }
  | { type: "toggleTranscriptOrBrowser" }
  | { type: "splitPaneBelow" }
  | { type: "restoreClosedPane" }
  | { type: "closePane" }
  | { type: "newGroup" }
  | { type: "newPane" };

export interface AppShortcutInput {
  key: string;
  code?: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  terminalTarget?: boolean;
  editableTarget?: boolean;
}

function normalizedKey(key: string): string {
  switch (key.toLowerCase()) {
    case "{":
      return "[";
    case "}":
      return "]";
    default:
      return key.toLowerCase();
  }
}

export function resolveAppShortcut(input: AppShortcutInput): AppShortcutCommand | null {
  // WebKit can expose the backquote key as "Dead" (or a composed character)
  // after focus moves through web content. Use the physical code for this
  // layout-independent app shortcut so it remains available on Research.
  const key = input.code === "Backquote" ? "`" : normalizedKey(input.key);
  const command = input.metaKey;
  const control = input.ctrlKey;
  const option = input.altKey;
  const shift = input.shiftKey;
  const onePrimaryModifier = command !== control;

  if (
    command &&
    !control &&
    option &&
    !shift &&
    !input.editableTarget &&
    (key === "arrowup" || key === "arrowdown")
  ) {
    return { type: "moveSidebarItem", direction: key === "arrowup" ? -1 : 1 };
  }

  if (command && !control && !option) {
    if (key === "+" || key === "=") {
      return { type: "fontZoomIn" };
    }
    if (key === "-" && !shift) {
      return { type: "fontZoomOut" };
    }
    if (key === "0" && !shift) {
      return { type: "fontZoomReset" };
    }
  }

  if (onePrimaryModifier && !option && !shift && /^[1-9]$/.test(key)) {
    return { type: "focusTab", tabIndex: Number(key) - 1 };
  }

  if (command && !control && !option && !shift && key === "n") {
    return { type: "homeOrCycleAdapter" };
  }
  if (command && !control && !option && shift && key === "h") {
    return { type: "focusHome" };
  }
  if (command && !control && !option && shift && key === "r") {
    return { type: "focusResearchMode" };
  }
  if (command && !control && !option && !shift && key === "`") {
    return { type: "toggleSidebarMode" };
  }
  if (!command && control && !option && key === "tab") {
    return { type: "cyclePaneTab", direction: shift ? -1 : 1 };
  }
  if (command && !control && !option && shift && (key === "[" || key === "]")) {
    return { type: "cycleAllTab", direction: key === "[" ? -1 : 1 };
  }
  if (onePrimaryModifier && !option && !shift && key === ",") {
    return { type: "openSettings" };
  }
  // Only for web targets: with a terminal focused, ⌘K stays native (clear
  // screen), so the native classifier deliberately doesn't claim it either.
  if (command && !control && !option && !shift && key === "k" && !input.terminalTarget) {
    return { type: "openCommandPalette" };
  }
  if (onePrimaryModifier && !option && shift && key === "e") {
    return { type: "toggleTranscriptOrBrowser" };
  }
  // Research-surface commands. Ghostty binds neither chord, so claiming them
  // while a terminal is focused costs nothing (its AppKit layer swallows any
  // unbound Command chord anyway); outside research mode they execute as
  // no-ops.
  if (command && !control && !option && !shift && key === "j") {
    return { type: "focusFollowups" };
  }
  if (command && !control && !option && !shift && key === "o") {
    return { type: "openFolderMenu" };
  }
  if (command && !control && !option && key === "d") {
    return { type: "splitPaneBelow" };
  }
  if (command && !control && !option && shift && key === "t") {
    return { type: "restoreClosedPane" };
  }
  if (command && !control && !option && shift && key === "n") {
    return { type: "newGroup" };
  }
  if (
    key === "w" &&
    !option &&
    !shift &&
    ((command && !control) ||
      (!command && control && !input.terminalTarget && !input.editableTarget))
  ) {
    return { type: "closePane" };
  }
  if (command && !control && !option && !shift && key === "t") {
    return { type: "newPane" };
  }

  return null;
}

// Commands whose meaning follows the active workspace are resolved here so
// both DOM and native-terminal shortcut paths share the same behavior.
export function contextualizeAppShortcut(
  command: AppShortcutCommand,
  sidebarMode: "terminal" | "research" | "encyclopedia",
): AppShortcutCommand {
  // Encyclopedia mode takes no research remaps: its sidebar has no tabs or
  // composer, so the default (terminal-flavored) behaviors stand.
  if (sidebarMode === "research" && command.type === "homeOrCycleAdapter") {
    return { type: "focusResearchHome" };
  }
  if (sidebarMode === "research" && command.type === "newPane") {
    return { type: "openNewResearch" };
  }
  if (sidebarMode === "research" && command.type === "focusTab") {
    return { type: "focusResearchTab", tabIndex: command.tabIndex };
  }
  if (sidebarMode === "research" && command.type === "focusHome") {
    return { type: "focusResearchHome" };
  }
  if (sidebarMode === "research" && command.type === "restoreClosedPane") {
    return { type: "focusTerminalMode" };
  }
  if (sidebarMode === "research" && command.type === "cycleAllTab") {
    return { type: "cyclePaneTab", direction: command.direction };
  }
  // ⌘D: research has no splits, so the chord creates the other research
  // artifact instead — a document alongside ⌘T's new query.
  if (sidebarMode === "research" && command.type === "splitPaneBelow") {
    return { type: "newDocument" };
  }
  return command;
}

// Commands whose execution acts on the currently active pane (close, split,
// the per-pane transcript/browser toggle). The native terminal shortcut path
// uses this to decide what may still run when the chord's origin pane no
// longer exists in React: running one of these against a different pane than
// the user aimed at would be worse than dropping the keystroke, while every
// other command (mode toggles, Home, settings, palette…) is safe to run from
// anywhere.
export function appShortcutTargetsActivePane(command: AppShortcutCommand): boolean {
  return (
    command.type === "closePane" ||
    command.type === "splitPaneBelow" ||
    command.type === "toggleTranscriptOrBrowser" ||
    // Reorders the active sidebar item (pane or research tree). A native chord
    // from an already-removed pane must be dropped, not run against whatever is
    // active now, or it persistently reorders an unrelated item.
    command.type === "moveSidebarItem"
  );
}

// Human-readable action phrases for the settings conflict warning.
function appShortcutLabel(command: AppShortcutCommand): string {
  switch (command.type) {
    case "fontZoomIn":
    case "fontZoomOut":
    case "fontZoomReset":
      return "adjust the terminal font size";
    case "focusTab":
    case "focusResearchTab":
      return `focus tab ${command.tabIndex + 1}`;
    case "homeOrCycleAdapter":
      return "open the launcher";
    case "openNewResearch":
      return "start a new research";
    case "focusHome":
    case "focusResearchHome":
      return "focus Home";
    case "focusTerminalMode":
      return "switch to terminal mode";
    case "focusResearchMode":
      return "switch to research mode";
    case "toggleSidebarMode":
      return "toggle terminal/research mode";
    case "cyclePaneTab":
    case "cycleAllTab":
      return "cycle tabs";
    case "moveSidebarItem":
      return "move the active tab";
    case "openSettings":
      return "open settings";
    case "openCommandPalette":
      return "open the command palette";
    case "newDocument":
      return "create a document";
    case "focusFollowups":
      return "jump to the follow-ups";
    case "openFolderMenu":
      return "open the research folder menu";
    case "toggleTranscriptOrBrowser":
      return "toggle the transcript or browser";
    case "splitPaneBelow":
      return "split the terminal";
    case "restoreClosedPane":
      return "restore a closed tab";
    case "closePane":
      return "close the tab";
    case "newGroup":
      return "create a group";
    case "newPane":
      return "open a new tab";
  }
}

// The backend stores the global show/hide chord in its display form
// ("Shift+Command+A", "Command+`", "Option+Space"...). Map it onto the input
// shape DOM keydowns produce so it can be checked against the in-app table.
// Keys with no DOM equivalent (function keys, numpad, media keys) cannot
// collide and return null.
function acceleratorToShortcutInput(accelerator: string): AppShortcutInput | null {
  const parts = accelerator.split("+").map((part) => part.trim());
  const key = parts.pop();
  if (!key) {
    return null;
  }
  const modifiers = new Set(parts);
  const namedKeys: Record<string, string> = {
    Up: "arrowup",
    Down: "arrowdown",
    Left: "arrowleft",
    Right: "arrowright",
    Space: " ",
    Enter: "enter",
    Tab: "tab",
    Escape: "escape",
    Backspace: "backspace",
    Delete: "delete",
    Home: "home",
    End: "end",
    PageUp: "pageup",
    PageDown: "pagedown",
  };
  const domKey = key.length === 1 ? key.toLowerCase() : namedKeys[key];
  if (!domKey) {
    return null;
  }
  return {
    key: domKey,
    code: domKey === "`" ? "Backquote" : undefined,
    metaKey: modifiers.has("Command"),
    ctrlKey: modifiers.has("Control"),
    altKey: modifiers.has("Option"),
    shiftKey: modifiers.has("Shift"),
    terminalTarget: false,
    editableTarget: false,
  };
}

/**
 * Names the in-app shortcut a system-wide show/hide accelerator would shadow,
 * or null when there is no collision. An OS-registered hotkey consumes its
 * chord before the app sees any key event, so a colliding registration
 * silently disables the in-app command everywhere.
 */
export function showHideShortcutConflict(accelerator: string | null): string | null {
  if (!accelerator) {
    return null;
  }
  const input = acceleratorToShortcutInput(accelerator);
  if (!input) {
    return null;
  }
  const command = resolveAppShortcut(input);
  return command ? appShortcutLabel(command) : null;
}

export function appShortcutAllowsRepeat(command: AppShortcutCommand): boolean {
  return (
    command.type === "fontZoomIn" ||
    command.type === "fontZoomOut" ||
    command.type === "fontZoomReset" ||
    command.type === "moveSidebarItem"
  );
}

export function parseAppShortcutCommand(
  command: unknown,
  tabIndex: unknown,
): AppShortcutCommand | null {
  switch (command) {
    case "fontZoomIn":
    case "fontZoomOut":
    case "fontZoomReset":
    case "homeOrCycleAdapter":
    case "openNewResearch":
    case "focusHome":
    case "focusResearchHome":
    case "focusTerminalMode":
    case "focusResearchMode":
    case "toggleSidebarMode":
    case "openSettings":
    case "openCommandPalette":
    case "newDocument":
    case "focusFollowups":
    case "openFolderMenu":
    case "toggleTranscriptOrBrowser":
    case "splitPaneBelow":
    case "restoreClosedPane":
    case "closePane":
    case "newGroup":
    case "newPane":
      return { type: command };
    case "focusTab":
      return typeof tabIndex === "number" && Number.isInteger(tabIndex) && tabIndex >= 0
        ? { type: "focusTab", tabIndex }
        : null;
    case "focusResearchTab":
      return typeof tabIndex === "number" && Number.isInteger(tabIndex) && tabIndex >= 0
        ? { type: "focusResearchTab", tabIndex }
        : null;
    case "cyclePaneTabPrevious":
      return { type: "cyclePaneTab", direction: -1 };
    case "cyclePaneTabNext":
      return { type: "cyclePaneTab", direction: 1 };
    case "cycleAllTabPrevious":
      return { type: "cycleAllTab", direction: -1 };
    case "cycleAllTabNext":
      return { type: "cycleAllTab", direction: 1 };
    case "moveSidebarItemUp":
      return { type: "moveSidebarItem", direction: -1 };
    case "moveSidebarItemDown":
      return { type: "moveSidebarItem", direction: 1 };
    default:
      return null;
  }
}
