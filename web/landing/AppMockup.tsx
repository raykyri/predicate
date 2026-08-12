// A full-fidelity replica of the qmux desktop window, built from the same class
// names, tokens, and geometry the app itself uses (src/styles/*). It replaces the
// screenshot in the hero: real text at the app's real type ramp, reflowing with
// the page and restyling with the app instead of going stale like a PNG.
//
// What is rendered here is the *finished* state, and it is inert: every control
// is a <span>/<div>, so with no JavaScript the replica adds no tab stops and is
// exposed to assistive tech as one labelled image.
//
// site/mockup.js progressively enhances it — replaying the session, wiring the
// composer's queue, and expanding sidebar groups — by rewriting exactly the parts
// it makes real. Two contracts it relies on:
//   * `data-step` marks the replay timeline. The terminal and the transcript
//     share one step sequence, so a command streams in as the agent turn that
//     ran it appears.
//   * `data-mock-features` lists the enhancements to run; drop a name and that
//     feature stays off while the rest keep working.
import React from "react";
import {
  BookMarkedIcon,
  BookOpenIcon,
  ChevronDownIcon,
  ChevronsDownUpIcon,
  ChevronsUpDownIcon,
  EllipsisIcon,
  EllipsisVerticalIcon,
  ExpandIcon,
  FolderIcon,
  GlobeIcon,
  HouseIcon,
  MessageSquareTextIcon,
  Minimize2Icon,
  PanelLeftCloseIcon,
  PanelLeftOpenIcon,
  PanelRightCloseIcon,
  PanelRightOpenIcon,
  PaperclipIcon,
  SettingsIcon,
  SquareCenterlineDashedVerticalIcon,
  SquareTerminalIcon,
  XIcon,
} from "./icons";
import {
  ARTIFACTS,
  ARTIFACT_COUNT,
  BROWSER_URL,
  COMPOSER_PLACEHOLDER,
  DEFAULT_SESSION_ID,
  MOCK_GROUPS,
  MOCK_SESSIONS,
  SAVED_PROMPTS,
  SESSION_LABEL,
  SESSION_LABELS,
  type MockGroup,
  type MockPane,
  type MockSession,
  type TerminalLine,
} from "./mockupData";

const MOCKUP_LABEL =
  "The qmux desktop window: a sidebar of open-source projects and their agents on the left, " +
  "a live Codex terminal in the middle, and the agent's rendered transcript with a turn " +
  "queue on the right.";

// The blocking replay bootstrap activates these hints before the mockup's body
// is parsed. Without that bootstrap they are inert data attributes, so a failed
// or disabled enhancement still leaves the complete static replica visible.
const DEFAULT_REPLAY_START_STEP = Math.min(
  ...MOCK_SESSIONS[DEFAULT_SESSION_ID].terminalBlocks.map((block) => block.step),
);

function replayPending(stageReplay: boolean, sessionId: string, step: number) {
  return stageReplay && sessionId === DEFAULT_SESSION_ID && step > DEFAULT_REPLAY_START_STEP
    ? ""
    : undefined;
}

function TrafficLights() {
  return (
    <span className="mock-traffic-lights">
      <span className="mock-traffic-light is-close" />
      <span className="mock-traffic-light is-minimize" />
      <span className="mock-traffic-light is-zoom" />
    </span>
  );
}

function PaneTab({ pane }: { pane: MockPane }) {
  return (
    <div
      className={`pane-tab-row${pane.selected ? " is-selected" : ""}`}
      data-mock-session-tab={pane.sessionId}
      data-mock-session-label={SESSION_LABELS[pane.sessionId]}
      data-mock-session-status={pane.status}
    >
      <span className="control-button pane-tab">
        <span className={`pane-tab-dot status-${pane.status}`} />
        <span className="pane-tab-content">
          <span className="pane-tab-title">{pane.title}</span>
        </span>
        {pane.badge ? (
          <span className="pane-tab-meta">
            <small className="pane-tab-status">{pane.badge}</small>
          </span>
        ) : null}
      </span>
    </div>
  );
}

