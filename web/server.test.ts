import assert from "node:assert/strict";
import { once } from "node:events";
import test from "node:test";
import {
  createResearchPublicationDraft,
  createTranscriptPublicationDraft,
} from "../src/lib/publicationDrafts";
import type {
  AgentInfo,
  PaneInfo,
  ResearchNode,
  ResearchTreeDetail,
  Turn,
} from "../src/types";
import { createQmuxWebServer } from "./server";
import { getAgentUiAdapter } from "../src/adapters";

const pane: PaneInfo = {
  id: "pane-1",
  title: "Test transcript",
  kind: "agent",
  agentId: "agent-1",
  groupId: "group-1",
  cwd: "/tmp/project",
  cols: 80,
  rows: 24,
  status: "running",
};

const agent: AgentInfo = {
  id: "agent-1",
  groupId: "group-1",
  adapter: "codex",
  worktreeDir: "/tmp/project",
  paneId: pane.id,
  status: "idle",
  createdAt: 1,
};

function turn(id: string, role: string, text: string): Turn {
  return {
    id,
    agentId: agent.id,
    role,
    blocks: [{ type: "text", text }],
    sourceIndex: Number(id.split("-").at(-1) ?? 0),
  };
}

test("the public server renders a valid transcript without executing raw HTML", async (t) => {
  const draft = await createTranscriptPublicationDraft({
    title: "Server render",
    pane,
    agent,
    assistantLabel: "Codex",
    publicationId: "pub_abcdefgh",
    createdAt: "2026-07-16T12:00:00.000Z",
    turns: [
      turn("turn-1", "user", "Question"),
      turn("turn-2", "assistant", "Answer <script>alert('no')</script>"),
    ],
  });
  const index = draft.files["publication.json"];
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "abcde12345",
        html_url: "https://gist.github.com/octocat/abcde12345",
        public: false,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: Buffer.byteLength(index),
            content: index,
            truncated: false,
          },
        },
        owner: {
          login: "octocat",
          html_url: "https://github.com/octocat",
        },
      }),
      { status: 200, headers: { ETag: '"v1"' } },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/abcde12345`);
  const body = await response.text();
  assert.equal(response.status, 200);
  assert.match(body, /Server render/);
  assert.match(body, /octocat/);
  assert.equal(body.includes("<script>alert"), false);
  assert.equal(body.includes("Answer"), true);
  assert.match(response.headers.get("content-security-policy") ?? "", /default-src 'none'/);
});

test("the public server renders transcript TeX math as MathJax SVG", async (t) => {
  const draft = await createTranscriptPublicationDraft({
    title: "Math render",
    pane,
    agent,
    assistantLabel: "Codex",
    publicationId: "pub_mathmath",
    createdAt: "2026-07-16T12:00:00.000Z",
    turns: [
      turn("turn-1", "user", "What is Euler's identity?"),
      turn("turn-2", "assistant", "It is $e^{i\\pi}+1=0$, and it costs $5 and $10 more."),
    ],
  });
  const index = draft.files["publication.json"];
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "fghij67890",
        html_url: "https://gist.github.com/octocat/fghij67890",
        public: false,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: Buffer.byteLength(index),
            content: index,
            truncated: false,
          },
        },
        owner: {
          login: "octocat",
          html_url: "https://github.com/octocat",
        },
      }),
      { status: 200, headers: { ETag: '"v1"' } },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/fghij67890`);
  const body = await response.text();
  assert.equal(response.status, 200);
  // The TeX renders to a self-contained MathJax SVG (CSP allows no external
  // fetches). The plain-text meta description keeps the raw source, so the
  // leak check scopes to the rendered document body.
  const rendered = body.slice(body.indexOf("<body"));
  assert.match(rendered, /<mjx-container class="MathJax" jax="SVG">/);
  assert.equal(rendered.includes("e^{i\\pi}"), false);
  // Dollar amounts survive as prose instead of being mathified.
  assert.equal(rendered.includes("costs $5 and $10 more."), true);
});

