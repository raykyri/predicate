import assert from "node:assert/strict";
import test from "node:test";
import {
  appShortcutAllowsRepeat,
  appShortcutTargetsActivePane,
  contextualizeAppShortcut,
  parseAppShortcutCommand,
  resolveAppShortcut,
  showHideShortcutConflict,
  type AppShortcutInput,
} from "../src/lib/appShortcuts";

const shortcut = (overrides: Partial<AppShortcutInput>): AppShortcutInput => ({
  key: "",
  metaKey: false,
  ctrlKey: false,
  altKey: false,
  shiftKey: false,
  ...overrides,
});

test("resolves qmux command and control shortcuts", () => {
  assert.deepEqual(resolveAppShortcut(shortcut({ key: "t", metaKey: true })), {
    type: "newPane",
  });
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "n", metaKey: true, shiftKey: true })),
    { type: "newGroup" },
  );
  assert.deepEqual(resolveAppShortcut(shortcut({ key: "Tab", ctrlKey: true })), {
    type: "cyclePaneTab",
    direction: 1,
  });
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "Tab", ctrlKey: true, shiftKey: true })),
    { type: "cyclePaneTab", direction: -1 },
  );
  assert.deepEqual(resolveAppShortcut(shortcut({ key: "4", ctrlKey: true })), {
    type: "focusTab",
    tabIndex: 3,
  });
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "r", metaKey: true, shiftKey: true })),
    { type: "focusResearchMode" },
  );
  assert.deepEqual(resolveAppShortcut(shortcut({ key: "`", metaKey: true })), {
    type: "toggleSidebarMode",
  });
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "ArrowUp", metaKey: true, altKey: true })),
    {
      type: "moveSidebarItem",
      direction: -1,
    },
  );
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "ArrowDown", metaKey: true, altKey: true })),
    {
      type: "moveSidebarItem",
      direction: 1,
    },
  );
  assert.equal(resolveAppShortcut(shortcut({ key: "ArrowUp", altKey: true })), null);
  assert.equal(
    resolveAppShortcut(
      shortcut({ key: "ArrowUp", metaKey: true, altKey: true, editableTarget: true }),
    ),
    null,
  );
  assert.deepEqual(
    resolveAppShortcut(shortcut({ key: "Dead", code: "Backquote", metaKey: true })),
    { type: "toggleSidebarMode" },
  );
  assert.equal(resolveAppShortcut(shortcut({ key: ";", metaKey: true })), null);
  assert.equal(resolveAppShortcut(shortcut({ key: ";", ctrlKey: true })), null);
  assert.equal(
    resolveAppShortcut(shortcut({ key: "r", metaKey: true, altKey: true })),
    null,
  );
  // ⌘K opens the palette for web targets only; the terminal keeps it (clear
  // screen), matching the native classifier which never claims ⌘K.
  assert.deepEqual(resolveAppShortcut(shortcut({ key: "k", metaKey: true })), {
    type: "openCommandPalette",
  });
});

test("uses command-shift-t to leave Research and reopen tabs elsewhere", () => {
  const command = resolveAppShortcut(
    shortcut({ key: "t", metaKey: true, shiftKey: true }),
  );
  assert.deepEqual(command, { type: "restoreClosedPane" });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "focusTerminalMode",
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), {
    type: "restoreClosedPane",
  });
});

test("uses command-n for Research Home and the launcher elsewhere", () => {
  const command = resolveAppShortcut(shortcut({ key: "n", metaKey: true }));
  assert.deepEqual(command, { type: "homeOrCycleAdapter" });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "focusResearchHome",
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), {
    type: "homeOrCycleAdapter",
  });
});

test("uses command-t for new research and new panes elsewhere", () => {
  const command = resolveAppShortcut(shortcut({ key: "t", metaKey: true }));
  assert.deepEqual(command, { type: "newPane" });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "openNewResearch",
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), {
    type: "newPane",
  });
});

test("uses command-d for a new document on Research and splits elsewhere", () => {
  const command = resolveAppShortcut(shortcut({ key: "d", metaKey: true }));
  assert.deepEqual(command, { type: "splitPaneBelow" });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "newDocument",
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), {
    type: "splitPaneBelow",
  });
});