// Every group renders its panes; a collapsed group just hides the list, so the
// enhancement script can expand one by toggling a class.
function PaneGroup({ group }: { group: MockGroup }) {
  return (
    <section
      className={`pane-group has-panes${group.collapsed ? " is-collapsed" : " is-active-group"}`}
    >
      <div className="pane-group-header">
        <span className="pane-group-title">
          <FolderIcon className="lucide pane-group-folder" size={13} />
          <span className="pane-group-name">{group.name}</span>
          <span className="pane-group-count">{group.panes.length}</span>
          {group.statuses?.length ? (
            <span className="pane-group-status-icons">
              {group.statuses.map((status, index) => (
                <span key={index} className={`pane-tab-dot status-${status}`} />
              ))}
            </span>
          ) : null}
        </span>
        <span className="pane-group-aux">
          <span className="control-button pane-group-collapse-button">
            <ChevronsUpDownIcon className="lucide mock-icon-expand" size={14} />
            <ChevronsDownUpIcon className="lucide mock-icon-collapse" size={14} />
          </span>
          <span className="control-button pane-group-menu-button">
            <EllipsisIcon size={14} />
          </span>
        </span>
      </div>
      <div className="pane-list-body">
        {group.panes.map((pane, index) => (
          <PaneTab key={index} pane={pane} />
        ))}
      </div>
    </section>
  );
}

function Sidebar() {
  return (
    <aside className="sidebar is-code-mode">
      <span className="sidebar-collapse-button" data-mock-action="hide-sidebar">
        <PanelLeftCloseIcon size={14} />
      </span>
      <div className="sidebar-mode-toggle">
        <span className="is-selected">
          <SquareTerminalIcon size={13} />
          <span>Terminal</span>
        </span>
        <span>
          <BookOpenIcon size={13} />
          <span>Research</span>
        </span>
      </div>
      <div className="pane-home-row">
        <div className="control-button pane-tab">
          <HouseIcon size={12} />
          <span className="pane-tab-content">
            <span className="pane-tab-title">Home</span>
          </span>
        </div>
      </div>
      <nav className="pane-list">
        {MOCK_GROUPS.map((group, index) => (
          <PaneGroup key={index} group={group} />
        ))}
      </nav>
      <div className="sidebar-actions">
        <div className="sidebar-action-with-hint">
          <span className="control-button">
            <SquareTerminalIcon size={14} />
            <span>New shell</span>
          </span>
        </div>
        <div className="sidebar-action-with-hint">
          <span className="control-button">
            <MessageSquareTextIcon size={14} />
            <span>New agent</span>
          </span>
        </div>
        <div className="sidebar-action-with-hint">
          <span className="control-button sidebar-settings-button">
            <SettingsIcon size={14} />
          </span>
        </div>
      </div>
    </aside>
  );
}

function TerminalRow({ line }: { line: TerminalLine }) {
  return (
    <div className="mock-terminal-line">
      {line.spans.map((span, index) => (
        <span key={index} className={span.tone ? `tt-${span.tone}` : undefined}>
          {span.text}
        </span>
      ))}
      {line.spans.length === 0 ? " " : null}
    </div>
  );
}

function TerminalSession({
  sessionId,
  session,
  stageReplay,
}: {
  sessionId: string;
  session: MockSession;
  stageReplay: boolean;
}) {
  return (
    <div
      className="mock-terminal-screen"
      data-mock-session-view={sessionId}
      hidden={sessionId !== DEFAULT_SESSION_ID}
    >
      {session.terminalBlocks.map((block, blockIndex) => (
        <div
          key={blockIndex}
          className="mock-terminal-block"
          data-step={block.step}
          data-replay-pending={replayPending(stageReplay, sessionId, block.step)}
        >
          {block.lines.map((line, lineIndex) => (
            <TerminalRow key={lineIndex} line={line} />
          ))}
        </div>
      ))}
      <div className="mock-terminal-line mock-terminal-tail">
        <span className="mock-terminal-cursor" />
      </div>
    </div>
  );
}

function TerminalPane({ stageReplay }: { stageReplay: boolean }) {
  return (
    <div className="mock-terminal">
      {Object.entries(MOCK_SESSIONS).map(([sessionId, session]) => (
        <TerminalSession
          key={sessionId}
          sessionId={sessionId}
          session={session}
          stageReplay={stageReplay}
        />
      ))}
    </div>
  );
}

