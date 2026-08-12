import completionSoundCatalog from "../assets/completion-sounds.json";

export interface CompletionSoundOption {
  id: string;
  label: string;
  systemName: string | null;
}

/** Canonical catalog shared with the Rust backend through the JSON source. */
export const COMPLETION_SOUND_OPTIONS: readonly CompletionSoundOption[] = completionSoundCatalog;

export type CompletionSoundId = string;

export const DEFAULT_COMPLETION_SOUND: CompletionSoundId = "chime";

export function isCompletionSoundId(value: unknown): value is CompletionSoundId {
  return (
    typeof value === "string" &&
    COMPLETION_SOUND_OPTIONS.some((option) => option.id === value)
  );
}