test("binds follow-up navigation and the folder menu to research chords", () => {
  const followups = resolveAppShortcut(shortcut({ key: "j", metaKey: true }));
  assert.deepEqual(followups, { type: "focusFollowups" });
  const folderMenu = resolveAppShortcut(shortcut({ key: "o", metaKey: true }));
  assert.deepEqual(folderMenu, { type: "openFolderMenu" });
  // Mode-independent commands: App executes them only on the research
  // surface, so contextualization leaves them alone.
  for (const command of [followups!, folderMenu!]) {
    assert.deepEqual(contextualizeAppShortcut(command, "research"), command);
    assert.deepEqual(contextualizeAppShortcut(command, "terminal"), command);
  }
  // Modified variants stay unclaimed.
  assert.equal(
    resolveAppShortcut(shortcut({ key: "j", metaKey: true, shiftKey: true })),
    null,
  );
  assert.equal(
    resolveAppShortcut(shortcut({ key: "o", metaKey: true, altKey: true })),
    null,
  );
  assert.equal(resolveAppShortcut(shortcut({ key: "o", ctrlKey: true })), null);
});

test("keeps the Home shortcut within the active mode", () => {
  const command = resolveAppShortcut(
    shortcut({ key: "h", metaKey: true, shiftKey: true }),
  );
  assert.deepEqual(command, { type: "focusHome" });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "focusResearchHome",
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), command);
});

test("keeps numbered tab shortcuts within the active mode", () => {
  const command = resolveAppShortcut(shortcut({ key: "4", metaKey: true }));
  assert.deepEqual(command, { type: "focusTab", tabIndex: 3 });
  assert.deepEqual(contextualizeAppShortcut(command!, "research"), {
    type: "focusResearchTab",
    tabIndex: 3,
  });
  assert.deepEqual(contextualizeAppShortcut(command!, "terminal"), command);
});

test("normalizes shifted bracket shortcuts", () => {
  const previous = resolveAppShortcut(
    shortcut({ key: "{", metaKey: true, shiftKey: true }),
  );
  const next = resolveAppShortcut(
    shortcut({ key: "}", metaKey: true, shiftKey: true }),
  );
  assert.deepEqual(previous, { type: "cycleAllTab", direction: -1 });
  assert.deepEqual(next, { type: "cycleAllTab", direction: 1 });
  assert.deepEqual(contextualizeAppShortcut(previous!, "research"), {
    type: "cyclePaneTab",
    direction: -1,
  });
  assert.deepEqual(contextualizeAppShortcut(next!, "research"), {
    type: "cyclePaneTab",
    direction: 1,
  });
  assert.deepEqual(contextualizeAppShortcut(previous!, "terminal"), previous);
  assert.deepEqual(contextualizeAppShortcut(next!, "terminal"), next);
});

test("leaves terminal command chords and control editing keys alone", () => {
  for (const key of ["a", "z", "Enter"]) {
    assert.equal(resolveAppShortcut(shortcut({ key, metaKey: true })), null);
  }
  assert.equal(
    resolveAppShortcut(shortcut({ key: "k", metaKey: true, terminalTarget: true })),
    null,
  );
  assert.equal(
    resolveAppShortcut(shortcut({ key: "w", ctrlKey: true, terminalTarget: true })),
    null,
  );
  assert.equal(resolveAppShortcut(shortcut({ key: "t", ctrlKey: true })), null);
});

test("continuous adjustment commands repeat", () => {
  assert.equal(appShortcutAllowsRepeat({ type: "fontZoomIn" }), true);
  assert.equal(
    appShortcutAllowsRepeat({ type: "moveSidebarItem", direction: -1 }),
    true,
  );
  assert.equal(appShortcutAllowsRepeat({ type: "newPane" }), false);
});

