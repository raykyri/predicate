// Whether fenced code blocks soft-wrap their long lines. One value for the
// whole app rather than one per block: flipping it from any block's menu (or
// the settings checkbox) rewraps every rendered block at once, and the choice
// is remembered across launches. Kept as a module-level store because the
// markdown renderer maps `pre` to a module-level component — there are no props
// to thread a value through — so every code block subscribes here directly.
const STORAGE_KEY = "qmux.code-wrap.v1";

let wrapped = readStoredCodeWrap();
const listeners = new Set<() => void>();

function readStoredCodeWrap(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY) === "true";
  } catch {
    // Storage unavailable (or no webview at all, as in tests): start unwrapped.
    return false;
  }
}

export function getCodeWrap(): boolean {
  return wrapped;
}

export function subscribeCodeWrap(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function setCodeWrap(next: boolean): void {
  if (next === wrapped) {
    return;
  }
  wrapped = next;
  try {
    localStorage.setItem(STORAGE_KEY, next ? "true" : "false");
  } catch {
    // Storage unavailable; the preference stays live for this session only.
  }
  for (const listener of listeners) {
    listener();
  }
}
