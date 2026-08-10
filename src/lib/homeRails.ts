import type { HomeRailPastTurn } from "../components/HomeRails";
import type { HomeTurnSummary, Turn } from "../types";
import { firstUserTurnText } from "./appHelpers";
import { stripTaggedInstructionBlocksForPreview } from "./taggedInstructions";

// Strips prepended/inline tagged instruction blocks (<system-reminder>,
// <command-args>, …) from a turn for the compact home rail cards. Unlike the
// right pane's copy filter this also drops indented tag blocks, so slash-command
// markers never leak into a preview. Queued turns keep the raw text if stripping
// empties them (a card should never be blank).
export function railQueuedTurnText(text: string): string {
  const stripped = stripTaggedInstructionBlocksForPreview(text).trim();
  return stripped.length > 0 ? stripped : text;
}

// The latest prompt, for the rail's current card; null when stripping empties
// it so the card falls back to its empty-state text.
export function railLatestUserTurn(turns: Turn[]): string | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    const text = firstUserTurnText(turns[index]);
    if (text) {
      const stripped = stripTaggedInstructionBlocksForPreview(text).trim();
      return stripped.length > 0 ? stripped : null;
    }
  }
  return null;
}

// When the latest prompt was sent — feeds the current rail card's elapsed time.
export function latestUserTurnTimestamp(turns: Turn[]): number | null {
  for (let index = turns.length - 1; index >= 0; index -= 1) {
    if (firstUserTurnText(turns[index])) {
      return turns[index].timestamp ?? null;
    }
  }
  return null;
}

// Past prompts for a home rail: every non-superseded user turn before the
// latest one (which renders as the workstream's current card), stripped like
// the cards above; prompts that strip to nothing (pure tagged instructions)
// are dropped rather than shown raw. settledAt is the timestamp of the
// exchange's last record before the next prompt, falling back to the prompt's
// own. Cached by the turns array's identity — agentTurnInfoById hands back the
// same array while an agent's turns are unchanged, so an event batch only
// re-walks agents that actually gained turns.
const railPastTurnsCache = new WeakMap<Turn[], HomeRailPastTurn[]>();

export function railPastTurns(turns: Turn[]): HomeRailPastTurn[] {
  const cached = railPastTurnsCache.get(turns);
  if (cached) {
    return cached;
  }
  const result: HomeRailPastTurn[] = [];
  let pending: HomeRailPastTurn | null = null;
  let exchangeLastTimestamp: number | null = null;
  for (const turn of turns) {
    const text = firstUserTurnText(turn);
    if (text) {
      if (pending) {
        pending.settledAt = exchangeLastTimestamp ?? pending.settledAt;
        result.push(pending);
      }
      const stripped = stripTaggedInstructionBlocksForPreview(text).trim();
      pending =
        stripped.length > 0
          ? { id: turn.id, text: stripped, settledAt: turn.timestamp ?? null }
          : null;
      exchangeLastTimestamp = null;
    } else if (
      turn.status !== "superseded" &&
      turn.contextStatus !== "rolledBack" &&
      typeof turn.timestamp === "number"
    ) {
      exchangeLastTimestamp = turn.timestamp;
    }
  }
  // The dangling pending prompt is the latest user turn — the current card.
  railPastTurnsCache.set(turns, result);
  return result;
}

export function railPastTurnSummaries(summaries: HomeTurnSummary[]): HomeRailPastTurn[] {
  return summaries.flatMap((summary) => {
    const text = stripTaggedInstructionBlocksForPreview(summary.text).trim();
    return text
      ? [{ id: summary.id, text, settledAt: summary.settledAt }]
      : [];
  });
}

export function mergeRailPastTurns(
  historical: HomeRailPastTurn[],
  recent: HomeRailPastTurn[],
): HomeRailPastTurn[] {
  const recentById = new Map(recent.map((turn) => [turn.id, turn]));
  const historicalIds = new Set(historical.map((turn) => turn.id));
  return [
    ...historical.map((turn) => recentById.get(turn.id) ?? turn),
    ...recent.filter((turn) => !historicalIds.has(turn.id)),
  ];
}
