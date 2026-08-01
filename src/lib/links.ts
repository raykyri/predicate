// Only let links through that the webview can safely open. Transcript markdown and
// terminal output can contain arbitrary agent/process text; a javascript:/file:/tauri:
// URL clicked inside the Tauri webview reaches a JS context with native IPC access.
// Anything that isn't http/https/mailto is rendered or treated as non-navigable text.
export function safeHref(href: unknown): string | undefined {
  if (typeof href !== "string") {
    return undefined;
  }
  let url: URL;
  try {
    url = new URL(href, "https://qmux.invalid/");
  } catch {
    return undefined;
  }
  // Return the resolved absolute URL, not the raw href: a relative ("/path") or
  // protocol-relative ("//host") href passes the protocol check once resolved
  // against the base, but handing the raw string downstream would let it resolve
  // unpredictably. Normalizing here means openLink always receives a fully
  // qualified http(s)/mailto URL.
  return url.protocol === "http:" || url.protocol === "https:" || url.protocol === "mailto:"
    ? url.href
    : undefined;
}

// The internal browser overlay can only load what the webview CSP's frame-src allows:
// http over loopback (127.0.0.1 / localhost), which covers file-server URLs and local
// dev servers. Anything else - external hosts, https, mailto, custom schemes - would
// be blocked by CSP and render as a blank iframe, so it must hand off to the OS browser.
// Keep this in lockstep with `frame-src` in tauri.conf.json.
export function canRenderInInternalBrowser(url: string): boolean {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  return (
    parsed.protocol === "http:" &&
    (parsed.hostname === "127.0.0.1" || parsed.hostname === "localhost")
  );
}

// A token-bearing file-server URL (see file_server.rs): its path is
// `/<64-hex-token>/<file path>` on the loopback file-server port. Such URLs must always
// load sandboxed (opaque origin) and must never be handed to the OS browser — an
// unsandboxed same-origin load would let served content read the token and fetch every
// sibling file under the pane's roots. Detection: loopback http on the known file-server
// port, OR (as a fallback before the port is known) a loopback http URL whose first path
// segment is exactly a 64-char hex token. A local dev server is intentionally excluded so
// it keeps its real same-origin context.
export function isFileServerUrl(url: string, fileServerPort: number | null): boolean {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  if (parsed.protocol !== "http:") {
    return false;
  }
  if (parsed.hostname !== "127.0.0.1" && parsed.hostname !== "localhost") {
    return false;
  }
  if (fileServerPort != null) {
    const port = parsed.port ? Number.parseInt(parsed.port, 10) : 80;
    if (port === fileServerPort) {
      return true;
    }
  }
  const firstSegment = parsed.pathname.replace(/^\/+/, "").split("/")[0] ?? "";
  return /^[0-9a-f]{64}$/.test(firstSegment);
}
