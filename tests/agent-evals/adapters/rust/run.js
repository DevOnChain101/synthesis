// Runs `synt run` (the Rust binary) on the task. Binary: $SYNT_BIN, else target/{debug,release}/synt.
//   EVAL_FAKE_SCRIPT=<script.json>  scripted model (needs a build with --features test-provider; selftest only)
//   EVAL_EXTRA_ARGS='["--deny","Bash(npm run test:*)"]'  extra `synt run` arguments (JSON array; selftest only)
//   EVAL_PERMISSION_MODE_OVERRIDE=<mode>  diagnostic only (e.g. bypassPermissions under the sandbox); the task's mode otherwise
//   EVAL_MAX_COST_USD=<n>           cost cap per run for a real model (else --no-cost-cap)
//   EVAL_MODEL="provider/model"      optional; real mode needs SYNT_* credentials in the environment
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '..', '..', '..', '..');
const exe = process.platform === 'win32' ? 'synt.exe' : 'synt';
const targetDir = process.env.CARGO_TARGET_DIR || path.join(repoRoot, 'target');
const bin = process.env.SYNT_BIN || ['debug', 'release'].map((p) => path.join(targetDir, p, exe)).find((p) => fs.existsSync(p));
const die = (msg) => { process.stderr.write(`adapter "rust": ${msg}\n`); process.exit(3); };

const cwd = process.env.EVAL_CWD;
const prompt = process.env.EVAL_PROMPT;
if (!cwd || !prompt) die('EVAL_CWD and EVAL_PROMPT are required');
if (!bin || !fs.existsSync(bin)) die('synt binary not found (cargo build -p synthesis-cli, or set SYNT_BIN)');

const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'eval-rust-')));
// Loopback: tasks start a local server in their tests (still no outside network). The node
// toolchain (npm lives next to node, often under $HOME) is granted read-only.
// EVAL_MAX_COST_USD caps a real-model run (nightly); the scripted runs and the default keep `--no-cost-cap`.
const args = ['run', '--cwd', cwd, ...(process.env.EVAL_MAX_COST_USD ? ['--max-cost-usd', process.env.EVAL_MAX_COST_USD] : ['--no-cost-cap']), '--no-worktree', '--no-session-store', '--network', 'loopback', '--sandbox-read', path.dirname(path.dirname(fs.realpathSync(process.execPath))), '--permission-mode', process.env.EVAL_PERMISSION_MODE_OVERRIDE || process.env.EVAL_PERMISSION_MODE || 'acceptEdits'];
const env = { ...process.env };
// One-shot credential file from the harness (0600): read, then deleted, so no sibling process finds it later.
if (process.env.EVAL_KEY_ONCE) {
  try { env.SYNT_API_KEY = fs.readFileSync(process.env.EVAL_KEY_ONCE, 'utf8').trim(); } finally { fs.rmSync(process.env.EVAL_KEY_ONCE, { force: true }); }
  delete env.EVAL_KEY_ONCE;
}
const fake = process.env.EVAL_FAKE_SCRIPT;
if (fake) { // never touch the real home in scripted mode
  Object.assign(env, { HOME: tmp, USERPROFILE: tmp, SYNT_HOME: path.join(tmp, 'synthesis') });
  args.push('--provider', `fake:${fake}`);
} else if (process.env.EVAL_MODEL) {
  args.push('--model', process.env.EVAL_MODEL);
}
if (process.env.EVAL_EXTRA_ARGS) args.push(...JSON.parse(process.env.EVAL_EXTRA_ARGS)); // selftest scenarios, e.g. a --deny rule
for (const [env_, flag] of [['EVAL_MAX_TURNS', '--max-turns'], ['EVAL_MAX_TOOL_CALLS', '--max-tool-calls']]) if (process.env[env_]) args.push(flag, process.env[env_]);
if (process.env.EVAL_MAX_WALL_MS) args.push('--max-wall-secs', String(Math.ceil(Number(process.env.EVAL_MAX_WALL_MS) / 1000)));

