import {
  TERMINAL_FONT_FAMILY,
  TERMINAL_FONT_SIZE,
  TERMINAL_FONT_SIZE_MAX,
  TERMINAL_FONT_SIZE_MIN,
} from "./terminalFont";
import {
  DEFAULT_CONFIRM_PASTE_OVER_CHARS,
  type PasteProtectionSettings,
} from "./paste";

export interface FontOption {
  id: string;
  label: string;
  /** Full CSS font-family stack applied to the terminal. */
  stack: string;
  /** A single font family name understood by Ghostty's native font matcher. */
  nativeFamily: string;
  /**
   * Extra inter-character spacing in px mapped to Ghostty's cell-width adjustment.
   * Defaults to 0 when omitted. Monaco's glyphs sit a touch tight, so
   * a hairline 0.01px nudge keeps the cells from looking cramped.
   */
  letterSpacing?: number;
}

export interface BodyFontOption {
  id: string;
  label: string;
  /** Full CSS font-family stack applied to non-terminal app UI. */
  stack: string;
  /** Full/PostScript local face names used to verify an optional installed font. */
  localNames?: readonly string[];
}

const SYSTEM_BODY_FONT_STACK =
  'ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif';

export const BODY_FONT_OPTIONS: BodyFontOption[] = [
  {
    id: "inter",
    label: "Inter",
    stack: `"Inter", ${SYSTEM_BODY_FONT_STACK}`,
    localNames: ["Inter Regular", "Inter-Regular"],
  },
  {
    id: "anthropic-sans-text",
    label: "Anthropic Sans Text",
    // Personal-use font referenced from the host system; do not bundle its files.
    stack: `"Anthropic Sans Text", ${SYSTEM_BODY_FONT_STACK}`,
    localNames: ["Anthropic Sans Text Regular", "AnthropicSansText-Regular"],
  },
  {
    id: "valley-sans",
    label: "Valley Sans",
    // Roman and italic variable faces are bundled with qmux under the SIL OFL 1.1.
    stack: `"Valley Sans", ${SYSTEM_BODY_FONT_STACK}`,
  },
  {
    id: "system",
    label: "System",
    stack: SYSTEM_BODY_FONT_STACK,
  },
];

export const DEFAULT_BODY_FONT_ID = BODY_FONT_OPTIONS[0].id;
export const SYSTEM_BODY_FONT_ID = "system";

function localFontSource(localName: string): string {
  const escapedName = localName.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  return `local("${escapedName}")`;
}

async function localFontIsAvailable(localName: string): Promise<boolean> {
  try {
    const probe = new FontFace("__qmux_local_font_probe__", localFontSource(localName));
    await probe.load();
    return true;
  } catch {
    return false;
  }
}

/** Returns bundled/generic fonts plus locally installed optional faces, in menu order. */
export async function detectAvailableBodyFonts(): Promise<BodyFontOption[]> {
  if (typeof FontFace === "undefined") {
    return BODY_FONT_OPTIONS.filter((option) => option.localNames === undefined);
  }

  const availability = await Promise.all(
    BODY_FONT_OPTIONS.map(async (option) => {
      if (option.localNames === undefined) {
        return true;
      }
      const matches = await Promise.all(option.localNames.map(localFontIsAvailable));
      return matches.some(Boolean);
    }),
  );

  return BODY_FONT_OPTIONS.filter((_option, index) => availability[index]);
}

// Shared fallback chain so a face that is missing on the host degrades to the
// platform monospace rather than a proportional font.
const MONO_FALLBACK =
  'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", monospace';

