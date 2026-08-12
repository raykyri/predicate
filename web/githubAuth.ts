import {
  createCipheriv,
  createDecipheriv,
  createHash,
  randomBytes,
  timingSafeEqual,
} from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";

const SESSION_COOKIE = "qmux_session";
const OAUTH_STATE_COOKIE = "qmux_oauth_state";
const SESSION_MAX_AGE_SECONDS = 30 * 24 * 60 * 60;
const OAUTH_STATE_MAX_AGE_SECONDS = 10 * 60;
const MAX_OAUTH_RESPONSE_BYTES = 128 * 1024;

export interface GitHubWebAuthConfig {
  clientId: string;
  clientSecret: string;
  sessionSecret: string;
  publicOrigin: string;
  secureCookies: boolean;
}

export interface ViewerSession {
  accessToken: string;
  login: string;
  csrfToken: string;
  expiresAt: number;
}

export interface GitHubWebAuthOptions {
  oauthClientId?: string | null;
  oauthClientSecret?: string | null;
  sessionSecret?: string | null;
  publicOrigin?: string | null;
  secureCookies?: boolean;
}

interface OAuthState {
  state: string;
  returnTo: string;
  expiresAt: number;
}

interface OAuthTokenResponse {
  access_token?: string;
  scope?: string;
  error?: string;
  error_description?: string;
}

interface GitHubUserResponse {
  login?: string;
}

export function resolveGitHubWebAuthConfig(
  options: GitHubWebAuthOptions = {},
): GitHubWebAuthConfig | null {
  const clientId = normalized(
    options.oauthClientId ?? process.env.GITHUB_OAUTH_CLIENT_ID,
  );
  const clientSecret = normalized(
    options.oauthClientSecret ?? process.env.GITHUB_OAUTH_CLIENT_SECRET,
  );
  const sessionSecret = normalized(
    options.sessionSecret ?? process.env.QMUX_SESSION_SECRET,
  );
  const configuredValues = [clientId, clientSecret, sessionSecret].filter(Boolean).length;
  if (configuredValues === 0) {
    return null;
  }
  if (!clientId || !clientSecret || !sessionSecret) {
    throw new Error(
      "GitHub web comments require GITHUB_OAUTH_CLIENT_ID, GITHUB_OAUTH_CLIENT_SECRET, and QMUX_SESSION_SECRET.",
    );
  }
  if (sessionSecret.length < 32) {
    throw new Error("QMUX_SESSION_SECRET must contain at least 32 characters.");
  }
  const publicOrigin = validatedPublicOrigin(
    options.publicOrigin ?? process.env.QMUX_PUBLIC_ORIGIN ?? "http://127.0.0.1:8787",
  );
  return {
    clientId,
    clientSecret,
    sessionSecret,
    publicOrigin,
    secureCookies:
      options.secureCookies ?? new URL(publicOrigin).protocol === "https:",
  };
}

export function viewerSessionFromRequest(
  request: IncomingMessage,
  config: GitHubWebAuthConfig | null,
): ViewerSession | null {
  if (!config) {
    return null;
  }
  const sealed = requestCookies(request)[SESSION_COOKIE];
  if (!sealed) {
    return null;
  }
  const session = unseal<ViewerSession>(sealed, config.sessionSecret, "session");
  if (
    !session ||
    session.expiresAt <= Date.now() ||
    !validLogin(session.login) ||
    !boundedToken(session.accessToken, 2_048) ||
    !boundedToken(session.csrfToken, 256)
  ) {
    return null;
  }
  return session;
}

export function beginGitHubAuthorization(
  response: ServerResponse,
  config: GitHubWebAuthConfig,
  returnTo: string,
) {
  const state: OAuthState = {
    state: randomBytes(24).toString("base64url"),
    returnTo: safeReturnTo(returnTo),
    expiresAt: Date.now() + OAUTH_STATE_MAX_AGE_SECONDS * 1_000,
  };
  appendSetCookie(
    response,
    cookie(
      OAUTH_STATE_COOKIE,
      seal(state, config.sessionSecret, "oauth-state"),
      OAUTH_STATE_MAX_AGE_SECONDS,
      config.secureCookies,
    ),
  );
  const authorizationUrl = new URL("https://github.com/login/oauth/authorize");
  authorizationUrl.searchParams.set("client_id", config.clientId);
  authorizationUrl.searchParams.set("redirect_uri", callbackUrl(config));
  authorizationUrl.searchParams.set("scope", "gist");
  authorizationUrl.searchParams.set("state", state.state);
  response.writeHead(302, {
    Location: authorizationUrl.toString(),
    "Cache-Control": "no-store",
  });
  response.end();
}

