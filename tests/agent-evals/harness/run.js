#!/usr/bin/env node
// Eval runner (spec eval-and-golden.md §A). Node ESM, no dependencies.
//   node harness/run.js --agent oracle [--task id[,id]] [--runs N] [--out dir]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const WIN = process.platform === 'win32';
const GIT = ['-c', 'user.name=eval', '-c', 'user.email=eval@localhost', '-c', 'core.autocrlf=false'];

export function loadTasks(ids) {
  const dir = path.join(ROOT, 'tasks');
  const all = fs.readdirSync(dir).sort().map((d) => JSON.parse(fs.readFileSync(path.join(dir, d, 'task.json'), 'utf8')));
  if (!ids || !ids.length) return all.filter((t) => !t.gaming); // gaming scenarios run only by id (selftest)
  const missing = ids.filter((i) => !all.some((t) => t.id === i));
  if (missing.length) throw new Error('unknown task(s): ' + missing.join(', '));
  return all.filter((t) => ids.includes(t.id));
}

// "python3" is "python" on Windows.
const fixCmd = (c) => (WIN ? c.replace(/^python3(\s|$)/, 'python$1') : c);

export function killTree(pid) {
  if (WIN) spawnSync(path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'taskkill.exe'), ['/PID', String(pid), '/T', '/F'], { stdio: 'ignore' });
  else {
    try { process.kill(-pid, 'SIGKILL'); } catch { /* already gone */ }
  }
}

// Only the adapter gets credentials: setup and graded checks run code the agent wrote.
function cleanEnv(extra, { credentials = false } = {}) {
  const env = { ...process.env, PYTHONDONTWRITEBYTECODE: '1', ...extra };
  delete env.NODE_TEST_CONTEXT; // an outer `node --test` must not leak into the graded `node --test`
  if (!credentials) for (const k of Object.keys(env)) if (/^SYNT_(API_KEY|AUTH_TOKEN)$|_API_KEY$|_AUTH_TOKEN$/.test(k)) delete env[k];
  return env;
}

// Runs a command (shell for check commands, direct for the adapter) in its own process group.
export function runCmd(command, { cwd, env, timeoutMs, args, shell = true }) {
  return new Promise((resolve) => {
    const t0 = Date.now();
    const p = args ? spawn(command, args, { cwd, env, detached: !WIN, stdio: ['ignore', 'pipe', 'pipe'] })
      : spawn(fixCmd(command), { cwd, env, shell, detached: !WIN, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    let timedOut = false;
    const cap = (s, d) => (s.length > 4e6 ? s : s + d);
    p.stdout.on('data', (d) => (out = cap(out, d)));
    p.stderr.on('data', (d) => (err = cap(err, d)));
    const timer = setTimeout(() => { timedOut = true; killTree(p.pid); }, timeoutMs);
    const done = (code, spawnError) => {
      clearTimeout(timer);
      resolve({ code, timedOut, out, err: spawnError ? err + String(spawnError) : err, ms: Date.now() - t0, pid: p.pid });
    };
    p.on('error', (e) => done(127, e));
    p.on('close', (code) => done(code));
  });
}

function git(cwd, ...a) {
  const r = spawnSync('git', [...GIT, ...a], { cwd, encoding: 'utf8', maxBuffer: 1 << 28 });
  if (r.status !== 0) throw new Error(`git ${a.join(' ')}: ${r.stderr || r.error}`);
  return r.stdout;
}

export function fixtureDir(task) { return path.join(ROOT, 'fixtures', task.fixture); }

// Copies repo/, optionally applies bug.patch, runs setup, commits the baseline.
export async function prepareRepo(task, { bug = true } = {}) {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), `eval-${task.id}-`)));
  const repo = path.join(dir, 'repo');
  fs.cpSync(path.join(fixtureDir(task), 'repo'), repo, { recursive: true });
  git(repo, 'init', '-q');
  if (bug) git(repo, 'apply', path.join(fixtureDir(task), 'bug.patch'));
  for (const cmd of task.setup || []) {
    const r = await runCmd(cmd, { cwd: repo, env: cleanEnv(), timeoutMs: 120000 });
    if (r.code !== 0) throw new Error(`setup failed: ${cmd}\n${r.out}${r.err}`);
  }
  git(repo, 'add', '-A');
  git(repo, 'commit', '-q', '-m', 'baseline');
  return { dir, repo };
}

export function copyHidden(task, repo) {
  fs.cpSync(path.join(fixtureDir(task), 'hidden'), path.join(repo, 'hidden'), { recursive: true });
}

