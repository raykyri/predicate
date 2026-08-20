// Sentinel scheme used for absolute local file paths in transcript markdown.
// Agents often write `[preview](/Users/.../report.html)`; resolving that against
// a base URL would turn it into `https://qmux.invalid/Users/...` and send the
// human browser at a non-existent host (or worse, load a custom-protocol path
// that panics). Instead we keep the path as a qmux-file: URL that openLink
// recognizes and routes through the token-scoped file server.
export const QMUX_FILE_HREF_PREFIX = "qmux-file:";

// Only let links through that the webview can safely open. Transcript markdown and
// terminal output can contain arbitrary agent/process text; a javascript:/file:/tauri:
// URL clicked inside the Tauri webview reaches a JS context with native IPC access.
// Anything that isn't http/https/mailto (or a recognized absolute local path) is
// rendered or treated as non-navigable text.
export function safeHref(href: unknown): string | undefined {
  if (typeof href !== "string") {
    return undefined;
  }
  const localPath = absoluteLocalFilePath(href);
  if (localPath) {
    return `${QMUX_FILE_HREF_PREFIX}${localPath}`;
  }
  let url: URL;
  try {
    url = new URL(href, "https://qmux.invalid/");
  } catch {
    return undefined;
  }
  // Reject resolutions that only "look" like https because an absolute Unix path
  // was joined onto the dummy base (handled above) — keep this as a belt-and-
  // braces check for any path-shaped input absoluteLocalFilePath missed.
  if (
    url.hostname === "qmux.invalid" &&
    absoluteLocalFilePath(url.pathname) !== undefined
  ) {
    return `${QMUX_FILE_HREF_PREFIX}${url.pathname}`;
  }
  // Return the resolved absolute URL, not the raw href: a relative ("/path") or
  // protocol-relative ("//host") href passes the protocol check once resolved
  // against the base, but handing the raw string downstream would let it resolve
  // unpredictably. Normalizing here means openLink always receives a fully
  // qualified http(s)/mailto URL (or a qmux-file: local path).
  return url.protocol === "http:" || url.protocol === "https:" || url.protocol === "mailto:"
    ? url.href
    : undefined;
}

/** Absolute local filesystem path from a markdown href, or undefined. */
export function absoluteLocalFilePath(href: string): string | undefined {
  const trimmed = href.trim();
  if (!trimmed) {
    return undefined;
  }
  if (trimmed.startsWith(QMUX_FILE_HREF_PREFIX)) {
    const path = trimmed.slice(QMUX_FILE_HREF_PREFIX.length);
    return path.startsWith("/") ? path : undefined;
  }
  if (trimmed.startsWith("file:")) {
    try {
      const url = new URL(trimmed);
      // file:///abs/path → hostname empty; file://localhost/abs/path also ok.
      if (url.hostname !== "" && url.hostname !== "localhost") {
        return undefined;
      }
      // URL pathname is percent-decoded for file URLs on modern engines, but
      // decode explicitly so `%20` survives older resolvers.
      const path = decodeURIComponent(url.pathname);
      return path.startsWith("/") ? path : undefined;
    } catch {
      return undefined;
    }
  }
  // Unix absolute path (not protocol-relative //host/...). Site-relative links
  // like `/docs/intro` are deliberately excluded: they lack a known filesystem
  // root prefix and would otherwise steal ordinary in-repo markdown links.
  if (trimmed.startsWith("/") && !trimmed.startsWith("//")) {
    if (
      /^\/(Users|home|tmp|var|private|opt|Volumes|mnt|root)\//.test(trimmed) ||
      // Multi-segment absolute path ending in a file-looking last segment
      // (has an extension). Covers e.g. /workspace/out/report.html in containers.
      (/^\/[^/]+\/.+\.[A-Za-z0-9]{1,16}$/.test(trimmed) &&
        !trimmed.includes("?") &&
        !trimmed.includes("#"))
    ) {
      return trimmed;
    }
  }
  // Windows drive path.
  if (/^[A-Za-z]:[\\/]/.test(trimmed)) {
    return trimmed;
  }
  return undefined;
}

export function isQmuxFileHref(url: string): boolean {
  return url.startsWith(QMUX_FILE_HREF_PREFIX);
}