// The small, curated list shown in the settings dropdown. JetBrains Mono is the
// only bundled face; the rest are common system monospace fonts. Adding an entry
// here is all it takes to grow the menu.
export const FONT_OPTIONS: FontOption[] = [
  {
    id: "jetbrains-mono",
    label: "JetBrains Mono",
    stack: TERMINAL_FONT_FAMILY,
    nativeFamily: "JetBrains Mono",
  },
  {
    id: "sf-mono",
    label: "SF Mono",
    stack: `"SF Mono", ${MONO_FALLBACK}`,
    nativeFamily: "SF Mono",
  },
  {
    id: "menlo",
    label: "Menlo",
    stack: `"Menlo", ${MONO_FALLBACK}`,
    nativeFamily: "Menlo",
  },
  {
    id: "monaco",
    label: "Monaco",
    stack: `"Monaco", ${MONO_FALLBACK}`,
    nativeFamily: "Monaco",
    letterSpacing: 0.01,
  },
];

export const DEFAULT_FONT_ID = FONT_OPTIONS[0].id;

/**
 * The built-in qmux terminal colors. Any other value names a Ghostty color
 * scheme from the catalog bundled with libghostty-spm; the native side falls
 * back to the qmux colors when a stored name no longer resolves.
 */
export const DEFAULT_THEME_ID = "qmux";

export type ColorTheme = "green-blob" | "orange-blob";
export type CursorStyle = "block" | "underline" | "bar";
export type MouseWheelSensitivity = "low" | "normal" | "high" | "veryHigh";
export type TabTitleProvider = "appleFoundationModels" | "openRouter" | "disabled";
export type WorktreeLocation = "global" | "localQmux" | "localClaude";

export const COLOR_THEME_OPTIONS: { id: ColorTheme; label: string }[] = [
  { id: "green-blob", label: "Cool" },
  { id: "orange-blob", label: "Warm" },
];

export const CURSOR_STYLE_OPTIONS: { id: CursorStyle; label: string }[] = [
  { id: "block", label: "Block" },
  { id: "bar", label: "Bar" },
  { id: "underline", label: "Underline" },
];

export const MOUSE_WHEEL_SENSITIVITY_OPTIONS: {
  id: MouseWheelSensitivity;
  label: string;
  value: number;
}[] = [
  { id: "low", label: "Low", value: 1 },
  { id: "normal", label: "Standard", value: 1.75 },
  { id: "high", label: "High", value: 3 },
  { id: "veryHigh", label: "Very high", value: 5 },
];

export const TAB_TITLE_PROVIDER_OPTIONS: { id: TabTitleProvider; label: string }[] = [
  { id: "appleFoundationModels", label: "Apple Foundation Models" },
  { id: "openRouter", label: "OpenRouter" },
  { id: "disabled", label: "Disable" },
];

export const WORKTREE_LOCATION_OPTIONS: { id: WorktreeLocation; label: string }[] = [
  { id: "global", label: "Global (default)" },
  { id: "localQmux", label: "Local .qmux/" },
  { id: "localClaude", label: "Local .claude/" },
];

export const DEFAULT_SCROLLBACK_ROWS = 10000;
export const SCROLLBACK_ROWS_MIN = 1000;
export const SCROLLBACK_ROWS_MAX = 200000;
export const DEFAULT_LINE_HEIGHT = 1;
export const LINE_HEIGHT_MIN = 0.7;
export const LINE_HEIGHT_MAX = 1.3;
export const LINE_HEIGHT_STEP = 0.1;
export const CONFIRM_PASTE_OVER_CHARS_MIN = 1;
export const CONFIRM_PASTE_OVER_CHARS_MAX = 5_000_000;
/**
 * Byte cap on the research launch instruction, mirroring
 * MAX_RESEARCH_LAUNCH_INSTRUCTION_BYTES on the backend (which refuses larger
 * values), so the dialog can never hold a value the backend won't persist.
 */
export const RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES = 4 * 1024;

/**
 * Trims a research launch instruction to the backend's byte cap at a code-point
 * boundary. The cap counts UTF-8 bytes — the unit the backend validates — not
 * UTF-16 length, so a multi-byte instruction cannot pass here yet be refused
 * on save.
 */
export function clampResearchLaunchInstruction(value: string): string {
  let bytes = 0;
  let end = 0;
  for (const char of value) {
    const codePoint = char.codePointAt(0) ?? 0;
    const size = codePoint < 0x80 ? 1 : codePoint < 0x800 ? 2 : codePoint < 0x10000 ? 3 : 4;
    if (bytes + size > RESEARCH_LAUNCH_INSTRUCTION_MAX_BYTES) {
      break;
    }
    bytes += size;
    end += char.length;
  }
  return value.slice(0, end);
}