// A collapsed pane's way back sits where that pane was: the sidebar's restore
// at the window's top-left, just past the traffic lights, and the transcript's
// at the top-right where its header button was. The app groups both into the
// turn pane's header instead, which is learnable in a window you use every day
// and invisible in a demo you have thirty seconds with.
//
// They hang off the window rather than a pane so they survive the terminal
// dropping out at narrow widths.
function FloatingRestoreControls() {
  return (
    <>
      <span
        className="icon-button turn-pane-header-button turn-pane-floating-restore-button mock-restore-left"
        data-mock-action="show-sidebar"
      >
        <PanelLeftOpenIcon size={14} />
      </span>
      <span
        className="icon-button turn-pane-header-button turn-pane-floating-restore-button mock-restore-right"
        data-mock-action="show-right"
      >
        <PanelRightOpenIcon size={14} />
      </span>
    </>
  );
}

function TurnPaneHeader() {
  return (
    <div className="turn-pane-header">
      <div className="turn-pane-session-control">
        <span className="turn-pane-session">{SESSION_LABEL}</span>
      </div>
      <div className="turn-pane-header-controls">
        <span
          className="control-button turn-pane-header-button"
          data-mock-action="prompt-library"
        >
          <BookMarkedIcon size={14} />
        </span>
        <span
          className="control-button turn-pane-header-button"
          data-mock-action="queue-split"
        >
          <SquareCenterlineDashedVerticalIcon size={14} />
        </span>
        <span className="control-button turn-pane-header-button" data-mock-action="browser">
          <GlobeIcon size={14} />
        </span>
        <span
          className="control-button turn-pane-header-button artifact-tray-toggle"
          data-mock-action="artifacts"
        >
          <PaperclipIcon size={14} />
          <span className="artifact-tray-badge">{ARTIFACT_COUNT}</span>
        </span>
        <span
          className="control-button turn-pane-header-button"
          data-mock-action="expand-transcript"
        >
          <ExpandIcon className="lucide mock-icon-expand" size={14} />
          <Minimize2Icon className="lucide mock-icon-collapse" size={14} />
        </span>
        <div className="turn-pane-sidebar-controls">
          <span className="icon-button turn-pane-header-button" data-mock-action="hide-right">
            <PanelRightCloseIcon size={14} />
          </span>
        </div>
      </div>
    </div>
  );
}

interface MockMenuItem {
  label: string;
  badge?: string;
  disabled?: boolean;
}

function MockMenu({
  name,
  label,
  items,
  className,
}: {
  name: string;
  label: string;
  items: MockMenuItem[];
  className: string;
}) {
  return (
    <div
      className={`popover-surface popover-surface--context ${className}`}
      data-mock-menu={name}
      role="menu"
      aria-label={label}
      hidden
    >
      {items.map((item) => (
        <span
          className={`menu-item turn-message-menu-item${item.disabled ? " is-disabled" : ""}`}
          data-mock-menu-item
          role="menuitem"
          aria-disabled={item.disabled || undefined}
          key={item.label}
        >
          <span className="turn-message-menu-label">{item.label}</span>
          {item.badge ? <span className="turn-message-menu-badge">{item.badge}</span> : null}
        </span>
      ))}
    </div>
  );
}

const USER_MESSAGE_MENU: MockMenuItem[] = [
  { label: "Regenerate title" },
  { label: "Fork from here", badge: "Preview" },
  { label: "Copy message" },
  { label: "Copy handoff" },
  { label: "Save to prompt library" },
];

const AGENT_MESSAGE_MENU: MockMenuItem[] = [
  { label: "Fork from here", badge: "Preview" },
  { label: "Copy response" },
  { label: "Copy handoff" },
];

function TurnHeader({ label }: { label: string }) {
  const menuItems = label === "You" ? USER_MESSAGE_MENU : AGENT_MESSAGE_MENU;
  return (
    <header>
      <span className="turn-card-role-label">{label}</span>
      <span className="turn-message-menu">
        <span className="turn-message-menu-trigger" data-mock-menu-trigger>
          <EllipsisIcon size={14} />
        </span>
        <MockMenu
          name="message"
          label={`${label} message options`}
          items={menuItems}
          className="turn-message-menu-popover"
        />
      </span>
    </header>
  );
}