test("the public server renders deep-linked research nodes and verifies their files", async (t) => {
  const root: ResearchNode = {
    id: "private-root",
    treeId: "private-tree",
    prompt: "Root question",
    title: "Root result",
    adapter: "codex",
    groupId: "private-group",
    worktreeDir: "/private/research",
    status: "complete",
    createdAt: 1,
    highlights: [],
  };
  const child: ResearchNode = {
    ...root,
    id: "private-child",
    parentNodeId: root.id,
    prompt: "Child question",
    title: "Child result",
    createdAt: 2,
  };
  const detail: ResearchTreeDetail = {
    tree: {
      id: "private-tree",
      title: "Research render",
      rootNodeId: root.id,
      workspaceId: "private-workspace",
      createdAt: 1,
      updatedAt: 2,
    },
    nodes: [root, child],
  };
  const content = (node: ResearchNode, answer: string, revision: string) => ({
    node,
    turns: [
      {
        id: `${node.id}-turn`,
        agentId: "private-agent",
        role: "assistant",
        blocks: [{ type: "text" as const, text: answer }],
        sourceIndex: 1,
      },
    ],
    children: [],
    responseRevision: revision,
  });
  const draft = await createResearchPublicationDraft({
    title: detail.tree.title,
    detail,
    selectedNodeId: child.id,
    mode: "tree",
    publicationId: "pub_render1234",
    createdAt: "2026-07-16T12:00:00.000Z",
    contents: [
      content(root, "Root **answer**.", "a".repeat(64)),
      content(child, "Child answer <script>alert('no')</script>", "b".repeat(64)),
    ],
  });
  assert.equal(draft.publication.kind, "research-tree");
  if (draft.publication.kind !== "research-tree") {
    assert.fail("expected research tree");
  }
  const selectedNodeId = draft.publication.research.selectedNodeId!;
  const gistFiles = Object.fromEntries(
    Object.entries(draft.files).map(([filename, fileContent]) => [
      filename,
      {
        filename,
        size: Buffer.byteLength(fileContent),
        content: fileContent,
        truncated: false,
      },
    ]),
  );
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "research12345",
        html_url: "https://gist.github.com/octocat/research12345",
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: gistFiles,
        owner: {
          login: "octocat",
          html_url: "https://github.com/octocat",
        },
      }),
      { status: 200, headers: { ETag: '"research-v1"' } },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(
    `http://127.0.0.1:${address.port}/p/research12345/n/${selectedNodeId}`,
  );
  const body = await response.text();
  assert.equal(response.status, 200);
  assert.match(body, /Research render/);
  assert.match(body, /Child result/);
  assert.match(body, /Child answer/);
  // The prompt card links back to the parent result, app-style.
  assert.match(body, /research-parent-link/);
  assert.match(body, /← Back/);
  assert.equal(body.includes("<script>alert"), false);
});