export async function completeGitHubAuthorization(
  request: IncomingMessage,
  response: ServerResponse,
  url: URL,
  config: GitHubWebAuthConfig,
  fetchImpl: typeof fetch,
) {
  const sealedState = requestCookies(request)[OAUTH_STATE_COOKIE];
  const stored = sealedState
    ? unseal<OAuthState>(sealedState, config.sessionSecret, "oauth-state")
    : null;
  const returnedState = url.searchParams.get("state") ?? "";
  const code = url.searchParams.get("code") ?? "";
  if (
    !stored ||
    stored.expiresAt <= Date.now() ||
    !constantTimeEqual(stored.state, returnedState) ||
    !boundedToken(code, 1_024)
  ) {
    throw new Error("GitHub sign-in could not be verified. Start the sign-in flow again.");
  }

  const tokenResponse = await fetchImpl("https://github.com/login/oauth/access_token", {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/x-www-form-urlencoded",
      "User-Agent": "qmux-publisher",
    },
    body: new URLSearchParams({
      client_id: config.clientId,
      client_secret: config.clientSecret,
      code,
      redirect_uri: callbackUrl(config),
    }),
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  const tokenBody = await responseTextLimited(
    tokenResponse,
    MAX_OAUTH_RESPONSE_BYTES,
    "GitHub OAuth response",
  );
  if (!tokenResponse.ok) {
    throw new Error(`GitHub sign-in failed with HTTP ${tokenResponse.status}.`);
  }
  const tokenPayload = parseJson<OAuthTokenResponse>(tokenBody, "GitHub OAuth response");
  if (tokenPayload.error) {
    throw new Error(
      tokenPayload.error_description ?? `GitHub sign-in failed: ${tokenPayload.error}`,
    );
  }
  const accessToken = tokenPayload.access_token ?? "";
  if (!boundedToken(accessToken, 2_048) || !hasGistScope(tokenPayload.scope)) {
    throw new Error("GitHub sign-in did not grant the required gist scope.");
  }

  const userResponse = await fetchImpl("https://api.github.com/user", {
    headers: {
      Accept: "application/vnd.github+json",
      Authorization: `Bearer ${accessToken}`,
      "User-Agent": "qmux-publisher",
      "X-GitHub-Api-Version": "2026-03-10",
    },
    redirect: "error",
    signal: AbortSignal.timeout(10_000),
  });
  const userBody = await responseTextLimited(
    userResponse,
    MAX_OAUTH_RESPONSE_BYTES,
    "GitHub user response",
  );
  if (!userResponse.ok) {
    throw new Error(`GitHub could not load the signed-in account (${userResponse.status}).`);
  }
  const user = parseJson<GitHubUserResponse>(userBody, "GitHub user response");
  if (!user.login || !validLogin(user.login)) {
    throw new Error("GitHub returned an invalid account.");
  }

  const session: ViewerSession = {
    accessToken,
    login: user.login,
    csrfToken: randomBytes(24).toString("base64url"),
    expiresAt: Date.now() + SESSION_MAX_AGE_SECONDS * 1_000,
  };
  appendSetCookie(
    response,
    cookie(
      SESSION_COOKIE,
      seal(session, config.sessionSecret, "session"),
      SESSION_MAX_AGE_SECONDS,
      config.secureCookies,
    ),
  );
  appendSetCookie(
    response,
    expiredCookie(OAUTH_STATE_COOKIE, config.secureCookies),
  );
  response.writeHead(303, {
    Location: stored.returnTo,
    "Cache-Control": "no-store",
  });
  response.end();
}

export function clearViewerSession(
  response: ServerResponse,
  config: GitHubWebAuthConfig,
  returnTo: string,
) {
  appendSetCookie(response, expiredCookie(SESSION_COOKIE, config.secureCookies));
  response.writeHead(303, {
    Location: safeReturnTo(returnTo),
    "Cache-Control": "no-store",
  });
  response.end();
}

export function safeReturnTo(value: string | null | undefined) {
  if (
    !value ||
    !value.startsWith("/") ||
    value.startsWith("//") ||
    value.includes("\\") ||
    /[\u0000-\u001f\u007f]/.test(value) ||
    value.length > 2_048
  ) {
    return "/";
  }
  try {
    const parsed = new URL(value, "https://qmux.app");
    return parsed.origin === "https://qmux.app"
      ? `${parsed.pathname}${parsed.search}${parsed.hash}`
      : "/";
  } catch {
    return "/";
  }
}

function callbackUrl(config: GitHubWebAuthConfig) {
  return `${config.publicOrigin}/auth/github/callback`;
}

function normalized(value: string | null | undefined) {
  const result = value?.trim() ?? "";
  return result || null;
}

export function validatedPublicOrigin(value: string) {
  const parsed = new URL(value);
  if (
    (parsed.protocol !== "http:" && parsed.protocol !== "https:") ||
    parsed.username ||
    parsed.password ||
    parsed.pathname !== "/" ||
    parsed.search ||
    parsed.hash
  ) {
    throw new Error("QMUX_PUBLIC_ORIGIN must be an HTTP(S) origin without a path.");
  }
  return parsed.origin;
}

function requestCookies(request: IncomingMessage) {
  const result: Record<string, string> = {};
  for (const item of (request.headers.cookie ?? "").split(";")) {
    const separator = item.indexOf("=");
    if (separator <= 0) {
      continue;
    }
    const name = item.slice(0, separator).trim();
    const value = item.slice(separator + 1).trim();
    if (name && value && value.length <= 8_192) {
      result[name] = value;
    }
  }
  return result;
}

function cookie(
  name: string,
  value: string,
  maxAgeSeconds: number,
  secure: boolean,
) {
  return [
    `${name}=${value}`,
    "Path=/",
    `Max-Age=${maxAgeSeconds}`,
    "HttpOnly",
    "SameSite=Lax",
    secure ? "Secure" : "",
  ]
    .filter(Boolean)
    .join("; ");
}

function expiredCookie(name: string, secure: boolean) {
  return cookie(name, "", 0, secure);
}

function appendSetCookie(response: ServerResponse, value: string) {
  const current = response.getHeader("Set-Cookie");
  const values = Array.isArray(current)
    ? current.map(String)
    : current
      ? [String(current)]
      : [];
  response.setHeader("Set-Cookie", [...values, value]);
}

function seal(value: unknown, secret: string, purpose: string) {
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", encryptionKey(secret, purpose), iv);
  cipher.setAAD(Buffer.from(purpose));
  const encrypted = Buffer.concat([
    cipher.update(JSON.stringify(value), "utf8"),
    cipher.final(),
  ]);
  return [
    "v1",
    iv.toString("base64url"),
    cipher.getAuthTag().toString("base64url"),
    encrypted.toString("base64url"),
  ].join(".");
}

function unseal<T>(value: string, secret: string, purpose: string): T | null {
  const [version, ivRaw, tagRaw, encryptedRaw, extra] = value.split(".");
  if (version !== "v1" || !ivRaw || !tagRaw || !encryptedRaw || extra) {
    return null;
  }
  try {
    const iv = Buffer.from(ivRaw, "base64url");
    const tag = Buffer.from(tagRaw, "base64url");
    const encrypted = Buffer.from(encryptedRaw, "base64url");
    if (iv.length !== 12 || tag.length !== 16 || encrypted.length > 4_096) {
      return null;
    }
    const decipher = createDecipheriv(
      "aes-256-gcm",
      encryptionKey(secret, purpose),
      iv,
    );
    decipher.setAAD(Buffer.from(purpose));
    decipher.setAuthTag(tag);
    const decoded = Buffer.concat([
      decipher.update(encrypted),
      decipher.final(),
    ]).toString("utf8");
    return JSON.parse(decoded) as T;
  } catch {
    return null;
  }
}

function encryptionKey(secret: string, purpose: string) {
  return createHash("sha256").update(`${purpose}\0${secret}`).digest();
}

function constantTimeEqual(left: string, right: string) {
  const leftBytes = Buffer.from(left);
  const rightBytes = Buffer.from(right);
  return (
    leftBytes.length === rightBytes.length &&
    timingSafeEqual(leftBytes, rightBytes)
  );
}

function validLogin(value: string) {
  return /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$/.test(value);
}

function boundedToken(value: string, maxLength: number) {
  return value.length > 0 && value.length <= maxLength && !/[\s\u0000-\u001f]/.test(value);
}

function hasGistScope(value: string | undefined) {
  return (value ?? "")
    .split(/[,\s]+/)
    .map((item) => item.trim())
    .includes("gist");
}

function parseJson<T>(raw: string, label: string) {
  try {
    return JSON.parse(raw) as T;
  } catch {
    throw new Error(`${label} was not valid JSON.`);
  }
}

async function responseTextLimited(
  response: Response,
  maxBytes: number,
  label: string,
) {
  const declaredLength = Number(response.headers.get("content-length"));
  if (Number.isFinite(declaredLength) && declaredLength > maxBytes) {
    await response.body?.cancel();
    throw new Error(`${label} was too large.`);
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
        throw new Error(`${label} was too large.`);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  return Buffer.concat(chunks.map((chunk) => Buffer.from(chunk)), totalBytes).toString("utf8");
}
