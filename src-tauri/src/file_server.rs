//! Loopback static file server for the browser overlay.
//!
//! Binds `127.0.0.1:0` (ephemeral, loopback-only) at startup and serves files via
//! `http://127.0.0.1:<port>/<token>/<percent-encoded-abs-path>`. Because any local
//! process can reach a loopback port, a random `token` (not loopback alone) is what
//! gates access. The token is *per pane* (minted in `AppState::pane_file_token`): the
//! server resolves it back to the requesting pane and only serves paths that
//! canonicalize under that pane's own roots (`pane_file_roots`), including the local
//! temporary directories where agents commonly write artifacts. So a token an agent
//! obtains for its own pane can't reach another pane's workspace, and `..`/symlinks
//! can't escape into other locations such as `~/.ssh/id_rsa`.
//!
//! Hand-rolled GET/HEAD + Range over `TcpListener` to keep the dependency posture of
//! the rest of the backend (cf. the hand-rolled base64 in events.rs). Each connection
//! serves one request then closes (`Connection: close`).

use crate::connection_limit::ConnectionLimiter;
use crate::state::AppState;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap on the bytes consumed for a request's start line + headers, so a client can't
/// stream an unbounded request head into memory within the read-timeout window.
/// Generous next to any real percent-encoded file path.
const MAX_REQUEST_HEAD_BYTES: u64 = 64 * 1024;
/// Cap on a single full-file (non-range) response so a giant file can't balloon
/// memory; browsers fetch large media via Range anyway.
const MAX_INLINE_BYTES: u64 = 64 * 1024 * 1024;
/// The Codex visualization contract caps fragments at 2 MB. Keeping that cap
/// here prevents a malformed directive from turning the wrapper render into an
/// unbounded allocation.
const MAX_CODEX_INLINE_VIS_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on concurrent connection-handler threads. Each connection serves one
/// request then closes, so this bounds in-flight requests; 64 comfortably covers
/// a browser overlay fetching a page full of assets in parallel while keeping a
/// connection-spamming local process from exhausting threads/FDs. At the cap the
/// accept loop blocks and excess connections wait in the kernel listen backlog.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
/// Backoff after a failed accept, so persistent accept errors (e.g. EMFILE under
/// FD exhaustion) can't spin the accept loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const DM_SANS_ROMAN_LATIN_PATH: &str = "/__qmux/fonts/DMSans-Variable-Latin.woff2";
const DM_SANS_ROMAN_LATIN_EXT_PATH: &str = "/__qmux/fonts/DMSans-Variable-LatinExt.woff2";
const DM_SANS_ITALIC_LATIN_PATH: &str = "/__qmux/fonts/DMSans-VariableItalic-Latin.woff2";
const DM_SANS_ITALIC_LATIN_EXT_PATH: &str = "/__qmux/fonts/DMSans-VariableItalic-LatinExt.woff2";
const DM_SANS_ROMAN_LATIN: &[u8] =
    include_bytes!("../../src/assets/fonts/DMSans-Variable-Latin.woff2");
const DM_SANS_ROMAN_LATIN_EXT: &[u8] =
    include_bytes!("../../src/assets/fonts/DMSans-Variable-LatinExt.woff2");
const DM_SANS_ITALIC_LATIN: &[u8] =
    include_bytes!("../../src/assets/fonts/DMSans-VariableItalic-Latin.woff2");
const DM_SANS_ITALIC_LATIN_EXT: &[u8] =
    include_bytes!("../../src/assets/fonts/DMSans-VariableItalic-LatinExt.woff2");
const VALLEY_SANS_ROMAN_PATH: &str = "/__qmux/fonts/ValleySans-Variable.woff2";
const VALLEY_SANS_ITALIC_PATH: &str = "/__qmux/fonts/ValleySans-VariableItalic.woff2";
const VALLEY_SANS_ROMAN: &[u8] = include_bytes!("../../src/assets/fonts/ValleySans-Variable.woff2");
const VALLEY_SANS_ITALIC: &[u8] =
    include_bytes!("../../src/assets/fonts/ValleySans-VariableItalic.woff2");

pub struct FileServerInfo {
    pub port: u16,
}

/// Starts the loopback file server and returns its port. The caller stores it in
/// `AppState` so the control socket can build URLs; the frontend never sees the port
/// or any token directly (it only receives fully-formed URLs in `browser.open`).
/// Access is gated by the per-pane tokens carried in each URL's path, resolved against
/// live state per request — so no token needs capturing at startup.
pub fn start_file_server(state: AppState) -> Result<FileServerInfo, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|err| format!("failed to bind file server: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("failed to read file server address: {err}"))?
        .port();

    thread::spawn(move || {
        let limiter = ConnectionLimiter::new(MAX_CONCURRENT_CONNECTIONS);
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            };
            let slot = limiter.acquire();
            let state = state.clone();
            thread::spawn(move || {
                let _slot = slot;
                handle_connection(&state, stream);
            });
        }
    });

    Ok(FileServerInfo { port })
}

/// Builds the loopback URL for an absolute file path. `abs_path` must be absolute
/// (start with `/`), so the encoded form sits directly after the token.
pub fn file_url(port: u16, token: &str, abs_path: &Path) -> String {
    format!(
        "http://127.0.0.1:{port}/{token}{}",
        percent_encode_path(&abs_path.to_string_lossy())
    )
}

/// Canonicalizes `requested` (resolving `..` and symlinks) and returns it only if it
/// lives under one of `roots`. This is the trust boundary for what may be served.
pub fn resolve_under_roots(requested: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(requested).ok()?;
    for root in roots {
        if let Ok(root_canonical) = std::fs::canonicalize(root)
            && canonical.starts_with(&root_canonical)
        {
            return Some(canonical);
        }
    }
    None
}

/// Resolve one of the exact canonical files granted to a pane. This is kept
/// separate from directory roots so a visualization under qmux's private
/// workspace metadata does not grant the preview token access to sibling
/// sessions, state, or credentials.
pub fn resolve_exact_file(requested: &Path, granted: &[PathBuf]) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(requested).ok()?;
    granted
        .iter()
        .any(|allowed| allowed == &canonical)
        .then_some(canonical)
}

/// Resolves the encoded path portion of a token-bearing preview URL back to the
/// canonical source file it is currently authorized to serve. External-open
/// commands use this instead of handing the capability-bearing localhost URL to
/// another application.
pub fn resolve_tokenized_file_path(
    state: &AppState,
    encoded_url_path: &str,
) -> Result<PathBuf, String> {
    let after_root = encoded_url_path
        .strip_prefix('/')
        .ok_or_else(|| "invalid qmux preview URL".to_string())?;
    let slash = after_root
        .find('/')
        .ok_or_else(|| "invalid qmux preview URL".to_string())?;
    let (token, encoded_path) = after_root.split_at(slash);
    let pane_id = state
        .pane_for_file_token(token)
        .ok_or_else(|| "qmux preview URL is no longer authorized".to_string())?;
    let decoded = percent_decode(encoded_path)
        .ok_or_else(|| "invalid path encoding in qmux preview URL".to_string())?;
    let requested = Path::new(&decoded);
    let roots = state.pane_file_roots(&pane_id);
    let grants = state.pane_file_preview_grants(&pane_id);
    resolve_under_roots(requested, &roots)
        .or_else(|| resolve_exact_file(requested, &grants))
        .ok_or_else(|| "qmux preview file is no longer authorized".to_string())
}

fn valid_codex_inline_vis_filename(file: &str) -> bool {
    let Some(stem) = file.strip_suffix(".html") else {
        return false;
    };
    !stem.is_empty()
        && stem.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn valid_visualization_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn dated_directories(root: &Path, digits: usize) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(root).map_err(|err| {
        format!(
            "failed to read visualization directory {}: {err}",
            root.display()
        )
    })?;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to inspect visualization directory {}: {err}",
                root.display()
            )
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() == digits
            && name.bytes().all(|byte| byte.is_ascii_digit())
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            directories.push(entry.path());
        }
    }
    directories.sort();
    Ok(directories)
}

