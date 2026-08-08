import { useState } from "react";
import { ChevronRight } from "lucide-react";
import {
  thinkingProseText,
  type ActivityGroupItem,
  type ActivityItem,
  type ActivityLeafItem,
  type ThinkingItem,
  type ToolEntry,
  type TurnTimelineStatus,
} from "../lib/turnTimeline";
import { formatEstimatedTokenCount } from "../lib/tokenEstimate";
import TranscriptMarkdown, { type OversizedMarkdownPolicy } from "./TranscriptMarkdown";

// Reasoning can run long, and react-markdown re-parses on every render. Past
// this size, fall back to a plain-text (truncated) view — same guardrail the
// assistant answer uses.
const OVERSIZED_THINKING_MARKDOWN: OversizedMarkdownPolicy = {
  maxCharacters: 100_000,
  maxDisplayCharacters: 100_000,
  fallbackClassName: "thinking-plaintext",
};

const TOOL_SUMMARY_ARGUMENT_KEYS = {
  exec_command: "cmd",
  "functions.exec_command": "cmd",
  Bash: "command",
  WebFetch: "url",
  Read: "file_path",
  Edit: "file_path",
  MultiEdit: "file_path",
  Write: "file_path",
} as const;

const TOOL_ACTION_NAMES = {
  readFile: new Set(["read", "read_file", "glob", "grep", "ls"]),
  editFile: new Set([
    "edit",
    "multi_edit",
    "multiedit",
    "notebook_edit",
    "notebookedit",
    "write",
    "apply_patch",
  ]),
  runCommand: new Set(["bash", "exec_command", "shell", "run_command"]),
} as const;

type ToolActionKind = keyof typeof TOOL_ACTION_NAMES;

export function timelineStatusClass(status: TurnTimelineStatus | undefined) {
  return status ? ` is-status-${status}` : "";
}

export function serializeActivityValue(value: unknown, maxCharacters?: number) {
  let serialized: string;
  try {
    if (typeof value === "string") {
      serialized = value;
    } else {
      serialized = JSON.stringify(value, null, 2) ?? String(value);
    }
  } catch {
    serialized = "(payload could not be serialized)";
  }
  if (maxCharacters && serialized.length > maxCharacters) {
    return `${serialized.slice(0, maxCharacters)}\n… (truncated)`;
  }
  return serialized;
}

export function TranscriptActivityItem({
  item,
  isRootActivity = false,
  maxPayloadCharacters,
  deferPayloads = false,
  showResultTokenCount = true,
}: {
  item: ActivityItem;
  isRootActivity?: boolean;
  maxPayloadCharacters?: number;
  deferPayloads?: boolean;
  showResultTokenCount?: boolean;
}) {
  switch (item.type) {
    case "tool":
      return (
        <ToolEntryView
          entry={item}
          showChevron={!isRootActivity}
          maxPayloadCharacters={maxPayloadCharacters}
          showResultTokenCount={showResultTokenCount}
        />
      );
    case "thinking":
      return (
        <ThinkingView
          item={item}
          showChevron={!isRootActivity}
          maxPayloadCharacters={maxPayloadCharacters}
          deferPayloads={deferPayloads}
        />
      );
    case "activityGroup":
      return (
        <ActivityGroupView
          group={item}
          showChevron={!isRootActivity}
          maxPayloadCharacters={maxPayloadCharacters}
          deferPayloads={deferPayloads}
          showResultTokenCount={showResultTokenCount}
        />
      );
  }
}

function ActivityGroupView({
  group,
  showChevron,
  maxPayloadCharacters,
  deferPayloads,
  showResultTokenCount,
}: {
  group: ActivityGroupItem;
  showChevron: boolean;
  maxPayloadCharacters?: number;
  deferPayloads: boolean;
  showResultTokenCount: boolean;
}) {
  return (
    <details
      className={`activity-group-block${showChevron ? "" : " is-root-activity"}${timelineStatusClass(
        group.status,
      )}`}
    >
      <summary>
        {showChevron ? <DisclosureChevron /> : null}
        <span
          className={`activity-group-label ${
            group.toolCallCount > 0 ? "is-tool-group" : "is-thinking-group"
          }`}
        >
          {activityGroupLabel(group)}
        </span>
      </summary>
      <div className="activity-group-children">
        {group.children.map((child) => (
          <TranscriptActivityItem
            key={child.key}
            item={child}
            maxPayloadCharacters={maxPayloadCharacters}
            deferPayloads={deferPayloads}
            showResultTokenCount={showResultTokenCount}
          />
        ))}
      </div>
    </details>
  );
}

