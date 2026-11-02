#!/usr/bin/env node
// Writes the npm packages for a release from already-built binaries.
//   node scripts/package-npm.mjs --version 0.1.0 --raw <dir> --out <dir> [--name @devonchain101/synthesis]
//        [--bin-names synt[,other]] [--require-all]
// <dir> holds one subdirectory per Rust target (optionally prefixed "raw-"), each with `synt[.exe]`
// and, on Linux, `synthesis-sandbox-exec`. Only targets that are present are emitted (and listed in
// optionalDependencies); --require-all fails unless the 5 release-gating targets are present.
import { chmodSync, copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { values: a } = parseArgs({
  options: {
    version: { type: "string" }, raw: { type: "string" }, out: { type: "string" },
    name: { type: "string", default: "@devonchain101/synthesis" },
    repo: { type: "string" }, // owner/repo; default: packaging/REPO (the single source, scripts/check-placeholders.mjs --set)
    "bin-names": { type: "string", default: "synt" }, // default: "synt" (the Node CLI is retired)
    "require-all": { type: "boolean", default: false },
  },
});
for (const k of ["version", "raw", "out"]) if (!a[k]) throw new Error(`--${k} is required`);
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(a.version)) throw new Error(`bad version ${a.version}`);

// Rust target -> npm platform suffix, os, cpu, libc
const TARGETS = {
  "aarch64-apple-darwin": ["darwin-arm64", "darwin", "arm64"],
  "x86_64-apple-darwin": ["darwin-x64", "darwin", "x64"],
  "x86_64-unknown-linux-gnu": ["linux-x64", "linux", "x64", "glibc"],
  "aarch64-unknown-linux-gnu": ["linux-arm64", "linux", "arm64", "glibc"],
  "x86_64-unknown-linux-musl": ["linux-x64-musl", "linux", "x64", "musl"],
  "x86_64-pc-windows-msvc": ["win32-x64", "win32", "x64"],
  "aarch64-pc-windows-msvc": ["win32-arm64", "win32", "arm64"],
};
const REQUIRED = ["aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
const binNames = a["bin-names"].split(",");
const slug = (a.repo ?? readFileSync(join(root, "packaging/REPO"), "utf8")).trim();
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(slug) || /\bOWNER\b/.test(slug))
  throw new Error(`repository slug ${JSON.stringify(slug)} is a placeholder or malformed; run scripts/check-placeholders.mjs --set <owner>/<repo>`);
const repo = `https://github.com/${slug}`;
const common = { version: a.version, license: "Apache-2.0", publishConfig: { access: "public", provenance: true }, repository: { type: "git", url: `git+${repo}.git` }, homepage: repo };

rmSync(a.out, { recursive: true, force: true });
const optional = {};
const missing = [];
for (const [target, [suffix, os, cpu, libc]] of Object.entries(TARGETS)) {
  const src = [join(a.raw, `raw-${target}`), join(a.raw, target)].find(existsSync);
  const exe = os === "win32" ? "synt.exe" : "synt";
  if (!src || !existsSync(join(src, exe))) { if (REQUIRED.includes(target)) missing.push(target); continue; }
  const name = `${a.name}-${suffix}`;
  const dir = join(a.out, name.split("/").pop()); // scoped names: no nested @scope dir
  mkdirSync(join(dir, "bin"), { recursive: true });
  for (const f of readdirSync(src)) {
    copyFileSync(join(src, f), join(dir, "bin", f));
    if (os !== "win32") chmodSync(join(dir, "bin", f), 0o755);
  }
  if (os === "linux" && !existsSync(join(dir, "bin", "synthesis-sandbox-exec")))
    throw new Error(`${target}: synthesis-sandbox-exec missing (Landlock backend requires it next to synt)`);
  for (const f of ["LICENSE", "NOTICE", "README.md"]) copyFileSync(join(root, f), join(dir, f));
  writeFileSync(join(dir, "package.json"), JSON.stringify({
    name, ...common, description: `Synthesis native binary for ${suffix}`, os: [os], cpu: [cpu],
    ...(libc ? { libc: [libc] } : {}), files: ["bin", "LICENSE", "NOTICE", "README.md"],
  }, null, 2) + "\n");
  optional[name] = a.version;
}
if (a["require-all"] && missing.length) throw new Error(`missing required targets: ${missing.join(", ")}`);
if (!Object.keys(optional).length) throw new Error(`no binaries found under ${a.raw}`);

const main = join(a.out, a.name.split("/").pop());
mkdirSync(join(main, "bin"), { recursive: true });
copyFileSync(join(root, "packaging/npm/bin/synt.js"), join(main, "bin/synt.js"));
chmodSync(join(main, "bin/synt.js"), 0o755);
for (const f of ["LICENSE", "NOTICE", "README.md"]) copyFileSync(join(root, f), join(main, f));
writeFileSync(join(main, "package.json"), JSON.stringify({
  name: a.name, ...common, description: "Synthesis autonomous coding agent (native binary, installed via platform packages)",
  bin: Object.fromEntries(binNames.map((n) => [n, "bin/synt.js"])),
  files: ["bin", "LICENSE", "NOTICE", "README.md"], engines: { node: ">=18" },
  optionalDependencies: optional, synthesis: { platformPrefix: a.name },
}, null, 2) + "\n");
console.log(`wrote ${Object.keys(optional).length + 1} packages to ${a.out}: ${[a.name, ...Object.keys(optional)].join(", ")}`);