/// Resolve the file named by a `codex-inline-vis` directive inside the owning
/// Codex thread's dated visualization directory. The directive supplies only a
/// contract-valid basename; the trusted pane session supplies the directory.
pub fn resolve_codex_inline_visualization(
    visualization_root: &Path,
    session_id: &str,
    file: &str,
) -> Result<PathBuf, String> {
    if !valid_visualization_session_id(session_id) {
        return Err("the attached Codex session has an invalid id".to_string());
    }
    if !valid_codex_inline_vis_filename(file) {
        return Err(format!("invalid codex-inline-vis file '{file}'"));
    }
    let canonical_root = fs::canonicalize(visualization_root).map_err(|_| {
        format!(
            "Codex visualization storage {} was not found",
            visualization_root.display()
        )
    })?;
    let mut matches = Vec::new();
    for year in dated_directories(&canonical_root, 4)? {
        for month in dated_directories(&year, 2)? {
            for day in dated_directories(&month, 2)? {
                let candidate = day.join(session_id).join(file);
                let Ok(canonical) = fs::canonicalize(&candidate) else {
                    continue;
                };
                if canonical.starts_with(&canonical_root) && canonical.is_file() {
                    matches.push(canonical);
                }
            }
        }
    }
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(format!(
            "Codex visualization '{file}' was not found for session {session_id}"
        )),
        _ => Err(format!(
            "Codex visualization '{file}' is ambiguous for session {session_id}"
        )),
    }
}

struct RequestHead {
    method: String,
    target: String,
    range: Option<String>,
    host: Option<String>,
}

struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn error(status: u16, reason: &'static str) -> Self {
        let mut response = Self::new(status, reason);
        response.header("Content-Length", "0");
        response
    }

    fn header(&mut self, key: &str, value: &str) {
        self.headers.push((key.to_string(), value.to_string()));
    }
}

fn handle_connection(state: &AppState, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CONNECTION_READ_TIMEOUT));
    let Some(head) = read_request_head(&stream) else {
        return;
    };
    let response = build_response(state, &head);
    let _ = write_response(&mut stream, response);
}

fn read_request_head(stream: &TcpStream) -> Option<RequestHead> {
    let cloned = stream.try_clone().ok()?;
    // Bound the total request-head bytes: once the cap is hit, reads return EOF and the
    // line below sees a truncated request, failing the parse and closing the connection.
    let mut reader = BufReader::new(cloned.take(MAX_REQUEST_HEAD_BYTES));

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut range = None;
    let mut host = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let trimmed = header.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim();
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
            } else if name.eq_ignore_ascii_case("host") {
                host = Some(value.trim().to_string());
            }
        }
    }

    Some(RequestHead {
        method,
        target,
        range,
        host,
    })
}

/// Whether a `Host` header names a loopback address. A DNS-rebinding attack from a
/// remote page reaches the loopback port with the *attacker's* hostname in `Host`,
/// so rejecting a non-loopback host is cheap defense-in-depth on top of the token.
/// Legit overlay/curl requests use `127.0.0.1:<port>` or `localhost:<port>`.
fn is_loopback_host_header(host: &str) -> bool {
    // Strip the port, honoring the [ipv6]:port bracket form.
    let host = if let Some(after_bracket) = host.strip_prefix('[') {
        match after_bracket.split_once(']') {
            Some((inner, _)) => inner,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return v4.is_loopback();
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback();
    }
    false
}

fn query_parameter<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn build_response(state: &AppState, head: &RequestHead) -> Response {
    // Reject a non-loopback Host (DNS-rebinding defense-in-depth). A missing Host
    // (e.g. a bare HTTP/1.0 client) is allowed — the per-pane token still gates access
    // — but a browser rebinding attack always carries the attacker's hostname here.
    if let Some(host) = &head.host
        && !is_loopback_host_header(host)
    {
        return Response::error(403, "Forbidden");
    }
    if head.method != "GET" && head.method != "HEAD" {
        return Response::error(405, "Method Not Allowed");
    }
    let is_head = head.method == "HEAD";

    // Split the query string off the target (dropping any fragment) before routing.
    // `?raw=1` opts a Markdown file out of the HTML rendering below.
    let without_fragment = head.target.split('#').next().unwrap_or("");
    let (path, query) = match without_fragment.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (without_fragment, None),
    };
    // These public, bundled assets contain no pane data and need no capability token.
    // Rendered Markdown lives in an opaque-origin iframe, so allow font CORS explicitly.
    if let Some(response) = embedded_font_response(path, is_head) {
        return response;
    }
    let raw_requested = query.is_some_and(|q| q.split('&').any(|p| p == "raw" || p == "raw=1"));
    let codex_inline_vis_requested = query.is_some_and(|q| {
        q.split('&')
            .any(|p| p == "codex-inline-vis" || p == "codex-inline-vis=1")
    });
    let body_font_id = query_parameter(query, "qmux-body-font");
    // The path is "/<token>/<abs path>": the first segment is the per-pane token, and
    // everything from the next '/' onward is the percent-encoded absolute path (with
    // its leading slash preserved). Tokens are hex, so they never contain a slash.
    let Some(after_root) = path.strip_prefix('/') else {
        return Response::error(404, "Not Found");
    };
    let Some(slash) = after_root.find('/') else {
        return Response::error(404, "Not Found");
    };
    let (token, encoded_path) = after_root.split_at(slash);
    // Resolve the token to its pane and serve only that pane's roots, so a URL minted
    // for one pane can never read another pane's files. An unknown token is an opaque
    // 404, indistinguishable from a missing route.
    let Some(pane_id) = state.pane_for_file_token(token) else {
        return Response::error(404, "Not Found");
    };
    let Some(decoded) = percent_decode(encoded_path) else {
        return Response::error(400, "Bad Request");
    };

    let roots = state.pane_file_roots(&pane_id);
    let grants = state.pane_file_preview_grants(&pane_id);
    let Some(canonical) = resolve_under_roots(Path::new(&decoded), &roots)
        .or_else(|| resolve_exact_file(Path::new(&decoded), &grants))
    else {
        // Either it doesn't exist or it isn't under an allowed root — same opaque 403
        // so the server isn't a probe for which paths exist.
        return Response::error(403, "Forbidden");
    };

    let Ok(file) = File::open(&canonical) else {
        return Response::error(404, "Not Found");
    };
    let Ok(meta) = file.metadata() else {
        return Response::error(404, "Not Found");
    };
    if meta.is_dir() {
        return Response::error(403, "Forbidden");
    }
    let total = meta.len();
    let content_type = mime_type(&canonical);
    // CSP for served content: the overlay already sandboxes it into an opaque origin
    // (so scripts can't read sibling responses cross-origin), and `connect-src 'none'`
    // closes the remaining channel — a hostile HTML file phoning the token home via
    // fetch/XHR/WebSocket/beacon. Passive subresources (a report's own CSS/JS/images)
    // still load, but only from this same file-server origin; nothing may talk to the
    // network. `state.file_server_port()` is always set once the server is serving.
    let csp = state.file_server_port().map(file_content_csp);

    // `codex-inline-vis` files are HTML fragments rather than standalone pages.
    // Supply the small host shell and theme tokens they expect, while retaining
    // the ordinary file preview's opaque-origin sandbox and no-network CSP.
    if codex_inline_vis_requested {
        if !canonical
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
        {
            return Response::error(400, "Bad Request");
        }
        if total > MAX_CODEX_INLINE_VIS_BYTES {
            return Response::error(413, "Payload Too Large");
        }
        let Ok(source) = read_slice(file, 0, total) else {
            return Response::error(500, "Internal Server Error");
        };
        let page = render_codex_inline_visualization_page(
            &canonical,
            &String::from_utf8_lossy(&source),
            body_font_id,
        );
        let mut response = Response::new(200, "OK");
        response.header("Content-Type", "text/html; charset=utf-8");
        response.header("Content-Length", &page.len().to_string());
        if let Some(csp) = csp {
            response.header("Content-Security-Policy", &csp);
        }
        if !is_head {
            response.body = page.into_bytes();
        }
        return response;
    }

    // Markdown is rendered into a styled HTML page at serve time (unless `?raw=1` opts
    // out), so the overlay shows a document instead of plain source. Rendering
    // transforms the entity, so byte offsets into the source are meaningless: Range is
    // ignored and the full page is served. A file over the inline cap falls through to
    // the plain-text path and its existing 413/Range flow.
    if !raw_requested && is_markdown(&canonical) && total <= MAX_INLINE_BYTES {
        let Ok(source) = read_slice(file, 0, total) else {
            return Response::error(500, "Internal Server Error");
        };
        let page =
            render_markdown_page(&canonical, &String::from_utf8_lossy(&source), body_font_id);
        let mut response = Response::new(200, "OK");
        response.header("Content-Type", "text/html; charset=utf-8");
        response.header("Content-Length", &page.len().to_string());
        // Rendered Markdown never needs script, so serve it under a CSP with no
        // `script-src` at all (it falls back to `default-src 'none'`). Raw HTML in the
        // Markdown passes through the renderer verbatim, so a hostile file could embed
        // `<script>`/`onerror` — dropping script execution entirely makes that inert
        // instead of relying solely on the overlay's opaque-origin sandbox.
        if let Some(port) = state.file_server_port() {
            response.header("Content-Security-Policy", &markdown_page_csp(port));
        }
        if !is_head {
            response.body = page.into_bytes();
        }
        return response;
    }

    // Source-code files get the same serve-time rendering treatment: their
    // mime would be application/octet-stream, which the overlay's webview
    // cannot display at all. The rendered page is escaped text with line
    // anchors (so `#L<n>` fragments work) and carries the no-script Markdown
    // CSP. `?raw=1` opts out; oversized files fall through unchanged.
    if !raw_requested && is_renderable_source(&canonical) && total <= MAX_INLINE_BYTES {
        let Ok(source) = read_slice(file, 0, total) else {
            return Response::error(500, "Internal Server Error");
        };
        let page = render_source_page(&canonical, &String::from_utf8_lossy(&source));
        let mut response = Response::new(200, "OK");
        response.header("Content-Type", "text/html; charset=utf-8");
        response.header("Content-Length", &page.len().to_string());
        if let Some(port) = state.file_server_port() {
            response.header("Content-Security-Policy", &markdown_page_csp(port));
        }
        if !is_head {
            response.body = page.into_bytes();
        }
        return response;
    }

    if let Some(range_raw) = &head.range {
        let Some((start, requested_end)) = parse_range(range_raw, total) else {
            let mut response = Response::error(416, "Range Not Satisfiable");
            response.header("Content-Range", &format!("bytes */{total}"));
            return response;
        };
        // Cap how much a single range response buffers. Without this, `Range: bytes=0-`
        // on a huge file allocates the whole file in one Vec — bypassing MAX_INLINE_BYTES
        // (which only guards the non-range path). Serving fewer bytes than requested is a
        // valid 206; a client that wants the rest issues the next range from `end + 1`.
        let end = cap_range_end(start, requested_end, MAX_INLINE_BYTES);
        let len = end - start + 1;
        let body = if is_head {
            Vec::new()
        } else {
            match read_slice(file, start, len) {
                Ok(bytes) => bytes,
                Err(_) => return Response::error(500, "Internal Server Error"),
            }
        };
        let mut response = Response::new(206, "Partial Content");
        response.header("Content-Type", &content_type);
        response.header("Content-Length", &len.to_string());
        response.header("Accept-Ranges", "bytes");
        response.header("Content-Range", &format!("bytes {start}-{end}/{total}"));
        if let Some(csp) = &csp {
            response.header("Content-Security-Policy", csp);
        }
        response.body = body;
        return response;
    }

    if total > MAX_INLINE_BYTES {
        // Force the client to range-request a file this large rather than buffering it.
        let mut response = Response::error(413, "Payload Too Large");
        response.header("Accept-Ranges", "bytes");
        return response;
    }
    let body = if is_head {
        Vec::new()
    } else {
        match read_slice(file, 0, total) {
            Ok(bytes) => bytes,
            Err(_) => return Response::error(500, "Internal Server Error"),
        }
    };
    let mut response = Response::new(200, "OK");
    response.header("Content-Type", &content_type);
    response.header("Content-Length", &total.to_string());
    response.header("Accept-Ranges", "bytes");
    if let Some(csp) = &csp {
        response.header("Content-Security-Policy", csp);
    }
    response.body = body;
    response
}

