// node --test scripts/package-desktop.test.mjs
// macOS only (codesign/ditto): packages a stand-in binary and checks where `synt` lands and that the seal covers it.
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, chmodSync, existsSync, statSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const script = join(fileURLToPath(import.meta.url), "..", "package-desktop.mjs");
const mac = process.platform === "darwin";

function pack(extra) {
  const d = mkdtempSync(join(tmpdir(), "pkgdesk-"));
  // /usr/bin/true is a signed Mach-O: a stand-in for both executables
  const bin = join(d, "bin");
  writeFileSync(bin, readFileSync("/usr/bin/true"));
  chmodSync(bin, 0o755);
  const r = spawnSync(
    "node",
    [script, "--target", "aarch64-apple-darwin", "--version", "0.0.1", "--bin", bin, "--out", join(d, "dist"), ...extra(bin)],
    { encoding: "utf8" },
  );
  return { d, r, macos: join(d, "dist", "synthesis-desktop-0.0.1-aarch64-apple-darwin", "Synthesis.app", "Contents", "MacOS") };
}

test("--synt puts a 0755 synt beside the app executable and the bundle verifies", { skip: !mac }, () => {
  const { r, macos } = pack((bin) => ["--synt", bin]);
  assert.equal(r.status, 0, r.stderr);
  assert.ok(existsSync(join(macos, "Synthesis")) && existsSync(join(macos, "synt")));
  assert.equal(statSync(join(macos, "synt")).mode & 0o777, 0o755);
  assert.doesNotMatch(r.stderr, /WARNING/);
  const v = spawnSync("codesign", ["--verify", "--strict", join(macos, "..", "..")], { encoding: "utf8" });
  assert.equal(v.status, 0, v.stderr);
});

test("without --synt the package is as before and says Outcomes cannot self-start", { skip: !mac }, () => {
  const { r, macos } = pack(() => []);
  assert.equal(r.status, 0, r.stderr);
  assert.ok(!existsSync(join(macos, "synt")));
  assert.match(r.stderr, /WARNING no --synt.*cannot start the orchestrator/);
});

test("a missing --synt file is refused before anything is staged", () => {
  const r = spawnSync(
    "node",
    [script, "--target", "aarch64-apple-darwin", "--version", "0.0.1", "--bin", script, "--out", tmpdir(), "--synt", "/nonexistent/synt"],
    { encoding: "utf8" },
  );
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /synt binary not found/);
});

test("a flag with no value (a trailing --synt) is an error, not silently ignored", () => {
  for (const argv of [["--synt"], ["--synt", "--out", "x"]]) {
    const r = spawnSync("node", [script, "--target", "aarch64-apple-darwin", "--version", "0.0.1", "--bin", script, "--out", tmpdir(), ...argv], { encoding: "utf8" });
    assert.notEqual(r.status, 0);
    assert.match(r.stderr, /missing value for --synt/);
  }
});
