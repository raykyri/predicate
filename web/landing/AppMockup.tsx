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
  ArchiveIcon,
  ArrowLeftIcon,
  ArrowRightIcon,
  BookMarkedIcon,
  BookOpenIcon,
  CheckIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  ChevronsDownUpIcon,
  ChevronsUpDownIcon,
  Columns2Icon,
  CopyIcon,
  EllipsisIcon,
  EllipsisVerticalIcon,
  ExpandIcon,
  ExternalLinkIcon,
  FileTextIcon,
  FolderGit2Icon,
  FolderIcon,
  FolderMinusIcon,
  FolderPlusIcon,
  GitBranchIcon,
  GlobeIcon,
  LayoutDashboardIcon,
  LoaderCircleIcon,
  MessageSquareTextIcon,
  Minimize2Icon,
  NotebookPenIcon,
  PanelBottomCloseIcon,
  PanelBottomOpenIcon,
  PanelLeftCloseIcon,
  PanelLeftOpenIcon,
  PanelRightCloseIcon,
  PanelRightOpenIcon,
  PaperclipIcon,
  PencilIcon,
  PlusIcon,
  RotateCwIcon,
  SettingsIcon,
  SquareCenterlineDashedVerticalIcon,
  SquareTerminalIcon,
  StarIcon,
  TerminalIcon,
  Trash2Icon,
  Undo2Icon,
  XIcon,
  TweetLikeIcon,
  TweetReplyIcon,
  VerifiedBadgeIcon,
} from "./icons";
import {
  ARTIFACTS,
  ARTIFACT_COUNT,
  BROWSER_URL,
  COMPOSER_PLACEHOLDER,
  DEFAULT_RESEARCH_DOC_ID,
  DEFAULT_SESSION_ID,
  JOURNAL_COMPOSER_PLACEHOLDER,
  JOURNAL_VIEW_ID,
  MOCK_GROUPS,
  MOCK_HOME_DRAFTS,
  MOCK_HOME_RAILS,
  MOCK_JOURNAL_ENTRIES,
  MOCK_RESEARCH_ARCHIVED,
  MOCK_RESEARCH_DOCS,
  MOCK_RESEARCH_STARRED,
  MOCK_RESEARCH_UNITS,
  MOCK_SESSIONS,
  MOCK_TAB_DETAILS,
  RESEARCH_COMPOSER_PLACEHOLDER,
  SAVED_PROMPTS,
  SESSION_LABEL,
  SESSION_LABELS,
  type MockGroup,
  type MockJournalEntry,
  type MockPane,
  type MockResearchBlock,
  type MockResearchDoc,
  type MockSession,
  type MockTweetRun,
  type TerminalLine,
} from "./mockupData";

const MOCKUP_LABEL =
  "The qmux desktop window: a sidebar of open-source projects and their agents on the left, " +
  "a live Codex terminal in the middle, and the agent's rendered transcript with a turn " +
  "queue on the right.";

const RESEARCH_MOCKUP_LABEL =
  "The qmux desktop window in research mode: a sidebar of starred research, folders, and a " +
  "Journal tab on the left, and an open research document — its question, answer, and an " +
  "anchored follow-up card — filling the rest of the window.";

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

// ------------------------------------------------------------------ research
//
// Research mode is the sidebar's other list and the stage it opens. Both halves
// ship inside the same window as the terminal ones, so switching modes is a
// class on the shell and a `hidden` flip — nothing is fetched or rebuilt, and
// with no script the replica is simply whichever mode the server rendered.
export type SidebarMode = "terminal" | "research";

// Documents and exported conversations are marked apart from research runs in
// the shared list, exactly as the app's tree kinds are.
function ResearchRowIcon({ kind }: { kind: MockResearchDoc["kind"] }) {
  if (kind === "document") {
    return <FileTextIcon className="lucide research-sidebar-doc-icon" size={12} />;
  }
  if (kind === "conversation") {
    return <TerminalIcon className="lucide research-sidebar-doc-icon" size={12} />;
  }
  return null;
}

// The app's ⌘-number jump hints appear only while ⌘ is held, so the replica
// shows none: a still frame of a held modifier reads as permanent chrome.

function ResearchRow({
  id,
  selected,
  archived = false,
  member = false,
}: {
  id: string;
  selected: boolean;
  archived?: boolean;
  member?: boolean;
}) {
  const doc = MOCK_RESEARCH_DOCS[id];
  return (
    <div
      className={`research-sidebar-row${archived ? " is-archived" : ""}${
        member ? " is-folder-member" : ""
      }${selected ? " is-selected" : ""}`}
      data-mock-research-row={id}
      data-mock-research-title={doc.title}
      data-mock-research-thread={doc.thread ?? ""}
      data-mock-research-archived={archived ? "true" : undefined}
    >
      <span
        className="control-button research-sidebar-select"
        aria-current={selected ? "page" : undefined}
      >
        <span className="research-sidebar-copy">
          <span className="research-sidebar-title">
            <ResearchRowIcon kind={doc.kind} />
            <span className="research-sidebar-title-text">{doc.title}</span>
          </span>
        </span>
        {!archived && doc.running ? (
          <span className="research-sidebar-spinner">
            <LoaderCircleIcon size={14} />
          </span>
        ) : !archived && doc.unseen ? (
          <span className="research-sidebar-unseen">New</span>
        ) : null}
      </span>
      <span className="control-button research-sidebar-menu-trigger" data-mock-research-menu-trigger>
        <EllipsisIcon size={14} />
      </span>
    </div>
  );
}