fn embedded_font_response(path: &str, is_head: bool) -> Option<Response> {
    let bytes = match path {
        DM_SANS_ROMAN_LATIN_PATH => DM_SANS_ROMAN_LATIN,
        DM_SANS_ROMAN_LATIN_EXT_PATH => DM_SANS_ROMAN_LATIN_EXT,
        DM_SANS_ITALIC_LATIN_PATH => DM_SANS_ITALIC_LATIN,
        DM_SANS_ITALIC_LATIN_EXT_PATH => DM_SANS_ITALIC_LATIN_EXT,
        VALLEY_SANS_ROMAN_PATH => VALLEY_SANS_ROMAN,
        VALLEY_SANS_ITALIC_PATH => VALLEY_SANS_ITALIC,
        _ => return None,
    };
    let mut response = Response::new(200, "OK");
    response.header("Content-Type", "font/woff2");
    response.header("Content-Length", &bytes.len().to_string());
    response.header("Cache-Control", "public, max-age=31536000, immutable");
    response.header("Access-Control-Allow-Origin", "*");
    if !is_head {
        response.body.extend_from_slice(bytes);
    }
    Some(response)
}

/// CSP applied to every served file. Served files always come back from
/// `http://127.0.0.1:<port>` (see `file_url`), so passive subresources are pinned to
/// that exact origin — a report's sibling CSS/JS/images/fonts render, but the document
/// cannot reach any other host. `connect-src 'none'` blocks all scripted network egress
/// (the token-exfiltration channel), and `object-src`/`base-uri`/`form-action` are
/// locked down for good measure. Inline scripts/styles are permitted because a served
/// report legitimately carries its own, and the sandbox opaque origin already contains
/// what they can read.
fn file_content_csp(port: u16) -> String {
    let origin = format!("http://127.0.0.1:{port}");
    format!(
        "default-src 'none'; \
         script-src 'unsafe-inline' {origin}; \
         style-src 'unsafe-inline' {origin}; \
         img-src data: blob: {origin}; \
         font-src data: {origin}; \
         media-src blob: {origin}; \
         connect-src 'none'; \
         object-src 'none'; \
         base-uri 'none'; \
         form-action 'none'"
    )
}

/// CSP for *rendered Markdown* pages. Identical to [`file_content_csp`] but with no
/// `script-src` directive, so it falls back to `default-src 'none'` and blocks all
/// script execution. The styled Markdown template carries only inline styles (allowed
/// below) and no script, and raw HTML embedded in the source passes through the
/// renderer verbatim — so omitting `script-src` turns any embedded `<script>` into
/// inert markup, a second line of defense alongside the overlay's opaque-origin
/// sandbox rather than the sole one.
fn markdown_page_csp(port: u16) -> String {
    let origin = format!("http://127.0.0.1:{port}");
    format!(
        "default-src 'none'; \
         style-src 'unsafe-inline' {origin}; \
         img-src data: blob: {origin}; \
         font-src data: {origin}; \
         media-src blob: {origin}; \
         connect-src 'none'; \
         object-src 'none'; \
         base-uri 'none'; \
         form-action 'none'"
    )
}

