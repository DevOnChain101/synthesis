#!/usr/bin/env node
// Fails (exit 1) on any "nemo" (any case) in a tracked file's content or path that scripts/nemo-leftovers.allow does not cover.
// The product was renamed Nemo -> Synthesis (2026-10-30); the only legitimate leftovers are the GitHub slug, references to the
// frozen Node repo, the legacy-migration hint, history and the Node-baseline tooling. Allow file lines (# starts a comment):
//   token <regex>              an occurrence that lies inside a match of the regex is fine in every file
//   in <path-glob> <regex>     same, in that file only
//   file <path-glob>           the whole file (and its path) is exempt
// A glob is an exact path, or a prefix ending in /** . Binary files are skipped.
//   node scripts/check-no-legacy-name.mjs
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const files = execFileSync("git", ["ls-files", "-z"], { cwd: root, encoding: "utf8", maxBuffer: 1 << 28 }).split("\0").filter(Boolean);
const tokens = [], perFile = [], whole = [];
const glob = (g) => (g.endsWith("/**") ? (p) => p.startsWith(g.slice(0, -2)) : (p) => p === g);
for (const raw of readFileSync(join(root, "scripts/nemo-leftovers.allow"), "utf8").split("\n")) {
  const l = raw.replace(/(^|\s)#.*$/, "").trim();
  if (!l) continue;
  const [kind, a, ...rest] = l.split(/\s+/);
  if (kind === "token") tokens.push(new RegExp([a, ...rest].join(" "), "gi"));
  else if (kind === "in") perFile.push([glob(a), new RegExp(rest.join(" "), "gi")]);
  else if (kind === "file") whole.push(glob(a));
  else throw new Error(`bad allow line: ${raw}`);
}

const covered = (line, at, len, res) =>
  res.some((re) => { re.lastIndex = 0; for (const m of line.matchAll(re)) if (m.index <= at && at + len <= m.index + m[0].length) return true; return false; });
const bad = [];
for (const f of files) {
  if (whole.some((w) => w(f))) continue;
  const res = [...tokens, ...perFile.filter(([w]) => w(f)).map(([, r]) => r)];
  if (/nemo/i.test(f)) bad.push(`${f}: path contains "nemo"`);
  let buf;
  try { buf = readFileSync(join(root, f)); } catch { continue; }
  if (buf.includes(0)) continue;
  buf.toString("utf8").split("\n").forEach((line, i) => {
    for (const m of line.matchAll(/nemo/gi))
      if (!covered(line, m.index, m[0].length, res)) { bad.push(`${f}:${i + 1}: ${line.trim().slice(0, 140)}`); break; }
  });
}
if (bad.length) {
  console.error(`check-no-legacy-name: ${bad.length} occurrence(s) of the old product name outside scripts/nemo-leftovers.allow:\n${bad.join("\n")}`);
  process.exit(1);
}
console.log(`check-no-legacy-name: ${files.length} tracked files ok`);

// The command is `synt` (2026-11-02); a whole-word `synth` is the retired command name. Allowed: history and recorded oracle data.
// Same for the config folder names renamed on 2026-11-02: `.synthesis/`, `synthesis.json`, `synthesis.db` (now `.synt/`, `synt.json`, `synt.db`), `SYNTHESIS.md` (now `AGENTS.md`), `SYNTHESIS_*` env (now `SYNT_*`);
// only `synt doctor` (crates/synthesis-app/src/doctor.rs) may name the old ones, to print how to migrate.
const synthOk = (f) => f === "CHANGELOG.md" || f === "CLAUDE.md" || f === "scripts/check-no-legacy-name.mjs" || f.startsWith("tests/golden/");
const oldCfgOk = (f) => f === "CHANGELOG.md" || f === "CLAUDE.md" || f === "scripts/check-no-legacy-name.mjs" || f === "crates/synthesis-app/src/doctor.rs" || f === "docs/MIGRATION.md" || f === "crates/synthesis-settings/tests/settings.rs";
const oldCfg = /(?<!^\s*)(?<![A-Za-z0-9_])\.synthesis(?![A-Za-z0-9_.(-])|(?<![A-Za-z0-9_-])synthesis\.(json|db)\b|(?<![A-Za-z0-9_])SYNTHESIS\.md\b|(?<![A-Za-z0-9_])SYNTHESIS_|"synthesis"(?=\s*:)/;
const old = [];
for (const f of files) {
  if (synthOk(f) && oldCfgOk(f)) continue;
  let buf;
  try { buf = readFileSync(join(root, f)); } catch { continue; }
  if (buf.includes(0)) continue;
  buf.toString("utf8").split("\n").forEach((line, i) => { if ((!synthOk(f) && /(?<![A-Za-z0-9])synth(?![A-Za-z])|CARGO_BIN_EXE_synth\b/.test(line)) || (!oldCfgOk(f) && oldCfg.test(line))) old.push(`${f}:${i + 1}: ${line.trim().slice(0, 140)}`); });
}
if (old.length) {
  console.error(`check-no-legacy-name: ${old.length} reference(s) to the retired command \`synth\` (now \`synt\`):\n${old.join("\n")}`);
  process.exit(1);
}