test("parses semantic commands from native payloads", () => {
  assert.deepEqual(parseAppShortcutCommand("focusTab", 2), {
    type: "focusTab",
    tabIndex: 2,
  });
  assert.deepEqual(parseAppShortcutCommand("cycleAllTabPrevious", null), {
    type: "cycleAllTab",
    direction: -1,
  });
  assert.deepEqual(parseAppShortcutCommand("newGroup", null), {
    type: "newGroup",
  });
  assert.deepEqual(parseAppShortcutCommand("focusTerminalMode", null), {
    type: "focusTerminalMode",
  });
  assert.deepEqual(parseAppShortcutCommand("focusResearchMode", null), {
    type: "focusResearchMode",
  });
  assert.deepEqual(parseAppShortcutCommand("toggleSidebarMode", null), {
    type: "toggleSidebarMode",
  });
  assert.deepEqual(parseAppShortcutCommand("moveSidebarItemUp", null), {
    type: "moveSidebarItem",
    direction: -1,
  });
  assert.deepEqual(parseAppShortcutCommand("moveSidebarItemDown", null), {
    type: "moveSidebarItem",
    direction: 1,
  });
  // Emitted by the native stranded-WKWebView fallback for Cmd+K; Swift has
  // already consumed the keyDown by then, so dropping it kills the chord.
  assert.deepEqual(parseAppShortcutCommand("openCommandPalette", null), {
    type: "openCommandPalette",
  });
  assert.deepEqual(parseAppShortcutCommand("newDocument", null), {
    type: "newDocument",
  });
  assert.deepEqual(parseAppShortcutCommand("focusFollowups", null), {
    type: "focusFollowups",
  });
  assert.deepEqual(parseAppShortcutCommand("openFolderMenu", null), {
    type: "openFolderMenu",
  });
  assert.equal(parseAppShortcutCommand("launcherOrCycleAdapter", null), null);
  assert.equal(parseAppShortcutCommand("focusTab", -1), null);
  assert.equal(parseAppShortcutCommand("notACommand", null), null);
});

test("show/hide accelerator conflicts name the shadowed in-app shortcut", () => {
  // Display-form accelerators come from the backend's shortcut normalizer.
  assert.equal(
    showHideShortcutConflict("Command+`"),
    "toggle terminal/research mode",
  );
  assert.equal(showHideShortcutConflict("Command+T"), "open a new tab");
  assert.equal(showHideShortcutConflict("Control+4"), "focus tab 4");
  assert.equal(
    showHideShortcutConflict("Option+Command+Up"),
    "move the active tab",
  );
  assert.equal(showHideShortcutConflict("Option+Up"), null);
  assert.equal(
    showHideShortcutConflict("Shift+Command+["),
    "cycle tabs",
  );
  // ⌘K only opens the palette for web targets, but a system-wide chord
  // shadows that too.
  assert.equal(showHideShortcutConflict("Command+K"), "open the command palette");
  assert.equal(showHideShortcutConflict("Command+J"), "jump to the follow-ups");
  assert.equal(
    showHideShortcutConflict("Command+O"),
    "open the research folder menu",
  );
  // Non-colliding and non-DOM chords stay quiet.
  assert.equal(showHideShortcutConflict("Option+Space"), null);
  assert.equal(showHideShortcutConflict("Shift+Command+A"), null);
  assert.equal(showHideShortcutConflict("Command+F13"), null);
  assert.equal(showHideShortcutConflict(null), null);
});

test("only pane-targeted commands are withheld from an unknown origin pane", () => {
  // These act on whatever pane is active, so a chord whose origin pane React
  // no longer knows must not run them against an unintended pane.
  assert.equal(appShortcutTargetsActivePane({ type: "closePane" }), true);
  assert.equal(appShortcutTargetsActivePane({ type: "splitPaneBelow" }), true);
  assert.equal(
    appShortcutTargetsActivePane({ type: "toggleTranscriptOrBrowser" }),
    true,
  );
  // Reorders the active sidebar item, so a stale native chord must be dropped.
  assert.equal(
    appShortcutTargetsActivePane({ type: "moveSidebarItem", direction: 1 }),
    true,
  );
  // Everything else is pane-independent and must survive: the native monitor
  // already consumed the keystroke, so withholding these would lose it.
  assert.equal(appShortcutTargetsActivePane({ type: "toggleSidebarMode" }), false);
  assert.equal(appShortcutTargetsActivePane({ type: "newDocument" }), false);
  assert.equal(appShortcutTargetsActivePane({ type: "focusFollowups" }), false);
  assert.equal(appShortcutTargetsActivePane({ type: "openFolderMenu" }), false);
  assert.equal(appShortcutTargetsActivePane({ type: "focusHome" }), false);
  assert.equal(appShortcutTargetsActivePane({ type: "openSettings" }), false);
  assert.equal(
    appShortcutTargetsActivePane({ type: "focusTab", tabIndex: 0 }),
    false,
  );
  assert.equal(
    appShortcutTargetsActivePane({ type: "cyclePaneTab", direction: 1 }),
    false,
  );
});