export interface AppSettings {
  /** color theme for application chrome and active states */
  colorTheme: ColorTheme;
  /** id into BODY_FONT_OPTIONS */
  bodyFontId: string;
  /** id into FONT_OPTIONS */
  fontId: string;
  /** terminal color theme: DEFAULT_THEME_ID or a catalog scheme name */
  themeId: string;
  /** terminal font size in px */
  fontSize: number;
  /** whether the focused terminal cursor blinks */
  cursorBlink: boolean;
  /** focused terminal cursor shape */
  cursorStyle: CursorStyle;
  /** approximate terminal scrollback rows retained by each native surface */
  scrollbackRows: number;
  /** scroll to bottom when the user types into the terminal */
  scrollOnUserInput: boolean;
  /** mouse wheel scrolling speed preset */
  mouseWheelSensitivity: MouseWheelSensitivity;
  /** terminal line-height multiplier */
  lineHeight: number;
  /** copy terminal selections as soon as text is selected */
  copyOnSelect: boolean;
  /** clear terminal selection after a copy action */
  selectionClearOnCopy: boolean;
  /** confirm pasted text containing more than one line */
  confirmMultiLinePaste: boolean;
  /** confirm pasted text above this many characters */
  confirmPasteOverChars: number;
  /** show Cmd-held shortcut badges in the sidebar */
  showShortcutHints: boolean;
  /** disable decorative/status pulse animations */
  reduceMotion: boolean;
  /** provider used to summarize first user messages into tab titles */
  tabTitleProvider: TabTitleProvider;
  /**
   * OpenRouter API key. Held in memory for the session but NOT persisted to
   * localStorage (saveSettings strips it) — the durable copy lives in the
   * backend's owner-only preferences file, loaded/saved via the openrouter_key_*
   * commands. A key found in a pre-existing localStorage blob is still read here
   * once so it can be migrated to the backend.
   */
  openRouterKey: string;
  /** OpenRouter model id */
  openRouterModel: string;
  /** keep the machine awake while any agent is running */
  preventSleep: boolean;
  /**
   * Run shells as login shells, sourcing the user's login profile files
   * (~/.zprofile + ~/.zlogin, or ~/.bash_profile/~/.profile) in addition to the
   * interactive rc. On by default so spawned shells match how terminal emulators
   * launch them. The backend persists its own copy (read on the spawn path,
   * including startup recovery); this mirror keeps the dialog in sync.
   */
  useLoginShell: boolean;
  /** root used for newly-created isolated worktrees */
  worktreeLocation: WorktreeLocation;
  /**
   * Custom instruction text sent with every research launch (fresh runs and
   * all follow-up kinds). Empty means launches send prompts unchanged. The
   * backend persists its own copy (read on every research launch path); this
   * mirror keeps the dialog in sync.
   */
  researchLaunchInstruction: string;
  /**
   * Show code-oriented context in tabs and the launcher: per-tab paths, git
   * worktree metadata, and the "New worktree" launcher option. When off, tabs
   * collapse to a single aligned row (status dot · title · status).
   */
  codeMode: boolean;
  /** show per-tab working directories when code mode is enabled */
  showTabDirectories: boolean;
  /** show tool calls and other activity detail in agent transcripts */
  showToolCalls: boolean;
  /** pin the latest user message to the top of the transcript while its reply scrolls */
  stickyUserMessages: boolean;
  /** require Command+Enter instead of bare Enter for composer submit shortcuts */
  requireCmdEnterToSend: boolean;
}