test("the public server renders a published conversation as labelled turn bubbles", async (t) => {
  const root: ResearchNode = {
    id: "private-conv-root",
    treeId: "private-tree",
    prompt: "Explain the build pipeline",
    title: "Build pipeline chat",
    adapter: "codex",
    kind: "conversation",
    origin: "terminalExport",
    groupId: "private-group",
    worktreeDir: "/private/research",
    status: "complete",
    createdAt: 1,
    highlights: [],
  };
  const detail: ResearchTreeDetail = {
    tree: {
      id: "private-tree",
      title: "Build pipeline chat",
      rootNodeId: root.id,
      workspaceId: "private-workspace",
      createdAt: 1,
      updatedAt: 2,
    },
    nodes: [root],
  };
  const conversationContent = {
    node: root,
    turns: [
      {
        id: `${root.id}-u1`,
        agentId: "private-agent",
        role: "user" as const,
        blocks: [{ type: "text" as const, text: "Explain the build pipeline" }],
        sourceIndex: 0,
      },
      {
        id: `${root.id}-a1`,
        agentId: "private-agent",
        role: "assistant" as const,
        blocks: [{ type: "text" as const, text: "The build runs scripts/build.sh." }],
        sourceIndex: 1,
      },
    ],
    children: [],
    responseRevision: "e".repeat(64),
  };
  const draft = await createResearchPublicationDraft({
    title: detail.tree.title,
    detail,
    selectedNodeId: root.id,
    mode: "tree",
    publicationId: "pub_convrender1",
    createdAt: "2026-07-19T12:00:00.000Z",
    contents: [conversationContent],
  });
  const gistFiles = Object.fromEntries(
    Object.entries(draft.files).map(([filename, fileContent]) => [
      filename,
      {
        filename,
        size: Buffer.byteLength(fileContent),
        content: fileContent,
        truncated: false,
      },
    ]),
  );
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "convrender123",
        html_url: "https://gist.github.com/octocat/convrender123",
        public: true,
        created_at: "2026-07-19T12:00:00Z",
        updated_at: "2026-07-19T12:00:00Z",
        files: gistFiles,
        owner: { login: "octocat", html_url: "https://github.com/octocat" },
      }),
      { status: 200, headers: { ETag: '"conv-v1"' } },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/convrender123`);
  const body = await response.text();
  assert.equal(response.status, 200);
  // Rendered as per-turn bubbles inside the anchorable answer root, not a
  // single prompt card + answer.
  assert.match(body, /research-conversation/);
  assert.match(body, /conversation-turn is-user/);
  assert.match(body, /conversation-turn is-assistant/);
  // Assistant turns are labelled with the adapter's display name.
  assert.match(body, new RegExp(getAgentUiAdapter("codex").label));
  assert.match(body, /Explain the build pipeline/);
  assert.match(body, /scripts\/build\.sh/);
  // A conversation tree still exposes the reader comment composer.
  assert.match(body, /proposal-composer/);
  // Public targeted asks are confined to one label-free turn body, matching
  // the app's conversation-anchor projection and context clamp.
  assert.match(body, /closest\("\.conversation-turn-body"\)/);
  assert.match(body, /startTurn !== endTurn/);
  assert.match(body, /pendingSelection\.contextStart/);
});

test("the public server redirects pinned-revision URLs to the latest view", async (t) => {
  const fetchImpl: typeof fetch = async () => {
    throw new Error("pinned-revision redirects must not call GitHub");
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const revision = "a".repeat(40);
  const nodeRedirect = await fetch(
    `http://127.0.0.1:${address.port}/p/abcde12345/r/${revision}/n/node_abcdefgh`,
    { redirect: "manual" },
  );
  assert.equal(nodeRedirect.status, 301);
  assert.equal(
    nodeRedirect.headers.get("location"),
    "/p/abcde12345/n/node_abcdefgh",
  );
  await nodeRedirect.arrayBuffer();
  const rootRedirect = await fetch(
    `http://127.0.0.1:${address.port}/p/abcde12345/r/${revision}`,
    { redirect: "manual" },
  );
  assert.equal(rootRedirect.status, 301);
  assert.equal(rootRedirect.headers.get("location"), "/p/abcde12345");
  await rootRedirect.arrayBuffer();
});

