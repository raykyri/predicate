import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { isIP } from "node:net";
import { dirname, join, normalize, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ReactMarkdown from "react-markdown";
import {
  transcriptRehypePlugins,
  transcriptRemarkPlugins,
} from "../src/lib/markdownPlugins";
import {
  MAX_RESEARCH_PROPOSAL_ANSWER_CHARACTERS,
  MAX_RESEARCH_PROPOSAL_PROMPT_CHARACTERS,
  encodeResearchProposal,
  parseProposalResolution,
  parseResearchProposal,
  researchProposalDigestInput,
  validateResearchProposalAnchor,
  type ProposalResolutionPayload,
  type ResearchProposalPayload,
} from "../src/lib/publicationComments";
import {
  MAX_PUBLICATION_FILE_BYTES,
  MAX_PUBLICATION_TOTAL_BYTES,
  PUBLICATION_INDEX_FILE,
  parsePublicationJson,
  publicationHashInput,
  type Publication,
  type PublishedResearchNode,
  type ResearchPublication,
  type TranscriptPublication,
} from "../src/lib/publication";
import { renderLandingPage } from "./landing/LandingPage";
import {
  beginGitHubAuthorization,
  clearViewerSession,
  completeGitHubAuthorization,
  resolveGitHubWebAuthConfig,
  validatedPublicOrigin,
  viewerSessionFromRequest,
  type GitHubWebAuthConfig,
  type GitHubWebAuthOptions,
  type ViewerSession,
} from "./githubAuth";

const DEFAULT_HOST = "127.0.0.1";
const DEFAULT_PORT = 8787;
const DEFAULT_PUBLIC_ORIGIN = "https://qmux.app";
const GITHUB_API_VERSION = "2026-03-10";
const LATEST_CACHE_MS = 60_000;
const COMMENTS_CACHE_MS = 30_000;
// A deterministic upstream verdict (missing/invalid gist) is cached briefly so a
// flood of repeat requests for the same bad id can't each spend a GitHub call.
const NEGATIVE_CACHE_MS = 60_000;
const MAX_NEGATIVE_CACHE_ENTRIES = 512;
// When GitHub itself rate-limits the shared reader token, stop calling upstream
// for a cooldown and serve 503 rather than piling on and prolonging the block.
const UPSTREAM_COOLDOWN_MS = 60_000;
// Per-client request budget for the GitHub-backed publication route. Distinct
// random ids miss every cache and would otherwise each cost an upstream call, so
// the primary abuse vector (draining the shared token's GitHub quota) is bounded
// per source rather than only per id. Static assets and auth are exempt.
const RATE_LIMIT_WINDOW_MS = 60_000;
const RATE_LIMIT_MAX_REQUESTS = 120;
const MAX_RATE_LIMIT_ENTRIES = 4_096;
const MAX_GITHUB_API_RESPONSE_BYTES = 20_000_000;
const MAX_GITHUB_COMMENTS_RESPONSE_BYTES = 5_000_000;
const MAX_PUBLICATION_CACHE_BYTES = 64_000_000;
const MAX_PUBLICATION_CACHE_ENTRIES = 128;
const MAX_COMMENTS_CACHE_BYTES = 32_000_000;
const MAX_COMMENTS_CACHE_ENTRIES = 128;
const MAX_CONCURRENT_PUBLICATION_LOADS = 8;
const MAX_PROPOSAL_FORM_BYTES = 128_000;
const MAX_GIST_COMMENTS = 300;
const SITE_FONT_FILES = new Set([
  "ValleySans-Variable.woff2",
  "ValleySans-VariableItalic.woff2",
  "JetBrainsMono-Regular.woff2",
  "JetBrainsMono-Italic.woff2",
  "JetBrainsMono-Bold.woff2",
  "JetBrainsMono-BoldItalic.woff2",
  // The desktop app's UI face; the landing page's app replica renders in it.
  "DMSans-Variable-Latin.woff2",
]);
// The landing page serves its logo and its one enhancement script from disk, so
// it relaxes img-src and script-src to 'self'. It stays inline-script free, and
// the script it does load only rewrites the page's own markup — no network.
const LANDING_CSP =
  "default-src 'none'; style-src 'unsafe-inline'; script-src 'self'; img-src 'self'; " +
  "font-src 'self'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";
const GIST_ID_PATTERN = /^[A-Za-z0-9]{5,128}$/;
const REVISION_PATTERN = /^[a-f0-9]{40}$/;
const GITHUB_RAW_HOSTS = new Set(["gist.githubusercontent.com", "raw.githubusercontent.com"]);

interface GitHubGistFile {
  filename: string;
  size: number;
  truncated?: boolean;
  content?: string;
  raw_url?: string;
}

interface GitHubGist {
  id: string;
  html_url: string;
  description?: string | null;
  public: boolean;
  created_at: string;
  updated_at: string;
  files: Record<string, GitHubGistFile>;
  owner?: {
    login: string;
    avatar_url?: string;
    html_url?: string;
  } | null;
}

interface GitHubGistComment {
  id: number;
  body: string;
  created_at: string;
  updated_at: string;
  user?: {
    login: string;
    html_url?: string;
  } | null;
  author_association?: string;
}

// A gist comment that carries qmux data: a follow-up proposal or an owner
// resolution. Plain human comments live on the Gist itself and are not
// surfaced here — the page links to the Gist for discussion.
interface PublicationComment {
  id: number;
  proposal: ResearchProposalPayload | null;
  resolution: ProposalResolutionPayload | null;
  createdAt: string;
  user: {
    login: string;
    htmlUrl: string;
  };
}

interface CachedPublication {
  gist: GitHubGist;
  publication: Publication;
  etag?: string | null;
  expiresAt: number;
  weightBytes: number;
}

interface CachedComments {
  comments: PublicationComment[];
  expiresAt: number;
  weightBytes: number;
}

interface CommentsCache {
  entries: Map<string, CachedComments>;
  totalBytes: number;
}

interface PublicationCache {
  entries: Map<string, CachedPublication>;
  totalBytes: number;
}

interface ServerOptions extends GitHubWebAuthOptions {
  fetchImpl?: typeof fetch;
  siteDir?: string;
  githubToken?: string | null;
  now?: () => number;
  // Fly Proxy supplies the real client address in Fly-Client-IP. Enable this
  // automatically on Fly, while allowing tests and other trusted-proxy
  // deployments to opt in explicitly.
  trustFlyClientIp?: boolean;
  // Per-client budget for the publication route; `null` disables it (used by
  // tests that intentionally issue many requests from one address).
  rateLimit?: { windowMs: number; maxRequests: number } | null;
}

export function createQmuxWebServer(options: ServerOptions = {}) {
  return createServer(createQmuxRequestHandler(options));
}

export function createQmuxRequestHandler(options: ServerOptions = {}) {
  const fetchImpl = options.fetchImpl ?? fetch;
  const now = options.now ?? Date.now;
  const siteDir = options.siteDir ?? resolve(dirname(fileURLToPath(import.meta.url)), "..", "site");
  const githubToken = options.githubToken ?? process.env.GITHUB_READER_TOKEN ?? null;
  const webAuth = resolveGitHubWebAuthConfig(options);
  // Absolute URLs for the landing page's canonical/og tags. GitHub auth may be
  // unconfigured (webAuth null), so the origin is resolved independently.
  const publicOrigin = validatedPublicOrigin(
    options.publicOrigin ?? process.env.QMUX_PUBLIC_ORIGIN ?? DEFAULT_PUBLIC_ORIGIN,
  );
  const cache: PublicationCache = {
    entries: new Map(),
    totalBytes: 0,
  };
  const rateLimit =
    options.rateLimit === undefined
      ? { windowMs: RATE_LIMIT_WINDOW_MS, maxRequests: RATE_LIMIT_MAX_REQUESTS }
      : options.rateLimit;
  const context: RouteContext = {
    fetchImpl,
    siteDir,
    githubToken,
    now,
    cache,
    commentsCache: {
      entries: new Map(),
      totalBytes: 0,
    },
    negativeCache: new Map(),
    rateLimit,
    rateLimiter: new Map(),
    trustFlyClientIp:
      options.trustFlyClientIp ?? Boolean(process.env.FLY_APP_NAME),
    upstreamCooldownUntil: 0,
    webAuth,
    publicOrigin,
    activePublicationLoads: 0,
    inFlightLoads: new Map(),
  };

  return async (request: IncomingMessage, response: ServerResponse) => {
    try {
      await routeRequest(request, response, context);
    } catch (error) {
      const status = error instanceof PublicationHttpError ? error.status : 500;
      const message =
        error instanceof PublicationHttpError
          ? error.message
          : "The publication could not be loaded.";
      if (!(error instanceof PublicationHttpError)) {
        console.error(error);
      }
      const extraHeaders =
        error instanceof PublicationHttpError ? error.headers : undefined;
      sendHtml(
        response,
        status,
        errorPage("Publication unavailable", message),
        "no-store",
        undefined,
        extraHeaders,
      );
    }
  };
}

interface NegativeCacheEntry {
  status: number;
  message: string;
  expiresAt: number;
}

interface RateLimitEntry {
  count: number;
  resetAt: number;
}

interface RouteContext {
  fetchImpl: typeof fetch;
  siteDir: string;
  githubToken: string | null;
  webAuth: GitHubWebAuthConfig | null;
  publicOrigin: string;
  now: () => number;
  cache: PublicationCache;
  commentsCache: CommentsCache;
  negativeCache: Map<string, NegativeCacheEntry>;
  rateLimit: { windowMs: number; maxRequests: number } | null;
  rateLimiter: Map<string, RateLimitEntry>;
  trustFlyClientIp: boolean;
  upstreamCooldownUntil: number;
  activePublicationLoads: number;
  // In-flight publication loads keyed by cache key, so a cold-cache burst for
  // the same Gist shares one upstream fetch instead of one per request.
  inFlightLoads: Map<string, Promise<CachedPublication>>;
}

async function routeRequest(
  request: IncomingMessage,
  response: ServerResponse,
  context: RouteContext,
) {
  const url = new URL(request.url ?? "/", "http://localhost");
  if (request.method === "GET" && url.pathname === "/auth/github") {
    if (!context.webAuth) {
      throw new PublicationHttpError(503, "GitHub comments are not configured.");
    }
    beginGitHubAuthorization(
      response,
      context.webAuth,
      url.searchParams.get("returnTo") ?? "/",
    );
    return;
  }
  if (request.method === "GET" && url.pathname === "/auth/github/callback") {
    if (!context.webAuth) {
      throw new PublicationHttpError(503, "GitHub comments are not configured.");
    }
    await completeGitHubAuthorization(
      request,
      response,
      url,
      context.webAuth,
      context.fetchImpl,
    );
    return;
  }
  if (request.method === "POST" && url.pathname === "/auth/logout") {
    await handleLogout(request, response, context, url);
    return;
  }
  const proposalRoute = publicationProposalRoute(url.pathname);
  if (request.method === "POST" && proposalRoute) {
    await handleCreateProposal(request, response, context, proposalRoute);
    return;
  }
  if (request.method !== "GET" && request.method !== "HEAD") {
    response.writeHead(405, { Allow: "GET, HEAD, POST" });
    response.end();
    return;
  }
  if (url.pathname === "/healthz") {
    response.writeHead(200, {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-store",
    });
    response.end(request.method === "HEAD" ? undefined : "ok\n");
    return;
  }
  if (url.pathname === "/") {
    sendHtml(
      response,
      200,
      renderLandingPage(context.publicOrigin),
      "public, max-age=300",
      request.method,
      { "Content-Security-Policy": LANDING_CSP },
    );
    return;
  }
  if (url.pathname === "/mockup.js") {
    await serveStaticFile(
      response,
      join(context.siteDir, "mockup.js"),
      "text/javascript; charset=utf-8",
      request.method,
    );
    return;
  }
  if (url.pathname === "/logo.png" || url.pathname === "/qmux.png") {
    const name = normalize(url.pathname.slice(1));
    await serveStaticFile(response, join(context.siteDir, name), "image/png", request.method);
    return;
  }
  if (url.pathname.startsWith("/fonts/")) {
    const name = url.pathname.slice("/fonts/".length);
    if (SITE_FONT_FILES.has(name)) {
      await serveStaticFile(
        response,
        join(context.siteDir, "fonts", name),
        "font/woff2",
        request.method,
        "public, max-age=31536000, immutable",
      );
      return;
    }
  }

  const route = publicationRoute(url.pathname);
  if (!route) {
    sendHtml(response, 404, errorPage("Not found", "This page does not exist."));
    return;
  }
  // Pinned-revision URLs no longer render; old links land on the living view.
  if (route.revision) {
    response.writeHead(301, {
      Location: route.nodeId
        ? `/p/${encodeURIComponent(route.gistId)}/n/${encodeURIComponent(route.nodeId)}`
        : `/p/${encodeURIComponent(route.gistId)}`,
      "Cache-Control": "public, max-age=86400",
    });
    response.end();
    return;
  }
  enforcePublicationRateLimit(request, context);
  if (context.activePublicationLoads >= MAX_CONCURRENT_PUBLICATION_LOADS) {
    throw new PublicationHttpError(
      503,
      "The publication server is busy. Try again shortly.",
    );
  }
  context.activePublicationLoads += 1;
  try {
    const loaded = await loadPublication(route.gistId, context);
    const viewer = viewerSessionFromRequest(request, context.webAuth);
    let comments: PublicationComment[] = [];
    let commentsError: string | null = null;
    if (
      loaded.publication.kind === "research-tree" &&
      (context.webAuth || context.githubToken)
    ) {
      try {
        comments = await loadPublicationComments(route.gistId, context);
      } catch (error) {
        commentsError =
          error instanceof Error ? error.message : "Proposals could not be loaded.";
      }
    }
    const page =
      loaded.publication.kind === "transcript"
        ? transcriptPage(loaded.gist, loaded.publication)
        : researchPage(
            loaded.gist,
            loaded.publication,
            route.nodeId,
            comments,
            commentsError,
            viewer,
            context.webAuth,
          );
    const cacheControl = viewer
      ? "private, no-store"
      : "public, max-age=30, stale-while-revalidate=120";
    sendHtml(response, 200, page, cacheControl, request.method);
  } finally {
    context.activePublicationLoads -= 1;
  }
}

function publicationRoute(pathname: string) {
  const revisionNode = pathname.match(/^\/p\/([^/]+)\/r\/([^/]+)\/n\/([^/]+)\/?$/);
  if (
    revisionNode &&
    GIST_ID_PATTERN.test(revisionNode[1]) &&
    REVISION_PATTERN.test(revisionNode[2]) &&
    /^[A-Za-z0-9_-]{8,128}$/.test(revisionNode[3])
  ) {
    return {
      gistId: revisionNode[1],
      revision: revisionNode[2],
      nodeId: revisionNode[3],
    };
  }
  const latestNode = pathname.match(/^\/p\/([^/]+)\/n\/([^/]+)\/?$/);
  if (
    latestNode &&
    GIST_ID_PATTERN.test(latestNode[1]) &&
    /^[A-Za-z0-9_-]{8,128}$/.test(latestNode[2])
  ) {
    return { gistId: latestNode[1], revision: null, nodeId: latestNode[2] };
  }
  const latest = pathname.match(/^\/p\/([^/]+)\/?$/);
  if (latest && GIST_ID_PATTERN.test(latest[1])) {
    return { gistId: latest[1], revision: null, nodeId: null };
  }
  const revision = pathname.match(/^\/p\/([^/]+)\/r\/([^/]+)\/?$/);
  if (
    revision &&
    GIST_ID_PATTERN.test(revision[1]) &&
    REVISION_PATTERN.test(revision[2])
  ) {
    return { gistId: revision[1], revision: revision[2], nodeId: null };
  }
  return null;
}

function publicationProposalRoute(pathname: string) {
  const node = pathname.match(/^\/p\/([^/]+)\/n\/([^/]+)\/proposals\/?$/);
  if (
    node &&
    GIST_ID_PATTERN.test(node[1]) &&
    /^[A-Za-z0-9_-]{8,128}$/.test(node[2])
  ) {
    return { gistId: node[1], nodeId: node[2] };
  }
  return null;
}

async function handleLogout(
  request: IncomingMessage,
  response: ServerResponse,
  context: RouteContext,
  url: URL,
) {
  if (!context.webAuth) {
    throw new PublicationHttpError(404, "GitHub comments are not configured.");
  }
  const session = viewerSessionFromRequest(request, context.webAuth);
  const form = await readUrlEncodedForm(request);
  if (!session || form.get("csrfToken") !== session.csrfToken) {
    throw new PublicationHttpError(403, "The sign-out request could not be verified.");
  }
  clearViewerSession(
    response,
    context.webAuth,
    form.get("returnTo") ?? url.searchParams.get("returnTo") ?? "/",
  );
}

async function handleCreateProposal(
  request: IncomingMessage,
  response: ServerResponse,
  context: RouteContext,
  route: { gistId: string; nodeId: string },
) {
  if (!context.webAuth) {
    throw new PublicationHttpError(503, "GitHub contributions are not configured.");
  }
  const session = viewerSessionFromRequest(request, context.webAuth);
  if (!session) {
    throw new PublicationHttpError(401, "Sign in with GitHub before proposing a follow-up.");
  }
  // Authentication and CSRF stop cross-site forgery but not deliberate abuse by
  // an authenticated user, so proposal mutations get the same per-client rate
  // limit as the page views — enforced before any upstream work.
  enforcePublicationRateLimit(request, context);
  const form = await readUrlEncodedForm(request);
  if (form.get("csrfToken") !== session.csrfToken) {
    throw new PublicationHttpError(403, "The proposal request could not be verified.");
  }
  const loaded = await loadPublicationGuarded(route.gistId, context);
  if (
    loaded.publication.kind !== "research-tree" ||
    !loaded.publication.research.nodes.some((node) => node.id === route.nodeId)
  ) {
    throw new PublicationHttpError(
      404,
      "That published research proposal target was not found.",
    );
  }
  const prompt = (form.get("prompt") ?? "").trim();
  const answerMarkdown = (form.get("answerMarkdown") ?? "").trim();
  if (!prompt || prompt.length > MAX_RESEARCH_PROPOSAL_PROMPT_CHARACTERS) {
    throw new PublicationHttpError(
      422,
      `Follow-up questions must contain between 1 and ${MAX_RESEARCH_PROPOSAL_PROMPT_CHARACTERS.toLocaleString()} characters.`,
    );
  }
  if (answerMarkdown.length > MAX_RESEARCH_PROPOSAL_ANSWER_CHARACTERS) {
    throw new PublicationHttpError(
      422,
      `Proposed answers cannot exceed ${MAX_RESEARCH_PROPOSAL_ANSWER_CHARACTERS.toLocaleString()} characters.`,
    );
  }
  // An anchored proposal quotes the passage it was asked about; the anchor
  // travels in the proposal payload so the owner's app (and this page) can
  // pair the question with its passage.
  const anchorRaw = (form.get("anchor") ?? "").trim();
  let anchor = null;
  if (anchorRaw) {
    try {
      anchor = validateResearchProposalAnchor(JSON.parse(anchorRaw));
    } catch (error) {
      throw new PublicationHttpError(
        422,
        error instanceof Error ? error.message : "The quoted passage is invalid.",
      );
    }
  }
  const body = encodeResearchProposal({
    publicationId: loaded.publication.publicationId,
    parentNodeId: route.nodeId,
    prompt,
    ...(answerMarkdown ? { answerMarkdown } : {}),
    ...(anchor ? { anchor } : {}),
  });
  const upstream = await context.fetchImpl(
    `https://api.github.com/gists/${encodeURIComponent(route.gistId)}/comments`,
    {
      method: "POST",
      headers: {
        Accept: "application/vnd.github+json",
        Authorization: `Bearer ${session.accessToken}`,
        "Content-Type": "application/json",
        "User-Agent": "qmux-publisher",
        "X-GitHub-Api-Version": GITHUB_API_VERSION,
      },
      body: JSON.stringify({ body }),
      redirect: "error",
      signal: AbortSignal.timeout(10_000),
    },
  );
  const responseBody = await readResponseTextLimited(
    upstream,
    MAX_GITHUB_COMMENTS_RESPONSE_BYTES,
    "GitHub proposal response",
  );
  if (!upstream.ok) {
    throw new PublicationHttpError(
      upstream.status,
      githubApiErrorMessage(responseBody, "GitHub could not create this proposal."),
    );
  }
  deleteCommentsCacheEntry(context.commentsCache, route.gistId);
  const returnTo = commentReturnPath(route.gistId, route.nodeId);
  response.writeHead(303, {
    Location: `${returnTo}#proposals`,
    "Cache-Control": "no-store",
  });
  response.end();
}

function commentReturnPath(gistId: string, nodeId: string | null) {
  return nodeId
    ? `/p/${encodeURIComponent(gistId)}/n/${encodeURIComponent(nodeId)}`
    : `/p/${encodeURIComponent(gistId)}`;
}

async function readUrlEncodedForm(request: IncomingMessage) {
  const contentType = request.headers["content-type"] ?? "";
  if (!contentType.toLowerCase().startsWith("application/x-www-form-urlencoded")) {
    throw new PublicationHttpError(415, "This form submission has an unsupported content type.");
  }
  const chunks: Buffer[] = [];
  let totalBytes = 0;
  for await (const chunk of request) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    totalBytes += bytes.byteLength;
    if (totalBytes > MAX_PROPOSAL_FORM_BYTES) {
      throw new PublicationHttpError(413, "This form submission is too large.");
    }
    chunks.push(bytes);
  }
  return new URLSearchParams(Buffer.concat(chunks, totalBytes).toString("utf8"));
}

