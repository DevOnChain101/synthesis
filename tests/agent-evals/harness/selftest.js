// Self-test: oracle must succeed everywhere it can run, noop must be a false_success everywhere.
//   node tests/agent-evals/harness/selftest.js   (or: node --test tests/agent-evals/harness/selftest.js)
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { loadTasks, runEval, summarize, prepareRepo, copyHidden, runChecks } from './run.js';

const out = fs.mkdtempSync(path.join(os.tmpdir(), 'eval-selftest-'));
process.env.EVAL_LEAK_GRACE_MS = '500';
// GitHub's log viewer shows only the tail of a long step: on failure print the logs of the failing
// commands LAST (tail 40 lines each). Logs live in `out`: fixture-*.log and every run dir's *.log
// next to a result.json with success=false.
const tail = (f) => fs.readFileSync(f, 'utf8').split('\n').slice(-40).join('\n');
process.on('exit', () => {
  if (process.exitCode) {
    const found = [];
    const walk = (d) => {
      for (const e of fs.readdirSync(d, { withFileTypes: true })) {
        const f = path.join(d, e.name);
        if (e.isDirectory()) walk(f);
        else if (e.name.startsWith('fixture-') && e.name.endsWith('.log')) found.push(f);
        else if (e.name === 'result.json' && JSON.parse(fs.readFileSync(f, 'utf8')).success === false) found.push(path.join(d, 'hidden.log'), path.join(d, 'stderr.log'));
      }
    };
    try { walk(out); } catch { /* best effort */ }
    for (const f of found) if (fs.existsSync(f)) console.log(`\n===== FAILURE LOG ${path.relative(out, f)} (last 40 lines) =====\n${tail(f)}`);
  }
  fs.rmSync(out, { recursive: true, force: true });
});
const names = (rs, pred) => rs.filter(pred).map((r) => r.task_id);

test('oracle succeeds on every runnable scenario', async () => {
  const rs = await runEval({ agent: 'oracle', runs: 1, out });
  console.log('\n== oracle ==\n' + summarize(rs).text);
  assert.deepEqual(names(rs, (r) => r.error), []);
  assert.deepEqual(names(rs, (r) => !r.skipped && !r.success), []);
  assert.deepEqual(names(rs, (r) => r.false_success), []);
});

test('noop is a false_success on every runnable scenario', async () => {
  const rs = await runEval({ agent: 'noop', runs: 1, out });
  console.log('\n== noop ==\n' + summarize(rs).text);
  assert.deepEqual(names(rs, (r) => r.error), []);
  assert.deepEqual(names(rs, (r) => !r.skipped && (r.success || !r.false_success)), []);
});

test('fixtures: regression passes before bug.patch, hidden fails after', async () => {
  for (const task of loadTasks()) {
    if (task.requires.includes('python3') && !fs.existsSync('/usr/bin/python3') && process.platform !== 'win32') continue;
    const clean = await prepareRepo(task, { bug: false });
    const buggy = await prepareRepo(task);
    try {
      const must = async (checks, repo, label, ok) => {
        const rs = await runChecks(checks, repo);
        if (!ok(rs)) fs.writeFileSync(path.join(out, `fixture-${task.id}-${label.replace(/\W+/g, "-")}.log`), rs.map((c) => c.log).join('\n'));
        assert.ok(ok(rs), `${task.id}: ${label}`);
      };
      if (task.regression_checks.length) await must(task.regression_checks, clean.repo, 'clean repo must pass its own tests', (rs) => rs.every((c) => c.passed));
      if (task.expected_status === 'completed') {
        copyHidden(task, buggy.repo);
        await must(task.hidden_checks, buggy.repo, 'hidden check must fail on the buggy repo', (rs) => rs.some((c) => !c.passed));
      }
    } finally {
      fs.rmSync(clean.dir, { recursive: true, force: true });
      fs.rmSync(buggy.dir, { recursive: true, force: true });
    }
  }
});

test('node adapter drives NativeAgent with the fake provider', async (t) => {
  if (!fs.existsSync(path.join(process.env.NEMO_NODE_REF || path.join(os.homedir(), 'Vibe', 'nemo-cli'), 'src/native/agent.js'))) return t.skip('Node reference not found');
  process.env.EVAL_FAKE_PROVIDER = '1';
  const rs = await runEval({ agent: 'node', taskIds: ['fe-cart-total'], runs: 1, out });
  delete process.env.EVAL_FAKE_PROVIDER;
  console.log('\n== node (fake provider) ==\n' + summarize(rs).text);
  assert.equal(rs[0].error, undefined);
  assert.equal(rs[0].claimed, 'completed');
  assert.equal(rs[0].metrics.tool_calls, 1);
  assert.equal(rs[0].false_success, true); // the scripted fake never fixes the bug
});

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..', '..', '..');
const synthesisBin = path.join(process.env.CARGO_TARGET_DIR || path.join(repoRoot, 'target'), 'debug', process.platform === 'win32' ? 'synt.exe' : 'synt');

test('rust adapter is skipped, not failed, when the binary is missing', async () => {
  process.env.SYNT_BIN = path.join(out, 'no-such-synthesis');
  const rs = await runEval({ agent: 'rust', taskIds: ['fe-cart-total'], runs: 1, out });
  delete process.env.SYNT_BIN;
  assert.equal(rs[0].skipped, true);
});

test('rust adapter fixes be-node-pagination through the real binary (scripted model)', async (t) => {
  const b = spawnSync('cargo', ['build', '-q', '-p', 'synthesis-cli', '--features', 'test-provider'], { cwd: repoRoot, encoding: 'utf8' });
  if (b.error || b.status !== 0) return t.skip('cannot build synt with --features test-provider: ' + (b.error || b.stderr).toString().slice(0, 300));
  process.env.SYNT_BIN = synthesisBin;
  process.env.EVAL_FAKE_SCRIPT = path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'adapters', 'rust', 'scripts', 'be-node-pagination.json');
  const rs = await runEval({ agent: 'rust', taskIds: ['be-node-pagination'], runs: 1, out });
  delete process.env.SYNT_BIN;
  delete process.env.EVAL_FAKE_SCRIPT;
  console.log('\n== rust (scripted model) ==\n' + summarize(rs).text);
  assert.equal(rs[0].error, undefined);
  assert.equal(rs[0].success, true);
  assert.equal(rs[0].claimed, 'completed');
  assert.equal(rs[0].metrics.tool_calls, 2);
});