// Saved prompts, dropped into the composer on click. The search filters for
// real — a field that looked like it searched but did not would be worse than
// no field at all.
function PromptLibrary() {
  return (
    <div className="popover-surface prompt-library-menu" data-mock-panel="prompt-library" hidden>
      <input
        className="prompt-library-search"
        type="search"
        placeholder="Search prompts"
        aria-label="Search saved prompts"
      />
      <div className="prompt-library-list">
        {SAVED_PROMPTS.map((prompt) => (
          <div className="prompt-library-item" key={prompt}>
            <span className="prompt-library-item-main" data-mock-prompt={prompt}>
              <span className="prompt-library-item-text">{prompt}</span>
            </span>
          </div>
        ))}
      </div>
      <p className="prompt-library-empty" hidden>
        No prompt matches.
      </p>
    </div>
  );
}

// The tray of files this agent opened with `qmux open`. Opening an HTML one
// hands it to the browser overlay, which is what the app does with it.
function ArtifactTray() {
  return (
    <div className="artifact-tray" data-mock-panel="artifacts">
      <div className="artifact-tray-titlebar">
        <PaperclipIcon className="lucide artifact-tray-clip" size={11} />
        <span className="artifact-tray-label">Artifacts</span>
        <span className="artifact-tray-chrome-button" data-mock-action="artifacts">
          <XIcon size={12} />
        </span>
      </div>
      <div className="artifact-tray-body">
        {ARTIFACTS.map((artifact) => (
          <span className="artifact-tray-row" key={artifact.name} data-mock-artifact={artifact.name}>
            <span className="artifact-tray-name">{artifact.name}</span>
            <span className="artifact-tray-meta">{artifact.meta}</span>
          </span>
        ))}
      </div>
    </div>
  );
}

// The browser overlay floats over the terminal, inset from the window edges and
// clear of the turn pane, previewing whatever the agent is building.
function BrowserOverlay() {
  return (
    <div className="browser-overlay" data-mock-panel="browser" hidden>
      <div className="browser-overlay-nav">
        <span className="browser-overlay-close" data-mock-action="browser">
          <XIcon size={13} />
        </span>
        <span className="browser-overlay-address" data-mock-browser-url>
          {BROWSER_URL}
        </span>
      </div>
      <div className="browser-overlay-page">
        <div className="mock-preview">
          <p className="mock-preview-eyebrow">qmux &middot; landing preview</p>
          <h4>Interactive session mock</h4>
          <p className="mock-preview-figure">14 sessions rendered</p>
          <div className="mock-preview-row">
            <span className="mock-preview-name">qmux default transcript</span>
            <span className="mock-preview-pass">active</span>
          </div>
          <div className="mock-preview-row">
            <span className="mock-preview-name">inline transcript image</span>
            <span className="mock-preview-pass">rendered</span>
          </div>
          <div className="mock-preview-row">
            <span className="mock-preview-name">server tests</span>
            <span className="mock-preview-pass">19 passing</span>
          </div>
        </div>
      </div>
    </div>
  );
}

function TranscriptSession({
  sessionId,
  session,
  stageReplay,
}: {
  sessionId: string;
  session: MockSession;
  stageReplay: boolean;
}) {
  const isWorking = MOCK_GROUPS.some((group) =>
    group.panes.some((pane) => pane.sessionId === sessionId && pane.status === "active"),
  );
  return (
    <div
      className="turn-timeline"
      data-mock-session-view={sessionId}
      hidden={sessionId !== DEFAULT_SESSION_ID}
    >
      <article className="turn-card role-user" data-step={0}>
        <TurnHeader label="You" />
        <div className="turn-blocks">
          {session.userTurn.tags.map((tag) => (
            <div key={tag} className="turn-message-block">
              <p className="turn-text is-tagged-instruction">{tag}</p>
            </div>
          ))}
          <div className="turn-message-block">
            <p className="turn-text">{session.userTurn.text}</p>
          </div>
        </div>
      </article>

      {/* Only the first message of a consecutive agent run carries the role
          label; continuations after a tool group drop it, as the app does. */}
      {session.agentTurn.map((item, index) =>
        item.type === "paragraph" ? (
          <article
            key={index}
            className="turn-card role-assistant"
            data-step={item.step}
            data-replay-pending={replayPending(stageReplay, sessionId, item.step)}
          >
            {index === 0 ? <TurnHeader label="Codex" /> : null}
            <div className="turn-blocks">
              <div className="turn-message-block">
                <div className="turn-markdown">
                  <p>
                    {item.runs.map((run, runIndex) =>
                      run.code ? (
                        <code key={runIndex}>{run.text}</code>
                      ) : (
                        <span key={runIndex}>{run.text}</span>
                      ),
                    )}
                  </p>
                </div>
              </div>
            </div>
          </article>
        ) : item.type === "image" ? (
          <article
            key={index}
            className="turn-card role-assistant turn-image-card"
            data-step={item.step}
            data-replay-pending={replayPending(stageReplay, sessionId, item.step)}
          >
            <div className="turn-blocks">
              <div className="turn-message-block">
                <figure className="turn-image-embed">
                  <img
                    className="turn-image"
                    src={item.src}
                    alt={item.alt}
                    width={2704}
                    height={1704}
                    decoding="async"
                  />
                </figure>
              </div>
            </div>
          </article>
        ) : (
          <div
            key={index}
            className="activity-group-block is-root-activity"
            data-step={item.step}
            data-replay-pending={replayPending(stageReplay, sessionId, item.step)}
          >
            <div className="activity-summary">
              <span className="activity-group-label is-tool-group">{item.label}</span>
            </div>
          </div>
        ),
      )}

      {isWorking ? (
        <div className="turn-thinking" data-step={1}>
          <span className="turn-thinking-dot" />
          <span className="turn-thinking-label">Working…</span>
        </div>
      ) : null}
    </div>
  );
}

