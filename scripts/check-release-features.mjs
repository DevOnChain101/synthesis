#!/usr/bin/env node
// Fails (exit 1) when a release build of synthesis-desktop could enable a development-only feature:
//   local-engine-for-tests  `engine::for_tests`, a way to start the agent on the local OS that bypasses the
//                           WSL gate of apps/desktop/src/tui_view/driver.rs
//   shot                    the offscreen screenshot tool (gpui test-support)
//   test-provider           the scripted model
// Each must (1) be defined in the crate's Cargo.toml (a rename would otherwise make this pass vacuously),
// (2) not be reachable from the `default` feature, (3) not be enabled by a dependency other than a dev one
// (the crate's own dev-dependency is how the integration tests get `local-engine-for-tests`, and `cargo build`
// never uses it), and (4) not be named in a workflow's `--features`/`-F` list for synthesis-desktop.
// A workflow may enable a feature of ANOTHER crate that happens to share a name (`synthesis-cli/test-provider`
// in ci.yml runs the workspace tests); that is the one allowed use and is recognised by its crate prefix.
// `--all-features` is refused in every workflow.
//   node scripts/check-release-features.mjs
//   node --test scripts/check-release-features.test.mjs
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const FEATURES = ["local-engine-for-tests", "shot", "test-provider"];
const CRATE = "synthesis-desktop";

/** Reasons (strings) why `meta` (cargo metadata, parsed) lets a guarded feature into a release build. */
export function leaks(meta, features = FEATURES) {
  const desktop = meta.packages.find((p) => p.name === CRATE);
  if (!desktop) return [`${CRATE} is not in the metadata; the check cannot run`];
  const bad = [];
  // features reachable from `default`
  const seen = new Set();
  const walk = (f) => {
    if (seen.has(f)) return;
    seen.add(f);
    for (const x of desktop.features[f] ?? []) walk(x.replace(/^dep:/, "").replace(/^[^/]*\//, ""));
  };
  walk("default");
  for (const f of features) {
    if (!(f in desktop.features)) bad.push(`${f} is not defined in ${CRATE}'s features; renamed? update this guard`);
    if (seen.has(f)) bad.push(`${f} is reachable from the default features of ${CRATE}`);
    for (const p of meta.packages) {
      for (const d of p.dependencies) {
        if (d.name === CRATE && d.kind !== "dev" && (d.features ?? []).includes(f)) {
          bad.push(`${p.name} depends on ${CRATE} with ${f} (kind ${d.kind ?? "normal"})`);
        }
      }
    }
  }
  return bad;
}

/** Feature lists of a workflow: the text after `--features`, `--features=`, `-F` on a line. */
function featureArgs(text) {
  const out = [];
  for (const line of text.split("\n")) {
    for (const m of line.matchAll(/(?:--features[= ]|-F[= ]|--features-?=)\s*("[^"]*"|'[^']*'|\S+)/g)) out.push(m[1]);
  }
  return out;
}

/** Workflow files (name -> text) that enable a guarded feature of synthesis-desktop (or everything). */
export function mentionedIn(files, features = FEATURES) {
  const bad = [];
  for (const [name, text] of Object.entries(files)) {
    if (/--all-features/.test(text)) bad.push(`--all-features in ${name}`);
    for (const arg of featureArgs(text)) {
      for (const item of arg.replace(/^["']|["']$/g, "").split(/[\s,]+/).filter(Boolean)) {
        const [pre, f] = item.includes("/") ? item.split("/") : ["", item];
        // `synthesis-cli/test-provider` is another crate's feature: allowed. Unqualified or desktop: not.
        if (features.includes(f) && (pre === "" || pre === CRATE)) bad.push(`${f} enabled in ${name} (${item})`);
      }
    }
  }
  return bad;
}

function main() {
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps", "--locked"], { encoding: "utf8", maxBuffer: 1 << 28 }),
  );
  const dir = ".github/workflows";
  const files = Object.fromEntries(readdirSync(dir).map((n) => [`${dir}/${n}`, readFileSync(`${dir}/${n}`, "utf8")]));
  const bad = [...leaks(meta), ...mentionedIn(files)];
  if (bad.length) {
    console.error(`check-release-features: development-only features must never reach a release build:\n  ${bad.join("\n  ")}`);
    process.exit(1);
  }
  console.log(`check-release-features: ${FEATURES.join(", ")} cannot reach a release build`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