fn write_response(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, response.reason);
    for (key, value) in &response.headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    // Don't let a text file be MIME-sniffed into HTML, and never leak the token-bearing
    // URL in a Referer when served content fetches something. (The overlay also
    // sandboxes file content into an opaque origin — see BrowserOverlay.)
    head.push_str("X-Content-Type-Options: nosniff\r\n");
    head.push_str("Referrer-Policy: no-referrer\r\n");
    // One request per connection keeps the hand-rolled server simple and correct.
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    if !response.body.is_empty() {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}

/// Clamps a requested inclusive range end so one response never serves more than `cap`
/// bytes starting at `start`. Returns the end actually served (≤ `requested_end`).
fn cap_range_end(start: u64, requested_end: u64, cap: u64) -> u64 {
    requested_end.min(start.saturating_add(cap - 1))
}

fn read_slice(mut file: File, start: u64, len: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = vec![0_u8; len as usize];
    file.read_exact(&mut buffer)?;
    Ok(buffer)
}

/// Parses a single-range `Range: bytes=...` value against a known total length.
/// Returns the inclusive `(start, end)` byte range, or `None` if unsatisfiable.
fn parse_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    let spec = raw.trim().strip_prefix("bytes=")?;
    // Only the first range of a (possibly multi-range) request is honored.
    let spec = spec.split(',').next()?.trim();
    let (start_str, end_str) = spec.split_once('-')?;

    if start_str.is_empty() {
        // Suffix range: the last N bytes.
        let suffix: u64 = end_str.parse().ok()?;
        if suffix == 0 || total == 0 {
            return None;
        }
        return Some((total.saturating_sub(suffix), total - 1));
    }

    let start: u64 = start_str.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_str.is_empty() {
        total - 1
    } else {
        end_str.parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown")
    )
}

/// Source-code files rendered as a line-numbered HTML page. Deliberately
/// limited to types the mime map would otherwise serve as
/// `application/octet-stream` — which the embedded browser cannot display —
/// so everything that already renders (html, js, css, txt, images, …) keeps
/// its current serving, including as subresources of served HTML reports.
fn is_renderable_source(path: &Path) -> bool {
    if matches!(
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_ascii_lowercase())
            .as_deref(),
        Some("makefile" | "dockerfile" | "justfile")
    ) {
        return true;
    }
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some(
            "rs" | "ts"
                | "tsx"
                | "jsx"
                | "py"
                | "go"
                | "c"
                | "h"
                | "cc"
                | "cpp"
                | "cxx"
                | "hpp"
                | "hh"
                | "java"
                | "kt"
                | "kts"
                | "swift"
                | "m"
                | "mm"
                | "rb"
                | "php"
                | "cs"
                | "scala"
                | "clj"
                | "cljs"
                | "ex"
                | "exs"
                | "erl"
                | "hs"
                | "ml"
                | "mli"
                | "lua"
                | "pl"
                | "pm"
                | "r"
                | "jl"
                | "zig"
                | "nim"
                | "dart"
                | "sql"
                | "proto"
                | "graphql"
                | "gql"
                | "vue"
                | "svelte"
                | "astro"
                | "tf"
                | "hcl"
                | "nix"
                | "cmake"
                | "gradle"
                | "groovy"
                | "ps1"
                | "bat"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "awk"
                | "sed"
                | "diff"
                | "patch"
        )
    )
}

/// Inline stylesheet for rendered source files. Explicit per-scheme colors for
/// the same reason as MARKDOWN_PAGE_CSS: the overlay's sandboxed frame has a
/// transparent canvas. `tr:target` highlights the line addressed by a `#L<n>`
/// fragment (minted from grep-style `path:line` links); scroll-margin keeps
/// that line away from the very top edge. The line-number cells are
/// unselectable so copying code doesn't drag the gutter along.
const SOURCE_PAGE_CSS: &str = "\
:root { color-scheme: light dark; }\
body { margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; line-height: 1.5; background: #ffffff; color: #1f2328; }\
@media (prefers-color-scheme: dark) { body { background: #1e2227; color: #e2e6ea; } }\
table { border-collapse: collapse; width: 100%; }\
td { padding: 0 0.75em 0 0; vertical-align: top; }\
td.ln { position: sticky; left: 0; min-width: 3.5em; padding: 0 0.75em; text-align: right; color: rgba(127, 127, 127, 0.8); background: inherit; user-select: none; -webkit-user-select: none; }\
td.code { white-space: pre; }\
td.code:empty::before { content: '\\a0'; }\
tr { scroll-margin-top: 35vh; background: inherit; }\
tr:target { background: rgba(255, 208, 90, 0.35); }\
@media (prefers-color-scheme: dark) { tr:target { background: rgba(210, 170, 60, 0.25); } }\
main { padding: 0.75rem 0; overflow-x: auto; }";

/// Renders a source file into a line-numbered standalone HTML page with
/// `L<n>` row anchors, so a `#L8` fragment scrolls to and highlights line 8.
/// Escaped text only — served under the same no-script CSP as rendered
/// Markdown.
fn render_source_page(path: &Path, source: &str) -> String {
    let title = escape_html(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Source"),
    );
    let mut rows = String::with_capacity(source.len() * 2);
    for (index, line) in source.lines().enumerate() {
        let number = index + 1;
        rows.push_str("<tr id=\"L");
        rows.push_str(&number.to_string());
        rows.push_str("\"><td class=\"ln\">");
        rows.push_str(&number.to_string());
        rows.push_str("</td><td class=\"code\">");
        rows.push_str(&escape_html(line));
        rows.push_str("</td></tr>\n");
    }
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{SOURCE_PAGE_CSS}</style>\n</head>\n\
         <body>\n<main>\n<table>\n{rows}</table>\n</main>\n</body>\n</html>\n"
    )
}

