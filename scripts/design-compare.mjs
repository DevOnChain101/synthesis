#!/usr/bin/env node
// Side-by-side comparison of the design (cli.html rendered by headless Chrome) with the desktop app's offscreen
// shot, for the five sizes of docs/specs/desktop.md "Design acceptance": [design | app | difference] in one PNG.
//   node scripts/design-compare.mjs <design-dir> <shots-dir> <out-dir>
// <design-dir> holds ref-WxH.png (1x), <shots-dir> tui-WxH.png (`synthesis-desktop --shot ... --size WxH`, 2x),
// both produced as in docs/specs/desktop.md. Needs Google Chrome (headless, remote debugging on 127.0.0.1 only).
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';

const [ref, shots, out] = process.argv.slice(2).map((p) => path.resolve(p));
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const SIZES = ['900x520', '1000x700', '1200x800', '1400x900', '1920x1080'];
fs.mkdirSync(out, { recursive: true });
const port = 9700 + Math.floor(Math.random() * 200);
const prof = fs.mkdtempSync(path.join(os.tmpdir(), 'cmp-'));
const chrome = spawn(CHROME, ['--headless=new', '--disable-gpu', '--hide-scrollbars', `--remote-debugging-port=${port}`, '--remote-debugging-address=127.0.0.1', `--user-data-dir=${prof}`, 'about:blank'], { stdio: 'ignore' });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let tabs;
for (let i = 0; i < 50; i++) { try { tabs = await (await fetch(`http://127.0.0.1:${port}/json`)).json(); if (tabs.length) break; } catch {} await sleep(200); }
const ws = new WebSocket(tabs.find((t) => t.type === 'page').webSocketDebuggerUrl);
await new Promise((r) => (ws.onopen = r));
let id = 0; const pend = new Map();
ws.onmessage = (m) => { const d = JSON.parse(m.data); if (d.id && pend.has(d.id)) { pend.get(d.id)(d); pend.delete(d.id); } };
const send = (method, params = {}) => new Promise((r) => { const i = ++id; pend.set(i, r); ws.send(JSON.stringify({ id: i, method, params })); });
for (const s of SIZES) {
  const [w, h] = s.split('x').map(Number);
  const a = `file://${ref}/ref-${s}.png`, b = `file://${shots}/tui-${s}.png`;
  const html = `<body style="margin:0;background:#222"><div style="display:flex;gap:8px">
    <img src="${a}" width="${w}" height="${h}"><img src="${b}" width="${w}" height="${h}">
    <div style="position:relative;width:${w}px;height:${h}px;background:#000"><img src="${a}" width="${w}" height="${h}" style="position:absolute;left:0;top:0">
    <img src="${b}" width="${w}" height="${h}" style="position:absolute;left:0;top:0;mix-blend-mode:difference"></div></div>`;
  const file = path.join(prof, `${s}.html`); fs.writeFileSync(file, html);
  await send('Emulation.setDeviceMetricsOverride', { width: 3 * w + 16, height: h, deviceScaleFactor: 1, mobile: false });
  await send('Page.navigate', { url: `file://${file}` });
  await sleep(1500);
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(path.join(out, `compare-${s}.png`), Buffer.from(shot.result.data, 'base64'));
  console.log('wrote', path.join(out, `compare-${s}.png`));
}
chrome.kill();
await sleep(500);
fs.rmSync(prof, { recursive: true, force: true, maxRetries: 3 });
process.exit(0);
