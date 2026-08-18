import { execFileSync } from "node:child_process";
import { rmSync } from "node:fs";
import { join } from "node:path";

// Compiled output stays inside the repo so Node can resolve react/jsx-runtime
// (the appHelpers import chain reaches the .tsx adapters) from node_modules.
const outDir = join(process.cwd(), "node_modules/.cache/qmux-pane-split-tests");
rmSync(outDir, { recursive: true, force: true });

execFileSync(
  join(process.cwd(), "node_modules/.bin/tsc"),
  [
    "--module",
    "commonjs",
    "--moduleResolution",
    "node",
    "--target",
    "ES2020",
    "--lib",
    "ES2020,DOM",
    "--types",
    "node",
    "--skipLibCheck",
    "--strict",
    "--jsx",
    "react-jsx",
    "--esModuleInterop",
    "--allowSyntheticDefaultImports",
    "--resolveJsonModule",
    "--outDir",
    outDir,
    "--rootDir",
    ".",
    "--noEmit",
    "false",
    "tests/paneSplits.test.ts",
  ],
  { stdio: "inherit" },
);

execFileSync("node", ["--test", join(outDir, "tests/paneSplits.test.js")], {
  stdio: "inherit",
});