// A folder always ships its members; collapsing only hides them, so the script
// can open one without fetching anything — the same contract the terminal
// sidebar's groups keep.
function ResearchFolder({
  name,
  collapsed,
  ids,
  selectedId,
}: {
  name: string;
  collapsed: boolean;
  ids: string[];
  selectedId: string;
}) {
  return (
    <div
      className={`research-sidebar-folder${collapsed ? " is-collapsed" : ""}`}
      role="group"
      aria-label={`${name} (${ids.length})`}
      data-mock-research-folder={name}
    >
      <div className="research-sidebar-row research-sidebar-folder-row">
        <span className="control-button research-sidebar-folder-collapse">
          <ChevronRightIcon size={12} />
        </span>
        <span className="research-sidebar-select research-sidebar-folder-heading">
          <span className="research-sidebar-copy">
            <span className="research-sidebar-title">
              <FolderIcon className="lucide research-sidebar-folder-icon" size={12} />
              <span className="research-sidebar-title-text">{name}</span>
              <span className="research-sidebar-folder-count">{ids.length}</span>
            </span>
          </span>
        </span>
      </div>
      {ids.map((id) => (
        <ResearchRow key={id} id={id} selected={id === selectedId} member />
      ))}
    </div>
  );
}

function ResearchSidebarList({ selectedId = DEFAULT_RESEARCH_DOC_ID }: { selectedId?: string }) {
  return (
    <>
      {/* The tab uses the row/select/copy nesting every research row uses, so
          it inherits the list's metrics rather than restating them. */}
      <div
        className="research-sidebar-row journal-sidebar-row"
        data-mock-research-row={JOURNAL_VIEW_ID}
        data-mock-research-title="Journal"
        data-mock-research-thread=""
      >
        <span className="control-button research-sidebar-select">
          <span className="research-sidebar-copy">
            <span className="research-sidebar-title">
              <NotebookPenIcon size={12} className="research-sidebar-doc-icon" />
              <span className="research-sidebar-title-text">Journal</span>
            </span>
          </span>
        </span>
      </div>
      <section className="research-sidebar-section" aria-label="Research">
        <div className="research-sidebar-heading">
          <span>Research</span>
          <span className="control-button">
            <FolderPlusIcon size={13} />
          </span>
        </div>
        <div className="research-sidebar-starred" role="group" aria-label="Starred research">
          {MOCK_RESEARCH_STARRED.map((id) => (
            <ResearchRow key={id} id={id} selected={id === selectedId} />
          ))}
        </div>
        {MOCK_RESEARCH_UNITS.map((unit) =>
          unit.kind === "doc" ? (
            <ResearchRow key={unit.id} id={unit.id} selected={unit.id === selectedId} />
          ) : (
            <ResearchFolder
              key={unit.name}
              name={unit.name}
              collapsed={unit.collapsed}
              ids={unit.ids}
              selectedId={selectedId}
            />
          ),
        )}
        {MOCK_RESEARCH_ARCHIVED.map((id) => (
          <ResearchRow key={id} id={id} selected={id === selectedId} archived />
        ))}
      </section>
    </>
  );
}