async function loadPublicationComments(
  gistId: string,
  context: RouteContext,
): Promise<PublicationComment[]> {
  const cached = context.commentsCache.entries.get(gistId);
  if (cached && cached.expiresAt > context.now()) {
    touchCommentsCacheEntry(context.commentsCache, gistId, cached);
    return cached.comments;
  }
  if (cached) {
    deleteCommentsCacheEntry(context.commentsCache, gistId);
  }
  const comments: PublicationComment[] = [];
  const maxPages = Math.ceil(MAX_GIST_COMMENTS / 100);
  const firstPage = await fetchGitHubCommentsPage(gistId, 1, context);
  const lastPage = firstPage.lastPage;
  const pageNumbers = lastPage
    ? Array.from(
        { length: Math.min(lastPage, maxPages) },
        (_, index) =>
          Math.max(1, lastPage - maxPages + 1) + index,
      )
    : Array.from({ length: maxPages }, (_, index) => index + 1);
  for (const page of pageNumbers) {
    const pageComments =
      page === 1
        ? firstPage.comments
        : (await fetchGitHubCommentsPage(gistId, page, context)).comments;
    for (const comment of pageComments) {
      const normalized = normalizeGitHubComment(comment);
      if (normalized) {
        comments.push(normalized);
      }
      if (comments.length >= MAX_GIST_COMMENTS) {
        break;
      }
    }
    if (comments.length >= MAX_GIST_COMMENTS) {
      break;
    }
    if (!firstPage.lastPage && pageComments.length < 100) {
      break;
    }
  }
  comments.sort((left, right) => Date.parse(left.createdAt) - Date.parse(right.createdAt));
  cacheComments(context.commentsCache, gistId, {
    comments,
    expiresAt: context.now() + COMMENTS_CACHE_MS,
    weightBytes: commentsCacheWeight(comments),
  });
  return comments;
}

async function fetchGitHubCommentsPage(
  gistId: string,
  page: number,
  context: RouteContext,
) {
  const headers: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "qmux-publisher",
    "X-GitHub-Api-Version": GITHUB_API_VERSION,
  };
  if (context.githubToken) {
    headers.Authorization = `Bearer ${context.githubToken}`;
  }
  const upstream = await context.fetchImpl(
    `https://api.github.com/gists/${encodeURIComponent(gistId)}/comments?per_page=100&page=${page}`,
    {
      headers,
      redirect: "error",
      signal: AbortSignal.timeout(10_000),
    },
  );
  const lastPage = githubLastPage(upstream.headers.get("link"));
  const raw = await readResponseTextLimited(
    upstream,
    MAX_GITHUB_COMMENTS_RESPONSE_BYTES,
    "GitHub comments response",
  );
  if (!upstream.ok) {
    throw new PublicationHttpError(
      upstream.status,
      githubApiErrorMessage(raw, "GitHub comments could not be loaded."),
    );
  }
  try {
    const parsed = JSON.parse(raw) as unknown;
    if (!Array.isArray(parsed)) {
      throw new Error("comments response is not an array");
    }
    return {
      comments: parsed as GitHubGistComment[],
      lastPage,
    };
  } catch {
    throw new PublicationHttpError(502, "GitHub returned invalid comments.");
  }
}

function githubLastPage(link: string | null) {
  if (!link) {
    return null;
  }
  for (const segment of link.split(",")) {
    const [target, ...parameters] = segment.split(";");
    if (!parameters.some((value) => value.trim() === 'rel="last"')) {
      continue;
    }
    const match = target.trim().match(/^<([^>]+)>$/);
    if (!match) {
      return null;
    }
    try {
      const page = Number(new URL(match[1]).searchParams.get("page"));
      return Number.isSafeInteger(page) && page > 0 ? page : null;
    } catch {
      return null;
    }
  }
  return null;
}

function commentsCacheWeight(comments: PublicationComment[]) {
  return comments.reduce(
    (total, comment) =>
      total +
      Buffer.byteLength(JSON.stringify(comment.proposal ?? "")) * 2 +
      Buffer.byteLength(JSON.stringify(comment.resolution ?? "")) +
      Buffer.byteLength(comment.user.login) +
      Buffer.byteLength(comment.user.htmlUrl) +
      512,
    1_024,
  );
}

function touchCommentsCacheEntry(
  cache: CommentsCache,
  key: string,
  entry: CachedComments,
) {
  cache.entries.delete(key);
  cache.entries.set(key, entry);
}

function deleteCommentsCacheEntry(cache: CommentsCache, key: string) {
  const existing = cache.entries.get(key);
  if (!existing) {
    return;
  }
  cache.entries.delete(key);
  cache.totalBytes -= existing.weightBytes;
}

function cacheComments(
  cache: CommentsCache,
  key: string,
  entry: CachedComments,
) {
  deleteCommentsCacheEntry(cache, key);
  if (entry.weightBytes > MAX_COMMENTS_CACHE_BYTES) {
    return;
  }
  while (
    cache.entries.size >= MAX_COMMENTS_CACHE_ENTRIES ||
    cache.totalBytes + entry.weightBytes > MAX_COMMENTS_CACHE_BYTES
  ) {
    const oldestKey = cache.entries.keys().next().value as string | undefined;
    if (!oldestKey) {
      break;
    }
    deleteCommentsCacheEntry(cache, oldestKey);
  }
  cache.entries.set(key, entry);
  cache.totalBytes += entry.weightBytes;
}

function normalizeGitHubComment(comment: GitHubGistComment): PublicationComment | null {
  if (
    !Number.isSafeInteger(comment.id) ||
    comment.id <= 0 ||
    typeof comment.body !== "string" ||
    comment.body.length > 65_536 ||
    !comment.user?.login ||
    Number.isNaN(Date.parse(comment.created_at))
  ) {
    return null;
  }
  const proposal = parseResearchProposal(comment.body);
  const resolution = parseProposalResolution(comment.body);
  // Only qmux payloads matter here; ordinary discussion stays on the Gist.
  if (!proposal && !resolution) {
    return null;
  }
  return {
    id: comment.id,
    proposal,
    resolution,
    createdAt: comment.created_at,
    user: {
      login: comment.user.login,
      htmlUrl: githubProfileUrl(comment.user.html_url, comment.user.login),
    },
  };
}

function researchProposalDigest(proposal: ResearchProposalPayload) {
  return createHash("sha256")
    .update(researchProposalDigestInput(proposal))
    .digest("hex");
}

function githubProfileUrl(value: string | undefined, login: string) {
  const fallback = `https://github.com/${encodeURIComponent(login)}`;
  if (!value) {
    return fallback;
  }
  try {
    const parsed = new URL(value);
    return parsed.protocol === "https:" &&
      parsed.hostname === "github.com" &&
      !parsed.username &&
      !parsed.password
      ? parsed.toString()
      : fallback;
  } catch {
    return fallback;
  }
}

function githubApiErrorMessage(raw: string, fallback: string) {
  try {
    const parsed = JSON.parse(raw) as { message?: unknown };
    return typeof parsed.message === "string" && parsed.message.trim()
      ? parsed.message
      : fallback;
  } catch {
    return fallback;
  }
}

