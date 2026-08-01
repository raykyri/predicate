---
name: open-in-browser
description: Open a file or a localhost URL in the qmux browser overlay (a panel that floats over the terminal, bound to this tab). Use when the user invokes /qmux:open-in-browser or asks to preview, render, view, or show a file (HTML, PDF, image, Markdown, etc.) or a local dev-server page in the browser overlay. Not for opening pages in an external/system browser.
---

# Open in the qmux browser overlay

Run this command with the file path or URL to render, then report the result:

```bash
"${QMUX_CLI:-qmux}" open <path-or-url>
```

Examples:

```bash
"${QMUX_CLI:-qmux}" open report.html          # a file (relative paths resolve from the cwd)
"${QMUX_CLI:-qmux}" open ./out/diagram.svg
"${QMUX_CLI:-qmux}" open http://localhost:5173 # a local dev server
```

This loads the target in qmux's browser overlay for the current tab. Files must live
under the workspace (an allowed root); a path outside it, or a missing file, is
rejected — relay that error to the user verbatim. Do not run anything else.