// The answer column. Saved highlights and the passage a targeted follow-up was
// asked about are painted with the Custom Highlight API in the app, which needs
// live ranges; the replica marks the runs in the markup and paints them with
// the same two gold washes.
function ResearchAnswer({ blocks }: { blocks: MockResearchBlock[] }) {
  return (
    <div className="research-response-content-root">
      <div className="research-response-item role-assistant">
        <div className="research-response-message">
          <div className="turn-markdown">
            {blocks.map((block, blockIndex) =>
              block.type === "list" ? (
                <ul key={blockIndex}>
                  {block.items.map((item) => (
                    <li key={item}>{item}</li>
                  ))}
                </ul>
              ) : (
                <p key={blockIndex}>
                  {block.runs.map((item, runIndex) =>
                    item.code ? (
                      <code key={runIndex}>{item.text}</code>
                    ) : item.mark ? (
                      <span key={runIndex} className={`mock-research-mark is-${item.mark}`}>
                        {item.text}
                      </span>
                    ) : (
                      <span key={runIndex}>{item.text}</span>
                    ),
                  )}
                </p>
              ),
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function ResearchFollowupCard({
  card,
}: {
  card: NonNullable<MockResearchDoc["followups"]>[number];
}) {
  return (
    <span
      className={`control-button research-followup-card${
        card.anchorTop === undefined ? "" : " is-anchored"
      }${card.unread ? " has-unread" : ""}`}
      style={card.anchorTop === undefined ? undefined : { top: card.anchorTop }}
    >
      {card.unread ? <span className="research-followup-unread" /> : null}
      <strong>{card.prompt}</strong>
      {card.preview ? <span className="research-followup-preview">{card.preview}</span> : null}
      {card.status ? (
        <small className={`is-${card.status}`}>
          {card.status === "running" ? (
            <LoaderCircleIcon className="lucide research-followup-status-spinner" size={11} />
          ) : null}
          {card.status === "running" ? "Running" : "Queued"}
        </small>
      ) : null}
    </span>
  );
}

function ResearchDocumentView({ id, selected }: { id: string; selected: boolean }) {
  const doc = MOCK_RESEARCH_DOCS[id];
  const followups = doc.followups ?? [];
  return (
    <article className="research-document-scroll" data-mock-research-doc={id} hidden={!selected}>
      <div className="research-document-content">
        <div className="research-thread-segment is-selected">
          {doc.question ? (
            <div className="research-prompt">
              {doc.quote ? (
                <blockquote className="research-prompt-quote">{doc.quote}</blockquote>
              ) : null}
              <div className="turn-markdown">
                <p>{doc.question}</p>
              </div>
            </div>
          ) : null}
          <div className="research-response-grid">
            <section className="research-response" aria-label="Research response">
              <ResearchAnswer blocks={doc.answer} />
              <footer className="research-answer-meta">
                {doc.words ? <span>{doc.words}</span> : null}
                {doc.duration ? <span>{doc.duration}</span> : null}
                <span className="control-button research-answer-menu-trigger">
                  <EllipsisIcon size={15} />
                </span>
                {/* A run still streaming carries its own terminal and cancel
                    controls on the segment, where the app puts them. */}
                {doc.running ? (
                  <span className="research-segment-actions">
                    <span className="control-button research-segment-action">Open terminal</span>
                    <span className="control-button research-segment-action">Cancel</span>
                  </span>
                ) : null}
              </footer>
            </section>
            <aside className="research-followups" aria-label="Follow-ups">
              <div className="research-followup-cards">
                {followups
                  .filter((card) => card.anchorTop === undefined)
                  .map((card) => (
                    <ResearchFollowupCard key={card.prompt} card={card} />
                  ))}
              </div>
              {followups
                .filter((card) => card.anchorTop !== undefined)
                .map((card) => (
                  <ResearchFollowupCard key={card.prompt} card={card} />
                ))}
            </aside>
          </div>
        </div>
        <div className="research-response-grid research-thread-composer-row">
          <div className="research-thread-composer-cell">
            <div className="research-followup-composer is-thread">
              <div
                className="sidebar-mode-toggle research-followup-mode-toggle"
                aria-label="Follow-up mode"
              >
                <span className="is-selected">
                  <span>Continue thread</span>
                </span>
                <span>
                  <span>New branch</span>
                </span>
              </div>
              <div className="mock-textarea">{RESEARCH_COMPOSER_PLACEHOLDER}</div>
              <div className="research-followup-footer">
                <div className="research-followup-hint-row">
                  <small>Continues the thread under this answer</small>
                </div>
                <div className="native-input-submit-actions">
                  <span className="control-button">
                    <span>Send</span>
                    <span className="shortcut-hint">⌘↵</span>
                  </span>
                </div>
              </div>
            </div>
          </div>
          <div aria-hidden="true" />
        </div>
      </div>
    </article>
  );
}

// The tweet card is the entry's whole content — an X-embed look with no wrapper
// chrome of its own, so the feed reads as tweets rather than tweets framed
// inside content items. Its author's avatar is the initial disc the app falls
// back to, which keeps the page loading nothing from anyone else's servers.
function TweetText({ runs, className }: { runs: MockTweetRun[]; className: string }) {
  return (
    <p className={className}>
      {runs.map((run, index) => (
        <span key={index} className={run.link ? "journal-tweet-link" : undefined}>
          {run.text}
        </span>
      ))}
    </p>
  );
}

function TweetAvatar({ name, size }: { name: string; size: number }) {
  return (
    <span
      className="journal-tweet-avatar journal-tweet-avatar-fallback"
      style={{ width: size, height: size }}
    >
      {[...name][0]}
    </span>
  );
}

function JournalEntryBody({ entry }: { entry: MockJournalEntry }) {
  if (entry.kind === "note") {
    return <p className="journal-note-text">{entry.text}</p>;
  }
  if (entry.kind === "link") {
    return <span className="journal-link-url">{entry.url}</span>;
  }
  const { tweet } = entry;
  return (
    // A timeline post, not an embed: the avatar takes its own column and the
    // header is one line — name, badge, handle, age.
    <article className="journal-tweet" aria-label={`Tweet by @${tweet.handle}`}>
      <span className="journal-tweet-avatar-link">
        <TweetAvatar name={tweet.name} size={40} />
      </span>
      <div className="journal-tweet-main">
        <div className="journal-tweet-head">
          <span className="journal-tweet-who">
            <span className="journal-tweet-author">{tweet.name}</span>
            {tweet.verified ? <VerifiedBadgeIcon /> : null}
            <span className="journal-tweet-handle">@{tweet.handle}</span>
          </span>
          <span className="journal-tweet-dot">·</span>
          <span className="journal-tweet-age">{tweet.age}</span>
        </div>
        <TweetText runs={tweet.runs} className="journal-tweet-text" />
        {tweet.media ? (
          <div className="journal-tweet-media">
            <span className="journal-tweet-media-item">
              <img
                src={tweet.media.src}
                alt={tweet.media.alt}
                width={tweet.media.width}
                height={tweet.media.height}
                loading="lazy"
                decoding="async"
              />
            </span>
          </div>
        ) : null}
        {tweet.quoted ? (
          <div className="journal-tweet-quote">
            <div className="journal-tweet-quote-head">
              <TweetAvatar name={tweet.quoted.name} size={18} />
              <span className="journal-tweet-author">{tweet.quoted.name}</span>
              {tweet.quoted.verified ? <VerifiedBadgeIcon /> : null}
              <span className="journal-tweet-handle">@{tweet.quoted.handle}</span>
            </div>
            <TweetText runs={tweet.quoted.runs} className="journal-tweet-text is-quote" />
          </div>
        ) : null}
        <div className="journal-tweet-stats">
          <span className="journal-tweet-stat">
            <TweetReplyIcon />
            {tweet.replies}
          </span>
          <span className="journal-tweet-stat">
            <TweetLikeIcon />
            {tweet.likes}
          </span>
        </div>
      </div>
    </article>
  );
}

function JournalView({ selected }: { selected: boolean }) {
  const feed = [...MOCK_JOURNAL_ENTRIES].reverse();
  return (
    <article
      className="research-document-scroll journal-scroll"
      data-mock-research-doc={JOURNAL_VIEW_ID}
      hidden={!selected}
    >
      <div className="journal-column">
        <div className="journal-composer">
          <div className="mock-textarea journal-composer-input" data-mock-journal-input>
            {JOURNAL_COMPOSER_PLACEHOLDER}
          </div>
        </div>
        {/* The undo bar only exists after a removal, so it ships closed and the
            journal enhancement raises it when an entry is deleted. */}
        <div className="journal-undo" role="status" data-mock-journal-undo hidden>
          <span className="journal-undo-label" data-mock-journal-undo-label>
            Entry removed
          </span>
          <span className="control-button journal-undo-restore" data-mock-journal-undo-restore>
            <Undo2Icon size={12} />
            <span>Undo</span>
            <kbd className="context-menu-shortcut is-keycap">⌘Z</kbd>
          </span>
          <span className="control-button journal-undo-dismiss" data-mock-journal-undo-dismiss>
            <XIcon size={12} />
          </span>
        </div>
        <div className="journal-feed" role="feed" aria-label="Journal entries">
          {feed.map((entry) => (
            <article
              key={entry.id}
              className={`journal-entry is-${entry.kind}`}
              data-mock-journal-entry={entry.id}
            >
              <JournalEntryBody entry={entry} />
              <span
                className="control-button journal-entry-menu-trigger"
                data-mock-journal-menu-trigger
              >
                <EllipsisIcon size={13} />
              </span>
            </article>
          ))}
        </div>
      </div>
    </article>
  );
}

// One header for every view rather than one per document: the chrome is
// identical across them in the app, and the script rewrites the breadcrumb and
// the thread count from the selected row, the way it rewrites the session
// label when a terminal tab changes.
function ResearchStage({ selectedId }: { selectedId: string }) {
  const selectedDoc = MOCK_RESEARCH_DOCS[selectedId];
  return (
    <div className="research-workspace">
      <main className="research-document">
        <header className="research-document-header">
          <div className="research-history-nav" aria-label="Research history">
            <span className="control-button research-history-button is-disabled">
              <ArrowLeftIcon size={16} />
            </span>
            <span className="control-button research-history-button is-disabled">
              <ArrowRightIcon size={16} />
            </span>
          </div>
          <div className="research-breadcrumb" aria-label="Research path">
            <span>
              <span className="control-button" data-mock-research-crumb>
                {selectedDoc ? selectedDoc.title : "Journal"}
              </span>
            </span>
          </div>
          <span className="research-document-followup-count" data-mock-research-thread-count>
            {selectedDoc?.thread ?? ""}
          </span>
        </header>
        {Object.keys(MOCK_RESEARCH_DOCS).map((id) => (
          <ResearchDocumentView key={id} id={id} selected={id === selectedId} />
        ))}
        <JournalView selected={selectedId === JOURNAL_VIEW_ID} />
      </main>
    </div>
  );
}

// The research sidebar's right-click menus, one per row, shipped closed at the
// window's root so the script positions them against the replica. Only the
// keycaps are load-bearing: every item dismisses, like the sidebar's own.
function ResearchRowMenu({ id }: { id: string }) {
  const doc = MOCK_RESEARCH_DOCS[id];
  const archived = MOCK_RESEARCH_ARCHIVED.includes(id);
  const starred = MOCK_RESEARCH_STARRED.includes(id);
  const inFolder = MOCK_RESEARCH_UNITS.some(
    (unit) => unit.kind === "folder" && unit.ids.includes(id),
  );
  return (
    <div
      className="popover-surface popover-surface--context pane-context-menu research-sidebar-menu"
      role="menu"
      aria-label={`Actions for ${doc.title}`}
      data-mock-research-menu={id}
      hidden
    >
      <div className="group-context-actions">
        {archived ? (
          <button type="button" role="menuitem" className="control-button" data-mock-context-item>
            <ArchiveIcon size={13} />
            <span>Unarchive research</span>
          </button>
        ) : (
          <>
            <button type="button" role="menuitem" className="control-button" data-mock-context-item>
              <StarIcon size={13} />
              <span>{starred ? "Unstar" : "Star"}</span>
            </button>
            <button type="button" role="menuitem" className="control-button" data-mock-context-item>
              <PencilIcon size={13} />
              <span>Rename</span>
            </button>
            {doc.kind === "run" ? (
              <button
                type="button"
                role="menuitem"
                className="control-button"
                data-mock-context-item
              >
                <RotateCwIcon size={13} />
                <span>Regenerate title</span>
              </button>
            ) : null}
            {inFolder ? (
              <button
                type="button"
                role="menuitem"
                className="control-button"
                data-mock-context-item
              >
                <FolderMinusIcon size={13} />
                <span>Remove from folder</span>
              </button>
            ) : null}
            <button type="button" role="menuitem" className="control-button" data-mock-context-item>
              <FolderPlusIcon size={13} />
              <span>New folder with item</span>
            </button>
            <div className="context-menu-divider" role="separator" />
            <button
              type="button"
              role="menuitem"
              className="control-button context-menu-has-shortcut"
              data-mock-context-item
            >
              <ArchiveIcon size={13} />
              <span>Archive</span>
              <kbd className="context-menu-shortcut is-keycap">A</kbd>
            </button>
          </>
        )}
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-danger context-menu-has-shortcut"
          data-mock-context-item
        >
          <Trash2Icon size={13} />
          <span>Delete</span>
          <kbd className="context-menu-shortcut is-keycap">D</kbd>
        </button>
      </div>
    </div>
  );
}

// A journal entry's menu, whose items depend on what the entry stands for: a
// note has no link to open, and only a tweet can be refetched. Delete is the
// one item that works here — the undo bar it raises is the point.
function JournalEntryMenu({ entry }: { entry: MockJournalEntry }) {
  return (
    <div
      className="popover-surface popover-surface--context pane-context-menu journal-entry-menu"
      role="menu"
      aria-label="Journal entry actions"
      data-mock-journal-menu={entry.id}
      hidden
    >
      <div className="group-context-actions">
        {entry.kind !== "note" ? (
          <button
            type="button"
            role="menuitem"
            className="control-button context-menu-has-shortcut"
            data-mock-context-item
          >
            <ExternalLinkIcon size={13} />
            <span>{entry.kind === "tweet" ? "Open on X" : "Open link"}</span>
            <kbd className="context-menu-shortcut is-keycap">O</kbd>
          </button>
        ) : null}
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
        >
          <CopyIcon size={13} />
          <span>{entry.kind === "note" ? "Copy text" : "Copy link"}</span>
          <kbd className="context-menu-shortcut is-keycap">C</kbd>
        </button>
        {entry.kind === "tweet" ? (
          <button
            type="button"
            role="menuitem"
            className="control-button context-menu-has-shortcut"
            data-mock-context-item
          >
            <RotateCwIcon size={13} />
            <span>Refresh tweet</span>
            <kbd className="context-menu-shortcut is-keycap">R</kbd>
          </button>
        ) : null}
        <div className="context-menu-divider" role="separator" />
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-danger context-menu-has-shortcut"
          data-mock-journal-delete
          data-mock-context-item
        >
          <Trash2Icon size={13} />
          <span>Delete</span>
          <kbd className="context-menu-shortcut is-keycap">D</kbd>
        </button>
      </div>
    </div>
  );
}

function Sidebar({ research, mode }: { research: boolean; mode: SidebarMode }) {
  const researchMode = mode === "research";
  return (
    <aside className={`sidebar is-code-mode${researchMode ? " is-research-mode" : ""}`}>
      <span className="sidebar-header-controls is-grouped">
        <span className="sidebar-header-button" data-mock-action="open-terminal-map">
          <LayoutDashboardIcon size={14} />
        </span>
        <span className="sidebar-header-button" data-mock-action="hide-sidebar">
          <PanelLeftCloseIcon size={14} />
        </span>
      </span>
      <div className="sidebar-mode-toggle" data-mock-mode-toggle>
        <span className={researchMode ? undefined : "is-selected"} data-mock-mode="terminal">
          <SquareTerminalIcon size={13} />
          <span>Terminal</span>
        </span>
        <span className={researchMode ? "is-selected" : undefined} data-mock-mode="research">
          <BookOpenIcon size={13} />
          <span>Research</span>
        </span>
      </div>
      <nav className="pane-list">
        {research ? (
          <>
            <div className="mock-sidebar-list" data-mock-sidebar-list="terminal" hidden={researchMode}>
              {MOCK_GROUPS.map((group, index) => (
                <PaneGroup key={index} group={group} />
              ))}
            </div>
            <div
              className="mock-sidebar-list"
              data-mock-sidebar-list="research"
              hidden={!researchMode}
            >
              <ResearchSidebarList />
            </div>
          </>
        ) : (
          MOCK_GROUPS.map((group, index) => <PaneGroup key={index} group={group} />)
        )}
      </nav>
      {/* The two modes keep different launchers, so each set ships and the
          mode class picks one — the same swap the app performs on state. */}
      <div className="sidebar-actions" data-mock-actions-for="terminal">
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
      {research ? (
        <div className="sidebar-actions" data-mock-actions-for="research">
          <div className="sidebar-action-with-hint">
            <span className="control-button">
              <PlusIcon size={14} />
              <span>New query</span>
            </span>
          </div>
          <div className="sidebar-action-with-hint">
            <span className="control-button">
              <FileTextIcon size={14} />
              <span>New doc</span>
            </span>
          </div>
          <div className="sidebar-action-with-hint">
            <span className="control-button sidebar-settings-button">
              <SettingsIcon size={14} />
            </span>
          </div>
        </div>
      ) : null}
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

// `sessionIds` narrows which terminal/transcript pairs a replica carries. The
// sidebar still lists every tab; a replica that is not demonstrating session
// switching has no reason to serialize fourteen scrollbacks to prove it.
function mockSessions(sessionIds?: readonly string[]) {
  const wanted = sessionIds ? new Set(sessionIds) : null;
  return Object.entries(MOCK_SESSIONS).filter(([sessionId]) => !wanted || wanted.has(sessionId));
}

function TerminalPane({
  stageReplay,
  sessionIds,
}: {
  stageReplay: boolean;
  sessionIds?: readonly string[];
}) {
  return (
    <div className="mock-terminal">
      {mockSessions(sessionIds).map(([sessionId, session]) => (
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

// The transcript thumbnail opens the same high-resolution source in a modal
// overlay. It ships hidden and inert; site/mockup.js promotes its controls when
// the image enhancement is enabled.
function MockImageLightbox() {
  return (
    <div
      className="mock-image-lightbox"
      data-mock-image-lightbox
      role="dialog"
      aria-modal="true"
      aria-label="Expanded transcript image"
      hidden
    >
      <span
        className="control-button mock-image-lightbox-close"
        data-mock-image-close
        aria-label="Close image"
      >
        <XIcon size={14} />
      </span>
      <img
        className="mock-image-lightbox-img"
        data-mock-image-full
        src="/qmux.png"
        alt=""
        width={2704}
        height={1704}
        decoding="async"
      />
    </div>
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

// The sidebar's dashboard button opens the terminal map: every agent's queue
// laid out side by side, the way the app's Home board does it. It ships hidden
// and inert — one column per sidebar pane, plus the global drafts rail — and
// the enhancement script makes the stream chips, composers, and rail heads
// live. A rail's current card is that session's own prompt, so a column and
// its transcript never disagree.
function HomeGroupChip({ group }: { group: MockGroup }) {
  return (
    <div className="home-group-chip" data-mock-home-chip={group.name}>
      <span
        className="control-button home-group-toggle"
        data-mock-action="toggle-home-stream"
        data-mock-stream={group.name}
      >
        <span className="home-group-checkbox">
          <CheckIcon size={10} strokeWidth={3} />
        </span>
        <span className="home-group-name">{group.name}</span>
        <span className="home-group-count">
          {group.panes.length}/{group.panes.length}
        </span>
      </span>
      <span
        className="control-button home-group-caret"
        data-mock-home-caret={group.name}
        title={`Choose terminals in ${group.name}`}
        aria-label={`Choose terminals in ${group.name}`}
        aria-haspopup="menu"
        aria-expanded="false"
      >
        <ChevronDownIcon size={13} />
      </span>
      <div
        className="popover-surface popover-surface--context home-group-menu"
        role="menu"
        aria-label={`Terminals in ${group.name}`}
        data-mock-home-menu={group.name}
        hidden
      >
        {group.panes.map((pane) => (
          <span
            key={pane.sessionId}
            className="menu-item home-group-menu-item is-shown"
            role="menuitemcheckbox"
            aria-checked="true"
            data-mock-home-menu-item={pane.sessionId}
          >
            <span className="home-group-checkbox">
              <CheckIcon size={10} strokeWidth={3} />
            </span>
            <span className="home-group-menu-item-name">{pane.title}</span>
          </span>
        ))}
      </div>
    </div>
  );
}

function HomeGroupSelector() {
  return (
    <div className="home-group-selector" role="group" aria-label="Home streams">
      <div className="home-group-chip" data-mock-home-chip="__drafts__">
        <span
          className="control-button home-group-toggle"
          data-mock-action="toggle-home-stream"
          data-mock-stream="__drafts__"
        >
          <span className="home-group-checkbox">
            <CheckIcon size={10} strokeWidth={3} />
          </span>
          <span className="home-group-name">Drafts</span>
        </span>
      </div>
      {MOCK_GROUPS.map((group) => (
        <HomeGroupChip key={group.name} group={group} />
      ))}
    </div>
  );
}

function HomeDraftCard({ text, label }: { text: string; label: string }) {
  return (
    <div className="queued-turn">
      <div className="queued-turn-text">{text}</div>
      <div className="queued-turn-actions">
        <span
          className="control-button home-rail-turn-remove"
          data-mock-remove-draft
          aria-label={`${label}: ${text}`}
        >
          <XIcon size={13} />
        </span>
      </div>
    </div>
  );
}

function HomeDraftsRail() {
  return (
    <div className="home-rail" data-mock-rail="__drafts__" data-mock-rail-group="__drafts__">
      <div className="home-rail-head is-static">
        <span className="home-rail-title">Drafts</span>
        {MOCK_HOME_DRAFTS.length > 0 ? (
          <span className="home-rail-count">{MOCK_HOME_DRAFTS.length}</span>
        ) : null}
      </div>
      <div className="home-rail-scroll">
        {MOCK_HOME_DRAFTS.map((text) => (
          <HomeDraftCard key={text} text={text} label="Delete draft" />
        ))}
      </div>
      <div className="home-rail-composer">
        <div className="mock-rail-composer">New draft…</div>
      </div>
    </div>
  );
}

function HomeRail({ group, pane }: { group: MockGroup; pane: MockPane }) {
  const rail = MOCK_HOME_RAILS[pane.sessionId];
  // The fixture keeps follow-ups in newest-first order; the map presents the
  // queue head first, matching the live queue surfaces.
  const queued = [...(rail?.queued ?? [])].reverse();
  return (
    <div
      className="home-rail"
      data-mock-rail={pane.sessionId}
      data-mock-rail-group={group.name}
    >
      <span className="home-rail-head" data-mock-open-session={pane.sessionId}>
        <span className={`pane-tab-dot status-${pane.status}`} />
        <span className="home-rail-title">{pane.title}</span>
        {queued.length > 0 ? (
          <span className="home-rail-count">{queued.length} queued</span>
        ) : null}
        {rail?.paused ? <span className="home-rail-paused">paused</span> : null}
      </span>
      <div className="home-rail-scroll">
        {(rail?.past ?? []).map((turn) => (
          <div className="queued-turn is-past" key={turn.text}>
            <div className="queued-turn-text">{turn.text}</div>
            <div className="queued-turn-receipt">
              <CheckIcon size={11} strokeWidth={2.5} className="queued-turn-receipt-ok" />
              {` ${turn.receipt}`}
            </div>
          </div>
        ))}
        {rail ? (
          <div
            className={`queued-turn is-current${
              rail.current.tone === "active" ? " tone-active" : " tone-done"
            }`}
          >
            <div className="queued-turn-text">{rail.current.text}</div>
            <div className="queued-turn-receipt">
              {rail.current.tone === "active" ? (
                <span className="queued-turn-receipt-live">●</span>
              ) : (
                <CheckIcon size={11} strokeWidth={2.5} className="queued-turn-receipt-ok" />
              )}
              {` ${rail.current.receipt}`}
            </div>
          </div>
        ) : null}
        {queued.map((text) => (
          <div className="queued-turn" key={text}>
            <div className="queued-turn-text">{text}</div>
            <div className="queued-turn-actions">
              <span
                className="control-button home-rail-turn-remove"
                data-mock-remove-queued
                aria-label={`Remove queued turn: ${text}`}
              >
                <XIcon size={13} />
              </span>
            </div>
          </div>
        ))}
      </div>
      <div className="home-rail-composer">
        <div className="mock-rail-composer">Queue a follow-up…</div>
      </div>
    </div>
  );
}

function TerminalMap() {
  return (
    <div className="confirm-dialog-backdrop terminal-map-backdrop" data-mock-terminal-map hidden>
      <div
        className="terminal-map-popover"
        role="dialog"
        aria-modal="true"
        aria-label="Terminal map"
        tabIndex={-1}
        data-mock-terminal-map-dialog
      >
        <div className="home-board">
          <HomeGroupSelector />
          <section className="home-rails-section" aria-label="Agent workstreams">
            <div className="home-rails">
              <div className="home-rails-inner">
                <div className="home-rails-columns">
                  <HomeDraftsRail />
                  {MOCK_GROUPS.flatMap((group) =>
                    group.panes.map((pane) => (
                      <HomeRail key={pane.sessionId} group={group} pane={pane} />
                    )),
                  )}
                </div>
              </div>
            </div>
          </section>
        </div>
      </div>
    </div>
  );
}

// The sidebar's right-click menus, exactly as the app pairs them: a tab's
// context menu carries its details above the action list, and a group's menu
// opens from the … button or a right-click on the group's background. They
// ship hidden at the window's root so the script can position them against
// the replica rather than the page; every item dismisses except the collapse
// toggle, which flips in place the way the app's does.
const TAB_STATUS_LABELS: Record<MockPane["status"], [string, string]> = {
  active: ["Running", "active"],
  idle: ["Idle", "idle"],
  done: ["Done", "done"],
  attention: ["Awaiting input", "attention"],
};

function PaneTabMenu({ pane }: { pane: MockPane }) {
  const session = MOCK_SESSIONS[pane.sessionId];
  const details = MOCK_TAB_DETAILS[pane.sessionId];
  const [statusLabel, statusTone] = TAB_STATUS_LABELS[pane.status];
  return (
    <div
      className="popover-surface popover-surface--context pane-context-menu"
      role="dialog"
      aria-label={`${pane.title} details`}
      data-mock-tab-menu={pane.sessionId}
      hidden
    >
      <dl className="pane-context-details">
        <div className={`pane-context-status-row status-${statusTone}`}>
          <dt>Agent</dt>
          <dd>{statusLabel}</dd>
        </div>
        <div>
          <dt>Directory</dt>
          <dd>~/code/{session.project}</dd>
        </div>
        {details?.branch ? (
          <div>
            <dt>Branch</dt>
            <dd>{details.branch}</dd>
          </div>
        ) : null}
      </dl>
      <div className="pane-context-actions" role="menu" aria-label="Tab actions">
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
        >
          <PanelBottomCloseIcon size={13} />
          <span>Add split below</span>
          <kbd className="context-menu-shortcut">⌘D</kbd>
        </button>
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
        >
          <Columns2Icon size={13} />
          <span>Add split to the right</span>
          <kbd className="context-menu-shortcut">⌘⇧D</kbd>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <Columns2Icon size={13} />
          <span>Split left and right</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <PanelBottomCloseIcon size={13} />
          <span>Join with terminal below</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <PanelBottomOpenIcon size={13} />
          <span>Detach from split</span>
        </button>
        <div className="context-menu-divider" role="separator" />
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <GitBranchIcon size={13} />
          <span>Fork session</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <PanelBottomCloseIcon size={13} />
          <span>Fork session in split</span>
        </button>
        <div className="context-menu-divider" role="separator" />
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <FolderGit2Icon size={13} />
          <span>Open worktree</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <FolderGit2Icon size={13} />
          <span>Fork session in worktree</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <MessageSquareTextIcon size={13} />
          <span>Export to Research…</span>
        </button>
        <div className="context-menu-divider" role="separator" />
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-danger"
          data-mock-context-item
          aria-label={`Close ${pane.title}`}
        >
          <XIcon size={13} />
          <span>Close tab</span>
        </button>
      </div>
    </div>
  );
}

function PaneGroupMenu({ group }: { group: MockGroup }) {
  return (
    <div
      className="popover-surface popover-surface--context pane-context-menu group-context-menu"
      role="menu"
      aria-label="Group options"
      data-mock-group-menu={group.name}
      hidden
    >
      <div className="group-context-actions">
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <FolderIcon size={13} />
          <span>Change directory</span>
        </button>
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
        >
          <PencilIcon size={13} />
          <span>Rename group</span>
          <kbd className="context-menu-shortcut is-keycap">R</kbd>
        </button>
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
          data-mock-menu-collapse
        >
          <span className="mock-menu-icon-expand" hidden={!group.collapsed}>
            <ChevronsUpDownIcon size={13} />
          </span>
          <span className="mock-menu-icon-collapse" hidden={group.collapsed}>
            <ChevronsDownUpIcon size={13} />
          </span>
          <span className="mock-menu-label-expand" hidden={!group.collapsed}>
            Expand group
          </span>
          <span className="mock-menu-label-collapse" hidden={group.collapsed}>
            Collapse group
          </span>
          <span className="context-menu-shortcut-options" hidden={!group.collapsed} aria-label="C or E">
            <kbd className="context-menu-shortcut is-keycap">C</kbd>
            <span aria-hidden="true">/</span>
            <kbd className="context-menu-shortcut is-keycap">E</kbd>
          </span>
          <kbd className="context-menu-shortcut is-keycap mock-menu-keycap" hidden={group.collapsed}>
            C
          </kbd>
        </button>
        <div className="context-menu-divider" role="separator" />
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <SquareTerminalIcon size={13} />
          <span>New shell</span>
        </button>
        <button type="button" role="menuitem" className="control-button" data-mock-context-item>
          <MessageSquareTextIcon size={13} />
          <span>New agent</span>
        </button>
        <div className="context-menu-divider" role="separator" />
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-has-shortcut"
          data-mock-context-item
        >
          <PlusIcon size={13} />
          <span>New group...</span>
          <kbd className="context-menu-shortcut">⌘⇧N</kbd>
        </button>
        <button
          type="button"
          role="menuitem"
          className="control-button context-menu-danger"
          data-mock-context-item
        >
          <XIcon size={13} />
          <span>Close group</span>
        </button>
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
                  <span
                    className="turn-image-thumb"
                    data-mock-image-src={item.src}
                    data-mock-image-alt={item.alt}
                  >
                    <img
                      className="turn-image"
                      src={item.src}
                      alt={item.alt}
                      width={2704}
                      height={1704}
                      decoding="async"
                    />
                  </span>
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

function Transcript({
  stageReplay,
  sessionIds,
}: {
  stageReplay: boolean;
  sessionIds?: readonly string[];
}) {
  return (
    <>
      {mockSessions(sessionIds).map(([sessionId, session]) => (
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
  "terminal-map",
  "panels",
  "menus",
  "sidebar-menus",
  "images",
] as const;

// The research replica's own set. Research mode has its own list, stage, and
// composer, so it enhances none of the terminal surfaces — and the terminal
// half it keeps behind the mode toggle stays the inert finished state.
export const RESEARCH_MOCKUP_FEATURES = ["research", "journal", "research-menus"] as const;

export default function AppMockup({
  labelledBy,
  features = MOCKUP_FEATURES,
  initialSidebarCollapsed = false,
  initialTranscriptExpanded = false,
  initialSidebarMode = "terminal",
  sessionIds,
}: {
  labelledBy?: string;
  features?: readonly string[];
  initialSidebarCollapsed?: boolean;
  initialTranscriptExpanded?: boolean;
  initialSidebarMode?: SidebarMode;
  /** Which sessions ship a terminal/transcript pair; every tab is listed. */
  sessionIds?: readonly string[];
}) {
  const stageReplay = features.includes("replay");
  // Research mode's markup follows either the feature or the rendered mode: a
  // replica that opens in it is complete whether or not the script ever runs.
  // The gates further down work the other way round — a surface that only ever
  // exists to be opened (a menu, a modal, the lightbox) ships only where
  // something can open it. What is visible is never conditional.
  const research = features.includes("research") || initialSidebarMode === "research";
  const shellClassName = [
    "app-shell",
    "has-turn-sidebar",
    initialSidebarCollapsed ? "is-sidebar-collapsed" : "",
    initialTranscriptExpanded ? "is-transcript-expanded" : "",
    initialSidebarMode === "research" ? "is-research-mode" : "",
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div className="app-mockup-frame">
      <div
        className="app-mockup"
        role="img"
        aria-labelledby={labelledBy}
        aria-label={
          labelledBy
            ? undefined
            : initialSidebarMode === "research"
              ? RESEARCH_MOCKUP_LABEL
              : MOCKUP_LABEL
        }
        data-mock-features={features.join(" ")}
      >
        <TrafficLights />
        <FloatingRestoreControls />
        <div className={shellClassName}>
          <Sidebar research={research} mode={initialSidebarMode} />
          <TerminalPane stageReplay={stageReplay} sessionIds={sessionIds} />
          {features.includes("panels") ? <BrowserOverlay /> : null}
          <div className="turn-pane">
            <div className="turn-sidebar has-header">
              <TurnPaneHeader />
              {features.includes("panels") ? <PromptLibrary /> : null}
              <ArtifactTray />
              <Transcript stageReplay={stageReplay} sessionIds={sessionIds} />
              <Composer />
            </div>
          </div>
          {research ? <ResearchStage selectedId={DEFAULT_RESEARCH_DOC_ID} /> : null}
        </div>
        {features.includes("images") ? <MockImageLightbox /> : null}
        {features.includes("sidebar-menus")
          ? MOCK_GROUPS.map((group) => <PaneGroupMenu key={group.name} group={group} />)
          : null}
        {features.includes("sidebar-menus")
          ? MOCK_GROUPS.flatMap((group) =>
              group.panes.map((pane) => <PaneTabMenu key={pane.sessionId} pane={pane} />),
            )
          : null}
        {features.includes("terminal-map") ? <TerminalMap /> : null}
        {features.includes("research-menus")
          ? Object.keys(MOCK_RESEARCH_DOCS).map((id) => <ResearchRowMenu key={id} id={id} />)
          : null}
        {/* An entry's menu belongs to the journal feature, which owns the one
            item here that acts: the removal, and the undo bar it raises. */}
        {features.includes("journal")
          ? MOCK_JOURNAL_ENTRIES.map((entry) => (
              <JournalEntryMenu key={entry.id} entry={entry} />
            ))
          : null}
      </div>
      <span className="mock-demo-status" role="status" aria-live="polite" />
    </div>
  );
}
