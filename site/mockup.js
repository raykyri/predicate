// @ts-check
//
// Progressive enhancement for the landing page's qmux replica.
//
// The server renders the replica's finished state as inert markup (see
// web/landing/AppMockup.tsx). This script turns parts of it on:
//
//   replay  — rewinds the session and streams it back: the terminal writes its
//             command output block by block while the transcript fills in behind
//             it, driven by the shared `data-step` timeline in the markup.
//   queue   — makes the composer real. Typing and queueing stacks turn cards
//             above it and updates the sidebar tab's queue pill, which is the
//             product's headline interaction rather than a decorative one.
//   groups  — expands and collapses the sidebar's project groups.
//   panes   — collapses and restores the sidebar and the transcript, and expands
//             the transcript over the window, with the app's own restore
//             affordances.
//   terminal-map — opens the sidebar dashboard's modal: every pane's queue side
//             by side, with working stream chips, per-rail composers, and rail
//             heads that hand you to their session.
//   panels  — the five surfaces the right pane's header opens: the prompt
//             library, the notification journal, the split queue, the browser
//             overlay, and the artifact tray. They interlock: a saved prompt
//             lands in the composer, opening an artifact hands it to the
//             browser, and the journal marks, mutes, and confirms-to-clear
//             notifications.
//   images  — opens transcript thumbnails at their original resolution in the
//             same modal lightbox as the desktop app.
//   menus   — opens the transcript-message and composer overflow menus. Their
//             items are intentionally inert: this is a product tour, not a
//             clipboard, publishing, or session-management surface.
//   sidebar-menus — the sidebar's right-click menus: a tab's details menu and
//             the group menu behind the … button. Only the collapse item does
//             work; the rest dismiss, like every other menu here.
//   research — the sidebar's Terminal/Research toggle and the half of the window
//             behind it: switching modes swaps the sidebar's list and the stage,
//             selecting a research row opens that document, and a folder expands
//             and collapses in place.
//   journal — the Journal tab's feed. Its composer adds entries for real, an
//             entry's menu deletes one, and the undo bar puts it back. Nothing
//             is hydrated here, so a pasted URL lands as a link card; the
//             embedded tweets are the ones the fixture shipped.
//   research-menus — the research sidebar's right-click menus, one per row,
//             carrying the app's A and D keycaps. Their items dismiss, like
//             every other menu here.
//
// Each is independent: `data-mock-features` on the replica selects which ones
// run, and any that is dropped leaves the rendered finished state in place.
//
// Two rules hold everywhere here:
//   * Nothing is required. If this file never loads, the page is the static
//     replica it already was.
//   * The replica is a labelled image only while it is inert. As soon as a
//     control becomes real, the image role has to go and the decorative parts
//     have to be hidden from assistive tech instead — a focusable control inside
//     a presentational subtree is worse than no control at all.

/**
 * Give one replica its own state, timers, and element-scoped handlers.
 * Document-level dismissal listeners remain harmlessly independent because
 * their open-menu state lives inside this closure.
 * @param {HTMLElement} mockup
 * @param {boolean} replayBoot
 */
