#!/usr/bin/env node
// Fails (exit 1) when a release build of synthesis-desktop could enable `local-engine-for-tests` (it exposes
// `engine::for_tests`, a way to start the agent on the local OS that bypasses the WSL gate of
// apps/desktop/src/tui_view/driver.rs). The feature may only come from the crate's own dev-dependency, which
// `cargo build` never uses. From `cargo metadata`: the feature is not (transitively) in the `default` feature,
// and no package depends on synthesis-desktop with it except as a dev-dependency; and no workflow names it.
//   node scripts/check-release-features.mjs
//   node --test scripts/check-release-features.test.mjs
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const FEATURE = "local-engine-for-tests";
const CRATE = "synthesis-desktop";

/** Reasons (strings) why `meta` (cargo metadata, parsed) lets the feature into a release build. */
export function leaks(meta) {
  const bad = [];
  const desktop = meta.packages.find((p) => p.name === CRATE);
  if (!desktop) return [`${CRATE} is not in the metadata; the check cannot run`];
  // features reachable from `default`
  const seen = new Set();
  const walk = (f) => {
    if (seen.has(f)) return;
    seen.add(f);
    for (const x of desktop.features[f] ?? []) walk(x.replace(/^dep:/, ""));
  };
  walk("default");
  if (seen.has(FEATURE)) bad.push(`${FEATURE} is reachable from the default features of ${CRATE}`);
  for (const p of meta.packages) {
    for (const d of p.dependencies) {
      if (d.name === CRATE && d.kind !== "dev" && (d.features ?? []).includes(FEATURE)) {
        bad.push(`${p.name} depends on ${CRATE} with ${FEATURE} (kind ${d.kind ?? "normal"})`);
      }
    }
  }
  return bad;
}

/** Workflow files (name -> text) that mention the feature. */
export function mentionedIn(files) {
  return Object.entries(files).filter(([, t]) => t.includes(FEATURE)).map(([n]) => `named in ${n}`);
}

function main() {
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps", "--locked"], { encoding: "utf8", maxBuffer: 1 << 28 }),
  );
  const dir = ".github/workflows";
  const files = Object.fromEntries(readdirSync(dir).map((n) => [`${dir}/${n}`, readFileSync(`${dir}/${n}`, "utf8")]));
  const bad = [...leaks(meta), ...mentionedIn(files)];
  if (bad.length) {
    console.error(`check-release-features: ${FEATURE} must never reach a release build:\n  ${bad.join("\n  ")}`);
    process.exit(1);
  }
  console.log(`check-release-features: ${FEATURE} cannot reach a release build`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