export function uniqueToolEntries(items: ActivityLeafItem[]) {
  const seen = new Set<string>();
  const entries: ToolEntry[] = [];
  for (const item of items) {
    if (item.type !== "tool") {
      continue;
    }
    const key = item.id ? `id:${item.id}` : `entry:${item.key}`;
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    entries.push(item);
  }
  return entries;
}

export function activityGroupLabel(group: ActivityGroupItem) {
  const entries = uniqueToolEntries(group.children);
  if (entries.length === 0) {
    return "Thought for a while";
  }
  return toolActionGroupLabel(entries) ?? calledToolsLabel(group.toolCallCount);
}

export function toolActionGroupLabel(entries: ToolEntry[]) {
  const counts: Record<ToolActionKind, number> = {
    readFile: 0,
    editFile: 0,
    runCommand: 0,
  };
  let unknownCount = 0;

  for (const entry of entries) {
    const kind = classifyToolAction(entry);
    if (kind) {
      counts[kind] += 1;
    } else {
      unknownCount += 1;
    }
  }

  const recognizedCount = counts.readFile + counts.editFile + counts.runCommand;
  if (recognizedCount === 0) {
    return null;
  }

  const parts = [
    fileActionLabel("read", counts.readFile),
    fileActionLabel("edited", counts.editFile),
    commandActionLabel(counts.runCommand),
    unknownCount > 0
      ? `called ${unknownCount} other tool${unknownCount === 1 ? "" : "s"}`
      : null,
  ].filter((part): part is string => Boolean(part));
  return capitalizeSentence(parts.join(", "));
}

function classifyToolAction(entry: ToolEntry): ToolActionKind | null {
  const name = normalizedToolName(entry.name);
  for (const [kind, names] of Object.entries(TOOL_ACTION_NAMES) as [
    ToolActionKind,
    ReadonlySet<string>,
  ][]) {
    if (names.has(name)) {
      return kind;
    }
  }
  return null;
}

function normalizedToolName(name: string) {
  const raw = name.trim();
  const dotted = raw.includes(".") ? (raw.split(".").pop() ?? raw) : raw;
  const namespaced = dotted.includes("__") ? (dotted.split("__").pop() ?? dotted) : dotted;
  return namespaced.replace(/[\s-]+/g, "_").toLowerCase();
}

function fileActionLabel(verb: "read" | "edited", count: number) {
  if (count === 0) {
    return null;
  }
  return count === 1 ? `${verb} a file` : `${verb} files`;
}

function commandActionLabel(count: number) {
  if (count === 0) {
    return null;
  }
  return count === 1 ? "ran a command" : `ran ${count} commands`;
}

function calledToolsLabel(count: number) {
  return `Called ${count} tool${count === 1 ? "" : "s"}`;
}

function capitalizeSentence(label: string) {
  return label.length > 0 ? `${label[0].toUpperCase()}${label.slice(1)}` : label;
}

function ToolEntryView({
  entry,
  showChevron,
  maxPayloadCharacters,
  showResultTokenCount,
}: {
  entry: ToolEntry;
  showChevron: boolean;
  maxPayloadCharacters?: number;
  showResultTokenCount: boolean;
}) {
  const summaryArgument = toolSummaryArgument(entry);
  const toolNameLabel = showChevron ? entry.name : `Called ${entry.name}`;
  const summaryLabel = summaryArgument ? `${toolNameLabel} ${summaryArgument}` : toolNameLabel;
  const [expanded, setExpanded] = useState(false);
  return (
    <details
      className={`tool-block tool-pair${entry.isError ? " is-error" : ""}${
        showChevron ? "" : " is-root-activity"
      }${timelineStatusClass(entry.status)}`}
      onToggle={(event) => setExpanded(event.currentTarget.open)}
    >
      <summary>
        {showChevron ? <DisclosureChevron /> : null}
        <span className="tool-summary">
          <span className="tool-summary-main" title={summaryLabel}>
            <span>{toolNameLabel}</span>
            {summaryArgument ? <span className="tool-summary-arg"> {summaryArgument}</span> : null}
          </span>
          <ToolEntryStatus
            entry={entry}
            showTokenCount={showChevron && showResultTokenCount}
          />
        </span>
      </summary>
      {expanded && entry.input !== undefined ? (
        <ToolPayload
          label="Input"
          value={entry.input}
          maxPayloadCharacters={maxPayloadCharacters}
        />
      ) : null}
      {expanded && entry.result !== undefined ? (
        <ToolPayload
          label={entry.isError ? "Error" : "Result"}
          value={entry.result}
          maxPayloadCharacters={maxPayloadCharacters}
          isError={entry.isError}
        />
      ) : null}
    </details>
  );
}