test("the public server rejects a research file that does not match publication.json", async (t) => {
  const answerFile = "node_abcdefgh.md";
  const index = JSON.stringify({
    schemaVersion: 1,
    publicationId: "pub_tamper1234",
    kind: "research-answer",
    title: "Tampered",
    createdAt: "2026-07-16T12:00:00.000Z",
    updatedAt: "2026-07-16T12:00:00.000Z",
    contentHash: "0".repeat(64),
    research: {
      rootNodeId: "node_abcdefgh",
      selectedNodeId: "node_abcdefgh",
      nodes: [
        {
          id: "node_abcdefgh",
          parentId: null,
          kind: "run",
          title: "Result",
          prompt: "Question",
          answerFile,
          contentHash: "f".repeat(64),
          responseRevision: "a".repeat(64),
          status: "complete",
          createdAt: 1,
        },
      ],
    },
  });
  const parsed = JSON.parse(index);
  const { canonicalJson } = await import("../src/lib/publication");
  const { createHash } = await import("node:crypto");
  const unhashed = { ...parsed };
  delete unhashed.contentHash;
  parsed.contentHash = createHash("sha256").update(canonicalJson(unhashed)).digest("hex");
  const validIndex = `${JSON.stringify(parsed)}\n`;
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "tamper12345",
        html_url: "https://gist.github.com/octocat/tamper12345",
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: Buffer.byteLength(validIndex),
            content: validIndex,
          },
          [answerFile]: {
            filename: answerFile,
            size: 8,
            content: "tampered",
          },
        },
      }),
      { status: 200 },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/tamper12345`);
  assert.equal(response.status, 422);
  assert.match(await response.text(), /invalid content hash/);
});

test("the public server reports malformed publication.json as unprocessable", async (t) => {
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "malformed12",
        html_url: "https://gist.github.com/octocat/malformed12",
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: 8,
            content: "not json",
          },
        },
      }),
      { status: 200 },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/malformed12`);
  assert.equal(response.status, 422);
  assert.match(await response.text(), /not valid JSON/);
});

test("the public server follows a trusted raw URL for truncated publication.json", async (t) => {
  const draft = await createTranscriptPublicationDraft({
    title: "Raw fallback",
    pane,
    agent,
    assistantLabel: "Codex",
    publicationId: "pub_rawfallback",
    createdAt: "2026-07-16T12:00:00.000Z",
    turns: [turn("turn-1", "assistant", "Loaded from the raw file.")],
  });
  const index = draft.files["publication.json"];
  let fetchCount = 0;
  const fetchImpl: typeof fetch = async (url) => {
    fetchCount += 1;
    if (String(url).startsWith("https://api.github.com/")) {
      return new Response(
        JSON.stringify({
          id: "rawfallback1",
          html_url: "https://gist.github.com/octocat/rawfallback1",
          public: true,
          created_at: "2026-07-16T12:00:00Z",
          updated_at: "2026-07-16T12:00:00Z",
          files: {
            "publication.json": {
              filename: "publication.json",
              size: Buffer.byteLength(index),
              truncated: true,
              raw_url:
                "https://gist.githubusercontent.com/octocat/rawfallback1/raw/publication.json",
            },
          },
        }),
        { status: 200 },
      );
    }
    return new Response(index, { status: 200 });
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/rawfallback1`);
  assert.equal(response.status, 200);
  assert.match(await response.text(), /Loaded from the raw file/);
  assert.equal(fetchCount, 2);
});

test("the public server refuses truncated files from non-GitHub raw hosts", async (t) => {
  const fetchImpl: typeof fetch = async () =>
    new Response(
      JSON.stringify({
        id: "rawuntrusted1",
        html_url: "https://gist.github.com/octocat/rawuntrusted1",
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: 100,
            truncated: true,
            raw_url: "https://example.com/publication.json",
          },
        },
      }),
      { status: 200 },
    );
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/rawuntrusted1`);
  assert.equal(response.status, 422);
  assert.match(await response.text(), /untrusted raw URL/);
});

