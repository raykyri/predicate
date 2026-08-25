export type LauncherTabAction = "capture" | "cycle-model" | "cycle-provider";

interface LauncherTabInput {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

export function launcherTabAction(
  event: LauncherTabInput,
  hasModelSelection: boolean,
): LauncherTabAction | null {
  if (event.key !== "Tab" || event.metaKey || event.ctrlKey || event.altKey) {
    return null;
  }
  if (event.shiftKey) {
    return "cycle-provider";
  }
  return hasModelSelection ? "cycle-model" : "capture";
}
