// Styles for <AppMockup />. These are a port of the desktop app's own stylesheet
// (src/styles/tokens.css + features/{shell,terminal,turn-pane,transcript,composer}.css),
// trimmed to what a static replica needs and scoped under `.app-mockup` so the
// marketing page's own type and color rules cannot leak in — or out.
//
// The mockup renders at the app's real geometry (276px sidebar, 420px turn pane,
// 13px UI type), so it reads as a screenshot rather than a diagram. When the app's
// look changes, port the values.
export const MOCKUP_CSS = `
.app-mockup {
  /* --- tokens.css --- */
  --fs-xs: 11px;
  --fs-sm: 12px;
  --fs-base: 13px;
  --control-h-md: 30px;
  --radius-sm: 4px;
  --radius-md: 6px;
  --radius-lg: 8px;
  --font-ui: "DM Sans", ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  --font-mono: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;

  --text-primary: #e7e7e2;
  --text-strong: #f1f0e8;
  --text-secondary: #c4cbc6;
  --text-body-soft: #d9ddd9;
  --text-muted: #8a938e;
  --text-activity: #8f9994;
  --text-interactive: #edf0ec;
  --text-subtle: #7f8884;
  --text-disabled: #797d80;
  --text-heading: #f4f3ec;
  --placeholder-color: #6b726d;
  --control-fg: var(--text-strong);
  --control-fg-muted: #b9c0bc;
  --control-h-sm: 26px;

  --status-active-fg: #d7a84f;
  --status-pending-fg: #7f8884;
  --split-border-active: rgba(255, 220, 143, 0.78);

  --surface-border-subtle: rgba(255, 255, 255, 0.075);
  --sidebar-switcher-bg: rgba(0, 0, 0, 0.18);
  --sidebar-switcher-active-bg: rgba(255, 255, 255, 0.105);
  --sidebar-group-bg: rgba(255, 255, 255, 0.013);

  /* green-blob (the default application theme) */
  --terminal-pane-bg: #111315;
  --right-pane-bg: #171b1d;
  --workspace-bg: #17191b;
  --field-bg: #111315;
  --chrome-header-bg: #14171a;
  --content-card-bg: #1d2224;
  --transcript-code-bg: #111416;
  --surface-divider: #2a2d2f;
  --surface-border-strong: #30383b;
  --chrome-control-bg: rgba(20, 24, 26, 0.9);
  --chrome-control-bg-hover: rgba(32, 38, 40, 0.95);
  --accent-color: #8fd6c7;
  --accent-shadow: rgba(143, 214, 199, 0.35);
  --content-inset-bg: #15191b;
  --queued-turn-border: #384246;
  --control-bg: #24282b;
  --control-bg-hover: #2c3134;
  --control-border: #3a3d3f;
  --control-border-hover-accent: #4a5d56;
  --focus-ring: #6cae9d;
  --popover-bg: #1d2224;
  --popover-item-hover-bg: #1b1f21;
  --popover-shadow: 0 10px 28px rgba(0, 0, 0, 0.5);
  --context-menu-bg: #1b1f21;
  --context-menu-shadow:
    0 0 0 0.5px rgba(0, 0, 0, 0.45),
    0 12px 32px rgba(0, 0, 0, 0.3);

  /* One hover step, shared by every control here: half the distance the app
     travels on hover. The app is a focused window where a strong hover reads as
     responsiveness; on a marketing page the same value reads as flicker. */
  --hover-bg: color-mix(in srgb, var(--control-bg-hover) 50%, var(--control-bg));
  --hover-border: color-mix(in srgb, var(--control-border-hover-accent) 50%, var(--control-border));
  --hover-chrome-bg: color-mix(in srgb, var(--chrome-control-bg-hover) 50%, var(--chrome-control-bg));
  --hover-fg: color-mix(in srgb, var(--text-heading) 50%, var(--control-fg));
  /* Header buttons rest a step dimmer than the rest, so their text gets its own
     half-step rather than jumping the whole way to the app's hover colour. */
  --hover-chrome-fg: color-mix(in srgb, var(--text-strong) 50%, var(--text-secondary));

  /* --- frame ---
     A macOS window edge: a hairline, a faint inner top bevel, and a drop shadow
     that lifts the replica off the page in either theme. */
  position: relative;
  overflow: hidden;
  border: 1px solid rgba(255, 255, 255, 0.09);
  border-radius: 12px;
  background: var(--workspace-bg);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.05),
    0 1px 2px rgba(0, 0, 0, 0.12),
    0 18px 48px -18px rgba(0, 0, 0, 0.45);
  color: var(--text-primary);
  font-family: var(--font-ui);
  font-size: var(--fs-base);
  font-synthesis: none;
  line-height: 1.5;
  letter-spacing: normal;
  color-scheme: dark;
  text-align: left;
  -webkit-font-smoothing: antialiased;
  user-select: none;
}

@media (prefers-color-scheme: light) {
  .app-mockup {
    border-color: rgba(0, 0, 0, 0.16);
    box-shadow:
      inset 0 1px 0 rgba(255, 255, 255, 0.05),
      0 1px 2px rgba(0, 0, 0, 0.08),
      0 18px 48px -20px rgba(0, 0, 0, 0.35);
  }
}

/* On the dark site the window and the page are within a few percent of each
   other, so the frame needs a brighter edge and a ring to stay a window rather
   than dissolving into the background. */
@media (prefers-color-scheme: dark) {
  .app-mockup {
    border-color: rgba(255, 255, 255, 0.16);
    box-shadow:
      inset 0 1px 0 rgba(255, 255, 255, 0.06),
      0 0 0 1px rgba(0, 0, 0, 0.6),
      0 24px 60px -20px rgba(0, 0, 0, 0.8);
  }
}

.app-mockup *,
.app-mockup *::before,
.app-mockup *::after {
  box-sizing: border-box;
}

.app-mockup p {
  margin: 0;
}

.app-mockup .lucide {
  flex: none;
}

/* --- base.css / primitives.css ---
   These come before the feature rules on purpose: the app keeps them in lower
   cascade layers, so a feature rule like .pane-tab's grid must win over
   .control-button even though both are single-class selectors. */

/* Decorative spans become real <button>s when the enhancement runs, which is
   also when the UA's button chrome would appear. Neutralise it once here rather
   than relying on every control's class to remember; the classes below all out-
   specify this, so the ones that do want a fill and a border keep them. */
.app-mockup button {
  margin: 0;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
}
.app-mockup .control-button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex-wrap: nowrap;
  gap: 2px 8px;
  min-height: var(--control-h-md);
  padding: 0 12px;
  border: 1px solid var(--control-border);
  border-radius: var(--radius-md);
  background: var(--control-bg);
  color: var(--control-fg);
  font-size: var(--fs-base);
}

.app-mockup .icon-button {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text-secondary);
}

.app-mockup .shortcut-hint {
  color: var(--text-disabled);
  font-size: var(--fs-xs);
  line-height: 1;
  white-space: nowrap;
}

/* --- shell.css --- */
.app-mockup .app-shell {
  /* The app's sidebar defaults to 268px and is user-resizable; 276px is just
     above the 270px threshold where it drops its action-button icons. */
  --sidebar-width: 276px;
  --turn-pane-width: 420px;
  position: relative;
  display: grid;
  grid-template-columns: var(--sidebar-width) minmax(0, 1fr) var(--turn-pane-width);
  height: 680px;
  max-height: calc(100vh - 4rem);
  min-height: 0;
  overflow: hidden;
  background: var(--workspace-bg);
}

.app-mockup .sidebar {
  position: relative;
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 14px;
  overflow: hidden;
  padding: 32px 6px 12px;
  /* --left-pane-bg composited over --workspace-bg: the app gets this tint from
     macOS window vibrancy, which a web page has no equivalent for. */
  background: #101314;
  border-right: 1px solid var(--surface-divider);
}

/* Positioned against the window, not the sidebar: macOS draws them over the
   frameless window's top-left corner whether a sidebar is there or not. */
.app-mockup .mock-traffic-lights {
  position: absolute;
  top: 11px;
  left: 13px;
  z-index: 10;
  display: flex;
  gap: 8px;
}

.app-mockup .mock-traffic-light {
  width: 12px;
  height: 12px;
  border-radius: 50%;
}

.app-mockup .mock-traffic-light.is-close {
  background: #ff5f57;
}

.app-mockup .mock-traffic-light.is-minimize {
  background: #febc2e;
}

.app-mockup .mock-traffic-light.is-zoom {
  background: #28c840;
}

.app-mockup .sidebar-collapse-button {
  position: absolute;
  top: 4px;
  right: 10px;
  display: inline-flex;
  width: 24px;
  height: 24px;
  align-items: center;
  justify-content: center;
  border-radius: var(--radius-md);
  color: var(--text-muted);
}

.app-mockup .sidebar-mode-toggle {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 2px;
  margin: 0 4px;
  padding: 3px 3px 2.5px;
  border: 1px solid var(--surface-border-subtle);
  border-radius: var(--radius-lg);
  background: var(--sidebar-switcher-bg);
}

.app-mockup .sidebar-mode-toggle > span {
  display: flex;
  min-width: 0;
  min-height: 27px;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 0 8px;
  border-radius: 5px;
  color: #818a85;
  font-size: 13px;
}

.app-mockup .sidebar-mode-toggle > span.is-selected {
  color: var(--text-interactive);
  background: var(--sidebar-switcher-active-bg);
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.25);
}

.app-mockup .pane-list {
  display: flex;
  min-height: 0;
  flex: 1;
  flex-direction: column;
  gap: 2px;
  overflow: hidden;
}

.app-mockup .pane-home-row {
  --pane-tab-left-rail-width: 3px;
  --pane-tab-left-offset: 10px;
  flex: 0 0 auto;
  /* Pull against the sidebar's 14px stack gap so Home sits closer to both the
     mode switcher and the first project group. */
  margin: -6px 4px;
}

.app-mockup .pane-home-row .pane-tab {
  column-gap: 8px;
  padding-bottom: 5px;
}

.app-mockup .pane-home-row .pane-tab .lucide {
  justify-self: center;
  color: var(--text-muted);
}

.app-mockup .pane-group {
  position: relative;
  display: flex;
  min-width: 0;
  flex-direction: column;
  padding: 3.5px 6px;
  border: 1px solid var(--surface-border-subtle);
  border-radius: 7px;
  background: var(--sidebar-group-bg);
}

.app-mockup .pane-group + .pane-group {
  margin-top: 3px;
}

.app-mockup .pane-group-header {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  align-items: center;
  gap: 4px;
  min-height: 25px;
  padding: 2px 4px 2px 5px;
  border-radius: 5px;
  color: var(--control-fg-muted);
  font-size: 12px;
  line-height: 1.2;
}

.app-mockup .pane-group.has-panes .pane-group-header {
  margin-bottom: 2px;
}

.app-mockup .pane-group-title {
  display: flex;
  min-width: 0;
  align-items: center;
  gap: 6px;
}

.app-mockup .pane-group-folder {
  color: var(--text-subtle);
}

.app-mockup .pane-group-name {
  min-width: 0;
  overflow: hidden;
  color: #d6dbd6;
  font-weight: 600;
  letter-spacing: 0.01em;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.app-mockup .pane-group.is-active-group .pane-group-name {
  color: var(--text-interactive);
}

.app-mockup .pane-group-count {
  flex: none;
  color: #747d79;
  font-weight: 500;
}

.app-mockup .pane-group-status-icons {
  display: inline-flex;
  flex: none;
  align-items: center;
  gap: 4px;
}

.app-mockup .pane-group-aux {
  display: inline-flex;
  align-items: center;
  justify-self: end;
  gap: 4px;
}

.app-mockup .pane-group-collapse-button,
.app-mockup .pane-group-menu-button {
  display: inline-flex;
  width: 18px;
  height: 18px;
  min-height: 0;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: 1px solid transparent;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-muted);
}

.app-mockup .pane-list-body {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 1px;
}

.app-mockup .pane-tab-row {
  position: relative;
  min-width: 0;
  --pane-tab-left-rail-width: 3px;
  --pane-tab-left-offset: 9px;
}

.app-mockup.is-interactive [data-mock-session-tab] .pane-tab {
  cursor: pointer;
}

.app-mockup .pane-tab {
  /* The app tints this chip with alpha over the vibrancy layer; here it is the
     flattened equivalent, kept opaque so a queue pill never lets the tab title
     show through from behind it. */
  --pane-tab-status-bg: #0f1112;
  position: relative;
  display: grid;
  grid-template-columns: 8px minmax(0, 1fr);
  align-items: center;
  column-gap: 6px;
  width: 100%;
  min-height: 30px;
  padding: 5px 9px 5px calc(var(--pane-tab-left-offset) - var(--pane-tab-left-rail-width));
  border: 1px solid transparent;
  border-left: var(--pane-tab-left-rail-width) solid transparent;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--control-fg-muted);
  font-size: 13px;
  text-align: left;
}

.app-mockup .pane-tab-row.is-selected .pane-tab {
  --pane-tab-status-bg: #2c3134;
  border-color: var(--control-border);
  border-left-color: var(--split-border-active);
  background: var(--control-bg-hover);
  color: #ffffff;
}

.app-mockup .pane-tab-content {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 1px;
}

.app-mockup .pane-tab-title {
  display: -webkit-box;
  min-width: 0;
  overflow: hidden;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 3;
  line-clamp: 3;
  line-height: 1.35;
  overflow-wrap: anywhere;
}

/* The status pill floats over the tab's right edge rather than taking a column,
   so a three-line title keeps its full width underneath it. */
.app-mockup .pane-tab-meta {
  position: absolute;
  top: 50%;
  right: 8px;
  z-index: 2;
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 6px;
  max-width: min(72%, calc(100% - 32px));
  min-width: 0;
  transform: translateY(-50%);
  pointer-events: none;
}

.app-mockup .pane-tab-status {
  display: inline-flex;
  max-width: 100%;
  align-items: center;
  overflow: hidden;
  padding: 1px 6px;
  border-radius: 999px;
  background: var(--pane-tab-status-bg);
  color: var(--text-subtle);
  font-size: 12px;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.app-mockup .pane-tab-dot {
  grid-column: 1;
  justify-self: start;
  align-self: center;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: transparent;
}

.app-mockup .pane-tab-dot.status-active {
  background: var(--status-active-fg);
  animation: mock-status-dot-pulse 1.3s ease-in-out infinite;
}

.app-mockup .pane-tab-dot.status-idle {
  border: 1px solid var(--status-pending-fg);
}

.app-mockup .pane-tab-dot.status-attention {
  background: #e0796d;
  animation: mock-status-dot-pulse 1s ease-in-out infinite;
}

.app-mockup .pane-tab-dot.status-done {
  background: #6cae9d;
}

@keyframes mock-status-dot-pulse {
  0%,
  100% {
    opacity: 0.45;
    transform: scale(0.9);
  }

  50% {
    opacity: 1;
    transform: scale(1.12);
  }
}

.app-mockup .sidebar-actions {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr) auto;
  align-items: stretch;
  gap: 6px;
  margin: 0 4px;
  --fs-base: 13.5px;
}

.app-mockup .sidebar-action-with-hint {
  position: relative;
  display: flex;
  min-width: 0;
}

.app-mockup .sidebar-action-with-hint > .control-button {
  width: 100%;
  min-width: 0;
  gap: 6px;
  padding: 0 8px;
}

.app-mockup .sidebar-settings-button {
  padding: 0 6px;
}

.app-mockup .sidebar-actions .control-button .lucide {
  opacity: 0.8;
}

/* Let the label ellipsize instead of wrapping under the icon in a narrow sidebar. */
.app-mockup .sidebar-actions .control-button > span {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* --- terminal --- */
.app-mockup .mock-terminal {
  /* With the sidebar gone the terminal owns the window's top-left corner, where
     the traffic lights and the sidebar's restore control sit. Reserve a strip so
     output starts below them instead of running under them. The padding is on
     the pane, not the screen: overflow clips at the padding box, so a screen
     with top padding would still paint its scrollback into the strip. */
  --mock-chrome-strip: 0px;
  position: relative;
  display: flex;
  min-width: 0;
  overflow: hidden;
  padding-top: var(--mock-chrome-strip);
  background: var(--terminal-pane-bg);
}

.app-mockup .app-shell.is-sidebar-collapsed .mock-terminal {
  /* Clears the restore control, which ends at 36px. */
  --mock-chrome-strip: 40px;
}

/* The pane is pinned to the terminal's tail, so the topmost row is cut mid-glyph
   by the pane edge. Fade it out so it reads as scrollback continuing above. */
.app-mockup .mock-terminal::before {
  content: "";
  position: absolute;
  top: var(--mock-chrome-strip);
  right: 0;
  left: 0;
  z-index: 1;
  height: 34px;
  background: linear-gradient(to bottom, var(--terminal-pane-bg) 30%, rgba(17, 19, 21, 0));
  pointer-events: none;
}

/* Rows are pre-wrapped at the pane's natural column count, so a narrower pane
   clips them at the right edge; fade that edge so it reads as "more to the
   right" instead of a hard cut through a glyph. */
.app-mockup .mock-terminal::after {
  content: "";
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  z-index: 1;
  width: 26px;
  background: linear-gradient(to right, rgba(17, 19, 21, 0), var(--terminal-pane-bg));
  pointer-events: none;
}

.app-mockup .mock-terminal-screen {
  display: flex;
  min-width: 0;
  flex: 1;
  flex-direction: column;
  /* A live terminal is pinned to its tail: older scrollback runs off the top. */
  justify-content: flex-end;
  overflow: hidden;
  padding: 10px 12px;
  color: #e7e7e2;
  font-family: var(--font-mono);
  font-size: 12.5px;
  /* Shell text should read literally; JetBrains Mono's ligatures would fuse
     glyph pairs the terminal renders separately. */
  font-variant-ligatures: none;
  line-height: 1.5;
  white-space: pre;
}

.app-mockup [data-mock-session-view][hidden] {
  display: none;
}

.app-mockup .mock-terminal-block + .mock-terminal-block {
  margin-top: 1.5em;
}

.app-mockup .mock-terminal-line {
  min-height: 1.5em;
  overflow: hidden;
  text-overflow: clip;
}

.app-mockup .mock-terminal-cursor {
  display: inline-block;
  width: 0.6em;
  height: 1.15em;
  vertical-align: text-bottom;
  background: #f2d37b;
  animation: mock-cursor-blink 1.1s steps(1, end) infinite;
}

@keyframes mock-cursor-blink {
  0%,
  49% {
    opacity: 1;
  }

  50%,
  100% {
    opacity: 0;
  }
}

.app-mockup .tt-strong {
  color: #f4f3ec;
  font-weight: 700;
}

.app-mockup .tt-command {
  color: #8fce9b;
}

.app-mockup .tt-argument {
  color: #7fc8d8;
}

.app-mockup .tt-string {
  color: #d9c07d;
}

.app-mockup .tt-path {
  color: #9fb8d9;
}

.app-mockup .tt-dim {
  color: #737f7b;
}

/* --- turn-pane.css --- */
.app-mockup .turn-pane {
  position: relative;
  display: flex;
  min-width: 0;
  min-height: 0;
  background: var(--right-pane-bg);
  border-left: 1px solid var(--surface-divider);
  --fs-xs: 11.5px;
  --fs-sm: 12.5px;
  --fs-base: 13.5px;
}

.app-mockup .turn-sidebar {
  position: relative;
  width: 100%;
  min-width: 0;
  min-height: 0;
  overflow: hidden;
  background: var(--right-pane-bg);
}

.app-mockup .turn-pane-header {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  z-index: 3;
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 10px;
  border-bottom: 1px solid var(--surface-divider);
  background: var(--chrome-header-bg);
}

.app-mockup .turn-pane-session-control {
  position: relative;
  display: flex;
  align-items: center;
  align-self: stretch;
  flex: 1;
  min-width: 0;
}

.app-mockup .turn-pane-session {
  display: block;
  width: 100%;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-muted);
  font-size: 12.5px;
}

.app-mockup .turn-pane-header-controls {
  flex: 0 0 auto;
  display: flex;
  gap: 6px;
}

.app-mockup .turn-pane-header-button {
  position: relative;
  display: inline-flex;
  width: 28px;
  height: 28px;
  min-height: 0;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: 1px solid var(--control-border);
  border-radius: var(--radius-md);
  background: var(--chrome-control-bg);
  color: var(--text-secondary);
}

.app-mockup .artifact-tray-badge {
  position: absolute;
  top: -5px;
  right: -5px;
  display: flex;
  min-width: 14px;
  height: 14px;
  align-items: center;
  justify-content: center;
  padding: 0 3px;
  border: 2px solid var(--chrome-header-bg);
  border-radius: 7px;
  background: var(--accent-color);
  color: var(--chrome-header-bg);
  font-size: 9.5px;
  font-weight: 700;
  line-height: 1;
}

/* --- transcript.css --- */
.app-mockup .turn-timeline {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  gap: 5px;
  padding: 50px 11px 132px 10px;
  overflow-x: hidden;
  overflow-y: auto;
  overscroll-behavior: auto;
  scrollbar-width: thin;
  line-height: 1.5;
}

.app-mockup .turn-card {
  border: 1px solid var(--surface-border-strong);
  border-radius: var(--radius-md);
  background: var(--content-card-bg);
  padding: 8px 9px;
}

.app-mockup .turn-card.role-assistant {
  border: 0;
  border-radius: 0;
  background: transparent;
  padding: 2px 10px;
}

.app-mockup .turn-card.role-user {
  margin: 7px 0;
}

.app-mockup .turn-card header {
  display: flex;
  align-items: center;
  gap: 8px;
  color: var(--accent-color);
  font-size: 11.5px;
  font-weight: 700;
  text-transform: uppercase;
}

.app-mockup .turn-message-menu {
  position: relative;
  margin-left: auto;
  display: inline-flex;
}

.app-mockup .turn-message-menu-trigger {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 2px;
  color: rgba(217, 221, 217, 0.42);
}

.app-mockup [data-mock-menu][hidden] {
  display: none;
}

.app-mockup .popover-surface {
  display: flex;
  min-width: 0;
  flex-direction: column;
  border: 1px solid var(--surface-divider);
  border-radius: var(--radius-lg);
  background: var(--popover-bg);
  box-shadow: var(--popover-shadow);
}

.app-mockup .popover-surface--context {
  border: 0.5px solid var(--control-border);
  background: var(--context-menu-bg);
  box-shadow: var(--context-menu-shadow);
}

.app-mockup .turn-message-menu-popover,
.app-mockup .composer-menu-popover {
  z-index: 16;
  gap: 2px;
  padding: 4px;
  overflow-y: auto;
}

.app-mockup .turn-message-menu-popover {
  position: absolute;
  top: calc(100% + 4px);
  right: 0;
  width: 188px;
}

.app-mockup .menu-item {
  display: flex;
  align-items: center;
  justify-content: flex-start;
  width: 100%;
  min-width: 0;
  min-height: var(--control-h-md);
  padding: 6px 10px;
  border: 1px solid transparent;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text-primary);
  font-family: var(--font-ui);
  font-size: var(--fs-base);
  font-weight: 400;
  line-height: 1.25;
  text-align: left;
  text-transform: none;
  white-space: nowrap;
}

.app-mockup .turn-message-menu-label {
  min-width: 0;
  flex: 1 1 auto;
}

.app-mockup .turn-message-menu-badge {
  flex: none;
  margin-left: 10px;
  color: var(--text-muted);
  font-size: var(--fs-xs);
}

.app-mockup .menu-item.is-disabled,
.app-mockup .menu-item:disabled {
  color: var(--text-disabled);
  cursor: default;
  opacity: 0.55;
}

.app-mockup .turn-blocks {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 6px;
}

.app-mockup .turn-card header + .turn-blocks {
  margin-top: 5px;
}

.app-mockup .turn-card.role-user .turn-blocks {
  margin-top: 2px;
  gap: 0;
}

.app-mockup .turn-text,
.app-mockup .turn-markdown {
  min-width: 0;
  color: var(--text-strong);
  font-size: 14px;
  overflow-wrap: anywhere;
}

.app-mockup .turn-text.is-tagged-instruction {
  color: var(--text-activity);
}

.app-mockup .turn-markdown code {
  border-radius: var(--radius-sm);
  background: var(--transcript-code-bg);
  padding: 1px 4px;
  color: var(--text-primary);
  font-family: var(--font-mono);
  font-size: 12.5px;
}

.app-mockup .turn-image-embed {
  width: fit-content;
  max-width: 100%;
  margin: 0;
}

.app-mockup .turn-image {
  display: block;
  width: 112px;
  max-width: 100%;
  height: auto;
  max-height: 70px;
  border: 1px solid var(--surface-border-strong);
  border-radius: var(--radius-md);
  background: var(--terminal-pane-bg);
  object-fit: cover;
  object-position: top;
}

.app-mockup .activity-group-block {
  color: var(--text-body-soft);
  font-size: 13px;
}

.app-mockup .activity-summary {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  align-items: center;
  border-radius: var(--radius-md);
  padding: 4px 10px;
}

.app-mockup .activity-group-label {
  color: var(--text-activity);
}

.app-mockup .turn-thinking {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 4px 10px 2px;
  color: var(--text-activity);
  font-size: 13px;
}

.app-mockup .turn-thinking-dot {
  flex: 0 0 auto;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: currentColor;
  animation: mock-status-dot-pulse 1.3s ease-in-out infinite;
}

.app-mockup .turn-thinking-label {
  font-style: italic;
}

/* --- composer --- */
.app-mockup .turn-sidebar-input {
  position: absolute;
  right: 0;
  bottom: 0;
  left: 0;
  display: flex;
  flex-direction: column;
  min-width: 0;
  max-height: 100%;
  overflow: hidden;
  padding: 10px 13px 10px 12px;
  background: linear-gradient(to top, var(--right-pane-bg) 88%, rgba(23, 27, 29, 0));
}

.app-mockup .native-input {
  display: flex;
  flex-direction: column;
  gap: 8px;
  justify-content: flex-end;
  min-width: 0;
}

.app-mockup .mock-textarea {
  min-height: 40px;
  border: 1px solid var(--control-border);
  border-radius: var(--radius-md);
  background: var(--field-bg);
  color: var(--placeholder-color);
  font-size: calc(var(--fs-base) + 1px);
  padding: 8px 9px;
}

.app-mockup .native-input-submit-actions {
  display: flex;
  align-items: center;
  flex-wrap: nowrap;
  justify-content: flex-end;
  gap: 6px;
}

.app-mockup .native-input-submit-actions .control-button {
  flex-wrap: nowrap;
  gap: 6px;
  min-height: var(--control-h-md);
  padding: 0 8px;
}

.app-mockup .composer-menu {
  position: relative;
  display: flex;
  flex: 0 0 auto;
  margin-left: auto;
  margin-right: 2px;
}

.app-mockup .composer-menu-trigger {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  color: var(--text-muted);
}

.app-mockup .composer-menu-popover {
  position: absolute;
  right: 0;
  bottom: calc(100% + 5px);
  width: 220px;
  max-height: 230px;
}

.app-mockup .queue-button-group {
  display: inline-flex;
  align-items: center;
}

.app-mockup .queue-button {
  column-gap: 7px;
}

.app-mockup .queue-button-group .queue-button-main {
  border-top-right-radius: 0;
  border-bottom-right-radius: 0;
}

.app-mockup .queue-menu-button {
  flex: 0 0 auto;
  min-width: 22px;
  margin-left: -1px;
  padding: 0 4px;
  border-top-left-radius: 0;
  border-bottom-left-radius: 0;
}

.app-mockup .queue-menu-button .lucide {
  opacity: 0.82;
}

/* ------------------------------------------------------------------ */
/* Collapsed panes. A collapsed pane keeps its grid track and zeroes it rather
   than leaving the grid, so every combination of collapsed/expanded resolves
   against one template — including the narrow layouts where the terminal has
   already dropped out. The panes clip their own overflow, so nothing leaks out
   of a zero-width track.

   Specificity is deliberately one class above the container-query rules below,
   which set the same custom properties. */
.app-mockup .app-shell.is-sidebar-collapsed {
  --sidebar-width: 0px;
}

.app-mockup .app-shell.is-right-collapsed {
  --turn-pane-width: 0px;
}

.app-mockup .app-shell.is-sidebar-collapsed .sidebar,
.app-mockup .app-shell.is-right-collapsed .turn-pane {
  border: 0;
}

/* A border-box element cannot narrow past its own padding, so the collapsed
   sidebar sheds its horizontal padding to reach zero. */
.app-mockup .app-shell.is-sidebar-collapsed .sidebar {
  padding-inline: 0;
}

.app-mockup .turn-pane {
  overflow: hidden;
}

/* Each pane's restore sits where that pane was, floating over the window: the
   sidebar's past the traffic lights at top-left, the transcript's at top-right
   where its header button was. Both are hidden until their pane is. */
.app-mockup .mock-restore-left,
.app-mockup .mock-restore-right {
  position: absolute;
  top: 6px;
  z-index: 11;
  display: none;
  width: 28px;
  height: 28px;
  backdrop-filter: blur(6px);
}

.app-mockup .mock-restore-left {
  /* Clear of the traffic lights, which end at 57px. */
  left: 74px;
}

.app-mockup .mock-restore-right {
  right: 10px;
}

.app-mockup:has(.app-shell.is-sidebar-collapsed) .mock-restore-left,
.app-mockup:has(.app-shell.is-right-collapsed) .mock-restore-right {
  display: inline-flex;
}


/* ------------------------------------------------------------------ */
/* The expanded transcript. It leaves the grid and covers everything right of
   the sidebar, exactly as the app does, and the timeline collapses to a
   centred reading column rather than running the full width. */
.app-mockup .app-shell.is-transcript-expanded .turn-pane {
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  left: var(--sidebar-width);
  z-index: 7;
  width: auto;
  box-shadow: inset 1px 0 0 var(--surface-divider);
}

.app-mockup .app-shell.is-transcript-expanded .turn-timeline {
  align-items: center;
  padding-right: 24px;
  padding-left: 24px;
}

.app-mockup .app-shell.is-transcript-expanded .turn-timeline > * {
  width: min(700px, 100%);
}

.app-mockup .app-shell.is-transcript-expanded .native-input {
  width: 100%;
  max-width: 700px;
  margin: 0 auto;
}

/* The expanded transcript already owns the full window when the sidebar is
   collapsed, so keep its header clear of the traffic lights. */
.app-mockup
  .app-shell.is-sidebar-collapsed.is-transcript-expanded
  .turn-pane-session-control,
.app-mockup:has(.app-shell.is-sidebar-collapsed.is-transcript-expanded)
  .mock-restore-left {
  display: none;
}

/* Same icon convention as the sidebar groups: -expand shows the action, and
   -collapse shows the way back. */
.app-mockup .app-shell:not(.is-transcript-expanded) [data-mock-action="expand-transcript"] .mock-icon-collapse,
.app-mockup .app-shell.is-transcript-expanded [data-mock-action="expand-transcript"] .mock-icon-expand {
  display: none;
}

/* A header button that is holding a mode open reads as pushed in, not as a
   different icon on the same neutral chip. */
.app-mockup .turn-pane-header-button.is-active {
  border-color: var(--accent-color);
  background: var(--accent-shadow);
  color: var(--text-strong);
  box-shadow: inset 0 1px 3px rgba(0, 0, 0, 0.45);
}

.app-mockup.is-interactive .turn-pane-header-button.is-active:hover {
  background: var(--accent-shadow);
  color: var(--text-strong);
}

/* ------------------------------------------------------------------ */
/* The four panels the header opens. Each ships hidden; the script only flips
   the hidden attribute and the button's pressed state. */
.app-mockup [data-mock-panel][hidden] {
  display: none;
}

/* Prompt library (turn-pane.css). Anchored under its trigger rather than
   portaled — inside the replica there is nothing to escape. */
.app-mockup .prompt-library-menu {
  position: absolute;
  top: 46px;
  right: 10px;
  z-index: 13;
  display: flex;
  width: min(300px, calc(100% - 20px));
  max-height: 280px;
  flex-direction: column;
  gap: 6px;
  padding: 8px;
  overflow: hidden;
  border: 1px solid var(--surface-divider);
  border-radius: var(--radius-lg);
  /* Keep the transcript completely occluded. This is intentionally a literal
     opaque fill rather than a translucent panel or inherited surface token. */
  background: #1d2224;
  opacity: 1;
  isolation: isolate;
  box-shadow: var(--popover-shadow);
}

.app-mockup .prompt-library-search {
  width: 100%;
  min-height: var(--control-h-md);
  padding: 0 10px;
  border: 1px solid var(--control-border);
  border-radius: var(--radius-md);
  background: var(--field-bg);
  color: var(--text-primary);
  font-family: var(--font-ui);
  font-size: var(--fs-base);
  outline: none;
}

.app-mockup .prompt-library-list {
  display: flex;
  flex: 1 1 auto;
  min-height: 0;
  flex-direction: column;
  gap: 1px;
  overflow-y: auto;
  overscroll-behavior: contain;
}

.app-mockup .prompt-library-item {
  position: relative;
  display: flex;
  align-items: stretch;
  min-width: 0;
}

.app-mockup .prompt-library-item-main {
  display: flex;
  align-items: center;
  flex: 1 1 auto;
  min-width: 0;
  min-height: 30px;
  padding: 5px 9px;
  border: 1px solid transparent;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-primary);
  font-family: var(--font-ui);
  font-size: var(--fs-base);
  text-align: left;
}

.app-mockup.is-interactive .prompt-library-item-main:hover {
  background: var(--popover-item-hover-bg);
}

.app-mockup .prompt-library-item-text {
  display: -webkit-box;
  overflow: hidden;
  color: #c9cfc9;
  font-size: var(--fs-base);
  line-height: 1.35;
  text-overflow: ellipsis;
  overflow-wrap: anywhere;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 3;
  line-clamp: 3;
}

.app-mockup .prompt-library-empty {
  padding: 10px;
  color: var(--text-muted);
  font-size: var(--fs-base);
  text-align: center;
}

/* Artifact tray (artifact-tray.css): a small card under the header's paperclip. */
.app-mockup .artifact-tray {
  position: absolute;
  top: 48px;
  right: 8px;
  z-index: 12;
  display: flex;
  width: 216px;
  max-width: calc(100% - 16px);
  flex-direction: column;
  border: 1px solid var(--control-border);
  border-radius: 10px;
  background: var(--right-pane-bg);
  box-shadow: 0 14px 38px rgb(0 0 0 / 38%);
}

.app-mockup .artifact-tray-titlebar {
  display: flex;
  height: 26px;
  flex: 0 0 26px;
  align-items: center;
  gap: 6px;
  padding: 0 9px;
  border-bottom: 1px solid var(--surface-divider);
  border-radius: 9px 9px 0 0;
  background: var(--chrome-header-bg);
}

.app-mockup .artifact-tray-clip,
.app-mockup .artifact-tray-label {
  color: var(--accent-color);
}

.app-mockup .artifact-tray-label {
  flex: 1;
  font-size: 10px;
  font-weight: 750;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}

.app-mockup .artifact-tray-chrome-button {
  display: inline-flex;
  width: 16px;
  height: 16px;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--text-faint, #6f7773);
}

.app-mockup.is-interactive .artifact-tray-chrome-button:hover {
  background: var(--hover-chrome-bg);
  color: var(--text-strong);
}

.app-mockup .artifact-tray-body {
  display: flex;
  flex-direction: column;
  gap: 1px;
  max-height: 244px;
  padding: 3px;
  overflow-y: auto;
  overscroll-behavior: contain;
}

.app-mockup .artifact-tray-row {
  display: flex;
  align-items: center;
  gap: 7px;
  width: 100%;
  min-height: 26px;
  padding: 3px 6px;
  border: 1px solid transparent;
  border-radius: var(--radius-md);
  background: transparent;
  color: var(--text-primary);
  font-family: var(--font-ui);
  font-size: 12px;
  text-align: left;
}

.app-mockup.is-interactive .artifact-tray-row:hover {
  background: var(--popover-item-hover-bg);
}

.app-mockup .artifact-tray-name {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.app-mockup .artifact-tray-meta {
  flex: none;
  color: var(--text-faint, #6f7773);
  font-size: 10px;
}

/* Browser overlay (browser.css): floats over the terminal, inset from the
   window and clear of the turn pane. */
.app-mockup .browser-overlay {
  position: absolute;
  top: 40px;
  right: calc(var(--turn-pane-width) + 20px);
  bottom: 30px;
  left: calc(var(--sidebar-width) + 24px);
  z-index: 8;
  display: flex;
  flex-direction: column;
  overflow: hidden;
  border: 1px solid var(--surface-divider);
  border-radius: 12px;
  background: #0e1011;
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.5);
}

.app-mockup .app-shell.is-transcript-expanded .browser-overlay {
  right: 20px;
}

.app-mockup .browser-overlay-nav {
  flex: 0 0 auto;
  display: flex;
  align-items: center;
  gap: 8px;
  height: 40px;
  padding: 0 10px;
  border-bottom: 1px solid var(--surface-divider);
  background: var(--chrome-header-bg);
}

.app-mockup .browser-overlay-close {
  display: inline-flex;
  width: 22px;
  height: 22px;
  flex: none;
  align-items: center;
  justify-content: center;
  border: 0;
  border-radius: var(--radius-sm);
  background: transparent;
  color: var(--text-muted);
}

.app-mockup.is-interactive .browser-overlay-close:hover {
  background: var(--hover-chrome-bg);
  color: var(--text-strong);
}

.app-mockup .browser-overlay-address {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  padding: 4px 9px;
  border: 1px solid var(--control-border);
  border-radius: 999px;
  background: var(--field-bg);
  color: var(--text-secondary);
  font-family: var(--font-mono);
  font-size: 11.5px;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.app-mockup .browser-overlay-page {
  flex: 1 1 auto;
  min-height: 0;
  overflow: hidden;
  padding: 22px 26px;
  background: #f7f7f4;
}

/* A stand-in for the dev server's page: enough structure to read as a rendered
   document, with none of it pretending to be live. */
.app-mockup .mock-preview {
  display: flex;
  max-width: 340px;
  flex-direction: column;
  gap: 10px;
  color: #1c2022;
}

.app-mockup .mock-preview-eyebrow {
  margin: 0;
  color: #6f7773;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.14em;
  text-transform: uppercase;
}

.app-mockup .mock-preview h4 {
  margin: 0;
  font-size: 20px;
  font-weight: 500;
  letter-spacing: -0.02em;
}

.app-mockup .mock-preview-figure {
  margin: 0;
  color: #2c7a65;
  font-size: 13px;
  font-variant-numeric: tabular-nums;
}

.app-mockup .mock-preview-row {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
  padding-top: 8px;
  border-top: 1px solid #e2e2dd;
  font-family: var(--font-mono);
  font-size: 10.5px;
}

.app-mockup .mock-preview-name {
  min-width: 0;
  overflow: hidden;
  color: #4a4f4d;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.app-mockup .mock-preview-fail {
  flex: none;
  color: #b4433a;
}

.app-mockup .mock-preview-pass {
  flex: none;
  color: #3f7a5f;
}

/* Queue split (transcript.css): the queue stops floating over the transcript
   and takes its own region below it. The transcript is genuinely shortened
   rather than covered, which is the point of the split. */
.app-mockup .turn-sidebar-input.is-split {
  height: 45%;
  overflow: hidden;
  padding-top: 10px;
  border-top: 1px solid var(--surface-divider);
  background: var(--right-pane-bg);
}

.app-mockup .turn-sidebar:has(.turn-sidebar-input.is-split) .turn-timeline {
  bottom: 45%;
  padding-bottom: 14px;
}

.app-mockup .turn-sidebar-input.is-split .native-input {
  flex: 1 1 auto;
  min-height: 0;
  justify-content: flex-end;
}

.app-mockup .turn-sidebar-input.is-split .queued-turn-stack {
  flex: 1 1 auto;
  align-content: start;
}

.app-mockup .turn-sidebar-input.is-split .queue-empty-state + .queued-turn-stack {
  /* The empty stack would otherwise claim half the flexible queue area, which
     centers the placeholder in only the upper half instead of the full region. */
  display: none;
}

.app-mockup .queue-empty-state {
  flex: 1 1 auto;
  display: grid;
  place-items: center;
  color: var(--text-muted);
  font-size: var(--fs-base);
}

/* ------------------------------------------------------------------ */
/* Collapsible groups. Every group ships its panes; collapsing is a class,
   so the static page and the enhanced one render from the same markup. */
.app-mockup .pane-group.is-collapsed .pane-list-body {
  display: none;
}

.app-mockup .pane-group.has-panes.is-collapsed .pane-group-header {
  margin-bottom: 0;
}

/* The count is a collapsed-group affordance in the app; expanding hides it. */
.app-mockup .pane-group:not(.is-collapsed) .pane-group-count,
.app-mockup .pane-group:not(.is-collapsed) .mock-icon-expand,
.app-mockup .pane-group.is-collapsed .mock-icon-collapse {
  display: none;
}

/* ------------------------------------------------------------------ */
/* Replay. The tiny blocking bootstrap activates server-rendered staging hints
   before first paint; the deferred enhancement atomically replaces them with
   its runtime classes. If either script is unavailable, the hints do nothing
   and the complete static session remains visible. */
html.mock-replay-boot .app-mockup [data-replay-pending],
.app-mockup [data-step].is-pending,
.app-mockup .mock-terminal-line.is-pending {
  display: none;
}

.app-mockup .is-revealed {
  animation: mock-reveal 260ms ease-out both;
}

.app-mockup .mock-terminal-line.is-revealed {
  animation-duration: 120ms;
}

@keyframes mock-reveal {
  from {
    opacity: 0;
    transform: translateY(3px);
  }

  to {
    opacity: 1;
    transform: none;
  }
}

/* ------------------------------------------------------------------ */
/* Queue demo (composer.css). The stack grows upward from the composer as
   turns are queued, exactly as it does in the app. */
.app-mockup .queued-turn-stack {
  display: grid;
  align-content: end;
  gap: 6px;
  min-width: 0;
  min-height: 0;
  flex: 0 1 auto;
  overflow: auto;
  overscroll-behavior: contain;
  padding-block: 4px;
}

.app-mockup .queued-turn {
  position: relative;
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 4px 8px;
  align-items: start;
  min-width: 0;
  border: 1px solid var(--queued-turn-border);
  border-radius: var(--radius-md);
  background: var(--content-inset-bg);
  color: var(--text-body-soft);
  padding: 7px 8px;
  font-size: var(--fs-base);
  line-height: 1.4;
}

.app-mockup .queued-turn-text {
  display: block;
  min-width: 0;
  align-self: center;
  overflow-wrap: anywhere;
  white-space: pre-wrap;
}

.app-mockup .queued-turn-actions {
  display: flex;
  gap: 4px;
}

.app-mockup .queued-turn-actions > button {
  min-height: var(--control-h-sm);
  padding: 2px 6px;
  font-size: var(--fs-sm);
}

.app-mockup .queued-turn-remove svg {
  width: 13px;
  height: 13px;
  opacity: 0.85;
}

/* The sidebar tab mirrors the queue depth, the way it does in the app. */
.app-mockup .pane-tab-status-queued {
  background: color-mix(in srgb, var(--status-active-fg) 8%, var(--pane-tab-status-bg));
  color: color-mix(in srgb, var(--status-active-fg) 70%, var(--text-subtle));
  box-shadow: inset 0 0 0 1px color-mix(in srgb, var(--status-active-fg) 18%, transparent);
}

/* ------------------------------------------------------------------ */
/* Enhanced-only affordances. Hover and focus states are scoped to
   .is-interactive so the static replica never advertises a control that
   would not respond. */
.app-mockup.is-interactive .mock-live {
  cursor: pointer;
}

/* Expanding groups can overflow the list, so it becomes a scroll surface — with
   the app's thin scrollbar, and without trapping the page's wheel at the ends. */
.app-mockup.is-interactive .pane-list {
  overflow-y: auto;
  overscroll-behavior: auto;
  scrollbar-width: thin;
}

.app-mockup.is-interactive .pane-group-header:hover .pane-group-name {
  color: var(--text-interactive);
}

/* Hovering a group row brightens its chevron rather than materialising a button
   under it: the row is the target, so a second box appearing inside it reads as
   a control the pointer is not actually on. */
.app-mockup.is-interactive .pane-group-header:hover .pane-group-collapse-button {
  color: var(--control-fg-muted);
}

.app-mockup.is-interactive .pane-group-collapse-button:focus-visible {
  outline: none;
  box-shadow: 0 0 0 2px var(--accent-shadow);
}

/* Every bordered control shares the one hover step. */
.app-mockup.is-interactive .sidebar-actions .control-button:hover,
.app-mockup.is-interactive .native-input-submit-actions button:hover:not(:disabled),
.app-mockup.is-interactive .queued-turn-actions > button:hover {
  border-color: var(--hover-border);
  background: var(--hover-bg);
  color: var(--hover-fg);
}

.app-mockup.is-interactive .menu-item:hover:not(:disabled) {
  border-color: transparent;
  background: var(--popover-item-hover-bg);
  color: var(--text-primary);
}

.app-mockup.is-interactive .sidebar-collapse-button:hover {
  background: var(--hover-bg);
  color: var(--hover-chrome-fg);
}

.app-mockup.is-interactive .turn-pane-header-button:hover {
  border-color: var(--hover-border);
  background: var(--hover-chrome-bg);
  color: var(--hover-chrome-fg);
}

.app-mockup.is-interactive .mock-textarea {
  min-height: 40px;
  max-height: 120px;
  overflow-y: auto;
  resize: none;
  font-family: inherit;
  line-height: 1.45;
  color: var(--text-strong);
  cursor: text;
}

.app-mockup.is-interactive .mock-textarea::placeholder {
  color: var(--placeholder-color);
  opacity: 1;
}

.app-mockup.is-interactive .mock-textarea:focus,
.app-mockup.is-interactive button:focus-visible {
  border-color: var(--focus-ring, #6cae9d);
  outline: none;
  box-shadow: inset 0 0 0 1px var(--focus-ring, #6cae9d);
}

/* ------------------------------------------------------------------ */
/* The sizing frame also hosts the visually hidden live region used by the
   progressive-enhancement script. */
.app-mockup-frame {
  position: relative;
  /* The replica sizes itself against its container, not the viewport, so it
     lays out correctly wherever it is embedded — the hero column, a narrower
     document, a side panel. */
  container-type: inline-size;
  container-name: mockup;
}

/* Queue and replay state is announced, not printed. */
.mock-demo-status {
  position: absolute;
  width: 1px;
  height: 1px;
  margin: -1px;
  padding: 0;
  border: 0;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}

@media (prefers-reduced-motion: reduce) {
  .app-mockup .is-revealed {
    animation: none;
  }
}

/* --- responsive ---
   The replica narrows the way the real window does — panes shrink toward their
   minimums first, and only then does the terminal drop out. The sidebar and its
   window chrome stay to the last step, so the replica never degrades into
   something that reads like a chat app rather than a desktop terminal.

   Thresholds are the replica's own width, not the page's. */
@container mockup (max-width: 68rem) {
  .app-mockup .app-shell {
    --turn-pane-width: 380px;
    height: 640px;
  }
}

@container mockup (max-width: 60rem) {
  .app-mockup .app-shell {
    --sidebar-width: 250px;
    --turn-pane-width: 340px;
    height: 600px;
  }
}

@container mockup (max-width: 56rem) {
  .app-mockup .app-shell {
    --sidebar-width: 240px;
    /* The app clamps the turn pane at 300px; so does the replica. */
    --turn-pane-width: 300px;
    height: 560px;
  }
}

/* Below this the terminal would be thinner than a usable pane, so it drops
   rather than becoming a sliver of clipped text. */
@container mockup (max-width: 50rem) {
  .app-mockup .app-shell {
    grid-template-columns: var(--sidebar-width) minmax(0, 1fr);
    height: 520px;
  }

  .app-mockup .mock-terminal {
    display: none;
  }

  /* With the terminal gone there is nothing behind the transcript to reveal, so
     hiding it would leave an empty window. The state is still handled rather
     than merely blocked, in case the window is narrowed while collapsed. */
  .app-mockup [data-mock-action="hide-right"] {
    display: none;
  }

  .app-mockup .app-shell.is-right-collapsed {
    grid-template-columns: minmax(0, 1fr) 0px;
  }

  /* The overlay normally floats over the terminal; with the terminal gone it
     takes the window instead, which is the app's full-width browser mode —
     rather than squeezing into a sliver between the two remaining panes. */
  .app-mockup .browser-overlay {
    right: 20px;
    left: 20px;
  }
}

@container mockup (max-width: 36rem) {
  /* The session id is the header's most useful text; drop the secondary
     controls before letting it truncate to nothing. */
  .app-mockup .turn-pane-header-controls > .turn-pane-header-button:nth-child(-n + 3) {
    display: none;
  }
}

@container mockup (max-width: 30rem) {
  .app-mockup .app-shell {
    --sidebar-width: 152px;
    height: 460px;
  }

  /* .sidebar.is-narrow in the app: no room for both icon and label. */
  .app-mockup .sidebar-actions .control-button:not(.sidebar-settings-button) .lucide {
    display: none;
  }

  .app-mockup .sidebar-mode-toggle > span > span {
    display: none;
  }
}

@media (prefers-reduced-motion: reduce) {
  .app-mockup .pane-tab-dot,
  .app-mockup .turn-thinking-dot,
  .app-mockup .mock-terminal-cursor {
    animation: none;
  }
}
`;