export async function runChecks(checks, repo) {
  const res = [];
  for (const c of checks) {
    const r = await runCmd(c.command, { cwd: repo, env: cleanEnv(), timeoutMs: c.timeout_ms || 120000 });
    res.push({ id: c.id, command: c.command, exit_code: r.code, timed_out: r.timedOut, passed: r.code === 0 && !r.timedOut, ms: r.ms, log: `$ ${c.command}\n${r.out}${r.err}${r.timedOut ? '\n[timed out]\n' : ''}\n[exit ${r.code}]\n` });
  }
  return res;
}

function checkRequires(reqs) {
  for (const r of reqs || []) {
    const m = /^([\w.-]+)(?:>=(\d+))?$/.exec(r);
    if (!m) return `unparseable requirement ${r}`;
    const [, name, min] = m;
    if (name === 'browser') return 'requires browser (R6)';
    if (name === 'node') {
      if (min && Number(process.versions.node.split('.')[0]) < Number(min)) return `requires node>=${min}`;
      continue;
    }
    const exe = WIN && name === 'python3' ? 'python' : name;
    const r2 = spawnSync(exe, ['--version'], { stdio: 'ignore', shell: WIN });
    if (r2.status !== 0) return `requires ${name} (not found on PATH)`;
  }
  return null;
}

const globRe = (g) => new RegExp('^' + g.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*\*/g, '\u0000').replace(/\*/g, '[^/]*').replace(/\u0000/g, '.*') + '$');
const matchesAny = (globs, f) => globs.some((g) => globRe(g).test(f));

// pids still alive in the adapter's process group; null where it cannot be checked.
function groupPids(pgid) {
  if (WIN) return null;
  const r = spawnSync('pgrep', ['-g', String(pgid)], { encoding: 'utf8' });
  if (r.error) return null;
  return r.stdout.split('\n').filter(Boolean).map(Number);
}

async function leakCheck(pgid, graceMs) {
  let alive = groupPids(pgid);
  if (alive === null) return null;
  const end = Date.now() + graceMs;
  while (alive.length && Date.now() < end) {
    await new Promise((r) => setTimeout(r, 100));
    alive = groupPids(pgid) || [];
  }
  if (alive.length) killTree(pgid);
  return alive.length;
}

function parseEvents(text) {
  const evs = [];
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    try { evs.push(JSON.parse(line)); } catch { /* not an event: ignored, stderr is for humans */ }
  }
  return evs;
}

function metricsFrom(evs, wall_ms, leaked) {
  const of = (t) => evs.filter((e) => e.type === t);
  const ended = of('task.ended').at(-1);
  const u = (ended && ended.payload.usage) || {};
  return {
    turns: of('turn.started').length,
    tool_calls: of('tool.requested').length,
    input_tokens: u.input_tokens || 0,
    output_tokens: u.output_tokens || 0,
    cost_usd: u.cost_usd || 0,
    wall_ms,
    approvals: of('tool.approval_required').length,
    // the model's own tool calls; `verify-*` ids are the completion gate's inferred checks
    model_approvals: of('tool.approval_required').filter((e) => !String(e.payload.tool_call_id).startsWith('verify-')).length,
    denials: of('tool.approval_resolved').filter((e) => ['deny', 'auto_denied'].includes(e.payload.decision)).length,
    leaked_processes: leaked,
  };
}

const safeSeg = (s) => String(s).replace(/[^\w.-]+/g, '_');