// Minimal self-contained host surface for the HTML fragments emitted through
// `::codex-inline-vis`. Fragment-specific CSS owns the visualization itself;
// these are the shared theme and utility classes the Codex contract expects.
// External resources deliberately remain blocked by file_content_csp: unlike
// Codex's standalone renderer, this page's URL carries a pane capability token.
const CODEX_INLINE_VIS_CSS: &str = "\
__QMUX_FONT_FACE__\
:root { color-scheme: light dark; --font-size-base: 14px; --background: #f7f8f7; --foreground: #1d2421; --card: #ffffff; --card-foreground: #1d2421; --popover: #ffffff; --popover-foreground: #1d2421; --primary: #26322d; --primary-foreground: #ffffff; --secondary: #e8eeeb; --secondary-foreground: #1d2421; --muted: #edf1ef; --muted-foreground: #5e6d66; --accent: #e1e9e5; --accent-foreground: #1d2421; --destructive: #a63d40; --border: #ccd6d1; --input: #aebcb5; --ring: #42554c; --viz-series-1: #187a54; --viz-series-2: #8a5d15; --viz-series-3: #496aa0; --viz-series-4: #8a4e86; --viz-series-5: #ad4e35; --viz-series-6: #4f7777; }\
@media (prefers-color-scheme: dark) { :root { --background: #111514; --foreground: #e5e9e7; --card: #181d1b; --card-foreground: #e5e9e7; --popover: #202624; --popover-foreground: #e5e9e7; --primary: #d7dfdb; --primary-foreground: #111514; --secondary: #29302d; --secondary-foreground: #e5e9e7; --muted: #242b28; --muted-foreground: #99a49f; --accent: #303936; --accent-foreground: #eef2f0; --destructive: #d06b6b; --border: #36403c; --input: #46514c; --ring: #a9b8b1; --viz-series-1: #8fd5b6; --viz-series-2: #d8b77a; --viz-series-3: #8eacd8; --viz-series-4: #c595c2; --viz-series-5: #df927d; --viz-series-6: #8eb8b8; } }\
* { box-sizing: border-box; }\
html, body { min-height: 100%; }\
body { margin: 0; padding: 16px; color: var(--foreground); background: var(--background); font-family: __QMUX_BODY_FONT__; font-size: var(--font-size-base); font-variant-ligatures: no-common-ligatures; }\
button, input, select, textarea { font: inherit; }\
svg, canvas, img { max-width: 100%; }\
.card { border: 1px solid var(--border); border-radius: 10px; color: var(--card-foreground); background: var(--card); }\
.viz-row, .viz-controls { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }\
.viz-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 180px), 1fr)); gap: 10px; }\
.viz-stat { padding: 12px; }\
.viz-stat-value { color: var(--foreground); font-size: 1.4em; font-weight: 500; }\
.viz-badge { display: inline-flex; align-items: center; min-height: 22px; padding: 2px 7px; border-radius: 999px; color: var(--accent-foreground); background: var(--accent); }\
.btn { display: inline-flex; align-items: center; justify-content: center; gap: 6px; min-height: 30px; padding: 5px 10px; border: 1px solid var(--border); border-radius: 7px; color: var(--secondary-foreground); background: var(--secondary); cursor: pointer; }\
.btn:not(:disabled):hover { border-color: var(--input); background: var(--accent); }\
.btn-primary { border-color: var(--primary); color: var(--primary-foreground); background: var(--primary); }\
.btn-ghost { border-color: transparent; color: var(--muted-foreground); background: transparent; }\
.btn-block { width: 100%; }\
.btn:disabled { cursor: not-allowed; opacity: 0.4; }\
.viz-tile { width: 100%; min-height: 52px; }\
.form-label { display: block; margin-bottom: 6px; color: var(--foreground); }\
.form-control, .form-select { display: block; width: 100%; min-height: 30px; padding: 4px 8px; border: 1px solid var(--input); border-radius: 7px; color: var(--foreground); background: var(--secondary); }\
.form-check { display: flex; align-items: center; gap: 7px; }\
.table-responsive { width: 100%; overflow-x: auto; }\
.table { width: 100%; border-collapse: collapse; }\
.table th, .table td { padding: 8px 10px; border-bottom: 1px solid var(--border); text-align: left; }\
.table-sm th, .table-sm td { padding: 5px 7px; }\
.text-small { font-size: max(11px, 0.85em); }\
.text-muted { color: var(--muted-foreground); }\
.text-destructive { color: var(--destructive); }\
.text-end { text-align: end !important; font-variant-numeric: tabular-nums; }\
.text-center { text-align: center !important; }\
.text-nowrap { white-space: nowrap; }\
.sr-only { position: absolute; width: 1px; height: 1px; margin: -1px; padding: 0; overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0; }\
[data-lucide] { display: inline-flex; width: 16px; height: 16px; align-items: center; justify-content: center; font-style: normal; }\
[data-lucide]:empty::before { content: '\\25c7'; font-size: 11px; }\
:focus-visible { outline: 2px solid var(--ring); outline-offset: 2px; }";

fn render_codex_inline_visualization_page(
    path: &Path,
    source: &str,
    body_font_id: Option<&str>,
) -> String {
    let title = escape_html(
        path.file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("Codex visualization"),
    );
    let css = CODEX_INLINE_VIS_CSS
        .replace("__QMUX_FONT_FACE__", markdown_font_face_css(body_font_id))
        .replace("__QMUX_BODY_FONT__", markdown_body_font(body_font_id));
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{css}</style>\n</head>\n\
         <body>\n{source}\n</body>\n</html>\n"
    )
}

/// Inline stylesheet for rendered Markdown. Background and text colors are set
/// explicitly per scheme (not left to UA defaults): the overlay loads this page in a
/// sandboxed iframe whose canvas is transparent, so UA-default dark-scheme text would
/// float over whatever backdrop the app has — white-on-white in practice. Translucent
/// grays handle the accents in both themes, and the file CSP already allows inline
/// styles.
const MARKDOWN_PAGE_CSS: &str = "\
__QMUX_FONT_FACE__\
:root { color-scheme: light dark; }\
body { margin: 0; font-family: __QMUX_BODY_FONT__; font-variant-ligatures: no-common-ligatures; line-height: 1.6; background: #ffffff; color: #1f2328; }\
@media (prefers-color-scheme: dark) { body { background: #1e2227; color: #e2e6ea; } }\
main { max-width: 48rem; margin: 0 auto; padding: 2rem 1.5rem 4rem; }\
h1, h2 { border-bottom: 1px solid rgba(127, 127, 127, 0.3); padding-bottom: 0.3em; }\
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; background: rgba(127, 127, 127, 0.15); padding: 0.1em 0.3em; border-radius: 4px; }\
pre { background: rgba(127, 127, 127, 0.12); padding: 0.75rem 1rem; border-radius: 6px; overflow-x: auto; }\
pre code { background: none; padding: 0; font-size: 0.85em; }\
blockquote { margin-left: 0; padding-left: 1em; border-left: 3px solid rgba(127, 127, 127, 0.4); opacity: 0.85; }\
table { border-collapse: collapse; display: block; overflow-x: auto; }\
th, td { border: 1px solid rgba(127, 127, 127, 0.35); padding: 0.35em 0.7em; }\
img { max-width: 100%; }\
hr { border: none; border-top: 1px solid rgba(127, 127, 127, 0.3); }";

const VALLEY_SANS_MARKDOWN_FONT_FACE_CSS: &str = "\
@font-face { font-family: 'Valley Sans'; src: url('/__qmux/fonts/ValleySans-Variable.woff2') format('woff2'); font-style: normal; font-weight: 100 900; font-display: swap; }\
@font-face { font-family: 'Valley Sans'; src: url('/__qmux/fonts/ValleySans-VariableItalic.woff2') format('woff2'); font-style: italic; font-weight: 100 900; font-display: swap; }";

const DM_SANS_MARKDOWN_FONT_FACE_CSS: &str = "\
@font-face { font-family: 'DM Sans'; src: url('/__qmux/fonts/DMSans-Variable-LatinExt.woff2') format('woff2'); font-style: normal; font-weight: 100 1000; font-display: swap; unicode-range: U+0100-02BA, U+02BD-02C5, U+02C7-02CC, U+02CE-02D7, U+02DD-02FF, U+0304, U+0308, U+0329, U+1D00-1DBF, U+1E00-1E9F, U+1EF2-1EFF, U+2020, U+20A0-20AB, U+20AD-20C0, U+2113, U+2C60-2C7F, U+A720-A7FF; }\
@font-face { font-family: 'DM Sans'; src: url('/__qmux/fonts/DMSans-Variable-Latin.woff2') format('woff2'); font-style: normal; font-weight: 100 1000; font-display: swap; unicode-range: U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, U+0304, U+0308, U+0329, U+2000-206F, U+20AC, U+2122, U+2191, U+2193, U+2212, U+2215, U+FEFF, U+FFFD; }\
@font-face { font-family: 'DM Sans'; src: url('/__qmux/fonts/DMSans-VariableItalic-LatinExt.woff2') format('woff2'); font-style: italic; font-weight: 100 1000; font-display: swap; unicode-range: U+0100-02BA, U+02BD-02C5, U+02C7-02CC, U+02CE-02D7, U+02DD-02FF, U+0304, U+0308, U+0329, U+1D00-1DBF, U+1E00-1E9F, U+1EF2-1EFF, U+2020, U+20A0-20AB, U+20AD-20C0, U+2113, U+2C60-2C7F, U+A720-A7FF; }\
@font-face { font-family: 'DM Sans'; src: url('/__qmux/fonts/DMSans-VariableItalic-Latin.woff2') format('woff2'); font-style: italic; font-weight: 100 1000; font-display: swap; unicode-range: U+0000-00FF, U+0131, U+0152-0153, U+02BB-02BC, U+02C6, U+02DA, U+02DC, U+0304, U+0308, U+0329, U+2000-206F, U+20AC, U+2122, U+2191, U+2193, U+2212, U+2215, U+FEFF, U+FFFD; }";