// Loads a publication under the same global concurrency budget the page views
// use, so mutation paths (proposals) can't open unbounded parallel upstream
// reads that the page's guard would have refused.
async function loadPublicationGuarded(
  gistId: string,
  context: RouteContext,
): Promise<CachedPublication> {
  if (context.activePublicationLoads >= MAX_CONCURRENT_PUBLICATION_LOADS) {
    throw new PublicationHttpError(
      503,
      "The publication server is busy. Try again shortly.",
    );
  }
  context.activePublicationLoads += 1;
  try {
    return await loadPublication(gistId, context);
  } finally {
    context.activePublicationLoads -= 1;
  }
}

async function loadPublication(
  gistId: string,
  context: RouteContext,
): Promise<CachedPublication> {
  const key = `${gistId}@latest`;
  const cached = context.cache.entries.get(key);
  if (cached && cached.expiresAt > context.now()) {
    touchCacheEntry(context.cache, key, cached);
    return cached;
  }
  const negative = context.negativeCache.get(key);
  if (negative && negative.expiresAt > context.now()) {
    throw new PublicationHttpError(negative.status, negative.message);
  }
  // GitHub is rate-limiting the shared reader token; don't add to the pile-up.
  if (context.upstreamCooldownUntil > context.now()) {
    throw new PublicationHttpError(
      503,
      "The publication server is briefly rate-limited. Try again shortly.",
    );
  }
  // Coalesce concurrent cold-cache loads for the same Gist so a parallel burst
  // (e.g. many proposal requests) issues one upstream fetch, not one each.
  const inFlight = context.inFlightLoads.get(key);
  if (inFlight) {
    return inFlight;
  }
  const load = fetchPublication(gistId, context, key, cached);
  context.inFlightLoads.set(key, load);
  try {
    return await load;
  } finally {
    context.inFlightLoads.delete(key);
  }
}

async function fetchPublication(
  gistId: string,
  context: RouteContext,
  key: string,
  cached: CachedPublication | undefined,
): Promise<CachedPublication> {
  const endpoint = `https://api.github.com/gists/${encodeURIComponent(gistId)}`;
  const headers: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "qmux-publisher",
    "X-GitHub-Api-Version": GITHUB_API_VERSION,
  };
  if (context.githubToken) {
    headers.Authorization = `Bearer ${context.githubToken}`;
  }
  if (cached?.etag) {
    headers["If-None-Match"] = cached.etag;
  }
  const upstream = await context.fetchImpl(endpoint, {
    headers,
    redirect: "follow",
    signal: AbortSignal.timeout(10_000),
  });
  if (upstream.status === 304 && cached) {
    cached.expiresAt = context.now() + LATEST_CACHE_MS;
    touchCacheEntry(context.cache, key, cached);
    return cached;
  }
  if (upstream.status === 404) {
    throwNegativePublication(context, key, 404, "The requested Gist was not found.");
  }
  // GitHub rate-limit (429, or 403 with the remaining-quota header at zero):
  // enter a global cooldown so every id — not just this one — stops calling
  // upstream until the token recovers.
  if (
    upstream.status === 429 ||
    (upstream.status === 403 && upstream.headers.get("x-ratelimit-remaining") === "0")
  ) {
    context.upstreamCooldownUntil = context.now() + UPSTREAM_COOLDOWN_MS;
    throw new PublicationHttpError(
      503,
      "The publication server is briefly rate-limited. Try again shortly.",
    );
  }
  if (!upstream.ok) {
    throw new PublicationHttpError(
      upstream.status,
      `GitHub returned ${upstream.status} while loading this publication.`,
    );
  }
  const gistRaw = await readResponseTextLimited(
    upstream,
    MAX_GITHUB_API_RESPONSE_BYTES,
    "GitHub Gist response",
  );
  let gist: GitHubGist;
  try {
    gist = JSON.parse(gistRaw) as GitHubGist;
  } catch {
    throw new PublicationHttpError(502, "GitHub returned an invalid Gist response.");
  }
  const index = gist.files?.[PUBLICATION_INDEX_FILE];
  if (!index) {
    // Deterministic for this gist's current content — a stranger's gist that
    // simply isn't a qmux publication. Cache it so probing such ids stays cheap.
    throwNegativePublication(
      context,
      key,
      422,
      `This Gist does not contain ${PUBLICATION_INDEX_FILE}.`,
    );
  }
  const indexContent = await loadGistFileContent(
    index,
    context,
    PUBLICATION_INDEX_FILE,
  );
  index.content = indexContent;
  index.truncated = false;
  let publication: Publication;
  try {
    publication = parsePublicationJson(indexContent);
  } catch (error) {
    throw new PublicationHttpError(
      422,
      error instanceof Error ? error.message : `${PUBLICATION_INDEX_FILE} is invalid.`,
    );
  }
  const expectedHash = createHash("sha256")
    .update(publicationHashInput(publication))
    .digest("hex");
  if (expectedHash !== publication.contentHash) {
    throw new PublicationHttpError(422, "The publication content hash is invalid.");
  }
  if (publication.kind !== "transcript") {
    await validateResearchFiles(gist, publication, context, Buffer.byteLength(indexContent));
  }
  retainPublicationFiles(gist, publication);
  const weightBytes = cachedPublicationWeight(gist, indexContent);
  const loaded = {
    gist,
    publication,
    etag: upstream.headers.get("etag"),
    expiresAt: context.now() + LATEST_CACHE_MS,
    weightBytes,
  };
  cachePublication(context.cache, key, loaded);
  return loaded;
}

async function validateResearchFiles(
  gist: GitHubGist,
  publication: ResearchPublication,
  context: RouteContext,
  initialBytes: number,
) {
  let totalBytes = initialBytes;
  for (const node of publication.research.nodes) {
    const file = gist.files[node.answerFile];
    if (!file) {
      throw new PublicationHttpError(422, `This Gist is missing ${node.answerFile}.`);
    }
    const content = await loadGistFileContent(file, context, node.answerFile);
    totalBytes += Buffer.byteLength(content);
    if (totalBytes > MAX_PUBLICATION_TOTAL_BYTES) {
      throw new PublicationHttpError(413, "This publication is too large to render.");
    }
    file.content = content;
    file.truncated = false;
    const actualHash = createHash("sha256").update(content).digest("hex");
    if (actualHash !== node.contentHash) {
      throw new PublicationHttpError(422, `${node.answerFile} has an invalid content hash.`);
    }
  }
}

async function loadGistFileContent(
  file: GitHubGistFile,
  context: RouteContext,
  label: string,
) {
  if (!Number.isFinite(file.size) || file.size < 0 || file.size > MAX_PUBLICATION_FILE_BYTES) {
    throw new PublicationHttpError(413, `${label} is too large to render.`);
  }
  if (!file.truncated && file.content !== undefined) {
    if (Buffer.byteLength(file.content) > MAX_PUBLICATION_FILE_BYTES) {
      throw new PublicationHttpError(413, `${label} is too large to render.`);
    }
    return file.content;
  }
  const rawUrl = validatedGitHubRawUrl(file.raw_url, label);
  const response = await context.fetchImpl(rawUrl, {
    headers: {
      "User-Agent": "qmux-publisher",
    },
    redirect: "follow",
    signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok) {
    throw new PublicationHttpError(
      response.status === 404 ? 422 : 502,
      `GitHub could not provide ${label}.`,
    );
  }
  if (response.url) {
    validatedGitHubRawUrl(response.url, label);
  }
  return readResponseTextLimited(response, MAX_PUBLICATION_FILE_BYTES, label);
}

function validatedGitHubRawUrl(value: string | undefined, label: string) {
  if (!value) {
    throw new PublicationHttpError(422, `${label} is truncated and has no raw URL.`);
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new PublicationHttpError(422, `${label} has an invalid raw URL.`);
  }
  if (
    url.protocol !== "https:" ||
    url.username ||
    url.password ||
    !GITHUB_RAW_HOSTS.has(url.hostname)
  ) {
    throw new PublicationHttpError(422, `${label} has an untrusted raw URL.`);
  }
  return url.toString();
}

async function readResponseTextLimited(
  response: Response,
  maxBytes: number,
  label: string,
) {
  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > maxBytes) {
    await response.body?.cancel();
    throw new PublicationHttpError(413, `${label} is too large to render.`);
  }
  if (!response.body) {
    return "";
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let totalBytes = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      totalBytes += value.byteLength;
      if (totalBytes > maxBytes) {
        await reader.cancel();
        throw new PublicationHttpError(413, `${label} is too large to render.`);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  return Buffer.concat(chunks.map((chunk) => Buffer.from(chunk)), totalBytes).toString("utf8");
}

function retainPublicationFiles(gist: GitHubGist, publication: Publication) {
  const names = new Set([PUBLICATION_INDEX_FILE]);
  if (publication.kind !== "transcript") {
    for (const node of publication.research.nodes) {
      names.add(node.answerFile);
    }
  }
  gist.files = Object.fromEntries(
    Object.entries(gist.files).filter(([name]) => names.has(name)),
  );
}

function cachedPublicationWeight(gist: GitHubGist, indexContent: string) {
  let bytes = Buffer.byteLength(indexContent) * 4 + 4_096;
  for (const [name, file] of Object.entries(gist.files)) {
    if (name === PUBLICATION_INDEX_FILE) {
      continue;
    }
    bytes += Buffer.byteLength(name) + Buffer.byteLength(file.content ?? "") * 2 + 1_024;
  }
  return bytes;
}

function touchCacheEntry(
  cache: PublicationCache,
  key: string,
  entry: CachedPublication,
) {
  cache.entries.delete(key);
  cache.entries.set(key, entry);
}

function cachePublication(
  cache: PublicationCache,
  key: string,
  entry: CachedPublication,
) {
  const replaced = cache.entries.get(key);
  if (replaced) {
    cache.entries.delete(key);
    cache.totalBytes -= replaced.weightBytes;
  }
  if (entry.weightBytes > MAX_PUBLICATION_CACHE_BYTES) {
    return;
  }
  while (
    cache.entries.size >= MAX_PUBLICATION_CACHE_ENTRIES ||
    cache.totalBytes + entry.weightBytes > MAX_PUBLICATION_CACHE_BYTES
  ) {
    const oldestKey = cache.entries.keys().next().value as string | undefined;
    if (!oldestKey) {
      break;
    }
    const oldest = cache.entries.get(oldestKey);
    cache.entries.delete(oldestKey);
    if (oldest) {
      cache.totalBytes -= oldest.weightBytes;
    }
  }
  cache.entries.set(key, entry);
  cache.totalBytes += entry.weightBytes;
}

class PublicationHttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
    readonly headers?: Record<string, string>,
  ) {
    super(message);
  }
}

// Fixed-window per-client limit on the GitHub-backed publication route. Fly
// Proxy's client header is trusted only inside a Fly Machine; elsewhere the
// socket peer remains authoritative so callers cannot spoof past the limit.
function enforcePublicationRateLimit(request: IncomingMessage, context: RouteContext) {
  if (!context.rateLimit) {
    return;
  }
  const now = context.now();
  const key = publicationRateLimitKey(request, context);
  const entry = context.rateLimiter.get(key);
  if (!entry || entry.resetAt <= now) {
    if (context.rateLimiter.size >= MAX_RATE_LIMIT_ENTRIES) {
      for (const [candidate, value] of context.rateLimiter) {
        if (value.resetAt <= now) {
          context.rateLimiter.delete(candidate);
        }
      }
    }
    context.rateLimiter.set(key, { count: 1, resetAt: now + context.rateLimit.windowMs });
    return;
  }
  if (entry.count >= context.rateLimit.maxRequests) {
    const retryAfter = Math.max(1, Math.ceil((entry.resetAt - now) / 1000));
    throw new PublicationHttpError(
      429,
      `Too many requests. Try again in ${retryAfter} second${retryAfter === 1 ? "" : "s"}.`,
      { "Retry-After": String(retryAfter) },
    );
  }
  entry.count += 1;
}

function publicationRateLimitKey(
  request: IncomingMessage,
  context: RouteContext,
) {
  if (context.trustFlyClientIp) {
    const flyClientIp = request.headers["fly-client-ip"];
    if (typeof flyClientIp === "string") {
      const normalizedIp = flyClientIp.trim();
      if (isIP(normalizedIp) !== 0) {
        return normalizedIp;
      }
    }
  }
  return request.socket?.remoteAddress ?? "unknown";
}

// Records a deterministic upstream verdict so repeat requests for the same bad
// id are answered from memory, then throws it.
function throwNegativePublication(
  context: RouteContext,
  key: string,
  status: number,
  message: string,
): never {
  const now = context.now();
  if (context.negativeCache.size >= MAX_NEGATIVE_CACHE_ENTRIES) {
    for (const [candidate, value] of context.negativeCache) {
      if (value.expiresAt <= now) {
        context.negativeCache.delete(candidate);
      }
    }
  }
  context.negativeCache.set(key, { status, message, expiresAt: now + NEGATIVE_CACHE_MS });
  throw new PublicationHttpError(status, message);
}

// Mirrors the app's exported-conversation reading view: user prompts render as
// quiet framed blocks and assistant turns as plain document text, in a single
// answer-width column under the document chrome header.
function transcriptPage(gist: GitHubGist, publication: TranscriptPublication) {
  const author = gist.owner?.login ?? "GitHub user";
  const preview = followupPreviewText(
    publication.transcript.messages.map((message) => message.text).join(" "),
  ).slice(0, 180);
  const description = preview || `A published qmux transcript by ${author}.`;
  const wordCount = countWords(
    publication.transcript.messages.map((message) => message.text).join(" "),
  );
  return documentPage({
    title: publication.title,
    description,
    body: workspaceShell({
      chrome: (
        <DocumentChrome
          gist={gist}
          breadcrumb={[{ label: publication.title, href: null }]}
          kindLabel="Published transcript"
        />
      ),
      contentClassName: "is-conversation",
      children: (
        <>
          <main className="conversation-column">
            {publication.transcript.messages.map((message) =>
              message.role === "user" ? (
                <div className="conversation-prompt" key={message.id}>
                  <div className="turn-markdown">
                    <SafeMarkdown>{message.text}</SafeMarkdown>
                  </div>
                </div>
              ) : (
                <div className="conversation-answer turn-markdown" key={message.id}>
                  <SafeMarkdown>{message.text}</SafeMarkdown>
                </div>
              ),
            )}
            <footer className="research-answer-meta">
              <span>
                {wordCount.toLocaleString()} {wordCount === 1 ? "word" : "words"}
              </span>
              <span>{formatDate(publication.updatedAt)}</span>
              <a href={gist.owner?.html_url ?? gist.html_url}>@{author}</a>
              <button
                type="button"
                className="research-answer-copy"
                data-qmux-copy="qmux-answer-markdown"
                title="Copy conversation as Markdown"
                hidden
              >
                Copy
              </button>
            </footer>
            <script
              type="application/json"
              id="qmux-answer-markdown"
              dangerouslySetInnerHTML={{
                __html: JSON.stringify(
                  publication.transcript.messages
                    .map((message) => `## ${message.label}\n\n${message.text.trim()}`)
                    .join("\n\n"),
                ).replace(/</g, "\\u003c"),
              }}
            />
          </main>
        </>
      ),
    }),
  });
}