function Transcript({ stageReplay }: { stageReplay: boolean }) {
  return (
    <>
      {Object.entries(MOCK_SESSIONS).map(([sessionId, session]) => (
        <TranscriptSession
          key={sessionId}
          sessionId={sessionId}
          session={session}
          stageReplay={stageReplay}
        />
      ))}
    </>
  );
}

function Composer() {
  const composerMenuItems: MockMenuItem[] = [
    { label: "Save current draft as prompt", disabled: true },
    { label: "Copy handoff" },
    { label: "Copy queued messages", disabled: true },
    { label: "Copy transcript" },
    { label: "Copy transcript as JSON" },
    { label: "Publish transcript…" },
  ];
  return (
    <div className="turn-sidebar-input">
      <div className="native-input">
        <div className="mock-textarea">{COMPOSER_PLACEHOLDER}</div>
        <div className="native-input-submit-actions">
          <span className="composer-menu">
            <span className="link-button composer-menu-trigger" data-mock-menu-trigger>
              <EllipsisVerticalIcon size={15} />
            </span>
            <MockMenu
              name="composer"
              label="More composer actions"
              items={composerMenuItems}
              className="composer-menu-popover"
            />
          </span>
          <span className="control-button">
            <span>Send Now</span>
          </span>
          <span className="queue-button-group">
            <span className="control-button queue-button queue-button-main">
              <span>Queue</span>
              <span className="shortcut-hint">⌘↵</span>
            </span>
            <span className="control-button queue-menu-button">
              <ChevronDownIcon size={14} />
            </span>
          </span>
        </div>
      </div>
    </div>
  );
}

// `labelledBy` lets a caption outside the component name it, so a page that
// already describes the shot does not make screen readers hear it twice.
// Enhancements the client script may run, in the order it applies them. Passing
// a shorter list ships a quieter demo without touching the markup.
export const MOCKUP_FEATURES = [
  "replay",
  "queue",
  "groups",
  "sessions",
  "panes",
  "panels",
  "menus",
] as const;

export default function AppMockup({
  labelledBy,
  features = MOCKUP_FEATURES,
}: {
  labelledBy?: string;
  features?: readonly string[];
}) {
  const stageReplay = features.includes("replay");
  return (
    <div className="app-mockup-frame">
      <div
        className="app-mockup"
        role="img"
        aria-labelledby={labelledBy}
        aria-label={labelledBy ? undefined : MOCKUP_LABEL}
        data-mock-features={features.join(" ")}
      >
        <TrafficLights />
        <FloatingRestoreControls />
        <div className="app-shell has-turn-sidebar">
          <Sidebar />
          <TerminalPane stageReplay={stageReplay} />
          <BrowserOverlay />
          <div className="turn-pane">
            <div className="turn-sidebar has-header">
              <TurnPaneHeader />
              <PromptLibrary />
              <ArtifactTray />
              <Transcript stageReplay={stageReplay} />
              <Composer />
            </div>
          </div>
        </div>
      </div>
      <span className="mock-demo-status" role="status" aria-live="polite" />
    </div>
  );
}
