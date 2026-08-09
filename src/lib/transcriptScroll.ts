export interface TranscriptScrollCaptureSlot {
  capture: () => void;
  register: (capture: () => void) => () => void;
}

/**
 * Holds the active transcript's synchronous scroll capture. Registration
 * cleanups are token-scoped so an outgoing pane cannot clear a newer pane's
 * capture when React switches their layout effects in the same commit.
 */
export function createTranscriptScrollCaptureSlot(): TranscriptScrollCaptureSlot {
  let current: (() => void) | null = null;

  return {
    capture: () => current?.(),
    register: (capture) => {
      current = capture;
      return () => {
        if (current === capture) {
          current = null;
        }
      };
    },
  };
}