fn markdown_font_face_css(font_id: Option<&str>) -> &'static str {
    match font_id {
        Some("dm-sans") => DM_SANS_MARKDOWN_FONT_FACE_CSS,
        Some("valley-sans") => VALLEY_SANS_MARKDOWN_FONT_FACE_CSS,
        _ => "",
    }
}

fn markdown_body_font(font_id: Option<&str>) -> &'static str {
    match font_id {
        Some("inter") => {
            "'Inter', ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif"
        }
        Some("dm-sans") => {
            "'DM Sans', ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif"
        }
        Some("anthropic-sans-text") => {
            "'Anthropic Sans Text', ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif"
        }
        Some("valley-sans") => {
            "'Valley Sans', ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif"
        }
        _ => "ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
    }
}

/// Renders Markdown source into a complete standalone HTML page. Raw HTML embedded in
/// the Markdown passes through untouched: the overlay's sandbox + CSP were designed to
/// contain fully hostile served HTML files, so rendered Markdown gets the same
/// containment rather than a sanitizer.
fn render_markdown_page(path: &Path, source: &str, body_font_id: Option<&str>) -> String {
    use pulldown_cmark::{Options, Parser, html};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let mut body = String::with_capacity(source.len() * 2);
    html::push_html(&mut body, Parser::new_ext(source, options));

    let title = escape_html(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Markdown"),
    );
    let markdown_page_css = MARKDOWN_PAGE_CSS
        .replace("__QMUX_FONT_FACE__", markdown_font_face_css(body_font_id))
        .replace("__QMUX_BODY_FONT__", markdown_body_font(body_font_id));
    format!(
        "<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n<style>{markdown_page_css}</style>\n</head>\n\
         <body>\n<main>\n{body}</main>\n</body>\n</html>\n"
    )
}