function enhanceQmuxMockup(mockup, replayBoot) {
  const frame = mockup.closest(".app-mockup-frame");
  const features = new Set((mockup.dataset.mockFeatures || "").split(/\s+/).filter(Boolean));

  if (features.size === 0) {
    return;
  }

  const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  const status = frame ? frame.querySelector(".mock-demo-status") : null;

  /** @param {string} message */
  function announce(message) {
    if (status) {
      status.textContent = message;
    }
  }

  /**
   * Subtrees that stay decorative. They hold no focusable elements, so hiding
   * them from assistive tech is safe, and it keeps a screen reader from wading
   * through a terminal dump to reach the controls that do work.
   * @param {string[]} selectors
   */
  function hideFromAssistiveTech(selectors) {
    for (const selector of selectors) {
      for (const node of mockup.querySelectorAll(selector)) {
        node.setAttribute("aria-hidden", "true");
      }
    }
  }

  // ---------------------------------------------------------------- replay

  const CANCELLED = Symbol("cancelled");

  function createReplay() {
    let token = 0;
    let playing = false;
    let played = false;
    /** @type {HTMLElement | null} */
    let preparedScreen = null;

    function activeContext() {
      const screen = mockup.querySelector('.mock-terminal-screen:not([hidden])');
      const timeline = mockup.querySelector('.turn-timeline:not([hidden])');
      if (!(screen instanceof HTMLElement) || !(timeline instanceof HTMLElement)) {
        return null;
      }
      const stepped = /** @type {HTMLElement[]} */ ([
        ...screen.querySelectorAll("[data-step]"),
        ...timeline.querySelectorAll("[data-step]"),
      ]);
      const cursor = screen.querySelector(".mock-terminal-cursor");
      const tail = screen.querySelector(".mock-terminal-tail");
      const dot = mockup.querySelector(".pane-tab-row.is-selected .pane-tab-dot");
      const selectedRow = mockup.querySelector(".pane-tab-row.is-selected");
      return {
        cursor,
        dot,
        dotClass: dot ? dot.className : "",
        isWorking:
          selectedRow instanceof HTMLElement && selectedRow.dataset.mockSessionStatus === "active",
        screen,
        stepped,
        tail,
        timeline,
      };
    }

    if (!activeContext()) {
      return null;
    }

    /**
     * A cancellable pause. Every await in a run checks the token it started
     * with, so a second play() (or a skip) abandons the first run's tail.
     * @param {number} ms
     * @param {number} runToken
     */
    const sleep = (ms, runToken) =>
      new Promise((resolve, reject) => {
        window.setTimeout(() => (runToken === token ? resolve(undefined) : reject(CANCELLED)), ms);
      });

    /** @param {Element} node */
    function reveal(node) {
      node.classList.remove("is-pending");
      node.classList.add("is-revealed");
    }

    /** @param {HTMLElement} timeline */
    function scrollTranscriptToTail(timeline) {
      // Follow new output while replaying; the timeline remains a user-scrollable
      // surface before, during, and after the replay.
      timeline.scrollTop = timeline.scrollHeight;
    }

    /**
     * @param {NonNullable<ReturnType<typeof activeContext>>} context
     * @param {number} visibleThroughStep
     * @param {boolean} [animateVisible]
     */
    function rewind(context, visibleThroughStep, animateVisible = true) {
      for (const node of context.stepped) {
        const visible = Number(node.dataset.step) <= visibleThroughStep;
        node.classList.toggle("is-pending", !visible);
        node.classList.toggle("is-revealed", visible && animateVisible);
        if (node.classList.contains("mock-terminal-block")) {
          for (const line of node.querySelectorAll(".mock-terminal-line")) {
            line.classList.toggle("is-pending", !visible);
            line.classList.toggle("is-revealed", visible && animateVisible);
          }
        }
      }
      if (context.cursor) {
        const visibleLines = context.screen.querySelectorAll(
          ".mock-terminal-block.is-revealed .mock-terminal-line",
        );
        const lastVisibleLine = visibleLines.item(visibleLines.length - 1);
        if (lastVisibleLine) {
          lastVisibleLine.appendChild(context.cursor);
        } else if (context.tail) {
          context.tail.appendChild(context.cursor);
        }
      }
      context.timeline.scrollTop = 0;
    }

    /** @param {NonNullable<ReturnType<typeof activeContext>>} context */
    function showFinalState(context) {
      preparedScreen = null;
      for (const node of context.stepped) {
        node.classList.remove("is-pending", "is-revealed");
        for (const line of node.querySelectorAll(".mock-terminal-line")) {
          line.classList.remove("is-pending", "is-revealed");
        }
      }
      if (context.tail && context.cursor) {
        context.tail.appendChild(context.cursor);
      }
      if (context.dot) {
        context.dot.className = context.dotClass;
      }
      scrollTranscriptToTail(context.timeline);
    }

    /** @param {NonNullable<ReturnType<typeof activeContext>>} context */
    function replayStartStep(context) {
      const terminalSteps = [...context.screen.querySelectorAll(".mock-terminal-block[data-step]")]
        .map((node) => Number(node.getAttribute("data-step")))
        .filter(Number.isFinite);
      return terminalSteps.length > 0 ? Math.min(...terminalSteps) : 0;
    }

    // Transfer the server-rendered, pre-paint staging hints to the replay's
    // runtime classes without animating or changing what is currently visible.
    function prepare() {
      const context = activeContext();
      if (!context || !context.isWorking || context.stepped.length === 0) {
        return;
      }
      rewind(context, replayStartStep(context), false);
      preparedScreen = context.screen;
    }

    async function play() {
      const context = activeContext();
      if (!context || context.stepped.length === 0) {
        return;
      }
      if (!context.isWorking) {
        showFinalState(context);
        return;
      }
      const visibleThroughStep = replayStartStep(context);
      const steps = [
        ...new Set(context.stepped.map((node) => Number(node.dataset.step))),
      ]
        .filter((step) => step > visibleThroughStep)
        .sort((a, b) => a - b);
      token += 1;
      const runToken = token;
      playing = true;
      played = true;
      mockup.classList.add("is-playing");
      if (context.dot) {
        context.dot.className = "pane-tab-dot status-active";
      }
      // A working session opens with its first completed command and matching
      // transcript already visible. Only newer activity streams in.
      if (preparedScreen === context.screen) {
        preparedScreen = null;
      } else {
        rewind(context, visibleThroughStep);
      }
      onStateChange();
      try {
        await sleep(260, runToken);
        for (const step of steps) {
          const nodes = context.stepped.filter((node) => Number(node.dataset.step) === step);
          for (const node of nodes) {
            if (!node.classList.contains("mock-terminal-block")) {
              reveal(node);
              scrollTranscriptToTail(context.timeline);
              await sleep(140, runToken);
              continue;
            }
            // A command's output lands in a burst, the way a real pane fills.
            reveal(node);
            for (const line of node.querySelectorAll(".mock-terminal-line")) {
              reveal(line);
              if (context.cursor) {
                line.appendChild(context.cursor);
              }
              await sleep(34, runToken);
            }
          }
          await sleep(520, runToken);
        }
        if (context.tail && context.cursor) {
          context.tail.appendChild(context.cursor);
        }
        playing = false;
        mockup.classList.remove("is-playing");
        if (context.dot) {
          context.dot.className = context.dotClass;
        }
        onStateChange();
        announce("Session replay finished.");
      } catch (error) {
        if (error !== CANCELLED) {
          throw error;
        }
      }
    }

    function skip() {
      token += 1;
      playing = false;
      mockup.classList.remove("is-playing");
      const context = activeContext();
      if (context) {
        showFinalState(context);
      }
      onStateChange();
    }

    /** @type {() => void} */
    let onStateChange = () => {};

    return {
      prepare,
      play,
      skip,
      isPlaying: () => playing,
      hasPlayed: () => played,
      isWorking: () => Boolean(activeContext()?.isWorking),
      /** @param {() => void} listener */
      onChange(listener) {
        onStateChange = listener;
      },
    };
  }

  // ----------------------------------------------------------------- queue

  const X_ICON =
    '<svg class="lucide" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" ' +
    'stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" ' +
    'aria-hidden="true" focusable="false"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';

  /**
   * Swaps a decorative <span> control for the real element, carrying its
   * attributes and contents over so the app's skin and the markup's own hooks
   * still apply.
   * @param {Element | null} node
   * @param {string} tagName
   */
  function promote(node, tagName) {
    if (!(node instanceof HTMLElement)) {
      return null;
    }
    const replacement = document.createElement(tagName);
    // Every attribute, not just the class: the markup's own hooks (data-mock-action)
    // have to survive the swap.
    for (const attribute of node.attributes) {
      replacement.setAttribute(attribute.name, attribute.value);
    }
    // A promoted control is by definition meant to be reachable, so it never
    // inherits the decorative subtree's aria-hidden (the sidebar header ships
    // hidden wholesale and its buttons become real one feature at a time).
    replacement.removeAttribute("aria-hidden");
    replacement.innerHTML = node.innerHTML;
    if (replacement instanceof HTMLButtonElement) {
      replacement.type = "button";
    }
    node.replaceWith(replacement);
    return replacement;
  }

  /**
   * Positions a portaled context menu at coordinates already relative to the
   * replica, then clamps it inside — the app's clampContextMenuToViewport,
   * against the window rather than the page.
   * @param {HTMLElement} menu
   * @param {number} x
   * @param {number} y
   */
  function placeContextMenu(menu, x, y) {
    menu.hidden = false;
    menu.style.left = `${Math.max(8, Math.min(x, mockup.clientWidth - menu.offsetWidth - 8))}px`;
    menu.style.top = `${Math.max(8, Math.min(y, mockup.clientHeight - menu.offsetHeight - 8))}px`;
  }

  /**
   * Pointer coordinates relative to the replica, which is what every menu here
   * is positioned against.
   * @param {MouseEvent} event
   */
  function pointerInMockup(event) {
    const rect = mockup.getBoundingClientRect();
    return { x: event.clientX - rect.left, y: event.clientY - rect.top };
  }

  /**
   * The open menu's item whose single-letter keycap matches a pressed key, if
   * any. Menus that display keycaps honor them while open; what the item then
   * does is the caller's decision, same as a click on it.
   * @param {HTMLElement} menu
   * @param {string} key
   */
  function keycapItem(menu, key) {
    if (key.length !== 1) {
      return null;
    }
    for (const item of menu.querySelectorAll("[data-mock-context-item]")) {
      const cap = item.querySelector("kbd");
      if (cap && cap.textContent && cap.textContent.trim().toLowerCase() === key.toLowerCase()) {
        return item instanceof HTMLElement ? item : null;
      }
    }
    return null;
  }

  function createQueue() {
    const composer = mockup.querySelector(".native-input");
    const field = mockup.querySelector(".mock-textarea");
    const actions = mockup.querySelector(".native-input-submit-actions");
    if (
      !composer ||
      !(field instanceof HTMLElement) ||
      !actions ||
      !mockup.querySelector(".turn-timeline:not([hidden])") ||
      !mockup.querySelector(".pane-tab-row.is-selected .pane-tab")
    ) {
      return null;
    }

    const currentTimeline = () => mockup.querySelector(".turn-timeline:not([hidden])");
    const currentPaneTab = () => mockup.querySelector(".pane-tab-row.is-selected .pane-tab");

    const textarea = document.createElement("textarea");
    textarea.className = field.className;
    textarea.rows = 1;
    textarea.placeholder = field.textContent || "";
    textarea.setAttribute("aria-label", "Message for the agent (demo)");
    field.replaceWith(textarea);

    const stack = document.createElement("div");
    stack.className = "queued-turn-stack";
    composer.prepend(stack);

    // Only the two submit buttons do anything here; the queue dropdown stays
    // decorative, while the overflow menu is enhanced independently below.
    const buttons = [...actions.querySelectorAll(".control-button")];
    const sendNowButton = promote(buttons[0] ?? null, "button");
    const queueButton = promote(buttons[1] ?? null, "button");
    for (const decorative of actions.querySelectorAll(".queue-menu-button")) {
      decorative.setAttribute("aria-hidden", "true");
    }

    function autoGrow() {
      textarea.style.height = "auto";
      textarea.style.height = `${Math.min(textarea.scrollHeight, 120)}px`;
    }

    function queuedCount() {
      return stack.childElementCount;
    }

    function syncPaneTab() {
      const paneTab = currentPaneTab();
      if (!paneTab) {
        return;
      }
      const count = queuedCount();
      let meta = paneTab.querySelector(".pane-tab-meta");
      if (count === 0) {
        if (meta) {
          meta.remove();
        }
        return;
      }
      if (!meta) {
        meta = document.createElement("span");
        meta.className = "pane-tab-meta";
        paneTab.append(meta);
      }
      meta.innerHTML = "";
      const pill = document.createElement("small");
      pill.className = "pane-tab-status pane-tab-status-queued";
      pill.textContent = `${count} queued`;
      meta.append(pill);
    }

    /** @param {string} text */
    function addQueuedTurn(text) {
      const card = document.createElement("div");
      card.className = "queued-turn is-revealed";
      const body = document.createElement("div");
      body.className = "queued-turn-text";
      body.textContent = text;
      const cardActions = document.createElement("div");
      cardActions.className = "queued-turn-actions";
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "control-button queued-turn-remove";
      remove.setAttribute("aria-label", `Remove queued turn: ${text}`);
      remove.innerHTML = X_ICON;
      remove.addEventListener("click", () => {
        card.remove();
        syncPaneTab();
        announce(
          queuedCount() === 0
            ? "Queue empty."
            : `Removed. ${queuedCount()} turn${queuedCount() === 1 ? "" : "s"} queued.`,
        );
      });
      cardActions.append(remove);
      card.append(body, cardActions);
      stack.append(card);
      stack.scrollTop = stack.scrollHeight;
      syncPaneTab();
      announce(`Queued. ${queuedCount()} turn${queuedCount() === 1 ? "" : "s"} waiting.`);
    }

    /** @param {string} text */
    function sendNow(text) {
      const timeline = currentTimeline();
      if (!timeline) {
        return;
      }
      const template = timeline.querySelector(".turn-card.role-user");
      if (!(template instanceof HTMLElement)) {
        return;
      }
      const card = /** @type {HTMLElement} */ (template.cloneNode(true));
      card.removeAttribute("data-step");
      card.classList.remove("is-pending");
      card.classList.add("is-revealed");
      const blocks = card.querySelector(".turn-blocks");
      if (blocks) {
        blocks.innerHTML = "";
        const block = document.createElement("div");
        block.className = "turn-message-block";
        const paragraph = document.createElement("p");
        paragraph.className = "turn-text";
        paragraph.textContent = text;
        block.append(paragraph);
        blocks.append(block);
      }
      const thinking = timeline.querySelector(".turn-thinking");
      timeline.insertBefore(card, thinking);
      if (thinking) {
        // The agent picks the steer up immediately, so the working indicator
        // stays pinned below it.
        thinking.classList.remove("is-pending");
      }
      timeline.scrollTop = timeline.scrollHeight;
      announce("Sent to the agent, interrupting its current work.");
    }

    function submit(/** @type {"queue" | "send"} */ mode) {
      const text = textarea.value.trim();
      if (!text) {
        textarea.focus();
        return;
      }
      if (mode === "queue") {
        addQueuedTurn(text);
      } else {
        sendNow(text);
      }
      textarea.value = "";
      autoGrow();
      textarea.focus();
    }

    textarea.addEventListener("input", autoGrow);
    textarea.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || event.shiftKey) {
        return;
      }
      event.preventDefault();
      submit("queue");
    });
    if (queueButton) {
      queueButton.addEventListener("click", () => submit("queue"));
    }
    if (sendNowButton) {
      sendNowButton.addEventListener("click", () => submit("send"));
    }

    mockup.addEventListener("mock-session-change", () => {
      stack.innerHTML = "";
      for (const status of mockup.querySelectorAll(".pane-tab-status-queued")) {
        status.closest(".pane-tab-meta")?.remove();
      }
    });

    autoGrow();
    return { reset: () => {
      stack.innerHTML = "";
      syncPaneTab();
    } };
  }

  // ----------------------------------------------------------------- panes

  const PANE_LABELS = {
    "hide-sidebar": "Hide the sidebar",
    "show-sidebar": "Show the sidebar",
    "hide-right": "Hide the transcript",
    "show-right": "Show the transcript",
    "expand-transcript": "Expand the transcript",
  };

  function createPanes() {
    const foundShell = mockup.querySelector(".app-shell");
    const foundSidebar = mockup.querySelector(".sidebar");
    const foundTurnPane = mockup.querySelector(".turn-pane");
    if (!foundShell || !foundSidebar || !foundTurnPane) {
      return null;
    }
    const shell = /** @type {Element} */ (foundShell);
    const sidebar = /** @type {Element} */ (foundSidebar);
    const turnPane = /** @type {Element} */ (foundTurnPane);
    const initiallyExpanded = shell.classList.contains("is-transcript-expanded");

    // Initial layout classes can ship from the server for replicas that open
    // on a different product state. Match their accessibility state before
    // promoting any controls inside those panes.
    sidebar.toggleAttribute("inert", shell.classList.contains("is-sidebar-collapsed"));
    turnPane.toggleAttribute("inert", shell.classList.contains("is-right-collapsed"));

    /**
     * Moves focus off a pane that is about to be hidden. Without this the
     * keyboard is left on a control inside a zero-width, clipped pane.
     * @param {string} action
     */
    function focusRestoreControl(action) {
      for (const candidate of mockup.querySelectorAll(`[data-mock-action="${action}"]`)) {
        if (candidate instanceof HTMLElement && candidate.offsetParent !== null) {
          candidate.focus();
          return;
        }
      }
    }

    /** @param {boolean} open */
    function setSidebar(open) {
      shell.classList.toggle("is-sidebar-collapsed", !open);
      // A collapsed pane is clipped, not removed, so `inert` is what actually
      // takes its controls out of the tab order and off the screen reader.
      sidebar.toggleAttribute("inert", !open);
      if (open) {
        focusRestoreControl("hide-sidebar");
      } else {
        focusRestoreControl("show-sidebar");
      }
      announce(open ? "Sidebar shown." : "Sidebar hidden.");
    }

    /** @param {boolean} expanded */
    function setExpanded(expanded) {
      shell.classList.toggle("is-transcript-expanded", expanded);
      for (const button of mockup.querySelectorAll('[data-mock-action="expand-transcript"]')) {
        button.classList.toggle("is-active", expanded);
        button.setAttribute("aria-pressed", String(expanded));
        const label = expanded ? "Restore the transcript" : "Expand the transcript";
        button.setAttribute("aria-label", label);
        button.setAttribute("title", label);
      }
      announce(expanded ? "Transcript expanded." : "Transcript restored.");
    }

    /** @param {boolean} open */
    function setRightPane(open) {
      if (!open) {
        // Hiding a pane that is currently covering the window would leave the
        // expanded state set with nothing to show it on.
        setExpanded(false);
      }
      shell.classList.toggle("is-right-collapsed", !open);
      turnPane.toggleAttribute("inert", !open);
      if (open) {
        focusRestoreControl("hide-right");
      } else {
        focusRestoreControl("show-right");
      }
      announce(open ? "Transcript shown." : "Transcript hidden.");
    }

    /** @type {Record<string, () => void>} */
    const handlers = {
      "hide-sidebar": () => setSidebar(false),
      "show-sidebar": () => setSidebar(true),
      "hide-right": () => setRightPane(false),
      "show-right": () => setRightPane(true),
      "expand-transcript": () =>
        setExpanded(!shell.classList.contains("is-transcript-expanded")),
    };

    for (const node of [...mockup.querySelectorAll("[data-mock-action]")]) {
      const action = node instanceof HTMLElement ? node.dataset.mockAction ?? "" : "";
      const handler = handlers[action];
      if (!handler) {
        continue;
      }
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      const label =
        action === "expand-transcript" && initiallyExpanded
          ? "Restore the transcript"
          : PANE_LABELS[/** @type {keyof typeof PANE_LABELS} */ (action)];
      button.setAttribute("aria-label", label);
      button.setAttribute("title", label);
      if (action === "expand-transcript") {
        // A toggle reports its state from the start, not only once pressed.
        button.classList.toggle("is-active", initiallyExpanded);
        button.setAttribute("aria-pressed", String(initiallyExpanded));
      }
      button.addEventListener("click", handler);
    }
    return {};
  }

  // ---------------------------------------------------------- terminal-map

  const CHECK_ICON =
    '<svg class="lucide" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" ' +
    'stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" ' +
    'aria-hidden="true" focusable="false"><path d="M20 6 9 17l-5-5"/></svg>';
  const MINUS_ICON =
    '<svg class="lucide" xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" ' +
    'stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" ' +
    'aria-hidden="true" focusable="false"><path d="M5 12h14"/></svg>';

  /** Sentinel rail id for the drafts column, matching the app's Home board. */
  const DRAFTS_RAIL_ID = "__drafts__";

  function createTerminalMap() {
    const foundBackdrop = mockup.querySelector("[data-mock-terminal-map]");
    const triggerNode = mockup.querySelector('[data-mock-action="open-terminal-map"]');
    if (!(foundBackdrop instanceof HTMLElement) || !triggerNode) {
      return null;
    }
    const backdrop = /** @type {HTMLElement} */ (foundBackdrop);
    const dialogNode = backdrop.querySelector("[data-mock-terminal-map-dialog]");
    const trigger = promote(triggerNode, "button");
    if (!trigger || !(dialogNode instanceof HTMLElement)) {
      return null;
    }
    const dialog = /** @type {HTMLElement} */ (dialogNode);
    // Re-bound so the hoisted setOpen closure sees a non-null trigger.
    const openTrigger = trigger;
    openTrigger.setAttribute("aria-haspopup", "dialog");
    openTrigger.setAttribute("aria-label", "Open the terminal map");
    openTrigger.setAttribute("title", "Open the terminal map");

    // One caret menu open at a time, like every other popover here.
    /** @type {HTMLElement | null} */
    let openMenu = null;
    /** @type {HTMLElement | null} */
    let openCaret = null;

    /** @param {boolean} refocus */
    function closeMenu(refocus) {
      if (openMenu && openCaret) {
        openMenu.hidden = true;
        openCaret.classList.remove("is-open");
        openCaret.setAttribute("aria-expanded", "false");
        if (refocus) {
          openCaret.focus();
        }
      }
      openMenu = null;
      openCaret = null;
    }

    /** @param {string} name */
    function streamRails(name) {
      return /** @type {HTMLElement[]} */ (
        [...backdrop.querySelectorAll(`[data-mock-rail-group="${name}"]`)]
      );
    }

    /**
     * A chip reads its rails' visibility from the DOM: filled when every rail
     * shows, hollow when none does, a dash on a hollow box when mixed.
     * @param {HTMLElement} chip
     */
    function syncChip(chip) {
      const name = chip.dataset.mockHomeChip ?? "";
      const rails = streamRails(name);
      if (rails.length === 0) {
        return;
      }
      const visible = rails.filter((rail) => !rail.hidden).length;
      chip.classList.toggle("is-off", visible === 0);
      chip.classList.toggle("is-mixed", visible > 0 && visible < rails.length);
      const checkbox = chip.querySelector(".home-group-checkbox");
      if (checkbox) {
        checkbox.innerHTML =
          visible === rails.length ? CHECK_ICON : visible > 0 ? MINUS_ICON : "";
      }
      const count = chip.querySelector(".home-group-count");
      if (count) {
        count.textContent = `${visible}/${rails.length}`;
      }
    }

    /**
     * @param {string} name
     * @param {boolean} show
     */
    function setStream(name, show) {
      for (const rail of streamRails(name)) {
        rail.hidden = !show;
      }
      const chip = backdrop.querySelector(`[data-mock-home-chip="${name}"]`);
      if (chip instanceof HTMLElement) {
        syncChip(chip);
      }
      announce(
        name === DRAFTS_RAIL_ID
          ? `Drafts rail ${show ? "shown" : "hidden"}.`
          : `${name} streams ${show ? "shown" : "hidden"}.`,
      );
    }

    // Anything short of fully shown reveals the whole stream; a fully-shown
    // one hides — the app's tristate checkbox resolving to all-on.
    /** @param {string} name */
    function toggleStream(name) {
      const rails = streamRails(name);
      const allVisible = rails.length > 0 && rails.every((rail) => !rail.hidden);
      setStream(name, !allVisible);
    }

    /** @param {HTMLElement} rail */
    function railTitle(rail) {
      return rail.querySelector(".home-rail-title")?.textContent?.trim() || "pane";
    }

    /**
     * The count pill follows the cards, the way the sidebar tab mirrors the
     * composer's queue. Drafts count everything; agent rails exclude the
     * settled and current turns that only give a column its history.
     * @param {HTMLElement} rail
     */
    function syncRailCount(rail) {
      const scroll = rail.querySelector(".home-rail-scroll");
      const head = rail.querySelector(".home-rail-head");
      if (!(scroll instanceof HTMLElement) || !(head instanceof HTMLElement)) {
        return;
      }
      const isDrafts = rail.dataset.mockRail === DRAFTS_RAIL_ID;
      const count = [...scroll.querySelectorAll(".queued-turn")].filter(
        (card) =>
          isDrafts ||
          !(card.classList.contains("is-current") || card.classList.contains("is-past")),
      ).length;
      let pill = head.querySelector(".home-rail-count");
      if (count === 0) {
        pill?.remove();
        return;
      }
      if (!(pill instanceof HTMLElement)) {
        pill = document.createElement("span");
        pill.className = "home-rail-count";
        const paused = head.querySelector(".home-rail-paused");
        if (paused) {
          head.insertBefore(pill, paused);
        } else {
          head.append(pill);
        }
      }
      pill.textContent = isDrafts ? String(count) : `${count} queued`;
    }

    /**
     * @param {HTMLElement} rail
     * @param {string} text
     */
    function addRailCard(rail, text) {
      const scroll = rail.querySelector(".home-rail-scroll");
      if (!(scroll instanceof HTMLElement)) {
        return;
      }
      const isDrafts = rail.dataset.mockRail === DRAFTS_RAIL_ID;
      const card = document.createElement("div");
      card.className = "queued-turn";
      const body = document.createElement("div");
      body.className = "queued-turn-text";
      body.textContent = text;
      const actions = document.createElement("div");
      actions.className = "queued-turn-actions";
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "control-button home-rail-turn-remove";
      remove.setAttribute(
        "aria-label",
        `${isDrafts ? "Delete draft" : "Remove queued turn"}: ${text}`,
      );
      remove.innerHTML = X_ICON;
      remove.addEventListener("click", () => {
        card.remove();
        syncRailCount(rail);
        announce(isDrafts ? "Draft deleted." : "Removed.");
      });
      actions.append(remove);
      card.append(body, actions);
      scroll.append(card);
      scroll.scrollTop = scroll.scrollHeight;
      syncRailCount(rail);
      announce(isDrafts ? "Draft saved." : `Queued on ${railTitle(rail)}.`);
    }

    // aria-modal semantics: while the map is up, nothing behind it takes
    // focus or clicks. Only children this feature inerted are restored, so
    // a pane the panes feature had already collapsed stays collapsed.
    /** @type {Element[]} */
    let inertedByMap = [];

    /** @param {boolean} open */
    function setOpen(open) {
      closeMenu(false);
      backdrop.hidden = !open;
      if (open) {
        for (const child of mockup.children) {
          if (child !== backdrop && !child.hasAttribute("inert")) {
            child.setAttribute("inert", "");
            inertedByMap.push(child);
          }
        }
        dialog.focus();
        announce("Terminal map open: every pane's queue, side by side.");
      } else {
        for (const child of inertedByMap) {
          child.removeAttribute("inert");
        }
        inertedByMap = [];
        openTrigger.focus();
        announce("Terminal map closed.");
      }
    }

    openTrigger.addEventListener("click", () => setOpen(backdrop.hidden));

    // The app dismisses on a click that lands on the scrim itself.
    backdrop.addEventListener("mousedown", (event) => {
      if (event.target === backdrop) {
        setOpen(false);
      }
    });

    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") {
        return;
      }
      if (openMenu && openCaret) {
        closeMenu(true);
        return;
      }
      if (!backdrop.hidden) {
        setOpen(false);
      }
    });

    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (
        openMenu &&
        openCaret &&
        target instanceof Element &&
        !openMenu.contains(target) &&
        !openCaret.contains(target)
      ) {
        closeMenu(false);
      }
    });

    for (const node of [...backdrop.querySelectorAll('[data-mock-action="toggle-home-stream"]')]) {
      const chip = node.closest(".home-group-chip");
      const name = node instanceof HTMLElement ? node.dataset.mockStream ?? "" : "";
      const toggle = promote(node, "button");
      if (!toggle || !(chip instanceof HTMLElement) || !name) {
        continue;
      }
      toggle.setAttribute(
        "aria-label",
        name === DRAFTS_RAIL_ID
          ? "Show or hide the drafts rail"
          : `Show or hide the ${name} streams`,
      );
      toggle.addEventListener("click", () => toggleStream(name));
    }

    for (const node of [...backdrop.querySelectorAll("[data-mock-home-caret]")]) {
      const chip = node.closest(".home-group-chip");
      const menu = chip ? chip.querySelector("[data-mock-home-menu]") : null;
      if (!(node instanceof HTMLElement) || !(menu instanceof HTMLElement)) {
        continue;
      }
      const caret = promote(node, "button");
      if (!caret) {
        continue;
      }
      caret.addEventListener("click", () => {
        const opening = menu.hidden;
        closeMenu(false);
        if (opening) {
          // Unhide before measuring: a menu that would run past the dialog's
          // right edge anchors to the chip's right corner instead — the same
          // clamp the app applies to its portaled menus.
          menu.classList.remove("is-flipped");
          menu.hidden = false;
          const dialogRect = dialog.getBoundingClientRect();
          const menuRect = menu.getBoundingClientRect();
          if (menuRect.right > dialogRect.right - 8) {
            menu.classList.add("is-flipped");
          }
          openMenu = menu;
          openCaret = caret;
        } else {
          menu.hidden = true;
        }
        caret.classList.toggle("is-open", opening);
        caret.setAttribute("aria-expanded", String(opening));
      });
      for (const itemNode of [...menu.querySelectorAll("[data-mock-home-menu-item]")]) {
        const item = promote(itemNode, "button");
        if (!(item instanceof HTMLElement)) {
          continue;
        }
        item.addEventListener("click", () => {
          const sessionId = item.dataset.mockHomeMenuItem ?? "";
          const shown = item.classList.toggle("is-shown");
          item.setAttribute("aria-checked", String(shown));
          const box = item.querySelector(".home-group-checkbox");
          if (box) {
            box.innerHTML = shown ? CHECK_ICON : "";
          }
          const rail = backdrop.querySelector(`[data-mock-rail="${sessionId}"]`);
          if (rail instanceof HTMLElement) {
            rail.hidden = !shown;
          }
          if (chip instanceof HTMLElement) {
            syncChip(chip);
          }
        });
      }
    }

    // A rail head hands you to its session: the map closes and the matching
    // sidebar tab — already a real control once the sessions feature ran —
    // does the switching, exactly as closing Home focuses the pane.
    for (const node of [...backdrop.querySelectorAll("[data-mock-open-session]")]) {
      const sessionId = node instanceof HTMLElement ? node.dataset.mockOpenSession ?? "" : "";
      const head = promote(node, "button");
      if (!head || !sessionId) {
        continue;
      }
      head.setAttribute(
        "aria-label",
        `Open session: ${head.querySelector(".home-rail-title")?.textContent?.trim() || sessionId}`,
      );
      head.addEventListener("click", () => {
        setOpen(false);
        const tab = mockup.querySelector(`[data-mock-session-tab="${sessionId}"] .pane-tab`);
        if (tab instanceof HTMLButtonElement) {
          tab.click();
        }
      });
    }

    // Server-rendered remove buttons become real, one per shipped card.
    for (const node of [
      ...backdrop.querySelectorAll("[data-mock-remove-queued], [data-mock-remove-draft]"),
    ]) {
      const isDrafts = node.hasAttribute("data-mock-remove-draft");
      const rail = node.closest("[data-mock-rail]");
      const button = promote(node, "button");
      if (!button || !(rail instanceof HTMLElement)) {
        continue;
      }
      button.addEventListener("click", () => {
        button.closest(".queued-turn")?.remove();
        syncRailCount(rail);
        announce(isDrafts ? "Draft deleted." : "Removed.");
      });
    }

    // Per-rail ghost composers: Enter queues onto that column, Shift+Enter
    // adds a line, exactly as the app's Home composers behave.
    for (const field of [...backdrop.querySelectorAll(".mock-rail-composer")]) {
      if (!(field instanceof HTMLElement)) {
        continue;
      }
      const rail = field.closest("[data-mock-rail]");
      if (!(rail instanceof HTMLElement)) {
        continue;
      }
      const isDrafts = rail.dataset.mockRail === DRAFTS_RAIL_ID;
      const textarea = document.createElement("textarea");
      textarea.className = field.className;
      textarea.rows = 1;
      textarea.placeholder = field.textContent || "";
      textarea.setAttribute(
        "aria-label",
        isDrafts ? "New draft" : `Queue a follow-up for ${railTitle(rail)}`,
      );
      field.replaceWith(textarea);
      textarea.addEventListener("input", () => {
        textarea.style.height = "auto";
        textarea.style.height = `${Math.min(textarea.scrollHeight, 120)}px`;
      });
      textarea.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" || event.shiftKey) {
          return;
        }
        event.preventDefault();
        const text = textarea.value.trim();
        if (!text) {
          return;
        }
        addRailCard(rail, text);
        textarea.value = "";
        textarea.style.height = "auto";
      });
    }

    return {};
  }

  // ---------------------------------------------------------------- panels

  const PANEL_LABELS = {
    "prompt-library": ["Show saved prompts", "Hide saved prompts"],
    journal: ["Journal", "Hide the journal"],
    "queue-split": ["Split the queue out of the transcript", "Float the queue over the transcript"],
    browser: ["Show the browser", "Hide the browser"],
    artifacts: ["Show the artifact tray", "Hide the artifact tray"],
  };
  const POPOVER_PANELS = ["prompt-library", "journal", "artifacts"];

  function createPanels() {
    const composerField = () => mockup.querySelector(".mock-textarea");
    /** @type {Map<string, HTMLElement>} */
    const panels = new Map();
    for (const panel of mockup.querySelectorAll("[data-mock-panel]")) {
      if (panel instanceof HTMLElement && panel.dataset.mockPanel) {
        panels.set(panel.dataset.mockPanel, panel);
      }
    }
    const splitTarget = mockup.querySelector(".turn-sidebar-input");
    if (panels.size === 0 && !splitTarget) {
      return null;
    }

    /** @type {Map<string, HTMLElement[]>} */
    const triggers = new Map();
    /** @param {string} name */
    const isOpen = (name) => {
      const panel = panels.get(name);
      return name === "queue-split"
        ? Boolean(splitTarget && splitTarget.classList.contains("is-split"))
        : Boolean(panel && !panel.hidden);
    };

    /**
     * @param {string} name
     * @param {boolean} open
     */
    function setPanel(name, open) {
      if (name === "queue-split") {
        if (splitTarget) {
          splitTarget.classList.toggle("is-split", open);
          syncQueueEmptyState();
        }
      } else {
        const panel = panels.get(name);
        if (panel) {
          panel.hidden = !open;
          panel.toggleAttribute("inert", !open);
        }
      }
      const labels = PANEL_LABELS[/** @type {keyof typeof PANEL_LABELS} */ (name)];
      for (const trigger of triggers.get(name) ?? []) {
        trigger.classList.toggle("is-active", open);
        trigger.setAttribute("aria-pressed", String(open));
        if (labels) {
          trigger.setAttribute("aria-label", open ? labels[1] : labels[0]);
          trigger.setAttribute("title", open ? labels[1] : labels[0]);
        }
      }
    }

    /** @param {"default" | "visualization"} kind */
    function showBrowserPage(kind) {
      const page = mockup.querySelector(".browser-overlay-page");
      if (page instanceof HTMLElement) {
        page.dataset.mockBrowserKind = kind;
      }
      for (const preview of mockup.querySelectorAll("[data-mock-browser-page]")) {
        if (preview instanceof HTMLElement) {
          preview.hidden = preview.dataset.mockBrowserPage !== kind;
        }
      }
    }

    // Only one popover at a time, the way the app's menus behave.
    /** @param {string} name */
    function togglePanel(name) {
      const open = !isOpen(name);
      if (open) {
        for (const other of POPOVER_PANELS) {
          if (other !== name) {
            setPanel(other, false);
          }
        }
      }
      setPanel(name, open);
    }

    // A split queue with nothing in it needs to say so rather than show a gap.
    function syncQueueEmptyState() {
      const stack = mockup.querySelector(".queued-turn-stack");
      if (!splitTarget) {
        return;
      }
      let empty = splitTarget.querySelector(".queue-empty-state");
      const needed =
        splitTarget.classList.contains("is-split") && (!stack || stack.childElementCount === 0);
      if (needed && !empty) {
        empty = document.createElement("p");
        empty.className = "queue-empty-state";
        empty.textContent = "Nothing queued.";
        (stack ?? splitTarget).before(empty);
      } else if (!needed && empty) {
        empty.remove();
      }
    }

    for (const node of [...mockup.querySelectorAll("[data-mock-action]")]) {
      const action = node instanceof HTMLElement ? node.dataset.mockAction ?? "" : "";
      if (!(action in PANEL_LABELS)) {
        continue;
      }
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      // A close button sitting inside the panel it dismisses is not a toggle:
      // it only ever closes, so it says so and keeps no pressed state.
      const inside = button.closest(`[data-mock-panel="${action}"]`) !== null;
      const labels = PANEL_LABELS[/** @type {keyof typeof PANEL_LABELS} */ (action)];
      if (inside) {
        const label = action === "browser" ? "Close the browser" : "Close the artifact tray";
        button.setAttribute("aria-label", label);
        button.setAttribute("title", label);
        button.addEventListener("click", () => setPanel(action, false));
        continue;
      }
      const existing = triggers.get(action) ?? [];
      existing.push(button);
      triggers.set(action, existing);
      const open = isOpen(action);
      button.classList.toggle("is-active", open);
      button.setAttribute("aria-pressed", String(open));
      button.setAttribute("aria-label", open ? labels[1] : labels[0]);
      button.setAttribute("title", open ? labels[1] : labels[0]);
      button.addEventListener("click", () => togglePanel(action));
    }

    // Saved prompts drop into the composer, which is where they are useful.
    for (const node of [...mockup.querySelectorAll("[data-mock-prompt]")]) {
      const text = node instanceof HTMLElement ? node.dataset.mockPrompt ?? "" : "";
      const button = promote(node, "button");
      if (!button || !text) {
        continue;
      }
      button.addEventListener("click", () => {
        const field = composerField();
        setPanel("prompt-library", false);
        if (field instanceof HTMLTextAreaElement) {
          field.value = text;
          field.dispatchEvent(new Event("input", { bubbles: true }));
          field.focus();
        }
        announce("Prompt inserted into the composer.");
      });
    }

    // Filtering for real: the field would otherwise promise a search it does
    // not perform.
    const search = mockup.querySelector(".prompt-library-search");
    const emptyNote = mockup.querySelector(".prompt-library-empty");
    if (search instanceof HTMLInputElement) {
      search.addEventListener("input", () => {
        const needle = search.value.trim().toLowerCase();
        let shown = 0;
        for (const item of mockup.querySelectorAll(".prompt-library-item")) {
          const matches = (item.textContent ?? "").toLowerCase().includes(needle);
          item.toggleAttribute("hidden", !matches);
          shown += matches ? 1 : 0;
        }
        if (emptyNote instanceof HTMLElement) {
          emptyNote.hidden = shown > 0;
        }
      });
    }

    // Opening an artifact hands it to the browser overlay while leaving the
    // tray available for opening another file.
    for (const node of [...mockup.querySelectorAll("[data-mock-artifact]")]) {
      const name = node instanceof HTMLElement ? node.dataset.mockArtifact ?? "" : "";
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      button.setAttribute("aria-label", `Open ${name} in the browser`);
      button.addEventListener("click", () => {
        const address = mockup.querySelector("[data-mock-browser-url]");
        if (address) {
          // Keep whatever origin the markup rendered rather than hardcoding one
          // here; the address is the data file's to choose.
          const origin = (address.textContent || "").split("/")[0];
          address.textContent = `${origin}/${name}`;
        }
        showBrowserPage("default");
        setPanel("browser", true);
        announce(`Opened ${name} in the browser.`);
      });
    }

    // Visualization attachments are transcript-native controls, but v1 hands
    // their local HTML fragment to the same sandboxed browser overlay as the
    // desktop app. Keep them separate from the artifact tray: opening one does
    // not claim that qmux registered another artifact.
    for (const node of [...mockup.querySelectorAll("[data-mock-visualization]")]) {
      const file = node instanceof HTMLElement ? node.dataset.mockVisualization ?? "" : "";
      const title =
        node instanceof HTMLElement
          ? node.dataset.mockVisualizationTitle ?? "Visualization"
          : "Visualization";
      const button = promote(node, "button");
      if (!button || !file) {
        continue;
      }
      button.setAttribute("aria-label", `Open visualization: ${title}`);
      button.setAttribute("title", `Open ${title}`);
      button.addEventListener("click", () => {
        const address = mockup.querySelector("[data-mock-browser-url]");
        if (address) {
          const origin = (address.textContent || "").split("/")[0];
          address.textContent = `${origin}/designs/${file}`;
        }
        showBrowserPage("visualization");
        setPanel("browser", true);
        announce(`Opened ${title} in the browser.`);
      });
    }

    // The journal is a feed of current and past notifications. Marking read is
    // immediate; clearing asks first. The checkbox only mutes overlay toasts,
    // which the replica does not render — the control still has to work.
    const logUnread = mockup.querySelector("[data-mock-log-unread]");
    const logEmpty = mockup.querySelector(".notification-journal-empty");
    const logMarkAllNode = mockup.querySelector("[data-mock-log-mark-all]");
    const logMarkAll = logMarkAllNode ? promote(logMarkAllNode, "button") : null;
    const logShowNode = mockup.querySelector("[data-mock-log-show]");
    const logShow = logShowNode ? promote(logShowNode, "button") : null;
    const logConfirm = mockup.querySelector("[data-mock-log-confirm]");
    const logConfirmMessage = mockup.querySelector("[data-mock-log-confirm-message]");
    const logConfirmCancelNode = mockup.querySelector("[data-mock-log-confirm-cancel]");
    const logConfirmOkNode = mockup.querySelector("[data-mock-log-confirm-ok]");
    const logConfirmCancel = logConfirmCancelNode ? promote(logConfirmCancelNode, "button") : null;
    const logConfirmOk = logConfirmOkNode ? promote(logConfirmOkNode, "button") : null;
    /** @type {HTMLElement | null} */
    let pendingClear = null;

    function logItems() {
      return /** @type {HTMLElement[]} */ (
        [...mockup.querySelectorAll("[data-mock-log-item]")].filter(
          (item) => item instanceof HTMLElement,
        )
      );
    }

    function syncJournalChrome() {
      const unread = logItems().some((item) => item.classList.contains("is-unread"));
      if (logUnread instanceof HTMLElement) {
        logUnread.hidden = !unread;
      }
      if (logMarkAll instanceof HTMLButtonElement) {
        logMarkAll.disabled = !unread;
      }
      if (logEmpty instanceof HTMLElement) {
        logEmpty.hidden = logItems().length > 0;
      }
    }

    /** @param {HTMLElement} item */
    function markLogItemRead(item) {
      item.classList.remove("is-unread");
      item.querySelector("[data-mock-log-dot]")?.remove();
      item.querySelector("[data-mock-log-read]")?.remove();
      syncJournalChrome();
    }

    /** @param {boolean} open */
    function setLogConfirmOpen(open) {
      if (!(logConfirm instanceof HTMLElement)) {
        return;
      }
      logConfirm.hidden = !open;
      logConfirm.toggleAttribute("inert", !open);
      if (!open) {
        pendingClear = null;
      }
    }

    if (logShow instanceof HTMLElement) {
      logShow.addEventListener("click", () => {
        const on = logShow.getAttribute("aria-checked") !== "true";
        logShow.setAttribute("aria-checked", String(on));
        logShow.classList.toggle("is-off", !on);
        const check = logShow.querySelector(".lucide");
        if (check instanceof SVGElement) {
          check.toggleAttribute("hidden", !on);
        }
        announce(on ? "Overlay toasts on." : "Overlay toasts off.");
      });
    }

    if (logMarkAll instanceof HTMLElement) {
      logMarkAll.addEventListener("click", () => {
        for (const item of logItems()) {
          markLogItemRead(item);
        }
        announce("All notifications marked read.");
      });
    }

    for (const node of [...mockup.querySelectorAll("[data-mock-log-read]")]) {
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      button.addEventListener("click", () => {
        const item = button.closest("[data-mock-log-item]");
        if (item instanceof HTMLElement) {
          markLogItemRead(item);
        }
      });
    }

    for (const node of [...mockup.querySelectorAll("[data-mock-log-open]")]) {
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      button.addEventListener("click", () => {
        const item = button.closest("[data-mock-log-item]");
        if (item instanceof HTMLElement) {
          markLogItemRead(item);
        }
        setPanel("journal", false);
        announce("Opened the pane.");
      });
    }

    for (const node of [...mockup.querySelectorAll("[data-mock-log-clear]")]) {
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      button.addEventListener("click", () => {
        const item = button.closest("[data-mock-log-item]");
        if (!(item instanceof HTMLElement) || !(logConfirmMessage instanceof HTMLElement)) {
          return;
        }
        const title = item.querySelector("strong")?.textContent ?? "this notification";
        logConfirmMessage.textContent = `Clear “${title}”? This cannot be undone.`;
        pendingClear = item;
        setLogConfirmOpen(true);
      });
    }

    logConfirmCancel?.addEventListener("click", () => setLogConfirmOpen(false));
    logConfirmOk?.addEventListener("click", () => {
      pendingClear?.remove();
      setLogConfirmOpen(false);
      syncJournalChrome();
      announce("Notification cleared.");
    });
    if (logConfirm instanceof HTMLElement) {
      logConfirm.addEventListener("mousedown", (event) => {
        if (event.target === logConfirm) {
          setLogConfirmOpen(false);
        }
      });
    }
    syncJournalChrome();

    // The empty state has to follow the queue, which is another feature's
    // business — so watch the stack rather than reaching into it.
    const stack = mockup.querySelector(".queued-turn-stack");
    if (stack && typeof MutationObserver === "function") {
      new MutationObserver(syncQueueEmptyState).observe(stack, { childList: true });
    }

    // A popover closes on Escape and on a click outside it.
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") {
        return;
      }
      if (logConfirm instanceof HTMLElement && !logConfirm.hidden) {
        setLogConfirmOpen(false);
        return;
      }
      for (const name of POPOVER_PANELS) {
        setPanel(name, false);
      }
    });
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (!(target instanceof Element)) {
        return;
      }
      if (logConfirm instanceof HTMLElement && !logConfirm.hidden) {
        return;
      }
      for (const name of POPOVER_PANELS) {
        const panel = panels.get(name);
        if (!panel || panel.hidden || panel.contains(target)) {
          continue;
        }
        if (target.closest(`[data-mock-action="${name}"]`)) {
          continue;
        }
        setPanel(name, false);
      }
    });

    return { syncQueueEmptyState };
  }

  // --------------------------------------------------------------- images

  function createImageLightbox() {
    const foundLightbox = mockup.querySelector("[data-mock-image-lightbox]");
    const foundImage = foundLightbox?.querySelector("[data-mock-image-full]");
    const closeNode = foundLightbox?.querySelector("[data-mock-image-close]");
    const thumbnailNodes = [...mockup.querySelectorAll("[data-mock-image-src]")];
    if (
      !(foundLightbox instanceof HTMLElement) ||
      !(foundImage instanceof HTMLImageElement) ||
      !closeNode ||
      thumbnailNodes.length === 0
    ) {
      return null;
    }

    const lightbox = foundLightbox;
    const fullImage = foundImage;
    const promotedCloseButton = promote(closeNode, "button");
    if (!promotedCloseButton) {
      return null;
    }
    const closeButton = /** @type {HTMLElement} */ (promotedCloseButton);
    closeButton.setAttribute("aria-label", "Close image");
    closeButton.setAttribute("title", "Close image");

    /** @type {HTMLElement | null} */
    let opener = null;
    /** @type {Element[]} */
    let inertedByLightbox = [];

    function close() {
      if (lightbox.hidden) {
        return;
      }
      lightbox.hidden = true;
      for (const child of inertedByLightbox) {
        child.removeAttribute("inert");
      }
      inertedByLightbox = [];
      opener?.focus();
      opener = null;
      announce("Image closed.");
    }

    /** @param {HTMLElement} trigger */
    function open(trigger) {
      const src = trigger.dataset.mockImageSrc ?? "";
      const alt = trigger.dataset.mockImageAlt ?? "Transcript image";
      if (!src) {
        return;
      }
      opener = trigger;
      fullImage.src = src;
      fullImage.alt = alt;
      lightbox.setAttribute("aria-label", alt);
      lightbox.hidden = false;
      for (const child of mockup.children) {
        if (child !== lightbox && !child.hasAttribute("inert")) {
          child.setAttribute("inert", "");
          inertedByLightbox.push(child);
        }
      }
      closeButton.focus();
      announce("Image opened at full resolution.");
    }

    for (const node of thumbnailNodes) {
      const button = promote(node, "button");
      if (!button) {
        continue;
      }
      const alt = button.dataset.mockImageAlt ?? "Transcript image";
      button.setAttribute("aria-label", `Open image: ${alt}`);
      button.setAttribute("title", "Open full-size image");
      button.setAttribute("aria-haspopup", "dialog");
      button.addEventListener("click", () => open(button));
    }

    closeButton.addEventListener("click", close);
    lightbox.addEventListener("click", (event) => {
      if (event.target === lightbox) {
        close();
      }
    });
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && !lightbox.hidden) {
        close();
      }
    });

    return {};
  }

  // ---------------------------------------------------------------- groups

  function createGroups() {
    const list = mockup.querySelector(".pane-list");
    if (!list) {
      return null;
    }
    const groups = [...list.querySelectorAll(".pane-group")];
    if (groups.length === 0) {
      return null;
    }

    for (const group of groups) {
      const name = group.querySelector(".pane-group-name");
      const label = name ? name.textContent || "project" : "project";
      const toggle = promote(group.querySelector(".pane-group-collapse-button"), "button");
      if (!toggle) {
        continue;
      }
      toggle.setAttribute("aria-label", `${label} agents`);
      toggle.setAttribute("aria-expanded", String(!group.classList.contains("is-collapsed")));
      const menu = group.querySelector(".pane-group-menu-button");
      if (menu) {
        menu.setAttribute("aria-hidden", "true");
      }
      const header = group.querySelector(".pane-group-header");
      if (header instanceof HTMLElement) {
        header.classList.add("mock-live");
      }
    }

    // One delegated listener: a click anywhere on a group header toggles it, and
    // the toggle button's own click bubbles into the same path.
    list.addEventListener("click", (event) => {
      const target = event.target;
      if (!(target instanceof Element)) {
        return;
      }
      const header = target.closest(".pane-group-header");
      const group = header ? header.closest(".pane-group") : null;
      if (!group) {
        return;
      }
      const collapsed = group.classList.toggle("is-collapsed");
      const toggle = group.querySelector(".pane-group-collapse-button");
      if (toggle) {
        toggle.setAttribute("aria-expanded", String(!collapsed));
      }
    });
    return {};
  }

  // --------------------------------------------------------------- sessions

  /**
   * Each visible sidebar row owns a complete terminal/transcript pair in the
   * server-rendered markup. Switching is only a hidden-state toggle, so it is
   * instant and still leaves a useful default session when JavaScript is off.
   * @param {ReturnType<typeof createReplay>} replay
   */
  function createSessions(replay) {
    const rows = [...mockup.querySelectorAll("[data-mock-session-tab]")];
    const views = [...mockup.querySelectorAll("[data-mock-session-view]")];
    if (rows.length === 0 || views.length === 0) {
      return null;
    }

    /** @param {Element} row */
    function select(row) {
      if (row.classList.contains("is-selected")) {
        return;
      }
      replay?.skip();
      const sessionId =
        row instanceof HTMLElement ? row.dataset.mockSessionTab ?? "" : "";
      if (!sessionId) {
        return;
      }

      for (const candidate of rows) {
        const selected = candidate === row;
        candidate.classList.toggle("is-selected", selected);
        const button = candidate.querySelector(".pane-tab");
        button?.setAttribute("aria-pressed", String(selected));
      }
      for (const view of views) {
        if (view instanceof HTMLElement) {
          view.hidden = view.dataset.mockSessionView !== sessionId;
        }
      }
      const sessionLabel = mockup.querySelector(".turn-pane-session");
      if (sessionLabel && row instanceof HTMLElement && row.dataset.mockSessionLabel) {
        sessionLabel.textContent = row.dataset.mockSessionLabel;
      }
      const activeGroup = row.closest(".pane-group");
      for (const group of mockup.querySelectorAll(".pane-group")) {
        group.classList.toggle("is-active-group", group === activeGroup);
      }

      // The initial artifact tray describes the default qmux session. Close
      // it on a session change instead of showing those files beside another
      // project's transcript.
      const artifactTray = mockup.querySelector('[data-mock-panel="artifacts"]');
      if (artifactTray instanceof HTMLElement) {
        artifactTray.hidden = true;
        artifactTray.setAttribute("inert", "");
      }
      for (const trigger of mockup.querySelectorAll('[data-mock-action="artifacts"]')) {
        if (trigger.closest('[data-mock-panel="artifacts"]')) {
          continue;
        }
        trigger.classList.remove("is-active");
        trigger.setAttribute("aria-pressed", "false");
        trigger.setAttribute("aria-label", "Show the artifact tray");
        trigger.setAttribute("title", "Show the artifact tray");
      }

      mockup.dispatchEvent(new CustomEvent("mock-session-change", { detail: { sessionId } }));
      const working = row instanceof HTMLElement && row.dataset.mockSessionStatus === "active";
      if (working && !reducedMotion.matches) {
        void replay?.play();
      } else {
        replay?.skip();
      }
      const title = row.querySelector(".pane-tab-title")?.textContent?.trim() || sessionId;
      announce(`Opened ${title}.`);
    }

    for (const row of rows) {
      const title = row.querySelector(".pane-tab-title")?.textContent?.trim() || "session";
      const button = promote(row.querySelector(".pane-tab"), "button");
      if (!button) {
        continue;
      }
      button.setAttribute("aria-label", `Open session: ${title}`);
      button.setAttribute("aria-pressed", String(row.classList.contains("is-selected")));
      button.addEventListener("click", () => select(row));
    }
    return {};
  }

  // ------------------------------------------------------------------ menus

  function createMenus() {
    /** @type {HTMLElement | null} */
    let activeMenu = null;
    /** @type {HTMLElement | null} */
    let activeTrigger = null;

    /**
     * Pin an open menu to its trigger the way the app's placePanePopover does:
     * right-aligned so it grows toward the pane's center, opening above the
     * composer trigger or below a message's, flipping to whichever side has
     * room, and clamped inside the turn pane so no edge ever cuts it.
     * @param {HTMLElement} menu
     * @param {HTMLElement} trigger
     */
    function placeMenu(menu, trigger) {
      const composer = menu.dataset.mockMenu === "composer";
      const gap = composer ? 5 : 4;
      const margin = 8;
      const pane = trigger.closest(".turn-pane");
      const bounds = (pane ?? mockup).getBoundingClientRect();
      const origin = mockup.getBoundingClientRect();
      const triggerRect = trigger.getBoundingClientRect();
      const width = menu.offsetWidth;
      const height = menu.offsetHeight;

      let left = Math.max(
        bounds.left + margin,
        Math.min(triggerRect.right - width, bounds.right - margin - width),
      );

      const roomAbove = Math.max(0, triggerRect.top - gap - bounds.top);
      const roomBelow = Math.max(0, bounds.bottom - (triggerRect.bottom + gap));
      let above = composer;
      if (above && height > roomAbove && roomBelow > roomAbove) {
        above = false;
      } else if (!above && height > roomBelow && roomAbove > roomBelow) {
        above = true;
      }

      const maxHeight = above ? roomAbove : roomBelow;
      const top = above
        ? triggerRect.top - gap - Math.min(height, maxHeight)
        : triggerRect.bottom + gap;

      menu.style.left = `${Math.round(left - origin.left)}px`;
      menu.style.top = `${Math.round(top - origin.top)}px`;
      menu.style.maxHeight = `${Math.floor(maxHeight)}px`;
      menu.style.maxWidth = `${Math.floor(bounds.right - bounds.left - margin * 2)}px`;
    }

    /**
     * @param {HTMLElement} menu
     * @param {HTMLElement} trigger
     * @param {boolean} open
     */
    function setOpen(menu, trigger, open) {
      if (open && activeMenu && activeMenu !== menu && activeTrigger) {
        setOpen(activeMenu, activeTrigger, false);
      }
      menu.hidden = !open;
      menu.toggleAttribute("inert", !open);
      trigger.classList.toggle("is-active", open);
      trigger.setAttribute("aria-expanded", String(open));
      if (open) {
        placeMenu(menu, trigger);
        activeMenu = menu;
        activeTrigger = trigger;
      } else if (activeMenu === menu) {
        activeMenu = null;
        activeTrigger = null;
      }
    }

    let triggerCount = 0;
    for (const node of [...mockup.querySelectorAll("[data-mock-menu-trigger]")]) {
      const parent = node.parentElement;
      const menu = parent ? parent.querySelector("[data-mock-menu]") : null;
      if (!(menu instanceof HTMLElement)) {
        continue;
      }
      const trigger = promote(node, "button");
      if (!trigger) {
        continue;
      }
      triggerCount += 1;
      const label = menu.dataset.mockMenu === "composer" ? "More actions" : "Message options";
      trigger.setAttribute("aria-label", label);
      trigger.setAttribute("title", label);
      trigger.setAttribute("aria-haspopup", "menu");
      trigger.setAttribute("aria-expanded", "false");
      menu.hidden = true;
      menu.setAttribute("inert", "");
      // Portal out of the panes: both the composer rail and the scrolling
      // transcript clip their overflow, and a menu anchored inside them opens
      // with its far edge cut off. From the mockup root, placeMenu owns the
      // geometry instead.
      mockup.append(menu);
      trigger.addEventListener("click", () => setOpen(menu, trigger, menu.hidden));

      for (const itemNode of [...menu.querySelectorAll("[data-mock-menu-item]")]) {
        const item = promote(itemNode, "button");
        if (!(item instanceof HTMLButtonElement)) {
          continue;
        }
        if (item.getAttribute("aria-disabled") === "true") {
          item.disabled = true;
          continue;
        }
        // A selection dismisses the simulated menu but deliberately performs no
        // clipboard, publishing, title, prompt, or session operation.
        item.addEventListener("click", () => {
          setOpen(menu, trigger, false);
          trigger.focus();
        });
      }
    }

    if (triggerCount === 0) {
      return null;
    }

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && activeMenu && activeTrigger) {
        const trigger = activeTrigger;
        setOpen(activeMenu, trigger, false);
        trigger.focus();
      }
    });
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (
        activeMenu &&
        activeTrigger &&
        target instanceof Node &&
        !activeMenu.contains(target) &&
        !activeTrigger.contains(target)
      ) {
        setOpen(activeMenu, activeTrigger, false);
      }
    });

    return {};
  }

  // ---------------------------------------------------------- sidebar-menus

  function createSidebarMenus() {
    const paneList = mockup.querySelector(".pane-list");
    const menus = [
      ...mockup.querySelectorAll("[data-mock-tab-menu], [data-mock-group-menu]"),
    ].filter((menu) => menu instanceof HTMLElement);
    if (!paneList || menus.length === 0) {
      return null;
    }

    // Group sections by their rendered name, so a group menu can find the
    // section it describes (the collapse item reads and flips its state).
    /** @type {Map<string, Element>} */
    const groupSectionByName = new Map();
    for (const section of mockup.querySelectorAll(".pane-group")) {
      const name = section.querySelector(".pane-group-name")?.textContent?.trim();
      if (name && !groupSectionByName.has(name)) {
        groupSectionByName.set(name, section);
      }
    }

    /** @type {HTMLElement | null} */
    let openMenu = null;
    /** @type {HTMLElement | null} */
    let openTrigger = null;

    /** @param {boolean} refocus */
    function close(refocus) {
      if (openMenu) {
        openMenu.hidden = true;
        openMenu = null;
      }
      if (openTrigger) {
        openTrigger.setAttribute("aria-expanded", "false");
        if (refocus) {
          openTrigger.focus();
        }
        openTrigger = null;
      }
    }

    /**
     * The collapse item mirrors the group's live state — the app keeps its
     * menu open across a collapse and flips the item in place.
     * @param {HTMLElement} menu
     */
    function syncCollapseItem(menu) {
      const name = menu.dataset.mockGroupMenu ?? "";
      const item = menu.querySelector("[data-mock-menu-collapse]");
      const section = groupSectionByName.get(name);
      if (!(item instanceof HTMLElement) || !(section instanceof Element)) {
        return;
      }
      const collapsed = section.classList.contains("is-collapsed");
      const show = (/** @type {string} */ selector, /** @type {boolean} */ visible) => {
        const node = item.querySelector(selector);
        if (node instanceof HTMLElement) {
          node.hidden = !visible;
        }
      };
      show(".mock-menu-icon-expand", collapsed);
      show(".mock-menu-icon-collapse", !collapsed);
      show(".mock-menu-label-expand", collapsed);
      show(".mock-menu-label-collapse", !collapsed);
      show(".context-menu-shortcut-options", collapsed);
      show(".mock-menu-keycap", !collapsed);
    }

    /**
     * Positions at window coordinates already relative to the replica, then
     * clamps inside it — the app's clampContextMenuToViewport.
     * @param {HTMLElement} menu
     * @param {number} x
     * @param {number} y
     * @param {HTMLElement | null} trigger
     */
    function open(menu, x, y, trigger) {
      close(openTrigger != null);
      syncCollapseItem(menu);
      menu.hidden = false;
      const width = mockup.clientWidth;
      const height = mockup.clientHeight;
      menu.style.left = `${Math.max(8, Math.min(x, width - menu.offsetWidth - 8))}px`;
      menu.style.top = `${Math.max(8, Math.min(y, height - menu.offsetHeight - 8))}px`;
      openMenu = menu;
      openTrigger = trigger;
    }

    // Right-click: a tab opens its own details menu; anywhere else in a group
    // opens that group's menu. The tab row wins, as the app's stopPropagation
    // makes it.
    paneList.addEventListener("contextmenu", (event) => {
      const target = event.target;
      if (!(target instanceof Element) || !(event instanceof MouseEvent)) {
        return;
      }
      const rect = mockup.getBoundingClientRect();
      const x = event.clientX - rect.left;
      const y = event.clientY - rect.top;
      const row = target.closest(".pane-tab-row");
      if (row instanceof HTMLElement) {
        const sessionId = row.dataset.mockSessionTab ?? "";
        const menu = sessionId
          ? mockup.querySelector(`[data-mock-tab-menu="${sessionId}"]`)
          : null;
        if (menu instanceof HTMLElement) {
          event.preventDefault();
          open(menu, x, y, null);
        }
        return;
      }
      const section = target.closest(".pane-group");
      if (!section) {
        return;
      }
      const name = section.querySelector(".pane-group-name")?.textContent?.trim() ?? "";
      const menu = name ? mockup.querySelector(`[data-mock-group-menu="${name}"]`) : null;
      if (menu instanceof HTMLElement) {
        event.preventDefault();
        open(menu, x, y, null);
      }
    });

    // The … button toggles its group's menu, anchored past its corner.
    for (const node of [...mockup.querySelectorAll(".pane-group-menu-button")]) {
      const section = node.closest(".pane-group");
      const name = section
        ? section.querySelector(".pane-group-name")?.textContent?.trim() ?? ""
        : "";
      const menu = name ? mockup.querySelector(`[data-mock-group-menu="${name}"]`) : null;
      const button = promote(node, "button");
      if (!button || !(menu instanceof HTMLElement)) {
        continue;
      }
      button.setAttribute("aria-haspopup", "menu");
      button.setAttribute("aria-expanded", "false");
      button.setAttribute("aria-label", "Group options");
      button.setAttribute("title", "Group options");
      button.addEventListener("click", (event) => {
        // The groups feature's delegated header click collapses the group;
        // the app's … button stops propagation for the same reason.
        event.stopPropagation();
        if (openMenu === menu) {
          close(true);
          return;
        }
        const rect = mockup.getBoundingClientRect();
        const buttonRect = button.getBoundingClientRect();
        open(menu, buttonRect.right - rect.left, buttonRect.bottom - rect.top + 2, button);
        button.setAttribute("aria-expanded", String(openMenu === menu));
      });
    }

    // Every item dismisses except the collapse toggle, which works and keeps
    // the menu up — this is a tour, not a session-management surface.
    for (const menu of menus) {
      if (!(menu instanceof HTMLElement)) {
        continue;
      }
      menu.addEventListener("click", (event) => {
        const target = event.target;
        if (!(target instanceof Element)) {
          return;
        }
        const item = target.closest("[data-mock-context-item]");
        if (!(item instanceof HTMLElement)) {
          return;
        }
        if (item.hasAttribute("data-mock-menu-collapse")) {
          const section = groupSectionByName.get(menu.dataset.mockGroupMenu ?? "");
          if (section instanceof HTMLElement) {
            const collapsed = section.classList.toggle("is-collapsed");
            const toggleButton = section.querySelector(".pane-group-collapse-button");
            toggleButton?.setAttribute("aria-expanded", String(!collapsed));
            syncCollapseItem(menu);
            announce(
              `${menu.dataset.mockGroupMenu} agents ${collapsed ? "collapsed" : "expanded"}.`,
            );
          }
          return;
        }
        close(true);
      });
    }

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && openMenu) {
        close(openTrigger != null);
      }
    });

    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (
        openMenu &&
        target instanceof Element &&
        !openMenu.contains(target) &&
        !(openTrigger && openTrigger.contains(target))
      ) {
        close(false);
      }
    });

    // A scrolling sidebar leaves the menu pointing at nothing.
    paneList.addEventListener("scroll", () => close(false), { passive: true });

    return {};
  }

  // --------------------------------------------------------------- research

  /**
   * Research mode. Both sidebar lists and both stages ship inside the same
   * window, so switching is a class on the shell plus a hidden flip: nothing is
   * fetched, and a replica whose script never loads is simply whichever mode
   * the server rendered.
   */
  function createResearch() {
    const shellNode = mockup.querySelector(".app-shell");
    const toggleNode = mockup.querySelector(".sidebar [data-mock-mode-toggle]");
    const lists = /** @type {HTMLElement[]} */ (
      [...mockup.querySelectorAll("[data-mock-sidebar-list]")]
    );
    const views = /** @type {HTMLElement[]} */ (
      [...mockup.querySelectorAll("[data-mock-research-doc]")]
    );
    const rowNodes = [...mockup.querySelectorAll("[data-mock-research-row]")];
    if (
      !(shellNode instanceof HTMLElement) ||
      !(toggleNode instanceof HTMLElement) ||
      lists.length === 0 ||
      views.length === 0 ||
      rowNodes.length === 0
    ) {
      return null;
    }
    const shell = shellNode;

    // A real tablist now that the toggle switches something. Each panel names
    // itself rather than pointing at its tab: ids would collide between the
    // replicas that share a page.
    toggleNode.setAttribute("role", "tablist");
    toggleNode.setAttribute("aria-label", "Sidebar mode");
    for (const list of lists) {
      list.setAttribute("role", "tabpanel");
      list.setAttribute(
        "aria-label",
        list.dataset.mockSidebarList === "research" ? "Research" : "Terminal tabs",
      );
    }

    /** @type {HTMLElement[]} */
    const tabs = [];

    /** @param {string} mode */
    function setMode(mode) {
      shell.classList.toggle("is-research-mode", mode === "research");
      for (const list of lists) {
        list.hidden = list.dataset.mockSidebarList !== mode;
      }
      for (const tab of tabs) {
        const selected = tab.dataset.mockMode === mode;
        tab.classList.toggle("is-selected", selected);
        tab.setAttribute("aria-selected", String(selected));
      }
      announce(
        mode === "research"
          ? "Research mode: the sidebar lists research and the Journal."
          : "Terminal mode: the sidebar lists terminals.",
      );
    }

    for (const node of [...toggleNode.children]) {
      const mode = node instanceof HTMLElement ? node.dataset.mockMode ?? "" : "";
      const tab = promote(node, "button");
      if (!tab || !mode) {
        continue;
      }
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-selected", String(tab.classList.contains("is-selected")));
      tab.addEventListener("click", () => setMode(mode));
      tabs.push(tab);
    }

    /** @type {{ row: HTMLElement, button: HTMLElement }[]} */
    const rows = [];

    /** @param {HTMLElement} row */
    function openRow(row) {
      const id = row.dataset.mockResearchRow ?? "";
      if (!id) {
        return;
      }
      for (const entry of rows) {
        const selected = entry.row === row;
        entry.row.classList.toggle("is-selected", selected);
        if (selected) {
          entry.button.setAttribute("aria-current", "page");
        } else {
          entry.button.removeAttribute("aria-current");
        }
      }
      for (const view of views) {
        view.hidden = view.dataset.mockResearchDoc !== id;
        if (!view.hidden) {
          // A document the reader comes back to opens at its top; the app
          // restores a remembered scroll, which a fresh selection has none of.
          view.scrollTop = 0;
        }
      }
      const title = row.dataset.mockResearchTitle || id;
      const crumb = mockup.querySelector("[data-mock-research-crumb]");
      if (crumb) {
        crumb.textContent = title;
      }
      const threadCount = mockup.querySelector("[data-mock-research-thread-count]");
      if (threadCount) {
        threadCount.textContent = row.dataset.mockResearchThread || "";
      }
      announce(`Opened ${title}.`);
    }

    for (const node of rowNodes) {
      if (!(node instanceof HTMLElement)) {
        continue;
      }
      // An ordinary row carries its control inside; the Journal tab is one.
      const select = node.querySelector(".research-sidebar-select");
      const button = promote(select ?? node, "button");
      if (!button) {
        continue;
      }
      const row = select ? node : button;
      const title = row.dataset.mockResearchTitle || row.dataset.mockResearchRow || "research";
      button.setAttribute("aria-label", `Open ${title}`);
      button.addEventListener("click", () => openRow(row));
      rows.push({ row, button });
    }

    // Every folder ships its members whatever its state, so opening one is a
    // class — the same contract the terminal sidebar's groups keep. The header
    // is the target, and the chevron's own click bubbles into it.
    for (const folder of [...mockup.querySelectorAll("[data-mock-research-folder]")]) {
      const name = folder instanceof HTMLElement ? folder.dataset.mockResearchFolder ?? "" : "";
      const collapse = promote(
        folder.querySelector(".research-sidebar-folder-collapse"),
        "button",
      );
      const header = folder.querySelector(".research-sidebar-folder-row");
      if (!collapse || !header) {
        continue;
      }
      collapse.setAttribute("aria-label", `${name} research`);
      collapse.setAttribute("aria-expanded", String(!folder.classList.contains("is-collapsed")));
      header.addEventListener("click", () => {
        const collapsed = folder.classList.toggle("is-collapsed");
        collapse.setAttribute("aria-expanded", String(!collapsed));
        announce(`${name} research ${collapsed ? "collapsed" : "expanded"}.`);
      });
    }

    return {};
  }

  // ---------------------------------------------------------------- journal

  const ELLIPSIS_ICON =
    '<svg class="lucide" xmlns="http://www.w3.org/2000/svg" width="13" height="13" ' +
    'viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" ' +
    'stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">' +
    '<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>' +
    '<circle cx="5" cy="12" r="1"/></svg>';

  const JOURNAL_MENU_GAP = 4;

  /**
   * The Journal's feed. The composer adds entries for real, an entry's menu
   * deletes one, and the undo bar puts it back — the removal is the only menu
   * item anywhere in this file that does work, because the bar it raises is
   * the thing worth showing. Nothing is hydrated here, so a pasted URL lands
   * as a link card; the embedded tweets are the ones the fixture shipped.
   */
  function createJournal() {
    const feedNode = mockup.querySelector(".journal-feed");
    const fieldNode = mockup.querySelector("[data-mock-journal-input]");
    const undoNode = mockup.querySelector("[data-mock-journal-undo]");
    if (
      !(feedNode instanceof HTMLElement) ||
      !(fieldNode instanceof HTMLElement) ||
      !(undoNode instanceof HTMLElement)
    ) {
      return null;
    }
    const feed = feedNode;
    const undoBar = undoNode;
    const undoLabel = undoBar.querySelector("[data-mock-journal-undo-label]");

    // One menu per entry kind, captured before anything can be deleted, so a
    // card added later carries exactly the items the app gives that kind.
    /** @type {Map<string, HTMLElement>} */
    const menuTemplates = new Map();
    for (const entry of [...mockup.querySelectorAll("[data-mock-journal-entry]")]) {
      const id = entry instanceof HTMLElement ? entry.dataset.mockJournalEntry ?? "" : "";
      const menu = id ? mockup.querySelector(`[data-mock-journal-menu="${id}"]`) : null;
      const kind = entry.classList.contains("is-note")
        ? "note"
        : entry.classList.contains("is-link")
          ? "link"
          : "tweet";
      if (menu instanceof HTMLElement && !menuTemplates.has(kind)) {
        menuTemplates.set(kind, /** @type {HTMLElement} */ (menu.cloneNode(true)));
      }
    }

    /** @type {HTMLElement | null} */
    let openMenu = null;
    /** @type {HTMLElement | null} */
    let openTrigger = null;
    /** @type {HTMLElement | null} */
    let openEntry = null;
    /** @type {{ node: HTMLElement, next: Element | null } | null} */
    let removed = null;
    let addedCount = 0;

    /** @param {boolean} refocus */
    function closeMenu(refocus) {
      openEntry?.classList.remove("has-open-menu");
      if (openMenu) {
        openMenu.hidden = true;
      }
      if (openTrigger) {
        openTrigger.setAttribute("aria-expanded", "false");
        if (refocus) {
          openTrigger.focus();
        }
      }
      openMenu = null;
      openTrigger = null;
      openEntry = null;
    }

    /** @param {HTMLElement} entry */
    function menuFor(entry) {
      const id = entry.dataset.mockJournalEntry ?? "";
      const menu = id ? mockup.querySelector(`[data-mock-journal-menu="${id}"]`) : null;
      return menu instanceof HTMLElement ? menu : null;
    }

    /**
     * @param {HTMLElement} entry
     * @param {number} x
     * @param {number} y
     * @param {HTMLElement | null} trigger
     */
    function openMenuAt(entry, x, y, trigger) {
      const menu = menuFor(entry);
      if (!menu) {
        return;
      }
      closeMenu(false);
      placeContextMenu(menu, x, y);
      entry.classList.add("has-open-menu");
      trigger?.setAttribute("aria-expanded", "true");
      openMenu = menu;
      openTrigger = trigger;
      openEntry = entry;
    }

    /** @param {HTMLElement} entry */
    function deleteEntry(entry) {
      const note = entry.classList.contains("is-note");
      removed = { node: entry, next: entry.nextElementSibling };
      entry.remove();
      if (undoLabel) {
        undoLabel.textContent = `${note ? "Note" : "Entry"} removed`;
      }
      undoBar.hidden = false;
      announce(`${note ? "Note" : "Entry"} removed. Undo is available.`);
    }

    function undoRemove() {
      if (!removed) {
        return;
      }
      if (removed.next && removed.next.parentElement === feed) {
        feed.insertBefore(removed.node, removed.next);
      } else {
        feed.append(removed.node);
      }
      removed = null;
      undoBar.hidden = true;
      announce("Entry restored.");
    }

    function dismissUndo() {
      removed = null;
      undoBar.hidden = true;
    }

    /** @param {HTMLElement} menu */
    function wireMenu(menu) {
      menu.addEventListener("click", (event) => {
        const target = event.target;
        if (!(target instanceof Element)) {
          return;
        }
        const item = target.closest("[data-mock-context-item]");
        if (!(item instanceof HTMLElement)) {
          return;
        }
        // Only the removal acts; opening, copying, and refetching are the
        // app's business, not a marketing page's.
        const entry = openEntry;
        const remove = item.hasAttribute("data-mock-journal-delete");
        closeMenu(!remove);
        if (remove && entry) {
          deleteEntry(entry);
        }
      });
      menu.addEventListener("contextmenu", (event) => event.preventDefault());
    }

    /** @param {HTMLElement} entry */
    function wireEntry(entry) {
      const existing = entry.querySelector(".journal-entry-menu-trigger");
      // A server-rendered entry ships its trigger as a span; one added at
      // runtime has none yet and gets the same control built here.
      let menuTrigger = promote(existing, "button");
      if (!menuTrigger) {
        const created = document.createElement("button");
        created.type = "button";
        created.className = "control-button journal-entry-menu-trigger";
        created.innerHTML = ELLIPSIS_ICON;
        entry.append(created);
        menuTrigger = created;
      }
      menuTrigger.setAttribute("aria-haspopup", "menu");
      menuTrigger.setAttribute("aria-expanded", "false");
      menuTrigger.setAttribute("aria-label", "Entry actions");
      menuTrigger.setAttribute("title", "Entry actions");
      menuTrigger.addEventListener("click", () => {
        if (openEntry === entry) {
          closeMenu(true);
          return;
        }
        const menu = menuFor(entry);
        if (!menu) {
          return;
        }
        // Unhide before measuring: the menu is right-aligned on its trigger,
        // which needs its width.
        menu.hidden = false;
        const origin = mockup.getBoundingClientRect();
        const rect = menuTrigger.getBoundingClientRect();
        openMenuAt(
          entry,
          rect.right - origin.left - menu.offsetWidth,
          rect.bottom - origin.top + JOURNAL_MENU_GAP,
          menuTrigger,
        );
      });
      entry.addEventListener("contextmenu", (event) => {
        if (!(event instanceof MouseEvent) || !menuFor(entry)) {
          return;
        }
        // Right-clicking a link or the quote card keeps the entry menu too:
        // the browser's own has nothing useful to offer inside the replica.
        event.preventDefault();
        const at = pointerInMockup(event);
        openMenuAt(entry, at.x, at.y, null);
      });
    }

    const textarea = document.createElement("textarea");
    textarea.className = fieldNode.className;
    textarea.rows = 2;
    textarea.placeholder = fieldNode.textContent || "";
    textarea.setAttribute("aria-label", "New journal entry (demo)");
    fieldNode.replaceWith(textarea);

    function submit() {
      const text = textarea.value.trim();
      if (!text) {
        textarea.focus();
        return;
      }
      const link = /^https?:\/\/\S+$/i.test(text);
      const kind = link ? "link" : "note";
      addedCount += 1;
      const id = `mock-journal-${addedCount}`;
      const entry = document.createElement("article");
      entry.className = `journal-entry is-${kind}`;
      entry.dataset.mockJournalEntry = id;
      const body = document.createElement(link ? "span" : "p");
      body.className = link ? "journal-link-url" : "journal-note-text";
      body.textContent = text;
      entry.append(body);
      const template = menuTemplates.get(kind);
      if (template) {
        const menu = /** @type {HTMLElement} */ (template.cloneNode(true));
        menu.dataset.mockJournalMenu = id;
        menu.hidden = true;
        mockup.append(menu);
        wireMenu(menu);
      }
      // The feed reads newest first, so a new entry lands at the top.
      feed.prepend(entry);
      wireEntry(entry);
      textarea.value = "";
      textarea.focus();
      announce(link ? "Link added to the journal." : "Note added to the journal.");
    }

    textarea.addEventListener("keydown", (event) => {
      if (event.key !== "Enter" || event.shiftKey) {
        return;
      }
      event.preventDefault();
      submit();
    });

    for (const entry of [...mockup.querySelectorAll("[data-mock-journal-entry]")]) {
      if (entry instanceof HTMLElement) {
        wireEntry(entry);
      }
    }
    for (const menu of [...mockup.querySelectorAll("[data-mock-journal-menu]")]) {
      if (menu instanceof HTMLElement) {
        wireMenu(menu);
      }
    }

    const restore = promote(undoBar.querySelector("[data-mock-journal-undo-restore]"), "button");
    if (restore) {
      restore.setAttribute("aria-label", "Undo the removal");
      restore.addEventListener("click", undoRemove);
    }
    const dismiss = promote(undoBar.querySelector("[data-mock-journal-undo-dismiss]"), "button");
    if (dismiss) {
      dismiss.setAttribute("aria-label", "Dismiss undo");
      dismiss.setAttribute("title", "Dismiss");
      dismiss.addEventListener("click", dismissUndo);
    }

    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && openMenu) {
        closeMenu(openTrigger !== null);
        return;
      }
      // The keycaps the menu displays are live while it is open, exactly as
      // they behave on an item's click: D removes the entry, the rest dismiss.
      if (openMenu && !event.metaKey && !event.ctrlKey && !event.altKey) {
        const item = keycapItem(openMenu, event.key);
        if (item) {
          event.preventDefault();
          const entry = openEntry;
          const remove = item.hasAttribute("data-mock-journal-delete");
          closeMenu(!remove && openTrigger !== null);
          if (remove && entry) {
            deleteEntry(entry);
          }
          return;
        }
      }
      // The app restores the last removal on the platform's undo chord, and
      // only while its bar is up.
      if (
        removed &&
        (event.metaKey || event.ctrlKey) &&
        !event.shiftKey &&
        !event.altKey &&
        event.key.toLowerCase() === "z"
      ) {
        event.preventDefault();
        undoRemove();
      }
    });
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (
        openMenu &&
        target instanceof Element &&
        !openMenu.contains(target) &&
        !(openTrigger && openTrigger.contains(target))
      ) {
        closeMenu(false);
      }
    });

    return {};
  }

  // --------------------------------------------------------- research-menus

  /**
   * The research sidebar's right-click menus, one per row. They carry the A and
   * D keycaps the app shows, and — like every other menu here — their items
   * dismiss rather than archiving or deleting anything.
   */
  function createResearchMenus() {
    const listNode = mockup.querySelector('[data-mock-sidebar-list="research"]');
    const menus = [...mockup.querySelectorAll("[data-mock-research-menu]")];
    if (!(listNode instanceof HTMLElement) || menus.length === 0) {
      return null;
    }
    const list = listNode;

    /** @type {HTMLElement | null} */
    let openMenu = null;
    /** @type {HTMLElement | null} */
    let openTrigger = null;
    /** @type {Element | null} */
    let openRow = null;

    /** @param {boolean} refocus */
    function close(refocus) {
      openRow?.classList.remove("has-open-menu");
      if (openMenu) {
        openMenu.hidden = true;
      }
      if (openTrigger) {
        openTrigger.setAttribute("aria-expanded", "false");
        if (refocus) {
          openTrigger.focus();
        }
      }
      openMenu = null;
      openTrigger = null;
      openRow = null;
    }

    /** @param {Element} row */
    function menuFor(row) {
      const id = row instanceof HTMLElement ? row.dataset.mockResearchRow ?? "" : "";
      const menu = id ? mockup.querySelector(`[data-mock-research-menu="${id}"]`) : null;
      return menu instanceof HTMLElement ? menu : null;
    }

    /**
     * @param {Element} row
     * @param {number} x
     * @param {number} y
     * @param {HTMLElement | null} trigger
     */
    function open(row, x, y, trigger) {
      const menu = menuFor(row);
      if (!menu) {
        return;
      }
      close(false);
      placeContextMenu(menu, x, y);
      row.classList.add("has-open-menu");
      trigger?.setAttribute("aria-expanded", "true");
      openMenu = menu;
      openTrigger = trigger;
      openRow = row;
    }

    // The Journal tab has no menu, so its right-click falls through rather than
    // opening an empty one.
    list.addEventListener("contextmenu", (event) => {
      const target = event.target;
      if (!(target instanceof Element) || !(event instanceof MouseEvent)) {
        return;
      }
      const row = target.closest("[data-mock-research-row]");
      if (!row || !menuFor(row)) {
        return;
      }
      event.preventDefault();
      const at = pointerInMockup(event);
      open(row, at.x, at.y, null);
    });

    for (const node of [...list.querySelectorAll(".research-sidebar-menu-trigger")]) {
      const row = node.closest("[data-mock-research-row]");
      const trigger = promote(node, "button");
      if (!trigger || !row || !menuFor(row)) {
        continue;
      }
      trigger.setAttribute("aria-haspopup", "menu");
      trigger.setAttribute("aria-expanded", "false");
      trigger.setAttribute("aria-label", "Research actions");
      trigger.setAttribute("title", "Research actions");
      trigger.addEventListener("click", (event) => {
        // The row's own click opens the document; the app's trigger stops
        // propagation for the same reason.
        event.stopPropagation();
        if (openRow === row) {
          close(true);
          return;
        }
        const menu = menuFor(row);
        if (!menu) {
          return;
        }
        menu.hidden = false;
        const origin = mockup.getBoundingClientRect();
        const rect = trigger.getBoundingClientRect();
        open(row, rect.right - origin.left - menu.offsetWidth, rect.bottom - origin.top + 2, trigger);
      });
    }

    for (const menu of menus) {
      menu.addEventListener("click", (event) => {
        const target = event.target;
        if (target instanceof Element && target.closest("[data-mock-context-item]")) {
          close(true);
        }
      });
      menu.addEventListener("contextmenu", (event) => event.preventDefault());
    }

    document.addEventListener("keydown", (event) => {
      if (!openMenu) {
        return;
      }
      if (event.key === "Escape") {
        close(openTrigger !== null);
        return;
      }
      // The A/D keycaps are live while the menu is open; like the items they
      // sit on, they dismiss rather than archiving or deleting anything.
      if (!event.metaKey && !event.ctrlKey && !event.altKey && keycapItem(openMenu, event.key)) {
        event.preventDefault();
        close(openTrigger !== null);
      }
    });
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (
        openMenu &&
        target instanceof Element &&
        !openMenu.contains(target) &&
        !(openTrigger && openTrigger.contains(target))
      ) {
        close(false);
      }
    });

    // A scrolling list leaves the menu pointing at nothing.
    list.addEventListener("scroll", () => close(false), { passive: true, capture: true });

    return {};
  }

  // ------------------------------------------------------------- bootstrap

  const replay = features.has("replay") ? createReplay() : null;
  const groups = features.has("groups") ? createGroups() : null;
  const sessions = features.has("sessions") ? createSessions(replay) : null;
  const queue = features.has("queue") ? createQueue() : null;
  const panes = features.has("panes") ? createPanes() : null;
  const terminalMap = features.has("terminal-map") ? createTerminalMap() : null;
  const panels = features.has("panels") ? createPanels() : null;
  const images = features.has("images") ? createImageLightbox() : null;
  const menus = features.has("menus") ? createMenus() : null;
  const sidebarMenus = features.has("sidebar-menus") ? createSidebarMenus() : null;
  const research = features.has("research") ? createResearch() : null;
  const journal = features.has("journal") ? createJournal() : null;
  const researchMenus = features.has("research-menus") ? createResearchMenus() : null;
  if (replayBoot) {
    replay?.prepare();
  }
  if (
    !replay &&
    !queue &&
    !groups &&
    !sessions &&
    !panes &&
    !terminalMap &&
    !panels &&
    !images &&
    !menus &&
    !sidebarMenus &&
    !research &&
    !journal &&
    !researchMenus
  ) {
    return;
  }

  // The replica has real controls now, so it stops being one labelled image.
  mockup.classList.add("is-interactive");
  mockup.removeAttribute("role");
  mockup.removeAttribute("aria-label");
  mockup.removeAttribute("aria-labelledby");
  hideFromAssistiveTech([
    ".mock-traffic-lights",
    // The mode toggle is decoration until the research feature turns it into a
    // real tablist; hiding the container would then take its tabs with it.
    ...(research ? [] : [".sidebar-mode-toggle"]),
    // The sidebar header's buttons are promoted to real controls (hide-sidebar
    // by panes, the terminal map by terminal-map), so the subtree as a whole
    // must stay exposed; promote() strips aria-hidden from each swap.
    ".sidebar-actions",
    ".mock-terminal-screen",
    ".turn-pane-session-control",
  ]);

  if (replay?.isWorking()) {
    // Autoplay is a courtesy, not the mechanism: it waits until the window is
    // actually on screen, runs once, and never starts when the reader has asked
    // for less motion.
    if (!reducedMotion.matches && typeof IntersectionObserver === "function") {
      const observer = new IntersectionObserver(
        (entries) => {
          for (const entry of entries) {
            if (!entry.isIntersecting || document.hidden) {
              continue;
            }
            observer.disconnect();
            void replay.play();
          }
        },
        { threshold: 0.35 },
      );
      observer.observe(mockup);
    }
  }
}

