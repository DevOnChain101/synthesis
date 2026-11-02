#!/usr/bin/env node
// Release gate for the repository slug. The single source of the GitHub slug is packaging/REPO.
//   node scripts/check-placeholders.mjs [--expect owner/repo]
// Fails if the slug is malformed, still OWNER, or differs from --expect (case-insensitive); if an unfilled
// placeholder (OWNER, @REPO@, @REPO_OWNER@, @SYNT_REPO@) remains in README/docs/packaging/installers; or if the
// installers' built-in default repository is not the slug. (@VERSION@/@SHA256_*@ are filled at submission.)
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

const { values: a } = parseArgs({ options: { expect: { type: "string" } } });
const SLUG = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const installers = ["scripts/install.sh", "scripts/install.ps1"];
const walk = (d) => readdirSync(d).flatMap((f) => (statSync(join(d, f)).isDirectory() ? walk(join(d, f)) : [join(d, f)]));
const fail = (m) => { console.error(`check-placeholders: ${m}`); process.exit(1); };

const slug = readFileSync("packaging/REPO", "utf8").trim();
if (!SLUG.test(slug)) fail(`packaging/REPO is not owner/repo: ${JSON.stringify(slug)}`);
if (/\bOWNER\b/.test(slug)) fail(`packaging/REPO still holds the placeholder ${slug}`);
if (a.expect && a.expect.toLowerCase() !== slug.toLowerCase()) fail(`packaging/REPO (${slug}) != ${a.expect}`);
const files = ["README.md", "docs/INSTALL.md", "docs/RELEASE.md", ...installers, ...walk("packaging")];
const left = files.filter((f) => /\bOWNER\b|@REPO@|@REPO_OWNER@|@SYNT_REPO@/.test(readFileSync(f, "utf8")));
if (left.length) fail(`placeholder remains in: ${left.join(", ")}`);
const stale = installers.filter((f) => !readFileSync(f, "utf8").includes(slug));
if (stale.length) fail(`default repository in ${stale.join(", ")} is not ${slug}`);
console.log(`check-placeholders: ok (${slug})`);
