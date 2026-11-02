#!/usr/bin/env node
// Resolves the platform package that holds the native Synthesis binary and runs it.
// Signals: SIGINT is NOT forwarded (the terminal already delivers Ctrl-C to the whole foreground
// process group, so forwarding would deliver it twice); SIGTERM/SIGHUP/SIGQUIT are forwarded.
// Exit: the child's exit code; if the child died from a signal, this process re-raises it.
"use strict";
const { spawn } = require("node:child_process");
const path = require("node:path");

const MAIN = require("../package.json");
const PREFIX = MAIN.synthesis.platformPrefix; // e.g. "@devonchain101/synthesis"

function candidates() {
  const { platform, arch } = process;
  const key = `${platform}-${arch}`;
  if (platform !== "linux") return [`${PREFIX}-${key}`];
  const glibc = Boolean(process.report?.getReport?.().header?.glibcVersionRuntime);
  return glibc ? [`${PREFIX}-${key}`, `${PREFIX}-${key}-musl`] : [`${PREFIX}-${key}-musl`, `${PREFIX}-${key}`];
}

function resolveBinary() {
  const exe = process.platform === "win32" ? "synt.exe" : "synt";
  for (const pkg of candidates()) {
    try {
      return path.join(path.dirname(require.resolve(`${pkg}/package.json`)), "bin", exe);
    } catch {
      /* try the next candidate */
    }
  }
  const tried = candidates().join(", ");
  console.error(
    `synt: no native binary for ${process.platform}-${process.arch} (looked for: ${tried}).\n` +
      `Supported: darwin-arm64, darwin-x64, linux-x64, linux-arm64, win32-x64, win32-arm64.\n` +
      `If your platform is supported, reinstall without --no-optional / --omit=optional.\n` +
      `Otherwise see https://github.com/DevOnChain101/nemo-acd/releases for other install methods.`,
  );
  process.exit(127);
}

const child = spawn(resolveBinary(), process.argv.slice(2), { stdio: "inherit", windowsHide: true });
child.on("error", (e) => {
  console.error(`synt: failed to start native binary: ${e.message}`);
  process.exit(126);
});

process.on("SIGINT", () => {});
for (const sig of ["SIGTERM", "SIGHUP", "SIGQUIT"]) {
  try {
    process.on(sig, () => child.kill(sig));
  } catch {
    /* signal not supported on this platform */
  }
}

child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