export function pathFromQmuxFileHref(url: string): string | undefined {
  if (!isQmuxFileHref(url)) {
    return undefined;
  }
  const path = url.slice(QMUX_FILE_HREF_PREFIX.length);
  return path.length > 0 ? path : undefined;
}

// Mirrors the file server's explicit browser-renderable MIME allowlist. This is
// only a UI hint for whether the context menu should offer an internal preview;
// the backend resolves the canonical path and makes the authoritative decision.
const INTERNAL_FILE_PREVIEW_EXTENSIONS = new Set([
  "avif",
  "csv",
  "css",
  "gif",
  "htm",
  "html",
  "ico",
  "jpeg",
  "jpg",
  "js",
  "json",
  "log",
  "markdown",
  "md",
  "mjs",
  "mp3",
  "mp4",
  "pdf",
  "png",
  "svg",
  "toml",
  "txt",
  "wav",
  "webm",
  "webp",
  "xml",
  "yaml",
  "yml",
]);

// Mirrors the file server's renderable-source list (is_renderable_source):
// these are served as line-anchored HTML pages despite an octet-stream mime.
const INTERNAL_SOURCE_PREVIEW_EXTENSIONS = new Set([
  "rs", "ts", "tsx", "jsx", "py", "go", "c", "h", "cc", "cpp", "cxx",
  "hpp", "hh", "java", "kt", "kts", "swift", "m", "mm", "rb", "php",
  "cs", "scala", "clj", "cljs", "ex", "exs", "erl", "hs", "ml", "mli",
  "lua", "pl", "pm", "r", "jl", "zig", "nim", "dart", "sql", "proto",
  "graphql", "gql", "vue", "svelte", "astro", "tf", "hcl", "nix",
  "cmake", "gradle", "groovy", "ps1", "bat", "sh", "bash", "zsh",
  "fish", "awk", "sed", "diff", "patch",
]);
const INTERNAL_SOURCE_PREVIEW_FILENAMES = new Set(["makefile", "dockerfile", "justfile"]);

export function canPreviewLocalFilePath(path: string): boolean {
  // Grep-style location suffixes (`session.rs:8`, `session.rs:8:14`) resolve
  // to the underlying file on the backend; judge previewability without them.
  const filename = (path.replace(/\\/g, "/").split("/").pop() ?? "").replace(
    /(?::\d+){1,2}$/,
    "",
  );
  if (INTERNAL_SOURCE_PREVIEW_FILENAMES.has(filename.toLowerCase())) {
    return true;
  }
  const extension = filename.includes(".") ? filename.split(".").pop()?.toLowerCase() : undefined;
  return (
    extension !== undefined &&
    (INTERNAL_FILE_PREVIEW_EXTENSIONS.has(extension) ||
      INTERNAL_SOURCE_PREVIEW_EXTENSIONS.has(extension))
  );
}

/**
 * A loopback HTML document that qmux can offer as an explicit browser preview.
 * Parse the URL instead of matching a prefix so lookalike hosts such as
 * `localhost.example.com` never receive the local-preview affordance.
 */
export function loopbackHtmlUrl(value: unknown): string | undefined {
  if (typeof value !== "string" || value.length === 0 || value !== value.trim()) {
    return undefined;
  }
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    return undefined;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return undefined;
  }
  if (parsed.username || parsed.password) {
    return undefined;
  }
  if (parsed.hostname !== "127.0.0.1" && parsed.hostname !== "localhost") {
    return undefined;
  }
  return /\.html$/iu.test(parsed.pathname) ? parsed.href : undefined;
}

// Normal http(s) links render through qmux's isolated Chromium automation profile.
// Token-bearing file previews are still detected separately and rendered in the
// webview's sandboxed iframe. mailto and custom schemes remain OS-owned.
export function canRenderInInternalBrowser(url: string): boolean {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  return parsed.protocol === "http:" || parsed.protocol === "https:";
}

// Fallback used only when the Chromium automation runtime is unavailable. The
// Tauri webview CSP permits unsandboxed frames for loopback HTTP development
// servers, but deliberately not arbitrary external pages.
export function canRenderInLocalPreviewFrame(url: string): boolean {
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