// Mirrors the in-app research document: breadcrumb chrome header, the prompt
// as a card above the answer column, and a follow-up rail on the right whose
// cards anchor beside the passages they were asked about.
function researchPage(
  gist: GitHubGist,
  publication: ResearchPublication,
  requestedNodeId: string | null,
  comments: PublicationComment[],
  commentsError: string | null,
  viewer: ViewerSession | null,
  webAuth: GitHubWebAuthConfig | null,
) {
  const selectedNodeId =
    requestedNodeId ??
    publication.research.selectedNodeId ??
    publication.research.rootNodeId;
  const selected = publication.research.nodes.find((node) => node.id === selectedNodeId);
  if (!selected) {
    throw new PublicationHttpError(404, "That published research result was not found.");
  }
  const author = gist.owner?.login ?? "GitHub user";
  const file = gist.files[selected.answerFile];
  const answerBody = answerBodyMarkdown(file.content ?? "", selected);
  const description =
    followupPreviewText(answerBody).slice(0, 180) ||
    (publication.kind === "research-answer"
      ? `A published qmux research answer by ${author}.`
      : `Published qmux research by ${author}.`);
  const children = publication.research.nodes
    .filter((node) => node.parentId === selected.id)
    .sort((left, right) => left.createdAt - right.createdAt);
  const parent = selected.parentId
    ? publication.research.nodes.find((node) => node.id === selected.parentId) ?? null
    : null;
  const followupCount = Math.max(0, publication.research.nodes.length - 1);
  const wordCount = countWords(answerBody);
  const runDuration =
    typeof selected.startedAt === "number" &&
    typeof selected.completedAt === "number" &&
    selected.completedAt > selected.startedAt
      ? selected.completedAt - selected.startedAt
      : null;
  const isDocument = selected.kind === "document";
  // A conversation node's published body is the transcript itself (opening
  // turn included), so it renders whole like a document rather than a prompt
  // card above an answer.
  const isConversation = selected.kind === "conversation";
  const showPromptCard = !isDocument && !isConversation;
  const anchoredChildren = children.filter((child) => child.queryAnchor);
  const anchorData = anchoredChildren.map((child) => ({
    nodeId: child.id,
    ...child.queryAnchor!,
  }));
  const hasRail = children.length > 0 || publication.kind === "research-tree";
  const breadcrumb = breadcrumbEntries(gist.id, publication, selected);
  const renderFollowupCard = (child: PublishedResearchNode) => {
    const preview = followupPreviewText(
      answerBodyMarkdown(gist.files[child.answerFile]?.content ?? "", child),
    );
    return (
      <a
        key={child.id}
        className="research-followup-card"
        href={publicationNodePath(gist.id, child.id)}
        data-anchor-node-id={child.queryAnchor ? child.id : undefined}
      >
        {child.queryAnchor ? (
          <span className="research-followup-quote">
            {quoteDisplayText(child.queryAnchor.exact)}
          </span>
        ) : null}
        <strong>{child.prompt || child.title}</strong>
        {preview ? <span className="research-followup-preview">{preview}</span> : null}
        {child.status !== "complete" ? (
          <small className={`is-${child.status}`}>{child.status}</small>
        ) : null}
        {child.contribution ? (
          <small className="is-contributed">@{child.contribution.githubLogin}</small>
        ) : null}
      </a>
    );
  };
  return documentPage({
    title: `${selected.title} · ${publication.title}`,
    description,
    body: workspaceShell({
      chrome: (
        <DocumentChrome
          gist={gist}
          breadcrumb={breadcrumb}
          kindLabel={
            publication.kind === "research-answer"
              ? "Published research answer"
              : "Published research"
          }
          followupCount={followupCount}
        />
      ),
      contentClassName: hasRail ? undefined : "is-single-doc",
      children: (
        <>
          {showPromptCard ? (
            <div className="research-prompt">
              {parent ? (
                <a
                  className="research-parent-link"
                  href={publicationNodePath(gist.id, parent.id)}
                >
                  ← Back
                </a>
              ) : null}
              {selected.queryAnchor ? (
                <blockquote className="research-prompt-quote">
                  {quoteDisplayText(selected.queryAnchor.exact)}
                </blockquote>
              ) : null}
              <div className="turn-markdown">
                <SafeMarkdown>{selected.prompt}</SafeMarkdown>
              </div>
            </div>
          ) : null}
          <div className={`research-response-grid${hasRail ? "" : " is-single"}`}>
            <section className="research-response" aria-label="Research response">
              {selected.status === "failed" ? (
                <p className="research-response-error" role="alert">
                  This research run failed.
                </p>
              ) : null}
              {selected.contribution ? (
                <p className="research-contribution">
                  Proposed by{" "}
                  <a
                    href={`https://github.com/${encodeURIComponent(selected.contribution.githubLogin)}`}
                  >
                    @{selected.contribution.githubLogin}
                  </a>
                </p>
              ) : null}
              <div className="research-response-content-root" id="qmux-answer-root">
                {isConversation && selected.conversation ? (
                  <div className="research-conversation">
                    {selected.conversation.map((turn, index) => (
                      <div
                        className={`conversation-turn is-${turn.role}`}
                        key={index}
                      >
                        <div className="conversation-turn-label">{turn.label}</div>
                        <div className="conversation-turn-body turn-markdown">
                          <SafeMarkdown>{turn.text}</SafeMarkdown>
                        </div>
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className="turn-markdown">
                    <SafeMarkdown>{answerBody}</SafeMarkdown>
                  </div>
                )}
              </div>
              <footer className="research-answer-meta">
                <span>
                  {wordCount.toLocaleString()} {wordCount === 1 ? "word" : "words"}
                </span>
                {runDuration !== null ? (
                  <span>
                    {selected.status !== "complete" ? "Ran for " : null}
                    {formatDuration(runDuration)}
                  </span>
                ) : null}
                <span>{formatDate(publication.updatedAt)}</span>
                <a href={gist.owner?.html_url ?? gist.html_url}>@{author}</a>
                {selected.status !== "complete" ? (
                  <span className={`research-meta-status is-${selected.status}`}>
                    {selected.status}
                  </span>
                ) : null}
                {answerBody ? (
                  <button
                    type="button"
                    className="research-answer-copy"
                    data-qmux-copy="qmux-answer-markdown"
                    title="Copy answer as Markdown"
                    hidden
                  >
                    Copy
                  </button>
                ) : null}
              </footer>
            </section>
            {hasRail ? (
              <aside className="research-followups" aria-label="Follow-ups">
                {publication.kind === "research-tree" ? (
                  <ProposalComposer
                    gist={gist}
                    nodeId={selected.id}
                    viewer={viewer}
                    webAuth={webAuth}
                  />
                ) : null}
                <div className="research-followup-cards">
                  {children.map((child) => renderFollowupCard(child))}
                </div>
                {commentsError ? (
                  <p className="proposals-error" role="status">
                    {commentsError}
                  </p>
                ) : null}
                {publication.kind === "research-tree" ? (
                  <PublicationProposals
                    gist={gist}
                    publication={publication}
                    nodeId={selected.id}
                    comments={comments}
                  />
                ) : null}
                {viewer && publication.kind === "research-tree" ? (
                  <ComposerSignOut
                    viewer={viewer}
                    returnTo={commentReturnPath(gist.id, selected.id)}
                  />
                ) : null}
              </aside>
            ) : null}
          </div>
          {anchorData.length > 0 ? (
            <script
              type="application/json"
              id="qmux-anchor-data"
              dangerouslySetInnerHTML={{
                __html: JSON.stringify(anchorData).replace(/</g, "\\u003c"),
              }}
            />
          ) : null}
          {answerBody ? (
            <script
              type="application/json"
              id="qmux-answer-markdown"
              dangerouslySetInnerHTML={{
                __html: JSON.stringify(answerBody).replace(/</g, "\\u003c"),
              }}
            />
          ) : null}
        </>
      ),
    }),
  });
}

// The document chrome bar the app renders above every research document:
// brand, breadcrumb path, follow-up count, and the page-level actions.
function DocumentChrome({
  gist,
  breadcrumb,
  kindLabel,
  followupCount,
}: {
  gist: GitHubGist;
  breadcrumb: { label: string; href: string | null }[];
  kindLabel: string;
  followupCount?: number;
}) {
  return (
    <header className="doc-header">
      <a className="brand" href="/" aria-label="qmux home">
        qmux
      </a>
      <nav className="doc-breadcrumb" aria-label="Path">
        {breadcrumb.map((entry, index) => (
          <span key={`${entry.label}-${index}`}>
            {index > 0 ? <span className="doc-breadcrumb-separator">/</span> : null}
            {entry.href ? (
              <a href={entry.href}>{entry.label}</a>
            ) : (
              <span className="is-current">{entry.label}</span>
            )}
          </span>
        ))}
      </nav>
      {followupCount ? (
        <span className="doc-followup-count">
          {followupCount} {followupCount === 1 ? "follow-up" : "follow-ups"}
        </span>
      ) : null}
      <span className="doc-kind">{kindLabel}</span>
      <a className="doc-header-action" href={gist.html_url}>
        View Gist
      </a>
    </header>
  );
}

function workspaceShell({
  chrome,
  children,
  contentClassName,
}: {
  chrome: React.ReactNode;
  children: React.ReactNode;
  contentClassName?: string;
}) {
  return (
    <div className="research-workspace">
      {chrome}
      <div className="research-document-scroll">
        <div
          className={`research-document-content${
            contentClassName ? ` ${contentClassName}` : ""
          }`}
        >
          {children}
          <footer className="page-footer">
            Published with <a href="/">qmux</a>
          </footer>
          <script dangerouslySetInnerHTML={{ __html: PAGE_SCRIPT }} />
        </div>
      </div>
    </div>
  );
}

function breadcrumbEntries(
  gistId: string,
  publication: ResearchPublication,
  selected: PublishedResearchNode,
) {
  const byId = new Map(publication.research.nodes.map((node) => [node.id, node]));
  const chain: PublishedResearchNode[] = [];
  let current: PublishedResearchNode | undefined = selected;
  while (current) {
    chain.unshift(current);
    current = current.parentId ? byId.get(current.parentId) : undefined;
  }
  const entries = chain.map((node, index) => ({
    label:
      index === 0
        ? publication.title
        : node.title,
    href:
      node.id === selected.id ? null : publicationNodePath(gistId, node.id),
  }));
  // Deep paths collapse their middle like the app's breadcrumb: first step,
  // an ellipsis, then the parent and current steps.
  if (entries.length > 4) {
    return [
      entries[0],
      { label: "…", href: null },
      ...entries.slice(entries.length - 2),
    ];
  }
  return entries;
}

// Published answer files wrap the response in "# Title / ## Question / ##
// Answer" sections for Gist readers; the page mirrors the app instead, where
// the prompt card is the question and the document body is only the answer.
function answerBodyMarkdown(fileContent: string, node: PublishedResearchNode) {
  const marker = fileContent.match(/^## Answer\s*$/m);
  if (marker && marker.index !== undefined) {
    const body = fileContent.slice(marker.index + marker[0].length).trim();
    if (body) {
      return body;
    }
  }
  return fileContent.replace(/^# [^\n]*\n/, "").trim() || node.title;
}

function countWords(markdown: string) {
  const words = markdown.trim().split(/\s+/).filter(Boolean);
  return words.length;
}

// The quote shown on cards and prompt blocks. Newlines inside a quote are
// already absent from the app's rendered-text projection, so collapse them.
function quoteDisplayText(exact: string) {
  return exact.split(/\s+/).join(" ").trim();
}

// A rough plain-text preview of a child answer for its follow-up card; the
// card clamps to two lines, so only the opening matters.
function followupPreviewText(markdown: string) {
  const text = markdown
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/^#{1,6}\s+[^\n]*$/gm, " ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/[*_`>|#-]/g, " ")
    .split(/\s+/)
    .filter(Boolean)
    .join(" ")
    .trim();
  return text.slice(0, 260);
}

// Resolves published query anchors against the rendered answer text (the same
// exact/prefix/suffix relocation the app uses), paints the passages via the
// CSS Custom Highlight API, and anchors each follow-up card beside its
// passage. Static markup stays complete without it: cards simply remain
// stacked in the rail with their quotes. Served inline and allowed by a CSP
// hash; keep it dependency-free and free of backticks.
const PAGE_SCRIPT = `(() => {
  // Copy-as-Markdown buttons: hidden in the static markup, revealed only when
  // a clipboard is actually available, sourcing the raw markdown from an
  // adjacent JSON data tag.
  var copyButtons = document.querySelectorAll("[data-qmux-copy]");
  for (var bIndex = 0; bIndex < copyButtons.length; bIndex += 1) {
    (function (button) {
      var source = document.getElementById(button.getAttribute("data-qmux-copy"));
      if (!source || !navigator.clipboard) return;
      var markdown;
      try { markdown = JSON.parse(source.textContent || '""'); } catch (err) { return; }
      if (!markdown) return;
      button.hidden = false;
      var label = button.textContent;
      var restoreTimer = null;
      button.addEventListener("click", function () {
        navigator.clipboard.writeText(markdown).then(function () {
          button.textContent = "Copied";
          if (restoreTimer) clearTimeout(restoreTimer);
          restoreTimer = setTimeout(function () {
            button.textContent = label;
          }, 1600);
        });
      });
    })(copyButtons[bIndex]);
  }

  var root = document.getElementById("qmux-answer-root");
  if (!root) return;
  var rail = document.querySelector(".research-followups");
  var grid = document.querySelector(".research-response-grid");
  var connectorSvg = null;

  // The rendered-text projection: what anchor offsets and quotes refer to.
  var nodes = [];
  var starts = [];
  var text = "";
  var walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  while (walker.nextNode()) {
    nodes.push(walker.currentNode);
    starts.push(text.length);
    text += walker.currentNode.nodeValue;
  }

  // An empty side constrains nothing, matching the app's resolver: context is
  // captured clamped to the passage's enclosing message, so a quote reaching
  // that message's edge saves none on that side — and a whole conversation
  // turn, which is one message, saves none on either. Reading emptiness as
  // "must sit at the projection's own edge" left those anchors unpainted here
  // while the app placed them fine.
  function contextMatches(start, exactLength, prefix, suffix) {
    var end = start + exactLength;
    var prefixOk =
      !prefix || text.slice(Math.max(0, start - prefix.length), start) === prefix;
    var suffixOk = !suffix || text.slice(end, end + suffix.length) === suffix;
    return prefixOk && suffixOk;
  }

  function resolveOffsets(anchor) {
    if (!anchor.exact) return null;
    if (
      anchor.start >= 0 &&
      anchor.end <= text.length &&
      text.slice(anchor.start, anchor.end) === anchor.exact &&
      contextMatches(anchor.start, anchor.exact.length, anchor.prefix, anchor.suffix)
    ) {
      return { start: anchor.start, end: anchor.end };
    }
    var best = -1;
    var bestDistance = Infinity;
    var candidate = text.indexOf(anchor.exact);
    while (candidate >= 0) {
      if (contextMatches(candidate, anchor.exact.length, anchor.prefix, anchor.suffix)) {
        var distance = Math.abs(candidate - anchor.start);
        if (distance < bestDistance) {
          best = candidate;
          bestDistance = distance;
        }
      }
      candidate = text.indexOf(anchor.exact, candidate + 1);
    }
    return best >= 0 ? { start: best, end: best + anchor.exact.length } : null;
  }

  function positionAt(offset) {
    for (var index = nodes.length - 1; index >= 0; index -= 1) {
      if (starts[index] <= offset) {
        return {
          node: nodes[index],
          offset: Math.min(offset - starts[index], nodes[index].nodeValue.length),
        };
      }
    }
    return null;
  }

  function rangeFor(offsets) {
    var start = positionAt(offsets.start);
    var end = positionAt(offsets.end);
    if (!start || !end) return null;
    var range = document.createRange();
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
    return range;
  }

  var canPaint = typeof Highlight !== "undefined" && CSS.highlights;

  // ------------------------------------------------------------------
  // Published query anchors: paint passages and anchor cards beside them.
  var dataEl = document.getElementById("qmux-anchor-data");
  var anchors = [];
  if (dataEl) {
    try { anchors = JSON.parse(dataEl.textContent || "[]"); } catch (err) { anchors = []; }
    if (!Array.isArray(anchors)) anchors = [];
  }

  var cardById = {};
  var anchoredCards = rail
    ? rail.querySelectorAll("[data-anchor-node-id]")
    : [];
  for (var cIndex = 0; cIndex < anchoredCards.length; cIndex += 1) {
    cardById[anchoredCards[cIndex].getAttribute("data-anchor-node-id")] =
      anchoredCards[cIndex];
  }

  var resolved = [];
  for (var index = 0; index < anchors.length; index += 1) {
    var offsets = resolveOffsets(anchors[index]);
    if (!offsets) continue;
    var range = rangeFor(offsets);
    if (!range) continue;
    resolved.push({
      nodeId: anchors[index].nodeId,
      range: range,
      start: offsets.start,
      end: offsets.end,
      card: cardById[anchors[index].nodeId] || null,
    });
  }

  if (resolved.length > 0 && canPaint) {
    var highlight = new Highlight();
    for (var hIndex = 0; hIndex < resolved.length; hIndex += 1) {
      highlight.add(resolved[hIndex].range);
    }
    CSS.highlights.set("qmux-research-query-anchors", highlight);

    // Regions where two or more anchors stack, repainted near the text color
    // (all anchors share one wash, so stacked coverage
    // would otherwise be invisible). Mirrors the app's overlap layer.
    var events = [];
    for (var oIndex = 0; oIndex < resolved.length; oIndex += 1) {
      events.push({ at: resolved[oIndex].start, delta: 1 });
      events.push({ at: resolved[oIndex].end, delta: -1 });
    }
    events.sort(function (a, b) { return a.at - b.at || a.delta - b.delta; });
    var overlaps = new Highlight();
    overlaps.priority = 1;
    var paintedOverlap = false;
    var depth = 0;
    var regionStart = null;
    for (var eIndex = 0; eIndex < events.length; eIndex += 1) {
      depth += events[eIndex].delta;
      if (depth >= 2 && regionStart === null) {
        regionStart = events[eIndex].at;
      } else if (depth < 2 && regionStart !== null) {
        if (events[eIndex].at > regionStart) {
          var overlapRange = rangeFor({ start: regionStart, end: events[eIndex].at });
          if (overlapRange) {
            overlaps.add(overlapRange);
            paintedOverlap = true;
          }
        }
        regionStart = null;
      }
    }
    if (paintedOverlap) {
      CSS.highlights.set("qmux-research-highlight-overlaps", overlaps);
    }
  }

  // Hover linking, both directions, as in the app: hovering the passage links
  // its card, and clicking the passage opens the follow-up. The passage keeps
  // its stable base paint throughout hover to avoid asynchronous highlight
  // registry repaint artifacts.
  var linkedEntry = null;
  function setLinkedEntry(entry) {
    if (entry === linkedEntry) return;
    if (linkedEntry && linkedEntry.card) {
      linkedEntry.card.classList.remove("is-anchor-linked");
    }
    if (linkedEntry && linkedEntry.connector) {
      linkedEntry.connector.classList.remove("is-anchor-linked");
    }
    linkedEntry = entry;
    if (entry && entry.card) {
      entry.card.classList.add("is-anchor-linked");
    }
    if (entry && entry.connector) {
      entry.connector.classList.add("is-anchor-linked");
    }
    root.classList.toggle("is-highlight-hovered", Boolean(entry));
  }

  function absoluteOffsetAt(clientX, clientY) {
    var node = null;
    var offset = 0;
    if (document.caretRangeFromPoint) {
      var caret = document.caretRangeFromPoint(clientX, clientY);
      if (caret) {
        node = caret.startContainer;
        offset = caret.startOffset;
      }
    } else if (document.caretPositionFromPoint) {
      var position = document.caretPositionFromPoint(clientX, clientY);
      if (position) {
        node = position.offsetNode;
        offset = position.offset;
      }
    }
    if (!node || node.nodeType !== 3) return -1;
    var nodeIndex = nodes.indexOf(node);
    if (nodeIndex < 0) return -1;
    return starts[nodeIndex] + offset;
  }

  function entryAtPoint(event) {
    var offset = absoluteOffsetAt(event.clientX, event.clientY);
    if (offset < 0) return null;
    for (var pIndex = 0; pIndex < resolved.length; pIndex += 1) {
      if (offset >= resolved[pIndex].start && offset < resolved[pIndex].end) {
        return resolved[pIndex];
      }
    }
    return null;
  }

  if (resolved.length > 0) {
    root.addEventListener("mousemove", function (event) {
      setLinkedEntry(entryAtPoint(event));
    });
    root.addEventListener("mouseleave", function () {
      setLinkedEntry(null);
    });
    root.addEventListener("click", function (event) {
      var entry = entryAtPoint(event);
      if (entry && entry.card && entry.card.href) {
        window.location.href = entry.card.href;
      }
    });
    for (var lIndex = 0; lIndex < resolved.length; lIndex += 1) {
      (function (entry) {
        if (!entry.card) return;
        entry.card.addEventListener("mouseenter", function () {
          setLinkedEntry(entry);
        });
        entry.card.addEventListener("mouseleave", function () {
          setLinkedEntry(null);
        });
        // Keyboard parity for the hover link: tabbing onto a card links the
        // pair, scrolling its passage into view when it sits off screen.
        // Guarded to focus-visible so mouse clicks don't jerk the page
        // before navigating.
        entry.card.addEventListener("focus", function () {
          if (!entry.card.matches(":focus-visible")) return;
          setLinkedEntry(entry);
          var passage = entry.range.getBoundingClientRect();
          if (passage.top < 0 || passage.bottom > window.innerHeight) {
            var target = entry.range.startContainer.parentElement;
            if (target) target.scrollIntoView({ block: "center" });
          }
        });
        entry.card.addEventListener("blur", function () {
          setLinkedEntry(null);
        });
      })(resolved[lIndex]);
    }
  }

  // Rounded-elbow connector path: out from the passage's line into the
  // gutter, a vertical run at midX, then into the card at the card's own
  // height. Degenerates to a straight segment when the pair is level or the
  // gutter is too tight for the turns. Ported from the app's research
  // document (connectorElbowPath).
  function connectorElbowPath(sx, sy, ex, ey, midX) {
    var dy = ey - sy;
    if (Math.abs(dy) < 2 || ex - sx < 8) {
      return "M " + sx + " " + sy + " L " + ex + " " + ey;
    }
    var radius = Math.min(10, Math.abs(dy) / 2, midX - sx, ex - midX);
    var dir = dy > 0 ? 1 : -1;
    return "M " + sx + " " + sy + " L " + (midX - radius) + " " + sy +
      " Q " + midX + " " + sy + " " + midX + " " + (sy + dir * radius) +
      " L " + midX + " " + (ey - dir * radius) +
      " Q " + midX + " " + ey + " " + (midX + radius) + " " + ey +
      " L " + ex + " " + ey;
  }

  function clearConnectors() {
    for (var ccIndex = 0; ccIndex < resolved.length; ccIndex += 1) {
      resolved[ccIndex].connector = null;
    }
    if (connectorSvg) {
      connectorSvg.remove();
      connectorSvg = null;
    }
  }

  // Dotted elbow leaders from each passage's first line to its anchored card,
  // as in the app. Overlapping vertical runs are assigned gutter lanes by
  // greedy interval colouring (top-to-bottom) and fanned leftward one lane
  // step per collision, capped per run so it never crowds the passage edge.
  function drawConnectors() {
    clearConnectors();
    if (!grid || !rail || window.innerWidth < 900) return;
    var gridRect = grid.getBoundingClientRect();
    var startX = Math.round(root.getBoundingClientRect().right - gridRect.left) + 8;
    var geometry = [];
    for (var gcIndex = 0; gcIndex < resolved.length; gcIndex += 1) {
      var connectorEntry = resolved[gcIndex];
      if (!connectorEntry.card || !connectorEntry.card.classList.contains("is-anchored")) {
        continue;
      }
      var lineRects = connectorEntry.range.getClientRects();
      var firstLine = null;
      for (var flIndex = 0; flIndex < lineRects.length; flIndex += 1) {
        if (lineRects[flIndex].width > 0) {
          firstLine = lineRects[flIndex];
          break;
        }
      }
      if (!firstLine) continue;
      var cardRect = connectorEntry.card.getBoundingClientRect();
      geometry.push({
        entry: connectorEntry,
        sy: Math.round(firstLine.top + firstLine.height / 2 - gridRect.top),
        ex: Math.round(cardRect.left - gridRect.left) - 6,
        ey: Math.round(cardRect.top - gridRect.top) + 17,
      });
    }
    if (geometry.length === 0) return;
    var laneEnds = [];
    var laneByIndex = {};
    geometry
      .map(function (run, index) {
        return {
          index: index,
          top: Math.min(run.sy, run.ey),
          bottom: Math.max(run.sy, run.ey),
        };
      })
      .sort(function (a, b) { return a.top - b.top || a.index - b.index; })
      .forEach(function (run) {
        var lane = -1;
        for (var laneIndex = 0; laneIndex < laneEnds.length; laneIndex += 1) {
          if (laneEnds[laneIndex] <= run.top) {
            lane = laneIndex;
            break;
          }
        }
        if (lane === -1) lane = laneEnds.length;
        laneEnds[lane] = run.bottom;
        laneByIndex[run.index] = lane;
      });
    var svgNs = "http://www.w3.org/2000/svg";
    connectorSvg = document.createElementNS(svgNs, "svg");
    connectorSvg.setAttribute("class", "research-anchor-connector");
    connectorSvg.setAttribute("aria-hidden", "true");
    for (var pcIndex = 0; pcIndex < geometry.length; pcIndex += 1) {
      var piece = geometry[pcIndex];
      var lane = laneByIndex[pcIndex] || 0;
      var maxOffset = 0.25 * (piece.ex - startX);
      var offset = Math.min(lane * 14, maxOffset);
      var midX = Math.round((startX + piece.ex) / 2 - offset);
      var pair = document.createElementNS(svgNs, "g");
      pair.setAttribute("class", "research-anchor-connector-pair");
      var pathEl = document.createElementNS(svgNs, "path");
      pathEl.setAttribute("d", connectorElbowPath(startX, piece.sy, piece.ex, piece.ey, midX));
      var dotEl = document.createElementNS(svgNs, "circle");
      dotEl.setAttribute("cx", String(startX));
      dotEl.setAttribute("cy", String(piece.sy));
      dotEl.setAttribute("r", "2");
      pair.appendChild(pathEl);
      pair.appendChild(dotEl);
      connectorSvg.appendChild(pair);
      piece.entry.connector = pair;
    }
    grid.appendChild(connectorSvg);
  }

  function positionCards() {
    if (!rail) return;
    // The narrow layout stacks the rail under the answer; anchored absolute
    // positioning has no passage-adjacent meaning there, so restore the flow.
    var cardsContainer = rail.querySelector(".research-followup-cards");
    if (window.innerWidth < 900) {
      for (var nIndex = 0; nIndex < anchoredCards.length; nIndex += 1) {
        anchoredCards[nIndex].classList.remove("is-anchored");
        anchoredCards[nIndex].style.top = "";
      }
      rail.style.minHeight = "";
      if (cardsContainer) cardsContainer.style.marginTop = "";
      clearConnectors();
      return;
    }
    if (cardsContainer) cardsContainer.style.marginTop = "";
    var railRect = rail.getBoundingClientRect();
    var entries = [];
    for (var rIndex = 0; rIndex < resolved.length; rIndex += 1) {
      var card = resolved[rIndex].card;
      if (!card) continue;
      var passage = resolved[rIndex].range.getBoundingClientRect();
      entries.push({ card: card, top: passage.top - railRect.top });
    }
    entries.sort(function (a, b) { return a.top - b.top; });
    var minTop = 0;
    var composer = rail.querySelector(".proposal-composer");
    if (composer) {
      minTop = composer.offsetTop + composer.offsetHeight + 16;
    }
    for (var eIndex = 0; eIndex < entries.length; eIndex += 1) {
      var entry = entries[eIndex];
      entry.card.classList.add("is-anchored");
      var top = Math.max(entry.top, minTop);
      entry.card.style.top = top + "px";
      minTop = top + entry.card.offsetHeight + 14;
    }
    var maxBottom = 0;
    for (var mIndex = 0; mIndex < entries.length; mIndex += 1) {
      var bottom = parseFloat(entries[mIndex].card.style.top) +
        entries[mIndex].card.offsetHeight;
      if (bottom > maxBottom) maxBottom = bottom;
    }
    if (maxBottom > 0) {
      rail.style.minHeight = maxBottom + "px";
    }
    // Anything still in the rail's flow (stacked cards, proposed follow-ups)
    // starts below the anchored cards instead of underneath them.
    if (cardsContainer && maxBottom > 0) {
      var hasFlowContent = rail.querySelector(".publication-proposals") !== null;
      for (var fIndex = 0; fIndex < cardsContainer.children.length; fIndex += 1) {
        if (!cardsContainer.children[fIndex].classList.contains("is-anchored")) {
          hasFlowContent = true;
        }
      }
      if (hasFlowContent) {
        var push = maxBottom + 20 - cardsContainer.offsetTop;
        if (push > 0) {
          cardsContainer.style.marginTop = push + "px";
        }
      }
    }
    drawConnectors();
  }

  if (resolved.length > 0) {
    positionCards();
    var positionCardsTimer = null;
    function schedulePositionCards() {
      if (positionCardsTimer !== null) window.clearTimeout(positionCardsTimer);
      positionCardsTimer = window.setTimeout(function () {
        positionCardsTimer = null;
        positionCards();
      }, 140);
    }
    window.addEventListener("resize", schedulePositionCards);
    if (typeof ResizeObserver !== "undefined") {
      var observedLayoutWidth = root.getBoundingClientRect().width;
      var layoutResizeObserver = new ResizeObserver(function (entries) {
        var nextWidth = entries[0] ? entries[0].contentRect.width : observedLayoutWidth;
        if (Math.abs(nextWidth - observedLayoutWidth) < 0.5) return;
        observedLayoutWidth = nextWidth;
        schedulePositionCards();
      });
      layoutResizeObserver.observe(root);
    }
    if (document.fonts && document.fonts.ready) {
      document.fonts.ready.then(positionCards);
    }
  }

  // ------------------------------------------------------------------
  // Anchored proposals: selecting answer text offers "Ask about this",
  // which quotes the passage into the rail's proposal composer.
  var proposalForm = document.querySelector("form.proposal-composer");
  var anchorInput = proposalForm
    ? proposalForm.querySelector('input[name="anchor"]')
    : null;
  var quoteRow = proposalForm
    ? proposalForm.querySelector("[data-qmux-proposal-quote]")
    : null;
  if (!proposalForm || !anchorInput || !quoteRow) return;
  var quoteText = quoteRow.querySelector(".research-followup-quote");
  var quoteDismiss = quoteRow.querySelector(".research-followup-quote-dismiss");

  var askButton = document.createElement("button");
  askButton.type = "button";
  askButton.className = "research-highlight-action";
  askButton.textContent = "Ask about this";
  askButton.hidden = true;
  document.body.appendChild(askButton);
  // Keep the selection alive through the click.
  askButton.addEventListener("mousedown", function (event) {
    event.preventDefault();
  });

  var pendingSelection = null;

  function selectionOffsets() {
    var selection = window.getSelection();
    if (!selection || selection.rangeCount === 0 || selection.isCollapsed) return null;
    var range = selection.getRangeAt(0);
    if (!root.contains(range.startContainer) || !root.contains(range.endContainer)) {
      return null;
    }
    // A conversation anchor must stay inside one turn body. Labels exist only
    // in the published projection, and crossing a label/turn seam creates an
    // anchor the app cannot relocate against its label-free conversation view.
    var contextRoot = root;
    if (root.querySelector(".research-conversation")) {
      var startElement = range.startContainer.nodeType === Node.ELEMENT_NODE
        ? range.startContainer
        : range.startContainer.parentElement;
      var endElement = range.endContainer.nodeType === Node.ELEMENT_NODE
        ? range.endContainer
        : range.endContainer.parentElement;
      var startTurn = startElement
        ? startElement.closest(".conversation-turn-body")
        : null;
      var endTurn = endElement
        ? endElement.closest(".conversation-turn-body")
        : null;
      if (!startTurn || startTurn !== endTurn || !root.contains(startTurn)) {
        return null;
      }
      contextRoot = startTurn;
    }
    var probe = document.createRange();
    probe.selectNodeContents(root);
    probe.setEnd(range.startContainer, range.startOffset);
    var start = (probe.cloneContents().textContent || "").length;
    probe.setEnd(range.endContainer, range.endOffset);
    var end = (probe.cloneContents().textContent || "").length;
    var contextStart = 0;
    var contextEnd = text.length;
    if (contextRoot !== root) {
      probe.setEnd(contextRoot, 0);
      contextStart = (probe.cloneContents().textContent || "").length;
      probe.setEnd(contextRoot, contextRoot.childNodes.length);
      contextEnd = (probe.cloneContents().textContent || "").length;
    }
    var slice = text.slice(start, end);
    var trimmedLeading = slice.length - slice.replace(/^\\s+/, "").length;
    var trimmedTrailing = slice.length - slice.replace(/\\s+$/, "").length;
    start += trimmedLeading;
    end -= trimmedTrailing;
    if (end <= start || end - start > 2000) return null;
    return {
      start: start,
      end: end,
      contextStart: contextStart,
      contextEnd: contextEnd,
      rect: range.getBoundingClientRect(),
    };
  }

  function updateAskAction() {
    pendingSelection = selectionOffsets();
    if (!pendingSelection) {
      askButton.hidden = true;
      return;
    }
    askButton.hidden = false;
    var left = Math.min(
      Math.max(8, pendingSelection.rect.left),
      window.innerWidth - askButton.offsetWidth - 8,
    );
    var top = Math.min(
      pendingSelection.rect.bottom + 8,
      window.innerHeight - askButton.offsetHeight - 8,
    );
    askButton.style.left = left + "px";
    askButton.style.top = top + "px";
  }

  document.addEventListener("mouseup", function () {
    setTimeout(updateAskAction, 0);
  });
  document.addEventListener("keyup", function () {
    setTimeout(updateAskAction, 0);
  });
  window.addEventListener("scroll", function () {
    askButton.hidden = true;
  }, { passive: true });

  askButton.addEventListener("click", function () {
    if (!pendingSelection) return;
    var start = pendingSelection.start;
    var end = pendingSelection.end;
    var exact = text.slice(start, end);
    if (!exact.trim()) return;
    anchorInput.value = JSON.stringify({
      start: start,
      end: end,
      exact: exact,
      prefix: text.slice(Math.max(pendingSelection.contextStart, start - 32), start),
      suffix: text.slice(end, Math.min(pendingSelection.contextEnd, end + 32)),
    });
    if (quoteText) {
      quoteText.textContent = exact.split(/\\s+/).join(" ").trim();
    }
    quoteRow.hidden = false;
    proposalForm.classList.add("is-anchored");
    askButton.hidden = true;
    var selection = window.getSelection();
    if (selection) selection.removeAllRanges();
    var promptField = proposalForm.querySelector("textarea");
    if (promptField) {
      promptField.focus();
      promptField.scrollIntoView({ block: "nearest" });
    }
  });

  if (quoteDismiss) {
    quoteDismiss.addEventListener("click", function () {
      anchorInput.value = "";
      quoteRow.hidden = true;
      proposalForm.classList.remove("is-anchored");
    });
  }
})();`;

const PAGE_SCRIPT_CSP_HASH = `'sha256-${createHash("sha256")
  .update(PAGE_SCRIPT)
  .digest("base64")}'`;

// The follow-up rail's composer analogue on the public page: signed-in
// visitors propose follow-up questions where the app's ask composer sits.
function ProposalComposer({
  gist,
  nodeId,
  viewer,
  webAuth,
}: {
  gist: GitHubGist;
  nodeId: string;
  viewer: ViewerSession | null;
  webAuth: GitHubWebAuthConfig | null;
}) {
  const returnTo = commentReturnPath(gist.id, nodeId);
  if (!viewer) {
    return webAuth ? (
      <div className="proposal-composer is-signed-out">
        <a
          className="control-button github-sign-in"
          href={`/auth/github?returnTo=${encodeURIComponent(`${returnTo}#proposals`)}`}
        >
          Sign in with GitHub to ask a follow-up
        </a>
      </div>
    ) : null;
  }
  return (
    <form className="proposal-composer" method="post" action={`${returnTo}/proposals`}>
      <input type="hidden" name="csrfToken" value={viewer.csrfToken} />
      <input type="hidden" name="anchor" defaultValue="" />
      <div className="research-followup-quote-row" data-qmux-proposal-quote hidden>
        <span className="research-followup-quote" />
        <button
          type="button"
          className="research-followup-quote-dismiss"
          aria-label="Remove the quoted passage"
          title="Remove quote"
        >
          ×
        </button>
      </div>
      <textarea
        className="textarea"
        id="proposal-prompt"
        name="prompt"
        aria-label="Propose a follow-up question"
        placeholder="Propose a follow-up…"
        required
        maxLength={MAX_RESEARCH_PROPOSAL_PROMPT_CHARACTERS}
        rows={2}
      />
      <details className="proposal-answer-details">
        <summary>Include a proposed answer</summary>
        <textarea
          className="textarea"
          id="proposal-answer"
          name="answerMarkdown"
          aria-label="Proposed answer (optional)"
          placeholder="Proposed answer (Markdown, optional)"
          maxLength={MAX_RESEARCH_PROPOSAL_ANSWER_CHARACTERS}
          rows={4}
        />
      </details>
      <div className="proposal-composer-actions">
        <small>as @{viewer.login}</small>
        <button className="control-button" type="submit">
          Send
        </button>
      </div>
    </form>
  );
}

function ComposerSignOut({
  viewer,
  returnTo,
}: {
  viewer: ViewerSession;
  returnTo: string;
}) {
  return (
    <form className="sign-out-form" method="post" action="/auth/logout">
      <input type="hidden" name="csrfToken" value={viewer.csrfToken} />
      <input type="hidden" name="returnTo" value={returnTo} />
      <span>Signed in as @{viewer.login}</span>
      <button className="btn-link" type="submit">
        Sign out
      </button>
    </form>
  );
}

function PublicationProposals({
  gist,
  publication,
  nodeId,
  comments,
}: {
  gist: GitHubGist;
  publication: ResearchPublication;
  nodeId: string;
  comments: PublicationComment[];
}) {
  const ownerLogin = gist.owner?.login ?? null;
  const proposals = comments.filter(
    (comment) =>
      comment.proposal?.publicationId === publication.publicationId &&
      comment.proposal.parentNodeId === nodeId,
  );
  const proposalsById = new Map(proposals.map((comment) => [comment.id, comment]));
  const resolutions = new Map<number, ProposalResolutionPayload>();
  for (const comment of comments) {
    const proposalComment = comment.resolution
      ? proposalsById.get(comment.resolution.proposalCommentId)
      : null;
    if (
      comment.resolution &&
      comment.resolution.publicationId === publication.publicationId &&
      comment.user.login === ownerLogin &&
      proposalComment?.proposal &&
      comment.resolution.proposalDigest ===
        researchProposalDigest(proposalComment.proposal)
    ) {
      resolutions.set(comment.resolution.proposalCommentId, comment.resolution);
    }
  }
  if (proposals.length === 0) {
    return null;
  }
  return (
    <section
      className="publication-proposals"
      id="proposals"
      aria-labelledby="proposals-title"
    >
      <h3 id="proposals-title">Proposed follow-ups</h3>
      {proposals.map((comment) => {
        const proposal = comment.proposal!;
        const resolution = resolutions.get(comment.id) ?? null;
        return (
          <article className="publication-proposal" key={comment.id}>
            <header>
              <a href={comment.user.htmlUrl}>@{comment.user.login}</a>
              <span className={`proposal-status is-${resolution?.status ?? "pending"}`}>
                {resolution?.status ?? "pending"}
              </span>
            </header>
            {proposal.anchor ? (
              <span className="research-followup-quote">
                {quoteDisplayText(proposal.anchor.exact)}
              </span>
            ) : null}
            <div className="proposal-question turn-markdown">
              <SafeMarkdown>{proposal.prompt}</SafeMarkdown>
            </div>
            {proposal.answerMarkdown ? (
              <details>
                <summary>Proposed answer</summary>
                <div className="proposal-answer-body turn-markdown">
                  <SafeMarkdown>{proposal.answerMarkdown}</SafeMarkdown>
                </div>
              </details>
            ) : null}
            {resolution?.publicNodeId ? (
              <a
                className="proposal-result-link"
                href={publicationNodePath(gist.id, resolution.publicNodeId)}
              >
                Open published result
              </a>
            ) : null}
          </article>
        );
      })}
    </section>
  );
}

function publicationNodePath(gistId: string, nodeId: string) {
  return `/p/${encodeURIComponent(gistId)}/n/${encodeURIComponent(nodeId)}`;
}

function SafeMarkdown({ children }: { children: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={transcriptRemarkPlugins}
      rehypePlugins={transcriptRehypePlugins}
      skipHtml
      components={{
        a: ({ href, children: label }) => (
          <a href={safeHref(href)} rel="noreferrer">
            {label}
          </a>
        ),
        img: ({ alt }) => <span className="image-omitted">[Image omitted{alt ? `: ${alt}` : ""}]</span>,
        // The app frames tables in a rounded scroll wrapper; mirror it so wide
        // tables scroll inside the answer column instead of widening it.
        table: ({ children: tableChildren }) => (
          <div className="turn-markdown-table-wrap">
            <table>{tableChildren}</table>
          </div>
        ),
      }}
    >
      {children}
    </ReactMarkdown>
  );
}

function safeHref(value?: string) {
  if (!value) {
    return "#";
  }
  try {
    const parsed = new URL(value, "https://qmux.app");
    return parsed.protocol === "http:" || parsed.protocol === "https:" || parsed.protocol === "mailto:"
      ? value
      : "#";
  } catch {
    return "#";
  }
}

function documentPage(input: { title: string; description: string; body: React.ReactNode }) {
  const markup = renderToStaticMarkup(
    <html lang="en">
      <head>
        <meta charSet="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <meta name="theme-color" content="#14171a" />
        <link rel="icon" type="image/png" href="/logo.png" />
        <meta name="description" content={input.description} />
        <meta property="og:type" content="article" />
        <meta property="og:site_name" content="qmux" />
        <meta property="og:title" content={input.title} />
        <meta property="og:description" content={input.description} />
        <meta property="og:image" content="/qmux.png" />
        <meta name="twitter:card" content="summary" />
        <title>{`${input.title} · qmux`}</title>
        <style>{PAGE_CSS}</style>
      </head>
      <body>{input.body}</body>
    </html>,
  );
  return `<!doctype html>${markup}`;
}

function errorPage(title: string, message: string) {
  return documentPage({
    title,
    description: message,
    body: (
      <main className="error-page">
        <a className="brand" href="/">
          qmux
        </a>
        <h1>{title}</h1>
        <p>{message}</p>
      </main>
    ),
  });
}

async function serveStaticFile(
  response: ServerResponse,
  path: string,
  contentType: string,
  method: string | undefined,
  cacheControl?: string,
) {
  try {
    const body = await readFile(path);
    response.writeHead(200, {
      "Content-Type": contentType,
      "Content-Length": body.byteLength,
      "Cache-Control":
        cacheControl ??
        (contentType.startsWith("image/") ? "public, max-age=86400" : "public, max-age=300"),
      "X-Content-Type-Options": "nosniff",
    });
    response.end(method === "HEAD" ? undefined : body);
  } catch {
    sendHtml(response, 404, errorPage("Not found", "This page does not exist."));
  }
}

function sendHtml(
  response: ServerResponse,
  status: number,
  body: string,
  cacheControl = "no-store",
  method?: string,
  extraHeaders?: Record<string, string>,
) {
  response.writeHead(status, {
    "Content-Type": "text/html; charset=utf-8",
    "Content-Length": Buffer.byteLength(body),
    "Cache-Control": cacheControl,
    // Scripts stay locked to the single static anchor-positioning script via
    // its hash; nothing dynamic or attacker-influenced is ever executable.
    "Content-Security-Policy":
      `default-src 'none'; style-src 'unsafe-inline'; script-src ${PAGE_SCRIPT_CSP_HASH}; img-src data:; font-src 'self'; connect-src 'none'; object-src 'none'; base-uri 'none'; form-action 'self'`,
    "Referrer-Policy": "no-referrer",
    "X-Content-Type-Options": "nosniff",
    ...extraHeaders,
  });
  response.end(method === "HEAD" ? undefined : body);
}

// The app's duration style: seconds under a minute, then m/s, then h/m.
function formatDuration(milliseconds: number) {
  const totalSeconds = Math.max(1, Math.round(milliseconds / 1000));
  if (totalSeconds < 60) {
    return `${totalSeconds}s`;
  }
  const totalMinutes = Math.floor(totalSeconds / 60);
  if (totalMinutes < 60) {
    return `${totalMinutes}m ${totalSeconds % 60}s`;
  }
  return `${Math.floor(totalMinutes / 60)}h ${totalMinutes % 60}m`;
}

function formatDate(value: string) {
  return new Intl.DateTimeFormat("en", {
    dateStyle: "medium",
    timeStyle: "short",
    timeZone: "UTC",
  }).format(new Date(value));
}

// The public page speaks the desktop app's design language: the green-blob
// theme's surfaces and text roles (spelled as literals — tokens.css cannot be
// imported here), the research document's chrome header + prompt card +
// answer-column/follow-up-rail grid, and the transcript feature's turn-markdown
// type ramp. When the app's look changes, port the values.
const PAGE_CSS = `
/* Valley Sans (100-900 variable) and JetBrains Mono webfonts, bundled under the
   SIL OFL 1.1 (licenses in site/fonts/), same files the desktop app bundles. */
@font-face { font-family:"Valley Sans"; src:url("/fonts/ValleySans-Variable.woff2") format("woff2"); font-style:normal; font-weight:100 900; font-display:swap; }
@font-face { font-family:"Valley Sans"; src:url("/fonts/ValleySans-VariableItalic.woff2") format("woff2"); font-style:italic; font-weight:100 900; font-display:swap; }
@font-face { font-family:"JetBrains Mono"; src:url("/fonts/JetBrainsMono-Regular.woff2") format("woff2"); font-style:normal; font-weight:400; font-display:swap; }
@font-face { font-family:"JetBrains Mono"; src:url("/fonts/JetBrainsMono-Italic.woff2") format("woff2"); font-style:italic; font-weight:400; font-display:swap; }
@font-face { font-family:"JetBrains Mono"; src:url("/fonts/JetBrainsMono-Bold.woff2") format("woff2"); font-style:normal; font-weight:700; font-display:swap; }
@font-face { font-family:"JetBrains Mono"; src:url("/fonts/JetBrainsMono-BoldItalic.woff2") format("woff2"); font-style:italic; font-weight:700; font-display:swap; }
:root {
  color-scheme:dark;
  --workspace-bg:#151719;
  --research-anchor-connector:#78827f;
  --chrome-header-bg:#14171a;
  --content-card-bg:#1d2224;
  --field-bg:#111315;
  --control-bg:#24282b;
  --control-bg-hover:#2c3134;
  --control-border:#3a3d3f;
  --control-border-hover:#474b4e;
  --text-primary:#e7e7e2;
  --text-strong:#f1f0e8;
  --text-heading:#f4f3ec;
  --text-secondary:#c4cbc6;
  --text-body-soft:#d9ddd9;
  --text-muted:#8a938e;
  --text-subtle:#7f8884;
  --text-faint:#6f7773;
  --accent-color:#8fd6c7;
  --status-failed-fg:#d98787;
  --status-success-fg:#81c784;
  --surface-border-faint:rgba(255,255,255,0.06);
  --surface-border-subtle:rgba(255,255,255,0.075);
  --surface-border-default:rgba(255,255,255,0.12);
  --surface-border-strong:#30383b;
  --surface-fill-hover:rgba(255,255,255,0.06);
  --markdown-blockquote-border:#3d4a4d;
  --markdown-table-border:#262d2f;
  --markdown-table-frame-border:#2f3639;
  --transcript-code-bg:#111416;
  --font-ui:"Valley Sans","Inter",ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
  --font-mono:"JetBrains Mono",ui-monospace,SFMono-Regular,Menlo,Monaco,Consolas,monospace;
}
* { box-sizing:border-box; }
/* Author display values on controls would otherwise override the UA's
   [hidden] rule; script-revealed elements rely on hidden actually hiding. */
[hidden] { display:none !important; }
html, body { height:100%; }
body { margin:0; background:var(--workspace-bg); color:#dfe3df; font:14px/1.5 var(--font-ui); font-synthesis:none; text-rendering:optimizeLegibility; -webkit-font-smoothing:antialiased; }
a { color:inherit; text-decoration:none; }

/* Document chrome bar, as in the app's research document header. */
.research-workspace { min-height:100%; display:flex; flex-direction:column; }
.doc-header { position:sticky; top:0; z-index:10; display:flex; align-items:center; gap:12px; min-height:48px; padding:8px 18px; border-bottom:1px solid var(--surface-border-subtle); background:var(--chrome-header-bg); }
.brand { flex:none; color:var(--text-strong); font-size:14px; font-weight:650; letter-spacing:-0.01em; }
.doc-breadcrumb { display:flex; min-width:0; flex:1; align-items:center; overflow:hidden; white-space:nowrap; font-size:13px; }
.doc-breadcrumb > span { display:inline-flex; min-width:0; align-items:center; }
.doc-breadcrumb a, .doc-breadcrumb .is-current { display:block; min-width:0; padding:0 3px; overflow:hidden; color:#aeb6b1; text-overflow:ellipsis; white-space:nowrap; }
.doc-breadcrumb a:hover { color:#f0f2ef; }
.doc-breadcrumb .is-current { color:#f0f2ef; }
.doc-breadcrumb-separator { margin:0 5px; color:#59615d; }
.doc-followup-count { flex:none; color:#78817c; font-size:13px; white-space:nowrap; }
.doc-kind { flex:none; display:inline-flex; align-items:center; padding:1px 8px; border:1px solid var(--surface-border-default); border-radius:999px; color:var(--text-subtle); font-size:10.5px; white-space:nowrap; }
.doc-header-action { flex:none; display:inline-flex; align-items:center; min-height:28px; padding:0 9px; border:1px solid var(--control-border); border-radius:6px; color:var(--text-muted); font-size:12.5px; white-space:nowrap; background:rgba(20,24,26,0.9); }
.doc-header-action:hover { color:var(--text-strong); background:rgba(32,38,40,0.95); }

/* Document scroller and content column widths, matching the app. */
.research-document-scroll { flex:1; padding:44px clamp(24px,5vw,72px) 40px; }
.research-document-content { width:min(100%,1160px); min-width:0; margin:0 auto; }
.research-response-grid { position:relative; display:grid; grid-template-columns:minmax(0,640px) minmax(220px,260px); align-items:start; gap:clamp(28px,4vw,52px); min-width:0; max-width:100%; }
.research-response-grid.is-single { grid-template-columns:minmax(0,640px); }
/* Without a rail (published research answers), the whole document narrows to
   the answer column and centers, like the transcript view. */
.research-document-content.is-single-doc { max-width:640px; }
.research-response { min-width:0; max-width:640px; }
.research-response-content-root { min-width:0; line-height:1.62; overflow-wrap:anywhere; word-break:break-word; }

/* The user's prompt, visually distinct from the answer below it. */
.research-prompt { width:fit-content; max-width:min(100%,640px); margin:0 0 26px -14px; padding:10px 14px; border-radius:10px; background:var(--content-card-bg); }
.research-prompt > * { max-width:100%; }
.research-prompt .turn-markdown { color:#f0f2ef; font-size:14.5px; font-weight:400; line-height:1.5; }
.research-prompt-quote { margin:0 0 8px; padding-left:10px; border-left:3px solid var(--accent-color); color:#a6aea9; font-size:13.5px; font-style:italic; line-height:1.45; }
.research-parent-link { display:inline-block; margin:0 0 8px; color:#8f9893; font-size:14px; }
.research-parent-link:hover { color:#c6cdc9; }
.research-contribution { margin:0 0 16px; color:var(--text-subtle); font-size:12px; }
.research-contribution a:hover { color:var(--text-secondary); }
.research-response-error { margin:0 0 18px; color:var(--status-failed-fg); line-height:1.5; }

/* Passages that published follow-ups were asked about keep one stable faint
   wash while hover links them to their associated card. */
::highlight(qmux-research-query-anchors) { color:inherit; background:rgba(216,196,95,0.14); }
::highlight(qmux-research-highlight-overlaps) { color:inherit; background:rgba(211,216,212,0.16); }
.research-response-content-root.is-highlight-hovered { cursor:pointer; }

/* Markdown body, ported from the app's transcript styles. */
.turn-markdown { min-width:0; color:var(--text-strong); font-size:14.5px; overflow-wrap:anywhere; }
.turn-markdown > :first-child { margin-top:0; }
.turn-markdown > :last-child { margin-bottom:0; }
.turn-markdown strong, .turn-markdown b { font-weight:600; }
.turn-markdown p, .turn-markdown ul, .turn-markdown ol, .turn-markdown blockquote, .turn-markdown pre { margin:0 0 8px; }
.turn-markdown h1, .turn-markdown h2, .turn-markdown h3, .turn-markdown h4, .turn-markdown h5, .turn-markdown h6 { margin:22px 0 7px; color:var(--text-heading); font-weight:600; line-height:1.42; letter-spacing:-0.007em; }
.turn-markdown h1 { font-size:21.5px; line-height:1.25; }
.turn-markdown h2 { font-size:18px; }
.turn-markdown h3 { font-size:16px; }
.turn-markdown h4 { font-size:14.5px; }
.turn-markdown h5 { font-size:13.5px; color:var(--text-body-soft); }
.turn-markdown h6 { margin-bottom:5px; font-size:12.5px; letter-spacing:0.04em; text-transform:uppercase; color:#a7b0ab; }
.turn-markdown ul, .turn-markdown ol { padding-left:18px; }
.turn-markdown ol { padding-left:20px; }
.turn-markdown li { margin:2px 0; }
.turn-markdown li > p { margin:0; }
.turn-markdown a { color:var(--accent-color); text-underline-offset:2px; }
.turn-markdown a:hover { text-decoration:underline; }
.turn-markdown blockquote { border-left:2px solid var(--markdown-blockquote-border); padding-left:9px; color:var(--text-secondary); }
.turn-markdown code { border:1px solid var(--surface-border-strong); border-radius:4px; background:var(--transcript-code-bg); padding:1px 4px; color:var(--text-primary); font-family:var(--font-mono); font-size:13px; font-variant-ligatures:none; }
.turn-markdown pre { overflow:auto; border:1px solid var(--surface-border-strong); border-radius:6px; background:var(--transcript-code-bg); padding:8px; }
.turn-markdown pre code { display:block; border:0; background:transparent; padding:0; white-space:pre; }
.turn-markdown hr { margin:24px 0; border:0; border-top:1px solid var(--surface-border-subtle); }
.turn-markdown-table-wrap { max-width:100%; margin:0 0 12px; border:1px solid var(--markdown-table-frame-border); border-radius:6px; overflow-x:auto; }
.turn-markdown-table-wrap table { width:100%; margin:0; border-collapse:collapse; font-size:14px; }
.turn-markdown mjx-container[jax="SVG"][display="true"] { margin:0 0 12px; overflow-x:auto; overflow-y:hidden; padding:2px 0; }
.turn-markdown th, .turn-markdown td { padding:8px 12px; border-right:1px solid var(--markdown-table-border); border-bottom:1px solid var(--markdown-table-border); text-align:left; vertical-align:top; overflow-wrap:break-word; }
.turn-markdown th { background:#1a1f21; border-bottom-color:#38403f; color:var(--text-body-soft); font-weight:600; }
.turn-markdown th:last-child, .turn-markdown td:last-child { border-right:0; }
.turn-markdown tr:last-child td { border-bottom:0; }
.image-omitted { color:var(--text-subtle); font-style:italic; }

/* Answer meta line under the response, as in the app. */
.research-answer-meta { display:flex; flex-wrap:wrap; min-height:24px; align-items:center; gap:6px 12px; margin-top:20px; color:#69716d; font-size:14px; line-height:1.3; }
.research-answer-meta a:hover { color:#c2cac5; }
.research-meta-status.is-failed { color:var(--status-failed-fg); }
.research-meta-status.is-cancelled { color:#9a938b; }
.research-answer-copy { display:inline-flex; align-items:center; padding:2px 0; border:0; color:#7d8782; font:inherit; background:transparent; cursor:pointer; }
.research-answer-copy:hover { color:#c2cac5; }

/* Follow-up rail: quiet link cards; anchored ones sit beside their passage. */
.research-followups { position:relative; display:flex; min-width:0; flex-direction:column; gap:16px; margin-top:2px; padding-right:2px; }
.research-followup-cards { display:flex; flex-direction:column; gap:20px; }
.research-followup-card { display:flex; flex-direction:column; align-items:stretch; min-height:0; text-align:left; transition:border-color 120ms ease; }
.research-followup-card.is-anchored { position:absolute; z-index:1; right:2px; left:0; padding:9px 11px; border:1px solid var(--surface-border-subtle); border-radius:10px; background:var(--content-card-bg); }
.research-followup-card.is-anchored:hover { z-index:4; border-color:rgba(255,255,255,0.1); }
.research-followup-card.is-anchored.is-anchor-linked { z-index:4; border-color:rgba(255,255,255,0.1); }
/* Dotted elbow leaders from each passage to its follow-up card, drawn into
   this overlay by the page script; mirrors the app's research document. */
.research-anchor-connector { position:absolute; inset:0; width:100%; height:100%; overflow:visible; pointer-events:none; }
.research-anchor-connector-pair { opacity:0.56; transition:opacity 120ms ease; }
.research-anchor-connector-pair.is-anchor-linked { opacity:1; }
.research-anchor-connector path { fill:none; stroke:var(--research-anchor-connector); stroke-width:1.4; stroke-dasharray:2 5; stroke-linecap:round; }
.research-anchor-connector circle { fill:var(--research-anchor-connector); }
.research-followup-card.is-anchor-linked > strong { color:#f2f4f1; }
.research-followup-card.is-anchor-linked .research-followup-preview { color:#7d8580; }
.research-followup-quote { display:-webkit-box; min-width:0; overflow:hidden; margin-bottom:5px; padding-left:8px; border-left:2px solid var(--accent-color); color:#9aa39e; font-size:13px; font-style:italic; line-height:1.4; -webkit-box-orient:vertical; -webkit-line-clamp:2; }
.research-followup-card > strong { display:-webkit-box; overflow:hidden; color:#e6e9e5; font-size:14px; font-weight:400; line-height:1.42; overflow-wrap:anywhere; -webkit-box-orient:vertical; -webkit-line-clamp:6; transition:color 120ms ease; }
.research-followup-card:hover > strong { color:#f2f4f1; }
.research-followup-preview { display:-webkit-box; margin-top:4px; overflow:hidden; color:#767e79; font-size:14px; line-height:1.42; overflow-wrap:anywhere; -webkit-box-orient:vertical; -webkit-line-clamp:2; transition:color 120ms ease; }
.research-followup-card:hover .research-followup-preview { color:#7d8580; }
.research-followup-card small { margin-top:8px; color:#707975; font-size:12px; text-transform:capitalize; }
.research-followup-card small.is-failed { color:var(--status-failed-fg); }
.research-followup-card small.is-cancelled { color:#9a938b; }
.research-followup-card small.is-contributed { color:#707975; }

/* The rail composer: propose a follow-up where the app's ask composer sits. */
.proposal-composer { display:flex; flex-direction:column; gap:8px; margin:0 0 8px; font-size:13px; }
.proposal-composer .textarea { width:100%; resize:vertical; padding:8px 10px; border:1px solid var(--control-border); border-radius:8px; background:var(--field-bg); color:var(--text-strong); font:inherit; font-size:13.5px; line-height:1.5; }
.proposal-composer .textarea::placeholder { color:#6b726d; }
.proposal-composer .textarea:focus { outline:none; border-color:var(--control-border-hover); }
.proposal-answer-details { color:var(--text-faint); font-size:12.5px; }
.proposal-answer-details summary { cursor:pointer; }
.proposal-answer-details .textarea { margin-top:8px; }
.proposal-composer-actions { display:flex; align-items:center; justify-content:space-between; gap:8px; }
.proposal-composer-actions small { color:#737c77; }
.proposal-composer.is-signed-out { margin:0 0 8px; }

/* Ask-about-passage: the floating selection action and the anchored state of
   the composer, both in the app's highlight-action styling. */
.research-highlight-action { position:fixed; z-index:1000; display:inline-flex; align-items:center; gap:7px; width:max-content; min-height:27px; padding:0 9px; border:1px solid var(--surface-border-default); border-radius:5px; color:#edf0ec; font-family:var(--font-ui); font-size:13px; font-weight:400; line-height:1; background:#303532; box-shadow:0 4px 14px rgba(0,0,0,0.38); cursor:pointer; }
.research-highlight-action:hover { background:#3a403c; }
.proposal-composer.is-anchored { z-index:5; padding:9px 10px; border:1px solid var(--surface-border-default); border-radius:10px; background:#303233; box-shadow:0 8px 24px rgba(0,0,0,0.35); }
.research-followup-quote-row { display:flex; align-items:flex-start; justify-content:space-between; gap:6px; }
.research-followup-quote-row[hidden] { display:none; }
.research-followup-quote-row .research-followup-quote { -webkit-line-clamp:3; margin-bottom:0; }
.research-followup-quote-dismiss { display:inline-flex; width:20px; min-width:20px; height:20px; align-items:center; justify-content:center; padding:0; border:0; border-radius:5px; color:var(--text-subtle); background:transparent; font:inherit; font-size:14px; line-height:1; cursor:pointer; }
.research-followup-quote-dismiss:hover { color:var(--text-strong); background:var(--surface-fill-hover); }
.publication-proposal .research-followup-quote { margin-bottom:0; }

/* Shared control style, ported from the app's control buttons. */
.control-button { display:inline-flex; align-items:center; justify-content:center; gap:6px; min-height:28px; padding:3px 10px; border:1px solid var(--control-border); border-radius:6px; color:var(--text-primary); background:var(--control-bg); font:inherit; font-size:12.5px; line-height:1.4; cursor:pointer; white-space:nowrap; }
.control-button:hover { background:var(--control-bg-hover); border-color:var(--control-border-hover); }
.btn-link { padding:0; border:0; background:none; color:var(--text-subtle); font:inherit; font-size:12px; cursor:pointer; }
.btn-link:hover { color:var(--text-primary); text-decoration:underline; }
.textarea { font-family:var(--font-ui); }
.github-sign-in { white-space:normal; text-align:center; }

/* Proposed follow-ups in the rail, like the app's community proposals. */
.publication-proposals { display:flex; flex-direction:column; gap:12px; padding-top:16px; border-top:1px solid var(--surface-border-subtle); }
.publication-proposals h3 { margin:0; color:var(--text-subtle); font-size:14px; font-weight:650; }
.publication-proposal { display:flex; min-width:0; flex-direction:column; gap:8px; padding:11px 0 0; border-top:1px solid var(--surface-border-subtle); }
.publication-proposal header { display:flex; align-items:center; justify-content:space-between; gap:8px; color:var(--text-faint); font-size:13px; }
.publication-proposal header > a { min-width:0; overflow:hidden; color:var(--text-subtle); font-weight:650; text-overflow:ellipsis; white-space:nowrap; }
.publication-proposal header > a:hover { color:var(--text-secondary); }
.proposal-status { text-transform:capitalize; }
.proposal-status.is-accepted { color:var(--status-success-fg); }
.proposal-status.is-declined { color:var(--status-failed-fg); }
.publication-proposal .turn-markdown { color:#edf0ec; font-size:14px; line-height:1.45; }
.publication-proposal details { color:var(--text-faint); font-size:13px; }
.publication-proposal details summary { cursor:pointer; }
.publication-proposal details .proposal-answer-body { margin-top:8px; }
.proposal-result-link { align-self:flex-start; font-size:13px; color:var(--accent-color); }
.proposal-result-link:hover { text-decoration:underline; }

/* Published transcript: the app's exported-conversation reading view. */
.conversation-column { max-width:640px; margin:0 auto; }
.research-document-content.is-conversation .page-footer { max-width:640px; margin-left:auto; margin-right:auto; }
.conversation-prompt { min-width:0; margin:18px 0 9px; padding:8px 12px; border-left:2px solid var(--markdown-blockquote-border); background:rgba(255,255,255,0.03); }
.conversation-prompt .turn-markdown { font-weight:550; }
.conversation-prompt:first-child { margin-top:0; }
.conversation-answer { margin:0 0 9px; line-height:1.62; }

/* Published research conversation: labelled per-turn bubbles inside the
   anchorable answer root, so a reader can quote any turn in a comment. */
.research-conversation { display:flex; flex-direction:column; gap:16px; }
.conversation-turn { min-width:0; display:flex; flex-direction:column; gap:5px; }
.conversation-turn-label { font-size:11px; font-weight:700; letter-spacing:0.06em; text-transform:uppercase; color:var(--text-faint); }
.conversation-turn-body { min-width:0; line-height:1.62; overflow-wrap:anywhere; }
.conversation-turn.is-user { padding:10px 14px; border-radius:10px; border:1px solid var(--surface-border-faint); background:rgba(255,255,255,0.03); }
.conversation-turn.is-user .conversation-turn-body { font-weight:550; }
.conversation-turn.is-assistant { padding:0 2px; }

/* Proposal load failures surface quietly in the rail. */
.proposals-error { margin:0; color:var(--status-failed-fg); font-size:13px; line-height:1.4; }
.sign-out-form { display:flex; align-items:center; gap:10px; color:var(--text-subtle); font-size:12px; }

/* Quiet page footer. */
.page-footer { margin-top:56px; padding-top:14px; border-top:1px solid var(--surface-border-faint); color:var(--text-faint); font-size:12.5px; }
.page-footer a { color:var(--text-subtle); }
.page-footer a:hover { color:var(--text-secondary); text-decoration:underline; }

/* Keyboard focus, in the app's accent. */
a:focus-visible, button:focus-visible, .textarea:focus-visible, summary:focus-visible { outline:2px solid var(--accent-color); outline-offset:2px; border-radius:4px; }
.research-followup-card:focus-visible { outline:2px solid var(--accent-color); outline-offset:3px; border-radius:10px; }
.textarea:focus-visible { outline-offset:0; }

.error-page { max-width:640px; margin:0 auto; padding:56px 24px 64px; }
.error-page .brand { font-size:15px; }
.error-page h1 { margin:36px 0 10px; color:var(--text-heading); font-size:22px; line-height:1.25; font-weight:600; letter-spacing:-0.02em; }
.error-page p { margin:0; color:var(--text-muted); }

@media (max-width:900px) {
  /* The rail stacks under the answer; there is no gutter to draw into. */
  .research-anchor-connector { display:none; }
  .research-document-scroll { padding-inline:24px; }
  .research-response-grid { grid-template-columns:minmax(0,1fr); }
  .research-followups { padding:28px 0 0; border-top:1px solid var(--surface-border-subtle); }
}
@media (max-width:640px) {
  .doc-header { flex-wrap:wrap; }
  .doc-kind { display:none; }
  .research-document-scroll { padding:24px 18px 32px; }
  .turn-markdown h1 { font-size:19px; }
}

/* Print/reader fallback: the dark theme becomes ink on paper, and everything
   interactive (chrome, rail, composers, forms) drops away. */
@media print {
  :root { color-scheme:light; }
  body { background:#ffffff; color:#1a1a1a; }
  .doc-header, .research-followups, .research-anchor-connector,
  .proposal-composer, .github-sign-in,
  .sign-out-form, .page-footer, .research-answer-copy,
  .research-highlight-action { display:none !important; }
  .research-response-grid { display:block; }
  .research-document-scroll { padding:0; }
  .turn-markdown, .research-response-content-root, .conversation-answer,
  .publication-proposal .turn-markdown { color:#1a1a1a; }
  .turn-markdown h1, .turn-markdown h2, .turn-markdown h3, .turn-markdown h4,
  .turn-markdown h5, .turn-markdown h6 { color:#000000; }
  .research-prompt { background:#f2f2f0; }
  .research-prompt .turn-markdown { color:#1a1a1a; }
  .research-prompt-quote { color:#555555; }
  .conversation-prompt { background:#f6f6f4; border-left-color:#bbbbbb; }
  .conversation-turn-body { color:#1a1a1a; }
  .conversation-turn-label { color:#666666; }
  .conversation-turn.is-user { background:#f6f6f4; border-color:#dddddd; }
  .turn-markdown code { background:#f4f4f2; border-color:#dddddd; color:#1a1a1a; }
  .turn-markdown pre { background:#f8f8f6; border-color:#dddddd; }
  .turn-markdown blockquote { color:#444444; border-left-color:#bbbbbb; }
  .turn-markdown a { color:#1a1a1a; text-decoration:underline; }
  .turn-markdown th { background:#f0f0ee; color:#1a1a1a; border-color:#cccccc; }
  .turn-markdown td { border-color:#dddddd; }
  .turn-markdown-table-wrap { border-color:#cccccc; }
  .research-answer-meta { color:#666666; }
  ::highlight(qmux-research-query-anchors) { background:rgba(178,142,29,0.15); }
  ::highlight(qmux-research-highlight-overlaps) { background:rgba(26,26,26,0.1); }
}
`;

const isDirectRun =
  process.argv[1] !== undefined &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href;

if (isDirectRun) {
  const host = process.env.HOST ?? DEFAULT_HOST;
  const port = Number.parseInt(process.env.PORT ?? String(DEFAULT_PORT), 10);
  const server = createQmuxWebServer();
  server.listen(port, host, () => {
    console.log(`qmux web listening on http://${host}:${port}`);
  });
}
