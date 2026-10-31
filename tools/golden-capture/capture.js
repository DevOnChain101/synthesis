#!/usr/bin/env node
'use strict';
// Captures golden fixtures from the frozen Node reference (NEMO_NODE_REF, default ~/Vibe/nemo-cli).
//   node tools/golden-capture/capture.js [--out DIR] [--areas a,b] [--real-date]
// Output: <out>/<area>/<case_id>/{input.json,expected.json,meta.json}. See tests/golden/README.md.
const { ref, realHome, envBase } = require('./env'); // must stay first
const fs = require('fs');
const path = require('path');
const { stable, sandbox, treeHash } = require('./lib');

const argv = process.argv.slice(2);
const opt = (n, d) => { const i = argv.indexOf(n); return i >= 0 ? argv[i + 1] : d; };
const out = path.resolve(opt('--out', path.join(__dirname, '..', '..', 'tests', 'golden')));
const AREAS = ['config', 'permissions', 'transcripts', 'schemas', 'tools', 'providers', 'settings', 'hooks'];
const only = opt('--areas', '') ? opt('--areas').split(',') : AREAS;
const realDate = argv.includes('--real-date');

if (!fs.existsSync(path.join(ref, 'package.json'))) throw new Error(`reference not found: ${ref}`);
const refVersion = JSON.parse(fs.readFileSync(path.join(ref, 'package.json'), 'utf8')).version;
const refHash = treeHash(ref, ['src', 'bin']);
const capturedAt = realDate ? new Date().toISOString() : '<TS>';

const leakNeedles = [envBase, realHome].filter((s) => s && s.length > 1);
const counts = {};
const leaks = [];

(async () => {
  for (const area of only) {
    const mod = require(`./areas/${area}`);
    const areaDir = path.join(out, area);
    // Re-capture must not silently drop the owner's review: remember reviewed_by per case before the area is wiped.
    const reviewed = {};
    if (fs.existsSync(areaDir)) {
      for (const id of fs.readdirSync(areaDir)) {
        try { reviewed[id] = JSON.parse(fs.readFileSync(path.join(areaDir, id, 'meta.json'), 'utf8')).reviewed_by ?? null; } catch {}
      }
    }
    fs.rmSync(areaDir, { recursive: true, force: true });
    const ids = new Set();
    counts[area] = 0;
    for (const c of mod.cases()) {
      if (ids.has(c.id)) throw new Error(`duplicate case id ${area}/${c.id}`);
      ids.add(c.id);
      const sb = sandbox();
      let expected;
      process.env.HOME = process.env.USERPROFILE = sb.home; // os.homedir() follows the per-case fake home
      process.env.NEMO_HOME = sb.nemoHome;
      try {
        expected = sb.normalize(await mod.run(sb.expand(c.input), sb, c));
      } finally {
        sb.cleanup();
      }
      let files = {};
      if (mod.finish) ({ expected, files } = mod.finish(expected)); // optional extra output files (e.g. render expected.txt)
      const text = stable(expected);
      for (const n of [...leakNeedles, sb.tmp]) if (text.includes(n)) leaks.push(`${area}/${c.id}: contains ${n}`);
      const dir = path.join(areaDir, c.id);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, 'input.json'), stable(c.input));
      fs.writeFileSync(path.join(dir, 'expected.json'), text);
      for (const [name, body] of Object.entries(files)) {
        const data = typeof body === 'string' ? body : stable(body);
        for (const n of [...leakNeedles, sb.tmp]) if (data.includes(n)) leaks.push(`${area}/${c.id}/${name}: contains ${n}`);
        fs.writeFileSync(path.join(dir, name), data);
      }
      fs.writeFileSync(path.join(dir, 'meta.json'), stable({
        source: c.source || mod.source,
        ref_version: refVersion,
        ref_tree_sha256: refHash,
        node: process.version,
        platform: process.platform,
        captured_at: capturedAt,
        reviewed_by: reviewed[c.id] ?? null,
        ...(c.note ? { note: c.note } : {}),
      }));
      counts[area]++;
    }
  }
  console.log(JSON.stringify({ out, ref_version: refVersion, ref_tree_sha256: refHash, counts, total: Object.values(counts).reduce((a, b) => a + b, 0), leaks }, null, 2));
  fs.rmSync(envBase, { recursive: true, force: true });
  if (leaks.length) process.exit(1);
})().catch((e) => { console.error(e); process.exit(2); });