async function runOne(task, run, agent, outRoot, opts) {
  const meta = JSON.parse(fs.readFileSync(path.join(ROOT, 'adapters', agent, 'meta.json'), 'utf8'));
  const base = { task_id: task.id, run, agent, os: process.platform };
  const skip = checkRequires(task.requires);
  if (skip) return { ...base, skipped: true, reason: skip };

  const started_at = new Date().toISOString();
  const { dir, repo } = await prepareRepo(task);
  try {
    const adapterEnv = {
      EVAL_PROMPT: task.prompt, EVAL_CWD: repo, EVAL_TASK_ID: task.id, EVAL_PERMISSION_MODE: task.permission_mode,
      EVAL_MAX_TURNS: String(task.budget.max_turns), EVAL_MAX_TOOL_CALLS: String(task.budget.max_tool_calls), EVAL_MAX_WALL_MS: String(task.budget.max_wall_ms),
      EVAL_EVENTS_OUT: path.join(dir, 'events.jsonl'),
    };
    if (agent === 'oracle') { // the answer key goes to the oracle only
      const p = path.join(fixtureDir(task), 'oracle', 'fix.patch');
      if (fs.existsSync(p)) adapterEnv.EVAL_ORACLE_PATCH = p;
      adapterEnv.EVAL_ORACLE_STATUS = task.expected_status;
    }
    const a = await runCmd(process.execPath, { cwd: repo, env: cleanEnv(adapterEnv, { credentials: true }), timeoutMs: Math.min(task.budget.max_wall_ms, opts.maxWallMs ?? Infinity), args: [path.join(ROOT, 'adapters', agent, 'run.js')] });
    if (a.code === 3 && !a.timedOut) return { ...base, skipped: true, reason: 'adapter unavailable: ' + a.err.trim().split('\n')[0] };
    const graceMs = Number(process.env.EVAL_LEAK_GRACE_MS ?? 5000);
    const leaked = a.timedOut ? (killTree(a.pid), 0) : await leakCheck(a.pid, graceMs);
    fs.writeFileSync(path.join(dir, 'events.jsonl'), a.out);
    const evs = parseEvents(a.out);

    // adapter exited: what changed in the repo?
    git(repo, 'add', '-A');
    const changed = git(repo, 'diff', '--cached', '--name-only').split('\n').filter(Boolean);
    const diff = git(repo, 'diff', '--cached');
    const forbidden = changed.filter((f) => matchesAny(task.forbidden_paths || [], f));
    for (const e of evs) { // Read events
      if (e.type !== 'tool.requested' || e.payload.name !== 'Read') continue;
      const p = e.payload.input && (e.payload.input.file_path || e.payload.input.path);
      if (!p) continue;
      const rel = path.relative(repo, path.resolve(repo, p)).split(path.sep).join('/');
      if (matchesAny(task.forbidden_paths || [], rel)) forbidden.push(rel + ' (read)');
    }
    const patterns = (task.forbidden_event_patterns || []).map((p) => new RegExp(p, 'i'));
    const violations = evs.filter((e) => e.type === 'tool.requested').filter((e) => patterns.some((re) => re.test(JSON.stringify(e.payload.input || {}))));

    copyHidden(task, repo);
    const hidden = await runChecks(task.hidden_checks || [], repo);
    const regression = await runChecks(task.regression_checks || [], repo);

    const ended = evs.filter((e) => e.type === 'task.ended').at(-1);
    const claimed = ended ? ended.payload.status : null;
    const started = evs.find((e) => e.type === 'session.started');
    const sp = (started && started.payload) || {};
    // session.started.model has no provider prefix (cc/claude-x is a model id); turn.started carries the provider.
    const turn = evs.find((e) => e.type === 'turn.started');
    const [provider, ...rest] = turn && turn.payload.provider ? [turn.payload.provider, sp.model || turn.payload.model] : String(sp.model || `${meta.provider}/${meta.model}`).split('/');
    const model = rest.join('/') || provider;
    // repo_ok: the workspace is right. success (= status_ok, the primary criterion): repo_ok AND the
    // claimed status is the expected one (a correct repo that ends `blocked` is not a pass).
    const repo_ok = hidden.every((c) => c.passed) && regression.every((c) => c.passed) && !forbidden.length && !violations.length;
    const success = repo_ok && claimed === task.expected_status;
    const wall_ms = a.ms;
    const result = {
      ...base, agent_version: sp.agent_version || meta.agent_version, ref_commit: meta.ref_commit, provider, model,
      success, repo_ok, claimed, false_success: claimed === 'completed' && !success, timed_out: a.timedOut, adapter_exit: a.code,
      checks: { hidden: hidden.map(({ log, ...c }) => c), regression: regression.map(({ log, ...c }) => c), forbidden_touched: forbidden, forbidden_events: violations.length },
      metrics: metricsFrom(evs, wall_ms, leaked), started_at, finished_at: new Date().toISOString(),
    };
    const utc = new Date().toISOString().slice(0, 10);
    const rd = path.join(outRoot, safeSeg(agent), safeSeg(result.agent_version), safeSeg(provider), safeSeg(model), utc, task.id, `run-${run}`);
    fs.mkdirSync(rd, { recursive: true });
    fs.writeFileSync(path.join(rd, 'result.json'), JSON.stringify(result, null, 2) + '\n');
    fs.copyFileSync(path.join(dir, 'events.jsonl'), path.join(rd, 'events.jsonl'));
    fs.writeFileSync(path.join(rd, 'diff.patch'), diff);
    fs.writeFileSync(path.join(rd, 'hidden.log'), [...hidden, ...regression].map((c) => c.log).join('\n'));
    fs.writeFileSync(path.join(rd, 'stderr.log'), a.err);
    return result;
  } finally {
    fs.rmSync(dir, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
  }
}

const median = (xs) => { if (!xs.length) return 0; const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]; };
const pct = (n, d) => (d ? `${Math.round((100 * n) / d)}%` : 'n/a');

