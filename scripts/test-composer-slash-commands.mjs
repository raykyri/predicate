import { execFileSync } from "node:child_process";
import { rmSync } from "node:fs";
import { join } from "node:path";

const outDir = join(process.cwd(), "node_modules/.cache/qmux-composer-slash-command-tests");
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
    "--jsx",
    "react-jsx",
    "--lib",
    "ES2020,DOM",
    "--types",
    "node",
    "--skipLibCheck",
    "--strict",
    "--esModuleInterop",
    "--allowSyntheticDefaultImports",
    "--outDir",
    outDir,
    "--rootDir",
    ".",
    "--noEmit",
    "false",
    "tests/composerSlashCommands.test.ts",
  ],
  { stdio: "inherit" },
);

execFileSync("node", ["--test", join(outDir, "tests/composerSlashCommands.test.js")], {
  stdio: "inherit",
});
