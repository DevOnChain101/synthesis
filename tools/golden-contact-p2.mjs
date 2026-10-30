#!/usr/bin/env node
// Contact sheet of redesign phase P2 (layout engine, panel registry): every golden case stamped
// "pending-owner-review (P2 ...)", drawn from its expected.ansi.json. The default-panel frames at the
// five reference sizes and the Tabs views are in colour; the rest are plain text (the file stays
// small). The owner reviews them against the HTML prototype ~/Vibe/synthesis-cli-ui.
// Usage: node tools/golden-contact-p2.mjs [--out FILE]
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const out = arg('--out', path.join(repo, 'docs/research/tui-p2-contact-sheet.html'));
const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const strip = (l) => l.replace(/\x1b\[[0-9;]*m/g, '');

function rowToHtml(row) {
  let fg = null, bg = null, bold = false, dim = false, ital = false, ul = false, rev = false, html = '';
  const style = () => {
    let f = fg, b = bg;
    if (rev) [f, b] = [b ?? '#080e12', f ?? '#c8ccd2'];
    return [f && `color:${f}`, b && `background:${b}`, bold && 'font-weight:700', dim && 'opacity:.6', ital && 'font-style:italic', ul && 'text-decoration:underline'].filter(Boolean).join(';');
  };
  const re = /\x1b\[([0-9;]*)m|([^\x1b]+)/g;
  let m;
  while ((m = re.exec(row))) {
    if (m[2] !== undefined) { const st = style(); html += st ? `<i style="${st}">${esc(m[2])}</i>` : esc(m[2]); continue; }
    const p = m[1] === '' ? [0] : m[1].split(';').map(Number);
    for (let i = 0; i < p.length; i++) {
      const n = p[i];
      if (n === 0) { fg = bg = null; bold = dim = ital = ul = rev = false; }
      else if (n === 1) bold = true; else if (n === 2) dim = true; else if (n === 3) ital = true;
      else if (n === 4) ul = true; else if (n === 7) rev = true; else if (n === 22) bold = dim = false;
      else if (n === 39) fg = null; else if (n === 49) bg = null;
      else if ((n === 38 || n === 48) && p[i + 1] === 2) {
        const c = `rgb(${p[i + 2]},${p[i + 3]},${p[i + 4]})`;
        if (n === 38) fg = c; else bg = c;
        i += 4;
      }
    }
  }
  return html;
}

const base = path.join(repo, 'tests/golden/render');
const COLOUR = /^layout-(80x24|100x30|120x36|140x40|200x50)-default$|^layout-80x24-tab-|^frame-agent-chat-/;
const cases = [];
for (const d of fs.readdirSync(base).sort()) {
  const meta = path.join(base, d, 'meta.json');
  if (!fs.existsSync(meta)) continue;
  const by = String(JSON.parse(fs.readFileSync(meta, 'utf8')).reviewed_by ?? '');
  if (!by.startsWith('pending-owner-review (P2')) continue;
  cases.push({ id: d, lines: JSON.parse(fs.readFileSync(path.join(base, d, 'expected.ansi.json'), 'utf8')).lines });
}
const card = ({ id, lines }) => {
  const colour = COLOUR.test(id);
  const body = colour ? lines.map(rowToHtml).join('\n') : esc(lines.map(strip).map((l) => l.trimEnd()).join('\n'));
  return `<section><h3>${esc(id)} <small>${lines.length} rows${colour ? '' : ', plain text'}</small></h3><pre>${body}</pre></section>`;
};
const group = (title, test) => `<h2>${title}</h2>\n${cases.filter((c) => test(c.id)).map(card).join('\n')}`;
const html = `<!doctype html><meta charset="utf-8"><title>TUI redesign P2 contact sheet</title>
<style>body{background:#080e12;color:#c8ccd2;font:13px system-ui;margin:16px}pre{font:11px/1.15 ui-monospace,Menlo,Consolas,monospace;background:#080e12;border:1px solid #3a3f4a;padding:6px;margin:0 0 10px;overflow:auto;color:#c8ccd2}
h2{font-size:15px;margin:22px 0 6px}h3{font-size:12px;margin:8px 0 2px;color:#8c929b}small{color:#6b717c;font-weight:400}i{font-style:normal}</style>
<h1>TUI redesign, phase P2: layout and panels (${cases.length} cases pending owner review)</h1>
<p>Reference: the HTML prototype ~/Vibe/synthesis-cli-ui (docs/specs/tui-design.md section 14). Deviations from the mockups are listed in the spec's P2 status. Default panels are on in every case that does not say otherwise; the page background is the default theme's #080E12.</p>
${group('1. Default panels at the reference sizes', (i) => /^layout-\d+x\d+-default$/.test(i))}
${group('2. Tabs mode and the narrow sizes', (i) => /^layout-(80x24-tab|59x20|40x12)/.test(i))}
${group('3. All optional panels off / all on', (i) => /^layout-\d+x\d+-(none|all)$/.test(i))}
${group('4. One optional panel hidden, and each sidebar without its optional panels', (i) => /^layout-\d+x\d+-(hide-|left-optionals|right-optionals)/.test(i))}
${group('5. Rebaselined dash and frame cases', (i) => /^(dash|frame)-/.test(i))}`;
fs.writeFileSync(out, html);
console.log(`wrote ${out}: ${cases.length} cases, ${(html.length / 1024).toFixed(0)} KiB`);