test("the public server rejects an oversized GitHub API response before reading it", async (t) => {
  const fetchImpl: typeof fetch = async () =>
    new Response("{}", {
      status: 200,
      headers: { "Content-Length": "20000001" },
    });
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/p/oversized1`);
  assert.equal(response.status, 413);
  assert.match(await response.text(), /too large/);
});

test("the public server evicts old publication cache entries", async (t) => {
  const draft = await createTranscriptPublicationDraft({
    title: "Cache entry",
    pane,
    agent,
    assistantLabel: "Codex",
    publicationId: "pub_cacheentry",
    createdAt: "2026-07-16T12:00:00.000Z",
    turns: [turn("turn-1", "assistant", "Cached answer")],
  });
  const index = draft.files["publication.json"];
  let fetchCount = 0;
  const fetchImpl: typeof fetch = async (url) => {
    fetchCount += 1;
    const gistId = String(url).split("/").at(-1)!;
    return new Response(
      JSON.stringify({
        id: gistId,
        html_url: `https://gist.github.com/octocat/${gistId}`,
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: Buffer.byteLength(index),
            content: index,
          },
        },
      }),
      { status: 200 },
    );
  };
  // This test deliberately issues 130 requests from one address; the per-client
  // rate limit is exercised separately.
  const server = createQmuxWebServer({ fetchImpl, rateLimit: null });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  for (let index = 0; index < 129; index += 1) {
    const gistId = `cache${index.toString().padStart(4, "0")}`;
    const pageResponse: Response = await fetch(
      `http://127.0.0.1:${address.port}/p/${gistId}`,
    );
    assert.equal(pageResponse.status, 200);
    await pageResponse.arrayBuffer();
  }
  const repeated = await fetch(`http://127.0.0.1:${address.port}/p/cache0000`);
  assert.equal(repeated.status, 200);
  assert.equal(fetchCount, 130);
});