(() => {
  const mockups = /** @type {HTMLElement[]} */ (
    [...document.querySelectorAll(".app-mockup")].filter(
      (node) => node instanceof HTMLElement,
    )
  );
  if (mockups.length === 0) {
    return;
  }

  // A drag that finishes in the gap above the mockup can include the grid's
  // block-boundary whitespace. Keep copying the hero description literal.
  const introCopy = document.querySelector(".intro-copy > p:first-child");
  if (introCopy instanceof HTMLElement) {
    document.addEventListener("copy", (event) => {
      const selection = window.getSelection();
      if (!selection || selection.isCollapsed || !event.clipboardData) {
        return;
      }
      const selectedText = selection.toString();
      const trimmedText = selectedText.trimEnd();
      if (trimmedText !== introCopy.innerText.trim() || trimmedText === selectedText) {
        return;
      }
      event.clipboardData.setData("text/plain", trimmedText);
      event.preventDefault();
    });
  }

  // Replay staging is selected globally before paint, but every replica must
  // transfer those hints into its own runtime state before the document-level
  // class is removed.
  const replayBoot = document.documentElement.classList.contains("mock-replay-boot");
  for (const mockup of mockups) {
    enhanceQmuxMockup(mockup, replayBoot);
  }
  if (replayBoot) {
    document.documentElement.classList.remove("mock-replay-boot");
  }
})();