function toolSummaryArgument(entry: ToolEntry) {
  const key =
    TOOL_SUMMARY_ARGUMENT_KEYS[entry.name as keyof typeof TOOL_SUMMARY_ARGUMENT_KEYS] ?? null;
  if (!key) {
    return null;
  }
  const input = objectValue(entry.input);
  if (!input) {
    return null;
  }
  return inlineSummaryValue(input[key]);
}

function objectValue(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function inlineSummaryValue(value: unknown) {
  if (typeof value === "string") {
    return value.length > 0 ? value : null;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return null;
}

function ToolPayload({
  label,
  value,
  maxPayloadCharacters,
  isError,
}: {
  label: string;
  value: unknown;
  maxPayloadCharacters?: number;
  isError?: boolean;
}) {
  return (
    <div className={`tool-payload${isError ? " is-error" : ""}`}>
      <div className="tool-payload-label">{label}</div>
      <pre>{serializeActivityValue(value, maxPayloadCharacters)}</pre>
    </div>
  );
}

function ThinkingView({
  item,
  showChevron,
  maxPayloadCharacters,
  deferPayloads,
}: {
  item: ThinkingItem;
  showChevron: boolean;
  maxPayloadCharacters?: number;
  deferPayloads: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <details
      className={`thinking-block${showChevron ? "" : " is-root-activity"}${timelineStatusClass(
        item.status,
      )}`}
      onToggle={
        deferPayloads ? (event) => setExpanded(event.currentTarget.open) : undefined
      }
    >
      <summary>
        {showChevron ? <DisclosureChevron /> : null}
        <span>Thought for a while</span>
      </summary>
      {!deferPayloads || expanded
        ? item.values.map((value, index) => {
            const key = `${item.key}-${index}`;
            const prose = thinkingProseText(value);
            // Known reasoning shapes render as prose, dropping the opaque
            // `signature` token that would otherwise dominate a JSON dump.
            // Anything unrecognized still falls back to serialized JSON so no
            // content is silently lost.
            if (prose !== null) {
              return (
                <div key={key} className="thinking-prose">
                  <TranscriptMarkdown
                    text={prose}
                    oversizedContent={OVERSIZED_THINKING_MARKDOWN}
                    artifactLinks
                  />
                </div>
              );
            }
            return (
              <pre key={key}>{serializeActivityValue(value, maxPayloadCharacters)}</pre>
            );
          })
        : null}
    </details>
  );
}

export function RawTranscriptDisclosure({
  value,
  maxPayloadCharacters,
  deferPayload = false,
}: {
  value: unknown;
  maxPayloadCharacters?: number;
  deferPayload?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <details
      className="tool-block"
      onToggle={deferPayload ? (event) => setExpanded(event.currentTarget.open) : undefined}
    >
      <summary>
        <DisclosureChevron />
        <span>Raw</span>
      </summary>
      {!deferPayload || expanded ? (
        <pre>{serializeActivityValue(value, maxPayloadCharacters)}</pre>
      ) : null}
    </details>
  );
}

export function DisclosureChevron() {
  return <ChevronRight className="disclosure-chevron" size={12} aria-hidden="true" />;
}

// Serialized token estimates for the collapsed rows' "~N tok" labels. The
// label is visible without expanding, so it can't be deferred — but
// stringifying a large result object costs O(result) and the timeline
// re-mounts on every tab switch, repaying it for every entry each time. Result
// objects are immutable and identity-stable across resets (turn reconciliation
// reuses turn objects), so the formatted estimate is cached per object for the
// app's lifetime.
const serializedTokenEstimateByValue = new WeakMap<object, string>();

function serializedActivityTokenEstimate(value: unknown): string {
  if (typeof value === "string") {
    return formatEstimatedTokenCount(value);
  }
  if (typeof value === "object" && value !== null) {
    let estimate = serializedTokenEstimateByValue.get(value);
    if (estimate === undefined) {
      estimate = formatEstimatedTokenCount(serializeActivityValue(value));
      serializedTokenEstimateByValue.set(value, estimate);
    }
    return estimate;
  }
  return formatEstimatedTokenCount(serializeActivityValue(value));
}

function ToolEntryStatus({
  entry,
  showTokenCount,
}: {
  entry: ToolEntry;
  showTokenCount: boolean;
}) {
  if (entry.result === undefined) {
    return <span className="tool-summary-meta">running</span>;
  }
  if (!showTokenCount) {
    return entry.isError ? (
      <span className="tool-summary-meta">
        <span className="tool-summary-error">error</span>
      </span>
    ) : null;
  }
  const tokenCount = serializedActivityTokenEstimate(entry.result);
  if (entry.isError) {
    return (
      <span className="tool-summary-meta">
        <span className="tool-summary-error">error</span> · {tokenCount}
      </span>
    );
  }
  return <span className="tool-summary-meta">{tokenCount}</span>;
}
