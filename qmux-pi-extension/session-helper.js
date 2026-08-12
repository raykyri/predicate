// One-shot bridge to Pi's own SessionManager for qmux forks.
//
// Keeping this operation in Pi's package means session ids, v1/v2 migrations,
// labels, parent re-chaining, headers, and atomic file creation stay owned by
// the Pi version qmux is launching instead of being reimplemented here.

import { existsSync, realpathSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

export async function loadPiSessionApi(piCliPath) {
  const resolvedCli = realpathSync(piCliPath);
  // npm installs expose `pi` as dist/cli.js beside dist/index.js. Resolving the
  // package name through createRequire does not work because Pi exports only an
  // ESM `import` condition, so anchor directly to the canonical CLI instead.
  const packageEntry = join(dirname(resolvedCli), "index.js");
  if (!existsSync(packageEntry)) {
    throw new Error(
      `cannot locate Pi's SessionManager module beside ${resolvedCli}; install the npm Pi package or configure adapters.pi.binary to its dist/cli.js`,
    );
  }
  const pi = await import(pathToFileURL(packageEntry).href);
  if (typeof pi.SessionManager !== "function") {
    throw new Error("installed Pi does not export SessionManager");
  }
  return pi;
}

export function createBranchedSession(
  { SessionManager },
  sourcePath,
  leafId,
  targetCwd,
  { fileExists = existsSync } = {},
) {
  // Ask SessionManager itself to resolve/create the target project's default
  // session directory; getDefaultSessionDir is intentionally not a public
  // package export in Pi 0.80.
  const sessionDir = SessionManager.create(targetCwd).getSessionDir();
  const manager = SessionManager.open(sourcePath, sessionDir, targetCwd);
  const sessionFile = manager.createBranchedSession(leafId);
  if (!sessionFile) {
    throw new Error("Pi did not persist the branched session");
  }
  if (!fileExists(sessionFile)) {
    throw new Error(
      "Pi deferred the branched session because this point has no assistant response; wait for the turn to finish before forking",
    );
  }
  return {
    session_file: sessionFile,
    session_id: manager.getSessionId(),
    leaf_id: manager.getLeafId(),
  };
}

async function main() {
  const [piCliPath, sourcePath, leafId, targetCwd] = process.argv.slice(2);
  if (!piCliPath || !sourcePath || !leafId || !targetCwd) {
    throw new Error("usage: session-helper <pi-cli> <source-jsonl> <leaf-id> <target-cwd>");
  }
  const pi = await loadPiSessionApi(piCliPath);
  process.stdout.write(JSON.stringify(createBranchedSession(pi, sourcePath, leafId, targetCwd)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
}