test("the public server caps concurrent publication loads", async (t) => {
  const draft = await createTranscriptPublicationDraft({
    title: "Concurrent load",
    pane,
    agent,
    assistantLabel: "Codex",
    publicationId: "pub_concurrent1",
    createdAt: "2026-07-16T12:00:00.000Z",
    turns: [turn("turn-1", "assistant", "Concurrent answer")],
  });
  const index = draft.files["publication.json"];
  let started = 0;
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const fetchImpl: typeof fetch = async (url) => {
    started += 1;
    await gate;
    const gistId = String(url).split("/").at(-1)!;
    return new Response(
      JSON.stringify({
        id: gistId,
        html_url: `https://gist.github.com/octocat/${gistId}`,
        public: true,
        created_at: "2026-07-16T12:00:00Z",
        updated_at: "2026-07-16T12:00:00Z",
        files: {
          "publication.json": {
            filename: "publication.json",
            size: Buffer.byteLength(index),
            content: index,
          },
        },
      }),
      { status: 200 },
    );
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const activeRequests = Array.from({ length: 8 }, (_, index) =>
    fetch(`http://127.0.0.1:${address.port}/p/concurrent${index}`),
  );
  while (started < 8) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  const busyResponse = await fetch(
    `http://127.0.0.1:${address.port}/p/concurrent8`,
  );
  assert.equal(busyResponse.status, 503);
  release();
  const responses = await Promise.all(activeRequests);
  for (const response of responses) {
    assert.equal(response.status, 200);
    await response.arrayBuffer();
  }
});

test("the public server negative-caches a missing Gist", async (t) => {
  let fetchCount = 0;
  const fetchImpl: typeof fetch = async () => {
    fetchCount += 1;
    return new Response("Not Found", { status: 404 });
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const first = await fetch(`http://127.0.0.1:${address.port}/p/missing12345`);
  assert.equal(first.status, 404);
  await first.arrayBuffer();
  const second = await fetch(`http://127.0.0.1:${address.port}/p/missing12345`);
  assert.equal(second.status, 404);
  await second.arrayBuffer();
  // The second request for the same missing id is served from the negative
  // cache, so the shared GitHub token is only spent once.
  assert.equal(fetchCount, 1);
});

test("the public server rate-limits the publication route per client", async (t) => {
  let fetchCount = 0;
  const fetchImpl: typeof fetch = async () => {
    fetchCount += 1;
    return new Response("Not Found", { status: 404 });
  };
  const server = createQmuxWebServer({
    fetchImpl,
    rateLimit: { windowMs: 60_000, maxRequests: 3 },
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  for (let index = 0; index < 3; index += 1) {
    const allowed: Response = await fetch(
      `http://127.0.0.1:${address.port}/p/ratelimit${index}`,
    );
    assert.equal(allowed.status, 404);
    await allowed.arrayBuffer();
  }
  const limited = await fetch(`http://127.0.0.1:${address.port}/p/ratelimit9`);
  assert.equal(limited.status, 429);
  assert.ok(limited.headers.get("retry-after"));
  await limited.arrayBuffer();
});

test("the public server uses Fly-Client-IP for rate limits on Fly", async (t) => {
  const fetchImpl: typeof fetch = async () =>
    new Response("Not Found", { status: 404 });
  const server = createQmuxWebServer({
    fetchImpl,
    rateLimit: { windowMs: 60_000, maxRequests: 1 },
    trustFlyClientIp: true,
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const request = (gistId: string, clientIp: string) =>
    fetch(`http://127.0.0.1:${address.port}/p/${gistId}`, {
      headers: { "Fly-Client-IP": clientIp },
    });
  const firstClient = await request("flyclient001", "203.0.113.1");
  assert.equal(firstClient.status, 404);
  await firstClient.arrayBuffer();
  const secondClient = await request("flyclient002", "203.0.113.2");
  assert.equal(secondClient.status, 404);
  await secondClient.arrayBuffer();
  const limited = await request("flyclient003", "203.0.113.1");
  assert.equal(limited.status, 429);
  await limited.arrayBuffer();
  const invalidHeader = await request("flyclient004", "not-an-ip");
  assert.equal(invalidHeader.status, 404);
  await invalidHeader.arrayBuffer();
  const invalidHeaderLimited = await request("flyclient005", "still-not-an-ip");
  assert.equal(invalidHeaderLimited.status, 429);
  await invalidHeaderLimited.arrayBuffer();
});

test("the public server ignores Fly-Client-IP outside Fly", async (t) => {
  const fetchImpl: typeof fetch = async () =>
    new Response("Not Found", { status: 404 });
  const server = createQmuxWebServer({
    fetchImpl,
    rateLimit: { windowMs: 60_000, maxRequests: 1 },
    trustFlyClientIp: false,
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const first = await fetch(`http://127.0.0.1:${address.port}/p/socketclient01`, {
    headers: { "Fly-Client-IP": "203.0.113.1" },
  });
  assert.equal(first.status, 404);
  await first.arrayBuffer();
  const limited = await fetch(
    `http://127.0.0.1:${address.port}/p/socketclient02`,
    { headers: { "Fly-Client-IP": "203.0.113.2" } },
  );
  assert.equal(limited.status, 429);
  await limited.arrayBuffer();
});

test("the public server backs off all ids after an upstream rate limit", async (t) => {
  let fetchCount = 0;
  const fetchImpl: typeof fetch = async () => {
    fetchCount += 1;
    return new Response("rate limited", {
      status: 403,
      headers: { "x-ratelimit-remaining": "0" },
    });
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const first = await fetch(`http://127.0.0.1:${address.port}/p/cooldown0001`);
  assert.equal(first.status, 503);
  await first.arrayBuffer();
  // A different id must not spend another upstream call while the shared token
  // is in its cooldown.
  const second = await fetch(`http://127.0.0.1:${address.port}/p/cooldown0002`);
  assert.equal(second.status, 503);
  await second.arrayBuffer();
  assert.equal(fetchCount, 1);
});

test("the landing page renders the app replica and its own image policy", async (t) => {
  const fetchImpl: typeof fetch = async () => {
    throw new Error("the landing page must not call upstream");
  };
  const server = createQmuxWebServer({
    fetchImpl,
    publicOrigin: "https://qmux.app",
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/`);
  const body = await response.text();
  assert.equal(response.status, 200);
  assert.match(body, /Your terminal, extended/);
  assert.match(body, /A simpler approach to coding agent orchestration\./);
  assert.match(body, /qmux powers up the terminal with artifacts, visual transcripts/);
  assert.match(body, /aria-label="Supported agents"/);
  assert.match(body, /class="hero-agents"/);
  for (const label of ["Claude Code", "Codex", "OpenCode", "Grok", "Muse", "Pi", "Cursor"]) {
    assert.match(body, new RegExp(`class="visually-hidden">${label}<`));
  }
  assert.doesNotMatch(body, /class="visually-hidden">ACP</);
  assert.match(body, /<a href="#features">Features<\/a>/);
  assert.match(body, /<section class="grid-section" id="features" aria-label="Features">/);
  assert.match(body, /html \{\s*scroll-behavior: smooth;/);
  assert.match(body, /href="https:\/\/github\.com\/raykyri\/qmux\/releases"/);
  assert.doesNotMatch(body, /releases\/download|\.dmg|Download v\d/);
  // The hero ships the HTML replica of the app window, not a screenshot.
  assert.match(body, /class="app-mockup"/);
  assert.match(body, /class="app-shell has-turn-sidebar"/);
  assert.match(body, /What should we investigate next\?/);
  assert.doesNotMatch(body, /qmux running a Codex agent over the Porffor JavaScript engine/);
  assert.doesNotMatch(body, /Play the session/);
  assert.doesNotMatch(body, /Type in the composer to queue a turn/);
  // The curated feature list reaches the markup without retired entries.
  assert.match(body, /Based on libghostty/);
  assert.match(body, /<strong>Open source<\/strong>/);
  assert.match(body, /Fully open-source, local-first, free forever\./);
  assert.doesNotMatch(body, /<strong>First-class agents<\/strong>/);
  assert.doesNotMatch(body, /<strong>Vertical splittable tabs<\/strong>/);
  assert.doesNotMatch(body, /<strong>Git worktrees<\/strong>/);
  assert.doesNotMatch(body, /<strong>Prompt library<\/strong>/);
  assert.doesNotMatch(body, /<strong>Browser overlay<\/strong>/);
  // The FAQ and footer brand mark have been removed completely.
  assert.doesNotMatch(body, /href="#faq-title"/);
  assert.doesNotMatch(body, /id="faq"/);
  assert.doesNotMatch(body, /What&#x27;s the business model\?/);
  assert.doesNotMatch(body, /class="footer-mark"/);
  // Absolute social/canonical URLs, which relative ones would not give scrapers.
  assert.match(body, /property="og:image" content="https:\/\/qmux\.app\/qmux\.png"/);
  assert.match(body, /rel="canonical" href="https:\/\/qmux\.app\/"/);

  // The replica is complete before any script runs, and it carries the shared
  // step timeline the enhancement replays from.
  assert.match(body, /data-mock-features="replay queue groups sessions panes panels menus"/);
  // Every visible sidebar tab ships a complete terminal/transcript pair. The
  // default is visible without JavaScript and the enhancement swaps the rest.
  assert.equal((body.match(/data-mock-session-tab=/g) ?? []).length, 14);
  assert.equal((body.match(/data-mock-session-view=/g) ?? []).length, 28);
  assert.ok((body.match(/class="mock-terminal-block"/g) ?? []).length >= 115);
  assert.ok((body.match(/class="mock-terminal-line"/g) ?? []).length >= 430);
  assert.equal((body.match(/data-mock-session-status="active"/g) ?? []).length, 4);
  assert.equal((body.match(/class="turn-thinking"/g) ?? []).length, 4);
  assert.match(body, /data-mock-session-view="qmux-landing-transcript"/);
  assert.match(body, /data-mock-session-view="porffor-replace-all" hidden/);
  assert.match(body, /data-mock-session-view="nanochat-tokenizer" hidden/);
  assert.match(body, /\.app-mockup \.turn-timeline \{[^}]*overflow-y: auto;/s);
  assert.ok(
    body.indexOf('<span class="pane-group-name">qmux</span>') <
      body.indexOf('<span class="pane-group-name">porffor</span>'),
  );
  const porfforNameIndex = body.indexOf('<span class="pane-group-name">porffor</span>');
  const porfforSection = body.slice(body.lastIndexOf("<section", porfforNameIndex), porfforNameIndex);
  assert.match(porfforSection, /pane-group has-panes is-active-group/);
  assert.doesNotMatch(porfforSection, /is-collapsed/);
  assert.match(body, /class="turn-image"/);
  assert.match(body, /src="\/qmux\.png"/);
  assert.match(body, /alt="The qmux desktop interface"/);
  assert.doesNotMatch(body, /qmux desktop layout reference/);
  assert.match(body, /class="turn-card role-assistant turn-image-card" data-step="12"/);
  assert.match(body, /\.\/porf \/tmp\/replaceall-smoke\.js/);
  assert.match(body, /10 passed in 0\.42s/);
  assert.match(body, /All results match for block_size=512\./);
  assert.doesNotMatch(body, /runner\/index\.js/);
  assert.doesNotMatch(body, /out\.wasm/);
  // Header panels are rendered in the markup rather than being built at runtime;
  // the artifact tray is the initial view, while transient popovers start closed.
  assert.match(body, /data-mock-panel="prompt-library" hidden/);
  assert.match(body, /data-mock-panel="artifacts"/);
  assert.doesNotMatch(body, /data-mock-panel="artifacts" hidden/);
  assert.match(body, /data-mock-panel="browser" hidden/);
  // Message and composer overflow menus also ship as closed, inert markup;
  // enhancement only gives their ellipsis triggers open/close behavior.
  assert.match(body, /data-mock-menu="message"[^>]*hidden/);
  assert.match(body, /data-mock-menu="composer"[^>]*hidden/);
  assert.match(body, /Copy transcript as JSON/);
  // Collapsing a pane needs a way back, so both restore controls ship inert in
  // the markup rather than being created at runtime.
  assert.match(body, /data-mock-action="hide-sidebar"/);
  assert.match(body, /data-mock-action="show-sidebar"/);
  assert.match(body, /data-mock-action="show-right"/);
  assert.match(body, /data-step="5"/);
  // Replay staging is inert unless the pre-paint bootstrap activates it, so the
  // serialized default session remains complete when JavaScript is unavailable.
  assert.equal(/class="[^"]*is-pending/.test(body), false);
  assert.match(body, /data-replay-pending=""/);
  assert.match(body, /html\.mock-replay-boot \.app-mockup \[data-replay-pending\]/);
  // Enhancement is in separate files, never an inline script. The small
  // bootstrap blocks parsing so it can select the first replay frame before
  // the mockup paints; the full behavior remains deferred.
  assert.match(body, /<script src="\/mockup-boot\.js"><\/script>/);
  assert.match(body, /<script src="\/mockup\.js" defer/);
  assert.ok(body.indexOf('src="/mockup-boot.js"') < body.indexOf('class="app-mockup"'));

  const csp = response.headers.get("content-security-policy") ?? "";
  // The landing page serves its logo and enhancement script from disk, so it
  // relaxes img-src and script-src to 'self' — and nothing inline may execute.
  assert.match(csp, /img-src 'self'/);
  assert.match(csp, /script-src 'self'/);
  assert.match(csp, /default-src 'none'/);
});

test("the landing page's enhancement script is served as JavaScript", async (t) => {
  const fetchImpl: typeof fetch = async () => {
    throw new Error("static assets must not call upstream");
  };
  const server = createQmuxWebServer({ fetchImpl });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(() => server.close());
  const address = server.address();
  assert.ok(address && typeof address === "object");

  const response = await fetch(`http://127.0.0.1:${address.port}/mockup.js`);
  const body = await response.text();
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type") ?? "", /text\/javascript/);
  assert.equal(response.headers.get("x-content-type-options"), "nosniff");
  assert.match(body, /app-mockup/);

  const bootResponse = await fetch(`http://127.0.0.1:${address.port}/mockup-boot.js`);
  const bootBody = await bootResponse.text();
  assert.equal(bootResponse.status, 200);
  assert.match(bootResponse.headers.get("content-type") ?? "", /text\/javascript/);
  assert.equal(bootResponse.headers.get("x-content-type-options"), "nosniff");
  assert.match(bootBody, /mock-replay-boot/);
});
