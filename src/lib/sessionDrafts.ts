import { getInterfaceDraft, setInterfaceDraft } from "./api";

const STORAGE_PREFIX = "qmux.interface-draft.";
const SAVE_DEBOUNCE_MS = 120;
/** Small composers flush to the process-local backend immediately so a hard
 * WebContent kill (which may skip `pagehide`) still keeps the latest draft.
 * Large document bodies keep the short debounce and rely on visibility/pagehide
 * flushes for the residual window. */
const IMMEDIATE_BACKEND_FLUSH_MAX_CHARS = 4_096;

export const SESSION_DRAFT_KEYS = {
  homeLauncher: "home-launcher",
  homeComposers: "home-composers",
  newResearchInline: "new-research-inline",
  newResearchModal: "new-research-modal",
  newDocumentContext: "new-document-context",
  newDocumentFields: "new-document-fields",
  globalTaskLauncher: "global-task-launcher",
} as const;

const pending = new Map<string, { raw: string; timer: number }>();
const clearedKeys = new Set<string>();

function storageKey(key: string) {
  return `${STORAGE_PREFIX}${key}`;
}

function storeLocal(key: string, raw: string | null) {
  try {
    if (raw === null) {
      sessionStorage.removeItem(storageKey(key));
    } else {
      sessionStorage.setItem(storageKey(key), raw);
    }
  } catch {
    // The backend copy remains available when WebKit denies session storage.
  }
}

function readLocal(key: string): string | null {
  try {
    return sessionStorage.getItem(storageKey(key));
  } catch {
    return null;
  }
}

function flushPending(key: string) {
  const entry = pending.get(key);
  if (!entry) {
    return;
  }
  window.clearTimeout(entry.timer);
  pending.delete(key);
  void setInterfaceDraft(key, entry.raw).catch(() => undefined);
}

function flushAllPending() {
  for (const key of [...pending.keys()]) {
    flushPending(key);
  }
}

export function saveSessionDraftJson(key: string, value: unknown) {
  clearedKeys.delete(key);
  const raw = JSON.stringify(value);
  if (raw === undefined) {
    clearSessionDraft(key);
    return;
  }
  storeLocal(key, raw);
  const existing = pending.get(key);
  if (existing) {
    window.clearTimeout(existing.timer);
  }
  if (raw.length <= IMMEDIATE_BACKEND_FLUSH_MAX_CHARS) {
    pending.delete(key);
    void setInterfaceDraft(key, raw).catch(() => undefined);
    return;
  }
  pending.set(key, {
    raw,
    timer: window.setTimeout(() => flushPending(key), SAVE_DEBOUNCE_MS),
  });
}

export function clearSessionDraft(key: string) {
  clearedKeys.add(key);
  const existing = pending.get(key);
  if (existing) {
    window.clearTimeout(existing.timer);
    pending.delete(key);
  }
  storeLocal(key, null);
  void setInterfaceDraft(key, null).catch(() => undefined);
}

export function readSessionDraftJson<T>(key: string): T | null {
  const raw = pending.get(key)?.raw ?? readLocal(key);
  if (raw === null) {
    return null;
  }
  try {
    return JSON.parse(raw) as T;
  } catch {
    storeLocal(key, null);
    return null;
  }
}

export async function loadSessionDraftJson<T>(key: string): Promise<T | null> {
  const local = readSessionDraftJson<T>(key);
  if (local !== null) {
    return local;
  }
  if (clearedKeys.has(key)) {
    return null;
  }
  const raw = await getInterfaceDraft(key);
  const latestLocal = readSessionDraftJson<T>(key);
  if (latestLocal !== null) {
    return latestLocal;
  }
  if (raw === null || clearedKeys.has(key)) {
    return null;
  }
  try {
    const parsed = JSON.parse(raw) as T;
    storeLocal(key, raw);
    return parsed;
  } catch {
    clearSessionDraft(key);
    return null;
  }
}

if (typeof window !== "undefined") {
  window.addEventListener("pagehide", flushAllPending);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      flushAllPending();
    }
  });
}