export function summarize(results) {
  const rows = [['task', 'runs', 'success', 'repo_ok', 'claimed', 'false_success', 'wall_ms', 'leaked']];
  const ids = [...new Set(results.map((r) => r.task_id))];
  for (const id of ids) {
    const rs = results.filter((r) => r.task_id === id);
    if (rs[0].skipped) { rows.push([id, '0', 'skipped', '-', '-', '-', '-', rs[0].reason]); continue; }
    rows.push([id, String(rs.length), `${rs.filter((r) => r.success).length}/${rs.length}`, `${rs.filter((r) => r.repo_ok).length}/${rs.length}`, [...new Set(rs.map((r) => r.claimed))].join(','),
      String(rs.filter((r) => r.false_success).length), String(median(rs.map((r) => r.metrics.wall_ms))), String(rs.reduce((n, r) => n + (r.metrics.leaked_processes || 0), 0))]);
  }
  const w = rows[0].map((_, i) => Math.max(...rows.map((r) => r[i].length)));
  const table = rows.map((r) => r.map((c, i) => c.padEnd(w[i])).join('  ').trimEnd()).join('\n');
  const done = results.filter((r) => !r.skipped);
  const claimedDone = done.filter((r) => r.claimed === 'completed');
  const agg = {
    runs: done.length, skipped: results.length - done.length,
    status_ok_rate: pct(done.filter((r) => r.success).length, done.length),
    repo_ok_rate: pct(done.filter((r) => r.repo_ok).length, done.length),
    completed_without_model_approval_rate: pct(done.filter((r) => r.claimed === 'completed' && !r.metrics.model_approvals).length, done.length),
    completed_without_any_approval_rate: pct(done.filter((r) => r.claimed === 'completed' && !r.metrics.approvals).length, done.length),
    verified_completion_rate: pct(claimedDone.filter((r) => r.success).length, claimedDone.length),
    false_success_rate: pct(done.filter((r) => r.false_success).length, done.length),
    median_wall_ms: median(done.map((r) => r.metrics.wall_ms)),
  };
  return { table, agg, text: table + '\n\n' + Object.entries(agg).map(([k, v]) => `${k}: ${v}`).join('\n') };
}

export async function runEval({ agent, taskIds, runs, out = path.join(ROOT, 'results'), maxWallMs, log = () => {} }) {
  if (!fs.existsSync(path.join(ROOT, 'adapters', agent, 'run.js'))) throw new Error(`unknown agent "${agent}"`);
  const results = [];
  for (const task of loadTasks(taskIds)) {
    const n = runs ?? task.runs;
    for (let i = 1; i <= n; i++) {
      log(`${task.id} run ${i}/${n}`);
      let r;
      try { r = await runOne(task, i, agent, out, { maxWallMs }); } catch (e) { r = { task_id: task.id, run: i, agent, os: process.platform, success: false, claimed: null, false_success: false, error: String(e.stack || e), metrics: { wall_ms: 0, leaked_processes: 0 } }; }
      results.push(r);
      if (r.skipped) { // recorded beside the real results, one file per task
        fs.mkdirSync(path.join(out, safeSeg(agent), 'skipped'), { recursive: true });
        fs.writeFileSync(path.join(out, safeSeg(agent), 'skipped', `${task.id}.json`), JSON.stringify({ ...r, status: 'skipped', at: new Date().toISOString() }, null, 2) + '\n');
        break; // same reason for every run
      }
    }
  }
  return results;
}

function parseArgs(argv) {
  const o = {};
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (!['--task', '--agent', '--runs', '--out'].includes(k) || i + 1 >= argv.length) throw new Error(`usage: run.js --agent <name> [--task id[,id]] [--runs N] [--out dir] (bad arg: ${k})`);
    o[k.slice(2)] = argv[++i];
  }
  return o;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const a = parseArgs(process.argv.slice(2));
    const results = await runEval({ agent: a.agent || 'node', taskIds: a.task && a.task.split(','), runs: a.runs && Number(a.runs), out: a.out && path.resolve(a.out), log: (m) => console.error(m) });
    const s = summarize(results);
    console.log(s.text);
    const bad = results.filter((r) => r.error);
    if (bad.length) { console.error(bad.map((r) => `${r.task_id}: ${r.error}`).join('\n')); process.exit(70); }
  } catch (e) {
    console.error(e.message);
    process.exit(2);
  }
}