// Enforce (the default) refuses to start on a host without a usable sandbox that also honours
// `--network loopback` (`enforce_ready: yes` = the standard policy builds and wraps) (e.g. CI runners with userns blocked). That is a property of the host, not of
// the task: fall back to best-effort there. EVAL_SANDBOX=enforce|best-effort|off overrides.
let sandbox = process.env.EVAL_SANDBOX;
if (!sandbox) {
  const doc = spawnSync(bin, ['doctor', '--cwd', cwd], { env, encoding: 'utf8' }).stdout || '';
  sandbox = /enforce_ready: yes/.test(doc) && /loopback=(Weak|Strong)/.test(doc) ? 'enforce' : 'best-effort';
}
process.stderr.write(`adapter "rust": --sandbox ${sandbox}\n`);
args.push('--sandbox', sandbox);
// Without an enforcing sandbox, `npm run` / `npm test` are not auto-safe (they run project code the
// agent may have rewritten), so a headless run would deny them. A scripted run on such a host (a CI
// Linux box without bubblewrap, Windows) states the permission the operator would: a USER-level
// allow for the fixture's runners. Hosts that enforce keep the real defaults.
if (process.env.EVAL_FAKE_SCRIPT && sandbox !== 'enforce') {
  for (const pre of ['npm run', 'npm test', 'node --test']) args.push('--allow', `Bash(${pre}:*)`);
}

// `node --test test/*.test.js` -> `node --test`; `python3 -m unittest discover -s tests` -> `python3 -m unittest`;
// `cargo test --offline` -> `cargo test`: the program, its mode flag (`--test`, `-m <module>`) or subcommand.
function checkPrefix(check) {
  const w = check.trim().split(/\s+/);
  let n = 1;
  if (w[1] && w[1].startsWith('-')) n = w[1] === '-m' ? 3 : 2;
  else if (w[1] && ['npm', 'yarn', 'pnpm'].includes(w[0]) && w[1] === 'run') n = 3;
  else if (w[1]) n = 2;
  return w.slice(0, n).join(' ');
}

// The task's acceptance criteria are what a user would hand to `--criteria-file`.
let taskPrompt = prompt;
const manifest = path.join(here, '..', '..', 'tasks', process.env.EVAL_TASK_ID || '', 'task.json');
if (process.env.EVAL_TASK_ID && fs.existsSync(manifest)) {
  const criteria = JSON.parse(fs.readFileSync(manifest, 'utf8')).acceptance_criteria || [];
  if (criteria.length) {
    const f = path.join(tmp, 'criteria.json');
    fs.writeFileSync(f, JSON.stringify(criteria));
    args.push('--criteria-file', f);
    // The operator who hands over a check authorises its command prefix (`Bash(cargo test:*)`), so the
    // usual variants (`... 2>&1 | tail`, extra flags) need no approval, and tells the model the command.
    const checks = criteria.filter((c) => c && typeof c.check === 'string' && c.check.trim()).map((c) => c.check.trim());
    for (const pre of new Set(checks.map(checkPrefix))) args.push('--allow', `Bash(${pre}:*)`);
    if (checks.length) taskPrompt += `\n\nVerify your work by running exactly: ${checks.map((c) => '`' + c + '`').join(' and ')}`;
  }
}
args.push('--prompt', taskPrompt);

// stdout (events) and stderr pass straight through to the harness.
const r = spawnSync(bin, args, { env, stdio: ['ignore', 'inherit', 'inherit'] });
fs.rmSync(tmp, { recursive: true, force: true });
if (r.error) die(String(r.error));
// synt exits 1/3/4 for failed/unverified/blocked; the claim is in task.ended. 3 would read as "adapter unavailable".
const code = r.status ?? 70;
process.exit([0, 1, 3, 4].includes(code) ? 0 : code);