export const DEFAULT_SETTINGS: AppSettings = {
  colorTheme: "green-blob",
  bodyFontId: DEFAULT_BODY_FONT_ID,
  fontId: DEFAULT_FONT_ID,
  themeId: DEFAULT_THEME_ID,
  fontSize: TERMINAL_FONT_SIZE,
  cursorBlink: false,
  cursorStyle: "block",
  scrollbackRows: DEFAULT_SCROLLBACK_ROWS,
  scrollOnUserInput: true,
  mouseWheelSensitivity: "normal",
  lineHeight: DEFAULT_LINE_HEIGHT,
  copyOnSelect: false,
  selectionClearOnCopy: false,
  confirmMultiLinePaste: false,
  confirmPasteOverChars: DEFAULT_CONFIRM_PASTE_OVER_CHARS,
  showShortcutHints: true,
  reduceMotion: false,
  tabTitleProvider: "appleFoundationModels",
  openRouterKey: "",
  openRouterModel: "",
  preventSleep: true,
  useLoginShell: true,
  worktreeLocation: "global",
  researchLaunchInstruction: "",
  codeMode: true,
  showTabDirectories: true,
  showToolCalls: true,
  stickyUserMessages: true,
  requireCmdEnterToSend: true,
};

/** Resolves a stored body font id to its CSS stack, falling back to the default. */
export function bodyFontStackFor(bodyFontId: string): string {
  return (
    BODY_FONT_OPTIONS.find((option) => option.id === bodyFontId) ?? BODY_FONT_OPTIONS[0]
  ).stack;
}

/** Resolves a stored font id to its CSS stack, falling back to the default. */
export function fontStackFor(fontId: string): string {
  return (FONT_OPTIONS.find((option) => option.id === fontId) ?? FONT_OPTIONS[0]).stack;
}

/**
 * Resolves a stored font id to its terminal letter spacing (px), falling back
 * to the default. Fonts without an explicit value spell out 0 (no extra gap).
 */
export function letterSpacingFor(fontId: string): number {
  return (FONT_OPTIONS.find((option) => option.id === fontId) ?? FONT_OPTIONS[0]).letterSpacing ?? 0;
}

export function nativeFontFamilyFor(fontId: string): string {
  return (FONT_OPTIONS.find((option) => option.id === fontId) ?? FONT_OPTIONS[0]).nativeFamily;
}

export function clampFontSize(size: number): number {
  if (!Number.isFinite(size)) {
    return TERMINAL_FONT_SIZE;
  }
  return Math.min(TERMINAL_FONT_SIZE_MAX, Math.max(TERMINAL_FONT_SIZE_MIN, Math.round(size)));
}

export function clampScrollbackRows(rows: number): number {
  if (!Number.isFinite(rows)) {
    return DEFAULT_SCROLLBACK_ROWS;
  }
  return Math.min(SCROLLBACK_ROWS_MAX, Math.max(SCROLLBACK_ROWS_MIN, Math.round(rows)));
}

export function clampLineHeight(lineHeight: number): number {
  if (!Number.isFinite(lineHeight)) {
    return DEFAULT_LINE_HEIGHT;
  }
  const clamped = Math.min(LINE_HEIGHT_MAX, Math.max(LINE_HEIGHT_MIN, lineHeight));
  return Math.round(clamped * 10) / 10;
}

export function clampConfirmPasteOverChars(chars: number): number {
  if (!Number.isFinite(chars)) {
    return DEFAULT_CONFIRM_PASTE_OVER_CHARS;
  }
  return Math.min(
    CONFIRM_PASTE_OVER_CHARS_MAX,
    Math.max(CONFIRM_PASTE_OVER_CHARS_MIN, Math.round(chars)),
  );
}

export function scrollSensitivityFor(setting: MouseWheelSensitivity): number {
  return (
    MOUSE_WHEEL_SENSITIVITY_OPTIONS.find((option) => option.id === setting) ??
    MOUSE_WHEEL_SENSITIVITY_OPTIONS[1]
  ).value;
}

export function pasteProtectionFor(settings: AppSettings): PasteProtectionSettings {
  return {
    confirmMultiLinePaste: settings.confirmMultiLinePaste,
    confirmPasteOverChars: settings.confirmPasteOverChars,
  };
}

// Bumped if the stored shape ever changes incompatibly; an unknown blob simply
// falls back to defaults.
const STORAGE_KEY = "qmux.settings.v1";

