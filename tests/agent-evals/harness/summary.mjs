// Markdown summary of an eval results directory: status-ok, false-success and cost. Used by evals-nightly.yml.
//   node tests/agent-evals/harness/summary.mjs <results-dir>
import fs from 'node:fs';
import path from 'node:path';

const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).flatMap((e) => {
  const f = path.join(d, e.name);
  return e.isDirectory() ? walk(f) : e.name === 'result.json' ? [f] : [];
});
const dir = process.argv[2];
const rs = fs.existsSync(dir) ? walk(dir).map((f) => JSON.parse(fs.readFileSync(f, 'utf8'))).filter((r) => !r.skipped) : [];
const pct = (n, d) => (d ? `${((100 * n) / d).toFixed(1)}%` : 'n/a');
const cost = rs.reduce((n, r) => n + (r.metrics?.cost_usd || 0), 0);
const lines = ['## Evals with a real model', '', `| runs | status-ok | false-success | cost (USD) |`, '|---|---|---|---|',
  `| ${rs.length} | ${pct(rs.filter((r) => r.success).length, rs.length)} | ${pct(rs.filter((r) => r.false_success).length, rs.length)} | ${cost.toFixed(4)} |`, '',
  '| task | status-ok | false-success | cost (USD) |', '|---|---|---|---|'];
for (const id of [...new Set(rs.map((r) => r.task_id))].sort()) {
  const t = rs.filter((r) => r.task_id === id);
  lines.push(`| ${id} | ${t.filter((r) => r.success).length}/${t.length} | ${t.filter((r) => r.false_success).length}/${t.length} | ${t.reduce((n, r) => n + (r.metrics?.cost_usd || 0), 0).toFixed(4)} |`);
}
console.log(lines.join('\n'));