/// Escapes text for interpolation into HTML (the page `<title>`).
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn mime_type(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "txt" | "log" | "md" | "markdown" | "csv" | "xml" | "yaml" | "yml" | "toml" => {
            "text/plain; charset=utf-8"
        }
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Whether the protected browser surface has an explicit rendering type for a
/// local file. Renderable source files count: their mime stays
/// `application/octet-stream`, but the server renders them into line-anchored
/// HTML pages. Everything else that would only trigger a download should be
/// revealed in the OS file manager instead.
pub(crate) fn is_browser_previewable_path(path: &Path) -> bool {
    mime_type(path) != "application/octet-stream" || is_renderable_source(path)
}

/// Percent-encodes a path, leaving `/` (the separator) and the RFC 3986 unreserved
/// set intact so the encoded form is a normal multi-segment URL path.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push(hi << 4 | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PaneBackend, PaneInfo, PaneKind, PaneRuntime, PaneStatus};
    use crate::workspace::{GroupInfo, WorkspaceScope};
    use portable_pty::{Child, ChildKiller, ExitStatus, PtySize, native_pty_system};
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct FakeChild;

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[test]
    fn markdown_page_csp_blocks_scripts() {
        let csp = markdown_page_csp(12345);
        // No script-src at all → falls back to default-src 'none', so an embedded
        // <script> in a rendered Markdown file cannot execute.
        assert!(
            !csp.contains("script-src"),
            "rendered markdown CSP must not grant script execution: {csp}"
        );
        assert!(csp.contains("default-src 'none'"), "{csp}");
        assert!(csp.contains("style-src 'unsafe-inline'"), "{csp}");
        assert!(csp.contains("connect-src 'none'"), "{csp}");
        // The general file-content CSP, by contrast, still permits (contained) inline
        // script for self-hosted reports.
        assert!(file_content_csp(12345).contains("script-src 'unsafe-inline'"));
    }

    #[test]
    fn browser_previewability_is_an_explicit_mime_allowlist() {
        assert!(is_browser_previewable_path(Path::new("report.HTML")));
        assert!(is_browser_previewable_path(Path::new("notes.md")));
        assert!(is_browser_previewable_path(Path::new("diagram.svg")));
        // Renderable source files are previewable via the line-anchored page.
        assert!(is_browser_previewable_path(Path::new("session.rs")));
        assert!(is_browser_previewable_path(Path::new("Makefile")));
        assert!(!is_browser_previewable_path(Path::new("release.dmg")));
        assert!(!is_browser_previewable_path(Path::new("installer.pkg")));
        assert!(!is_browser_previewable_path(Path::new("archive.zip")));
        assert!(!is_browser_previewable_path(Path::new("unknown")));
    }

    #[test]
    fn embedded_body_fonts_are_cacheable_and_cors_readable() {
        let response = embedded_font_response(DM_SANS_ROMAN_LATIN_PATH, false).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, DM_SANS_ROMAN_LATIN);
        assert!(
            response
                .headers
                .contains(&("Content-Type".to_string(), "font/woff2".to_string()))
        );
        assert!(
            response
                .headers
                .contains(&("Access-Control-Allow-Origin".to_string(), "*".to_string()))
        );

        let head = embedded_font_response(DM_SANS_ITALIC_LATIN_EXT_PATH, true).unwrap();
        assert!(head.body.is_empty());
        assert!(head.headers.contains(&(
            "Content-Length".to_string(),
            DM_SANS_ITALIC_LATIN_EXT.len().to_string()
        )));

        let valley = embedded_font_response(VALLEY_SANS_ROMAN_PATH, false).unwrap();
        assert_eq!(valley.body, VALLEY_SANS_ROMAN);
        assert!(embedded_font_response("/__qmux/fonts/unknown.woff2", false).is_none());
    }

    #[test]
    fn loopback_host_header_accepts_loopback_and_rejects_remote() {
        assert!(is_loopback_host_header("127.0.0.1:5173"));
        assert!(is_loopback_host_header("localhost:5173"));
        assert!(is_loopback_host_header("127.0.0.1"));
        assert!(is_loopback_host_header("LOCALHOST"));
        assert!(is_loopback_host_header("[::1]:5173"));
        assert!(is_loopback_host_header("127.9.9.9"));

        assert!(!is_loopback_host_header("evil.com"));
        assert!(!is_loopback_host_header("evil.com:5173"));
        assert!(!is_loopback_host_header("127.0.0.1.evil.com"));
        assert!(!is_loopback_host_header("0.0.0.0"));
        assert!(!is_loopback_host_header("192.168.1.5:5173"));
    }

    #[test]
    fn percent_round_trips_paths_with_spaces_and_specials() {
        let original = "/Users/x/my proj/réport (1).md";
        let encoded = percent_encode_path(original);
        assert!(!encoded.contains(' '));
        assert!(encoded.contains('/'));
        assert_eq!(percent_decode(&encoded).as_deref(), Some(original));
    }

    #[test]
    fn percent_decode_rejects_truncated_escapes() {
        assert!(percent_decode("%2").is_none());
        assert!(percent_decode("%zz").is_none());
    }

    #[test]
    fn parse_range_handles_open_closed_and_suffix() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        // End past the file is clamped.
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
        // Start past the end, or an empty file, is unsatisfiable.
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=-10", 0), None);
    }

    #[test]
    fn cap_range_end_limits_a_single_response_to_the_inline_cap() {
        // An open-ended range over a large total is capped to `cap` bytes from start.
        assert_eq!(cap_range_end(0, 999, 100), 99);
        assert_eq!(cap_range_end(50, 999, 100), 149);
        // A range already within the cap is served whole.
        assert_eq!(cap_range_end(0, 40, 100), 40);
        // Clamping saturates near u64::MAX rather than overflowing.
        assert_eq!(cap_range_end(u64::MAX - 1, u64::MAX, 100), u64::MAX);
    }

    #[test]
    fn rendered_markdown_uses_safe_prose_typography_and_known_body_font_stacks() {
        let path = Path::new("doc.md");
        let source = "# Hello";
        assert_eq!(
            query_parameter(Some("raw=1&qmux-body-font=inter"), "qmux-body-font"),
            Some("inter")
        );

        let default_page = render_markdown_page(path, source, None);
        assert!(default_page.contains("font-family: ui-sans-serif, system-ui"));
        assert!(default_page.contains("font-variant-ligatures: no-common-ligatures"));

        let selected_page = render_markdown_page(path, source, Some("anthropic-sans-text"));
        assert!(selected_page.contains("font-family: 'Anthropic Sans Text', ui-sans-serif"));
        assert!(!selected_page.contains("__QMUX_BODY_FONT__"));
        assert!(!selected_page.contains("ValleySans-Variable.woff2"));

        let dm_sans_page = render_markdown_page(path, source, Some("dm-sans"));
        assert!(dm_sans_page.contains("font-family: 'DM Sans', ui-sans-serif"));
        assert!(dm_sans_page.contains("DMSans-Variable-Latin.woff2"));
        assert!(dm_sans_page.contains("DMSans-Variable-LatinExt.woff2"));
        assert!(dm_sans_page.contains("DMSans-VariableItalic-Latin.woff2"));
        assert!(dm_sans_page.contains("DMSans-VariableItalic-LatinExt.woff2"));
        assert!(!dm_sans_page.contains("__QMUX_BODY_FONT__"));
        assert!(!dm_sans_page.contains("__QMUX_FONT_FACE__"));

        let valley_page = render_markdown_page(path, source, Some("valley-sans"));
        assert!(valley_page.contains("font-family: 'Valley Sans', ui-sans-serif"));
        assert!(valley_page.contains("ValleySans-Variable.woff2"));
        assert!(valley_page.contains("ValleySans-VariableItalic.woff2"));
        assert!(!valley_page.contains("__QMUX_BODY_FONT__"));
        assert!(!valley_page.contains("__QMUX_FONT_FACE__"));

        let unknown_page = render_markdown_page(path, source, Some("body{};color:red"));
        assert!(unknown_page.contains("font-family: ui-sans-serif, system-ui"));
        assert!(!unknown_page.contains("body{};color:red"));
    }

    /// Issues a GET and returns the full response head (status line + headers) and body.
    fn http_get_full(port: u16, path: &str, range: Option<&str>) -> (String, Vec<u8>) {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let method_path = path;
        let mut request = format!("GET {method_path} HTTP/1.1\r\nHost: localhost\r\n");
        if let Some(range) = range {
            request.push_str(&format!("Range: {range}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(raw.len());
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        (head, raw[split..].to_vec())
    }

    fn http_get(port: u16, path: &str, range: Option<&str>) -> (String, Vec<u8>) {
        let (head, body) = http_get_full(port, path, range);
        let status = head.lines().next().unwrap_or("").to_string();
        (status, body)
    }

    fn url_path(port: u16, token: &str, abs: &Path) -> String {
        file_url(port, token, abs)
            .strip_prefix(&format!("http://127.0.0.1:{port}"))
            .unwrap()
            .to_string()
    }

    /// Scratch space outside the production temp-directory allowlist, used by
    /// tests that need to distinguish a pane root from a forbidden sibling.
    fn non_temp_test_dir(label: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qmux-file-server-tests")
            .join(format!("{label}-{}", std::process::id()))
    }

    /// Builds an `AppState` with a live pane scoped to `root`, matching the
    /// production invariant required before a pane file token may serve files.
    fn test_state(root: &Path, base: &Path, pane_id: &str) -> AppState {
        use crate::config::{
            AdapterConfigs, ClaudeAdapterConfig, CodexAdapterConfig, GrokAdapterConfig,
            MuseAdapterConfig, OpencodeAdapterConfig, QmuxConfig,
        };
        let config = QmuxConfig {
            remotes: Default::default(),
            workspace_root: base.join("state"),
            socket_path: base.join("x.sock"),
            adapters: AdapterConfigs {
                acp: Default::default(),
                pi: Default::default(),
                claude: ClaudeAdapterConfig {
                    binary: Some("claude".to_string()),
                },
                codex: CodexAdapterConfig {
                    binary: Some("codex".to_string()),
                },
                opencode: OpencodeAdapterConfig {
                    binary: Some("opencode".to_string()),
                },
                grok: GrokAdapterConfig {
                    binary: Some("grok".to_string()),
                },
                muse: MuseAdapterConfig {
                    binary: Some("muse".to_string()),
                },
                cursor: Default::default(),
                devin: Default::default(),
            },
            legacy_claude_binary: None,
            claude_plugin_dir: PathBuf::new(),
            opencode_plugin_dir: PathBuf::new(),
            pi_extension_dir: PathBuf::new(),
            cursor_plugin_dir: PathBuf::new(),
        };
        let state = AppState::new(config);
        state
            .insert_group_after(
                GroupInfo {
                    id: "group-1".to_string(),
                    name: "group-1".to_string(),
                    name_override: None,
                    dir: root.display().to_string(),
                    managed_dir: base.join("managed").display().to_string(),
                    base_repo: None,
                    base_ref: None,
                    parent_id: None,
                    created_at: 1,
                    collapsed: false,
                    scope: WorkspaceScope::Terminal,
                    imported_research_archive_id: None,
                    remote: None,
                    agents: Vec::new(),
                },
                None,
            )
            .unwrap();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        drop(pair.slave);
        state
            .insert_pane(PaneRuntime {
                info: PaneInfo {
                    id: pane_id.to_string(),
                    title: "Shell".to_string(),
                    last_osc_title: None,
                    kind: PaneKind::Shell,
                    agent_id: None,
                    group_id: "group-1".to_string(),
                    cwd: root.display().to_string(),
                    cols: 80,
                    rows: 24,
                    status: PaneStatus::Running,
                    last_active_at: 1,
                    recovered: false,
                    depth: 0,
                },
                backend: PaneBackend::HostPty {
                    child: Arc::new(Mutex::new(Box::new(FakeChild))),
                    master: Arc::new(Mutex::new(pair.master)),
                    writer: Arc::new(Mutex::new(Box::new(io::sink()))),
                    backlog: Default::default(),
                    native_surface: false,
                },
            })
            .unwrap();
        state
    }

    #[test]
    fn serves_files_under_root_with_range_and_blocks_the_rest() {
        let base = non_temp_test_dir("serve");
        let root = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("hello.txt"), b"hello world").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();

        let state = test_state(&root, &base, "pane-1");
        let info = start_file_server(state.clone()).unwrap();
        let token = state.pane_file_token("pane-1").unwrap();

        let hello = std::fs::canonicalize(root.join("hello.txt")).unwrap();

        // Full GET returns the file.
        let (status, body) = http_get(info.port, &url_path(info.port, &token, &hello), None);
        assert!(status.contains("200"), "status: {status}");
        assert_eq!(body, b"hello world");

        // Range GET returns the requested slice with 206.
        let (status, body) = http_get(
            info.port,
            &url_path(info.port, &token, &hello),
            Some("bytes=0-4"),
        );
        assert!(status.contains("206"), "status: {status}");
        assert_eq!(body, b"hello");

        // A file outside every root is forbidden (even though it exists).
        let (status, _) = http_get(
            info.port,
            &url_path(info.port, &token, &outside.join("secret.txt")),
            None,
        );
        assert!(status.contains("403"), "status: {status}");

        // An unknown token can't reach any file.
        let correct = url_path(info.port, &token, &hello);
        let wrong = format!(
            "/deadbeef{}",
            correct.strip_prefix(&format!("/{token}")).unwrap()
        );
        let (status, _) = http_get(info.port, &wrong, None);
        assert!(status.contains("404"), "status: {status}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn renders_markdown_as_html_unless_raw_is_requested() {
        let base = std::env::temp_dir().join(format!("qmux-fs-md-{}", std::process::id()));
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let source = "# Hello\n\nSome *text* in a table:\n\n| a | b |\n| - | - |\n| 1 | 2 |\n";
        std::fs::write(root.join("doc.md"), source).unwrap();

        let state = test_state(&root, &base, "pane-md");
        let info = start_file_server(state.clone()).unwrap();
        let token = state.pane_file_token("pane-md").unwrap();
        let doc = std::fs::canonicalize(root.join("doc.md")).unwrap();
        let path = url_path(info.port, &token, &doc);

        // A plain GET returns a rendered HTML page.
        let (head, body) = http_get_full(info.port, &path, None);
        let body_text = String::from_utf8(body).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(head.contains("Content-Type: text/html"), "head: {head}");
        assert!(body_text.contains("<h1>Hello</h1>"), "body: {body_text}");
        assert!(body_text.contains("<table>"), "body: {body_text}");

        // `?raw=1` opts out and serves the source as plain text.
        let (head, body) = http_get_full(info.port, &format!("{path}?raw=1"), None);
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(head.contains("Content-Type: text/plain"), "head: {head}");
        assert_eq!(body, source.as_bytes());

        // Range on Markdown is ignored: the full rendered page comes back as a 200.
        let (head, body) = http_get_full(info.port, &path, Some("bytes=0-4"));
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(
            String::from_utf8(body).unwrap().contains("<h1>Hello</h1>"),
            "range response should carry the full rendered page"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn renders_source_files_as_line_anchored_html_unless_raw_is_requested() {
        let base = std::env::temp_dir().join(format!("qmux-fs-src-{}", std::process::id()));
        let root = base.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let source = "fn main() {\n    println!(\"<hello>\");\n}\n";
        std::fs::write(root.join("main.rs"), source).unwrap();

        let state = test_state(&root, &base, "pane-src");
        let info = start_file_server(state.clone()).unwrap();
        let token = state.pane_file_token("pane-src").unwrap();
        let file = std::fs::canonicalize(root.join("main.rs")).unwrap();
        let path = url_path(info.port, &token, &file);

        // A plain GET returns a rendered HTML page with per-line anchors and
        // escaped source text.
        let (head, body) = http_get_full(info.port, &path, None);
        let body_text = String::from_utf8(body).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(head.contains("Content-Type: text/html"), "head: {head}");
        assert!(body_text.contains("<tr id=\"L2\">"), "body: {body_text}");
        assert!(
            body_text.contains("println!(&quot;&lt;hello&gt;&quot;);"),
            "body: {body_text}"
        );
        assert!(!body_text.contains("<hello>"), "body: {body_text}");

        // `?raw=1` opts out and serves the bytes unrendered.
        let (head, body) = http_get_full(info.port, &format!("{path}?raw=1"), None);
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(
            head.contains("Content-Type: application/octet-stream"),
            "head: {head}"
        );
        assert_eq!(body, source.as_bytes());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolves_only_thread_scoped_codex_inline_visualizations() {
        let base =
            std::env::temp_dir().join(format!("qmux-fs-inline-vis-resolve-{}", std::process::id()));
        let root = base.join("visualizations");
        let thread = root
            .join("2026")
            .join("08")
            .join("08")
            .join("019fe2fe-2ef7-7aa2-a632-3f1d6d2bf391");
        std::fs::create_dir_all(&thread).unwrap();
        let visual = thread.join("artifact-tray-options.html");
        std::fs::write(&visual, "<div>preview</div>").unwrap();

        assert_eq!(
            resolve_codex_inline_visualization(
                &root,
                "019fe2fe-2ef7-7aa2-a632-3f1d6d2bf391",
                "artifact-tray-options.html",
            )
            .unwrap(),
            std::fs::canonicalize(&visual).unwrap()
        );
        assert!(
            resolve_codex_inline_visualization(
                &root,
                "019fe2fe-2ef7-7aa2-a632-3f1d6d2bf391",
                "../artifact.html",
            )
            .is_err()
        );
        assert!(resolve_codex_inline_visualization(&root, "../../other", "artifact.html").is_err());

        let duplicate = root
            .join("2026")
            .join("08")
            .join("09")
            .join("019fe2fe-2ef7-7aa2-a632-3f1d6d2bf391");
        std::fs::create_dir_all(&duplicate).unwrap();
        std::fs::write(duplicate.join("artifact-tray-options.html"), "duplicate").unwrap();
        assert!(
            resolve_codex_inline_visualization(
                &root,
                "019fe2fe-2ef7-7aa2-a632-3f1d6d2bf391",
                "artifact-tray-options.html",
            )
            .unwrap_err()
            .contains("ambiguous")
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn exact_preview_grant_serves_wrapped_inline_vis_but_not_siblings() {
        let base = non_temp_test_dir("inline-vis");
        let root = base.join("ws");
        let visuals = base.join("private-visuals");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&visuals).unwrap();
        let visual = visuals.join("preview.html");
        let sibling = visuals.join("other.html");
        std::fs::write(&visual, "<div class=\"card\">preview</div>").unwrap();
        std::fs::write(&sibling, "private sibling").unwrap();

        let state = test_state(&root, &base, "pane-vis");
        let canonical = state.grant_pane_file_preview("pane-vis", &visual).unwrap();
        let info = start_file_server(state.clone()).unwrap();
        state.set_file_server(info.port);
        let token = state.pane_file_token("pane-vis").unwrap();
        let path = format!(
            "{}?codex-inline-vis=1",
            url_path(info.port, &token, &canonical)
        );
        let (head, body) = http_get_full(info.port, &path, None);
        let body = String::from_utf8(body).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "head: {head}");
        assert!(head.contains("Content-Security-Policy"), "head: {head}");
        assert!(body.starts_with("<!doctype html>"), "body: {body}");
        assert!(body.contains("--background:"), "body: {body}");
        assert!(body.contains("<div class=\"card\">preview</div>"));

        let (status, _) = http_get(info.port, &url_path(info.port, &token, &sibling), None);
        assert!(status.contains("403"), "status: {status}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tokenized_url_paths_resolve_only_authorized_source_files() {
        let base = non_temp_test_dir("external-source");
        let root = base.join("ws");
        let outside = base.join("outside");
        let granted_dir = base.join("private-visuals");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&granted_dir).unwrap();
        let report = root.join("report with spaces.html");
        let secret = outside.join("secret.html");
        let granted = granted_dir.join("visual.html");
        std::fs::write(&report, "report").unwrap();
        std::fs::write(&secret, "secret").unwrap();
        std::fs::write(&granted, "visual").unwrap();

        let state = test_state(&root, &base, "pane-external");
        let token = state.pane_file_token("pane-external").unwrap();
        let report = std::fs::canonicalize(report).unwrap();
        let secret = std::fs::canonicalize(secret).unwrap();
        let granted = state
            .grant_pane_file_preview("pane-external", &granted)
            .unwrap();

        assert_eq!(
            resolve_tokenized_file_path(&state, &url_path(8123, &token, &report)).unwrap(),
            report
        );
        assert_eq!(
            resolve_tokenized_file_path(&state, &url_path(8123, &token, &granted)).unwrap(),
            granted
        );
        assert!(resolve_tokenized_file_path(&state, &url_path(8123, &token, &secret)).is_err());
        assert!(resolve_tokenized_file_path(&state, &url_path(8123, "unknown", &report)).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_under_roots_blocks_traversal_outside_roots() {
        let base = std::env::temp_dir().join(format!("qmux-fs-{}", std::process::id()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("ok.txt"), b"ok").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();

        let roots = vec![root.clone()];
        assert!(resolve_under_roots(&root.join("ok.txt"), &roots).is_some());
        // A path that resolves outside every root is rejected, including via `..`.
        assert!(resolve_under_roots(&outside.join("secret.txt"), &roots).is_none());
        assert!(resolve_under_roots(&root.join("../outside/secret.txt"), &roots).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}