/**
 * Reads the persisted application settings from localStorage. Any missing,
 * corrupt, or out-of-range field is replaced with its default, so a bad blob
 * never breaks startup — it just yields the defaults for the offending field.
 */
export function loadSettings(): AppSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) {
      return { ...DEFAULT_SETTINGS };
    }
    const parsed = JSON.parse(raw) as Omit<Partial<AppSettings>, "colorTheme"> & {
      colorTheme?: unknown;
      openRouterTitlesEnabled?: boolean;
    };
    const storedColorTheme = typeof parsed.colorTheme === "string" ? parsed.colorTheme : null;
    const colorTheme =
      COLOR_THEME_OPTIONS.find((option) => option.id === storedColorTheme)?.id ??
      DEFAULT_SETTINGS.colorTheme;
    const bodyFontId =
      typeof parsed.bodyFontId === "string" &&
      BODY_FONT_OPTIONS.some((option) => option.id === parsed.bodyFontId)
        ? parsed.bodyFontId
        : DEFAULT_BODY_FONT_ID;
    const fontId =
      typeof parsed.fontId === "string" && FONT_OPTIONS.some((option) => option.id === parsed.fontId)
        ? parsed.fontId
        : DEFAULT_FONT_ID;
    // Any non-empty string is accepted: the catalog lives on the native side,
    // which falls back to the default colors for names it cannot resolve.
    const themeId =
      typeof parsed.themeId === "string" && parsed.themeId.length > 0
        ? parsed.themeId
        : DEFAULT_THEME_ID;
    const fontSize =
      typeof parsed.fontSize === "number" ? clampFontSize(parsed.fontSize) : TERMINAL_FONT_SIZE;
    const cursorBlink =
      typeof parsed.cursorBlink === "boolean"
        ? parsed.cursorBlink
        : DEFAULT_SETTINGS.cursorBlink;
    const cursorStyle =
      typeof parsed.cursorStyle === "string" &&
      CURSOR_STYLE_OPTIONS.some((option) => option.id === parsed.cursorStyle)
        ? parsed.cursorStyle
        : DEFAULT_SETTINGS.cursorStyle;
    const scrollbackRows =
      typeof parsed.scrollbackRows === "number"
        ? clampScrollbackRows(parsed.scrollbackRows)
        : DEFAULT_SETTINGS.scrollbackRows;
    const scrollOnUserInput =
      typeof parsed.scrollOnUserInput === "boolean"
        ? parsed.scrollOnUserInput
        : DEFAULT_SETTINGS.scrollOnUserInput;
    const mouseWheelSensitivity =
      typeof parsed.mouseWheelSensitivity === "string" &&
      MOUSE_WHEEL_SENSITIVITY_OPTIONS.some((option) => option.id === parsed.mouseWheelSensitivity)
        ? parsed.mouseWheelSensitivity
        : DEFAULT_SETTINGS.mouseWheelSensitivity;
    const lineHeight =
      typeof parsed.lineHeight === "number"
        ? clampLineHeight(parsed.lineHeight)
        : DEFAULT_SETTINGS.lineHeight;
    const copyOnSelect =
      typeof parsed.copyOnSelect === "boolean"
        ? parsed.copyOnSelect
        : DEFAULT_SETTINGS.copyOnSelect;
    const selectionClearOnCopy =
      typeof parsed.selectionClearOnCopy === "boolean"
        ? parsed.selectionClearOnCopy
        : DEFAULT_SETTINGS.selectionClearOnCopy;
    const confirmMultiLinePaste =
      typeof parsed.confirmMultiLinePaste === "boolean"
        ? parsed.confirmMultiLinePaste
        : DEFAULT_SETTINGS.confirmMultiLinePaste;
    const confirmPasteOverChars =
      typeof parsed.confirmPasteOverChars === "number"
        ? clampConfirmPasteOverChars(parsed.confirmPasteOverChars)
        : DEFAULT_SETTINGS.confirmPasteOverChars;
    const preventSleep =
      typeof parsed.preventSleep === "boolean"
        ? parsed.preventSleep
        : DEFAULT_SETTINGS.preventSleep;
    const useLoginShell =
      typeof parsed.useLoginShell === "boolean"
        ? parsed.useLoginShell
        : DEFAULT_SETTINGS.useLoginShell;
    const worktreeLocation =
      typeof parsed.worktreeLocation === "string" &&
      WORKTREE_LOCATION_OPTIONS.some((option) => option.id === parsed.worktreeLocation)
        ? parsed.worktreeLocation
        : DEFAULT_SETTINGS.worktreeLocation;
    const researchLaunchInstruction =
      typeof parsed.researchLaunchInstruction === "string"
        ? clampResearchLaunchInstruction(parsed.researchLaunchInstruction)
        : DEFAULT_SETTINGS.researchLaunchInstruction;
    const showShortcutHints =
      typeof parsed.showShortcutHints === "boolean"
        ? parsed.showShortcutHints
        : DEFAULT_SETTINGS.showShortcutHints;
    const reduceMotion =
      typeof parsed.reduceMotion === "boolean"
        ? parsed.reduceMotion
        : DEFAULT_SETTINGS.reduceMotion;
    const tabTitleProvider =
      typeof parsed.tabTitleProvider === "string" &&
      TAB_TITLE_PROVIDER_OPTIONS.some((option) => option.id === parsed.tabTitleProvider)
        ? parsed.tabTitleProvider
        : parsed.openRouterTitlesEnabled === true
          ? "openRouter"
          : DEFAULT_SETTINGS.tabTitleProvider;
    const codeMode =
      typeof parsed.codeMode === "boolean" ? parsed.codeMode : DEFAULT_SETTINGS.codeMode;
    const showTabDirectories =
      typeof parsed.showTabDirectories === "boolean" ? parsed.showTabDirectories : codeMode;
    const showToolCalls =
      typeof parsed.showToolCalls === "boolean" ? parsed.showToolCalls : codeMode;
    const stickyUserMessages =
      typeof parsed.stickyUserMessages === "boolean"
        ? parsed.stickyUserMessages
        : codeMode;
    const requireCmdEnterToSend =
      typeof parsed.requireCmdEnterToSend === "boolean"
        ? parsed.requireCmdEnterToSend
        : codeMode;
    const openRouterKey =
      typeof parsed.openRouterKey === "string"
        ? parsed.openRouterKey
        : DEFAULT_SETTINGS.openRouterKey;
    const openRouterModel =
      typeof parsed.openRouterModel === "string"
        ? parsed.openRouterModel
        : DEFAULT_SETTINGS.openRouterModel;
    return {
      colorTheme,
      bodyFontId,
      fontId,
      themeId,
      fontSize,
      cursorBlink,
      cursorStyle,
      scrollbackRows,
      scrollOnUserInput,
      mouseWheelSensitivity,
      lineHeight,
      copyOnSelect,
      selectionClearOnCopy,
      confirmMultiLinePaste,
      confirmPasteOverChars,
      showShortcutHints,
      reduceMotion,
      tabTitleProvider,
      openRouterKey,
      openRouterModel,
      preventSleep,
      useLoginShell,
      worktreeLocation,
      researchLaunchInstruction,
      codeMode,
      showTabDirectories,
      showToolCalls,
      stickyUserMessages,
      requireCmdEnterToSend,
    };
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

/**
 * Persists the application settings. Failures (e.g. storage disabled or over
 * quota) are swallowed: the settings stay live for the session, just not saved.
 */
export function saveSettings(settings: AppSettings): void {
  try {
    // Never persist the OpenRouter key to localStorage: it lives in the backend's
    // owner-only preferences file instead, so an injected script can't read the secret
    // out of webview storage at rest.
    const { openRouterKey: _omitted, ...persistable } = settings;
    localStorage.setItem(STORAGE_KEY, JSON.stringify(persistable));
  } catch {
    // Storage unavailable; preferences remain in-memory for this session only.
  }
}
