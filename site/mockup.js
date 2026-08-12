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
//   panels  — the four surfaces the right pane's header opens: the prompt
//             library, the split queue, the browser overlay, and the artifact
//             tray. They interlock: a saved prompt lands in the composer, and
//             opening an artifact hands it to the browser.
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

(() => {
  const found = document.querySelector(".app-mockup");
  if (!(found instanceof HTMLElement)) {
    return;
  }
  // Re-bound with an explicit type: narrowing from the guard above does not
  // reach the closures below.
  const mockup = /** @type {HTMLElement} */ (found);
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
    const screen = mockup.querySelector(".mock-terminal-screen");
    const found = mockup.querySelector(".turn-timeline");
    const cursor = mockup.querySelector(".mock-terminal-cursor");
    const tail = mockup.querySelector(".mock-terminal-tail");
    const stepped = /** @type {HTMLElement[]} */ ([...mockup.querySelectorAll("[data-step]")]);
    if (!screen || !found || stepped.length === 0) {
      return null;
    }
    const timeline = /** @type {Element} */ (found);

    // The selected tab's dot reports the agent's state; it goes amber while the
    // replay is running and returns to whatever the markup shipped.
    const dot = mockup.querySelector(".pane-tab-row.is-selected .pane-tab-dot");
    const dotClass = dot ? dot.className : "";

    const steps = [...new Set(stepped.map((node) => Number(node.dataset.step)))].sort(
      (a, b) => a - b,
    );

    let token = 0;
    let playing = false;
    let played = false;

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

    function scrollTranscriptToTail() {
      // The timeline keeps overflow:hidden, so this scrolls without ever
      // catching the page's wheel.
      timeline.scrollTop = timeline.scrollHeight;
    }

    function rewind() {
      for (const node of stepped) {
        node.classList.add("is-pending");
        node.classList.remove("is-revealed");
        if (node.classList.contains("mock-terminal-block")) {
          for (const line of node.querySelectorAll(".mock-terminal-line")) {
            line.classList.add("is-pending");
            line.classList.remove("is-revealed");
          }
        }
      }
      if (tail && cursor) {
        tail.appendChild(cursor);
      }
      timeline.scrollTop = 0;
    }

    function showFinalState() {
      for (const node of stepped) {
        node.classList.remove("is-pending", "is-revealed");
        for (const line of node.querySelectorAll(".mock-terminal-line")) {
          line.classList.remove("is-pending", "is-revealed");
        }
      }
      if (tail && cursor) {
        tail.appendChild(cursor);
      }
      if (dot) {
        dot.className = dotClass;
      }
      scrollTranscriptToTail();
    }

    async function play() {
      token += 1;
      const runToken = token;
      playing = true;
      played = true;
      mockup.classList.add("is-playing");
      if (dot) {
        dot.className = "pane-tab-dot status-active";
      }
      rewind();
      onStateChange();
      try {
        await sleep(260, runToken);
        for (const step of steps) {
          const nodes = stepped.filter((node) => Number(node.dataset.step) === step);
          for (const node of nodes) {
            if (!node.classList.contains("mock-terminal-block")) {
              reveal(node);
              scrollTranscriptToTail();
              await sleep(140, runToken);
              continue;
            }
            // A command's output lands in a burst, the way a real pane fills.
            reveal(node);
            for (const line of node.querySelectorAll(".mock-terminal-line")) {
              reveal(line);
              if (cursor) {
                line.appendChild(cursor);
              }
              await sleep(34, runToken);
            }
          }
          await sleep(520, runToken);
        }
        if (tail && cursor) {
          tail.appendChild(cursor);
        }
        playing = false;
        mockup.classList.remove("is-playing");
        if (dot) {
          dot.className = dotClass;
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
      showFinalState();
      onStateChange();
    }

    /** @type {() => void} */
    let onStateChange = () => {};

    return {
      play,
      skip,
      isPlaying: () => playing,
      hasPlayed: () => played,
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
    replacement.innerHTML = node.innerHTML;
    if (replacement instanceof HTMLButtonElement) {
      replacement.type = "button";
    }
    node.replaceWith(replacement);
    return replacement;
  }

  function createQueue() {
    const composer = mockup.querySelector(".native-input");
    const field = mockup.querySelector(".mock-textarea");
    const actions = mockup.querySelector(".native-input-submit-actions");
    const foundTimeline = mockup.querySelector(".turn-timeline");
    const foundTab = mockup.querySelector(".pane-tab-row.is-selected .pane-tab");
    if (
      !composer ||
      !(field instanceof HTMLElement) ||
      !actions ||
      !foundTimeline ||
      !foundTab
    ) {
      return null;
    }
    const timeline = /** @type {Element} */ (foundTimeline);
    const paneTab = /** @type {Element} */ (foundTab);

    const textarea = document.createElement("textarea");
    textarea.className = field.className;
    textarea.rows = 1;
    textarea.placeholder = field.textContent || "";
    textarea.setAttribute("aria-label", "Message for the agent (demo)");
    field.replaceWith(textarea);

    const stack = document.createElement("div");
    stack.className = "queued-turn-stack";
    composer.prepend(stack);

    // Only the two submit buttons do anything; the overflow menu and the queue
    // dropdown stay decorative rather than pretending to open.
    const buttons = [...actions.querySelectorAll(".control-button")];
    const sendNowButton = promote(buttons[0] ?? null, "button");
    const queueButton = promote(buttons[1] ?? null, "button");
    for (const decorative of actions.querySelectorAll(
      ".composer-menu, .queue-menu-button",
    )) {
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
      const label = PANE_LABELS[/** @type {keyof typeof PANE_LABELS} */ (action)];
      button.setAttribute("aria-label", label);
      button.setAttribute("title", label);
      if (action === "expand-transcript") {
        // A toggle reports its state from the start, not only once pressed.
        button.setAttribute("aria-pressed", "false");
      }
      button.addEventListener("click", handler);
    }
    return {};
  }

  // ---------------------------------------------------------------- panels

  const PANEL_LABELS = {
    "prompt-library": ["Show saved prompts", "Hide saved prompts"],
    "queue-split": ["Split the queue out of the transcript", "Float the queue over the transcript"],
    browser: ["Show the browser", "Hide the browser"],
    artifacts: ["Show the artifact tray", "Hide the artifact tray"],
  };

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

    // Only one popover at a time, the way the app's menus behave.
    /** @param {string} name */
    function togglePanel(name) {
      const open = !isOpen(name);
      if (open && (name === "prompt-library" || name === "artifacts")) {
        setPanel(name === "prompt-library" ? "artifacts" : "prompt-library", false);
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
      button.setAttribute("aria-label", labels[0]);
      button.setAttribute("title", labels[0]);
      button.setAttribute("aria-pressed", "false");
      const existing = triggers.get(action) ?? [];
      existing.push(button);
      triggers.set(action, existing);
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

    // Opening an artifact hands it to the browser overlay, which is what the
    // app does with one.
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
        setPanel("artifacts", false);
        setPanel("browser", true);
        announce(`Opened ${name} in the browser.`);
      });
    }

    // The empty state has to follow the queue, which is another feature's
    // business — so watch the stack rather than reaching into it.
    const stack = mockup.querySelector(".queued-turn-stack");
    if (stack && typeof MutationObserver === "function") {
      new MutationObserver(syncQueueEmptyState).observe(stack, { childList: true });
    }

    // A popover closes on Escape and on a click outside it.
    document.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        setPanel("prompt-library", false);
        setPanel("artifacts", false);
      }
    });
    document.addEventListener("pointerdown", (event) => {
      const target = event.target;
      if (!(target instanceof Element)) {
        return;
      }
      for (const name of ["prompt-library", "artifacts"]) {
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
      // The tab list itself is not interactive, so it stays decorative.
      const body = group.querySelector(".pane-list-body");
      if (body) {
        body.setAttribute("aria-hidden", "true");
      }
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

  // ------------------------------------------------------------- bootstrap

  const replay = features.has("replay") ? createReplay() : null;
  const queue = features.has("queue") ? createQueue() : null;
  const groups = features.has("groups") ? createGroups() : null;
  const panes = features.has("panes") ? createPanes() : null;
  const panels = features.has("panels") ? createPanels() : null;
  if (!replay && !queue && !groups && !panes && !panels) {
    return;
  }

  // The replica has real controls now, so it stops being one labelled image.
  mockup.classList.add("is-interactive");
  mockup.removeAttribute("role");
  mockup.removeAttribute("aria-label");
  mockup.removeAttribute("aria-labelledby");
  hideFromAssistiveTech([
    ".mock-traffic-lights",
    ".sidebar-mode-toggle",
    ".pane-home-row",
    ".sidebar-actions",
    ".mock-terminal-screen",
    ".turn-pane-session-control",
    ".turn-timeline",
  ]);

  const controls = frame ? frame.querySelector(".mock-demo-controls") : null;
  const replayButton = controls ? controls.querySelector('[data-mock-action="replay"]') : null;
  if (controls instanceof HTMLElement) {
    if (!replay && replayButton) {
      replayButton.remove();
    }
    controls.hidden = false;
  }

  if (replay && replayButton instanceof HTMLElement) {
    const syncButton = () => {
      replayButton.textContent = replay.isPlaying()
        ? "Skip to the end"
        : replay.hasPlayed()
          ? "Replay the session"
          : "Play the session";
    };
    replay.onChange(syncButton);
    syncButton();
    replayButton.addEventListener("click", () => {
      if (replay.isPlaying()) {
        replay.skip();
        return;
      }
      if (queue) {
        queue.reset();
      }
      void replay.play();
    });

    // Autoplay is a courtesy, not the mechanism: it waits until the window is
    // actually on screen, runs once, and never starts when the reader has asked
    // for less motion — the button still offers it on request.
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
})();
