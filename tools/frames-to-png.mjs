#!/usr/bin/env node
// Visual loop of the new TUI: turns raw-SGR frame files (`<theme>-<screen>-<cols>x<rows>.ansi`, written by
// `SYNTHESIS_FRAMES_DIR=<dir> cargo test -p synthesis-tui-render --test golden write_visual_frames`)
// into PNGs with headless Chrome. Chrome draws them with the generic `monospace` stack, so this is an
// APPROXIMATION of a terminal (cell aspect and glyph shapes differ; the prototype's fonts are not shipped).
// Usage: node tools/frames-to-png.mjs <frames-dir> <out-dir> [--pair view1.png=mission view2.png=chat --proto <dir>]
//   with --proto, every mission/chat frame is also put next to the owner's screenshot.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

const [dir, out] = process.argv.slice(2, 4);
const arg = (k) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : null; };
const proto = arg('--proto');
const only = arg('--only');
const chrome = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const FONT = 13, LH = 1.22;
const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

function rowToHtml(row) {
  let fg = null, bg = null, bold = false, dim = false, ital = false, ul = false, rev = false, html = '';
  const style = () => {
    let f = fg, b = bg;
    if (rev) [f, b] = [b ?? '#0b0c0f', f ?? '#c4cad6'];
    return [f && `color:${f}`, b && `background:${b}`, bold && 'font-weight:700', dim && 'opacity:.6', ital && 'font-style:italic', ul && 'text-decoration:underline'].filter(Boolean).join(';');
  };
  const re = /\x1b\[([0-9;]*)m|([^\x1b]+)/g;
  let m;
  while ((m = re.exec(row))) {
    if (m[2] !== undefined) { html += `<span style="${style()}">${esc(m[2])}</span>`; continue; }
    const p = m[1] === '' ? [0] : m[1].split(';').map(Number);
    for (let i = 0; i < p.length; i++) {
      const n = p[i];
      if (n === 0) { fg = bg = null; bold = dim = ital = ul = rev = false; }
      else if (n === 1) bold = true; else if (n === 2) dim = true; else if (n === 3) ital = true;
      else if (n === 4) ul = true; else if (n === 7) rev = true; else if (n === 22) bold = dim = false;
      else if (n === 24) ul = false; else if (n === 27) rev = false;
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

fs.mkdirSync(out, { recursive: true });
const shot = (html, png, w, h) => {
  const f = png.replace(/\.png$/, '.html');
  fs.writeFileSync(f, html);
  execFileSync(chrome, ['--headless=new', '--disable-gpu', '--hide-scrollbars', `--window-size=${w},${h}`, `--screenshot=${png}`, `file://${path.resolve(f)}`], { stdio: 'ignore' });
  fs.unlinkSync(f);
};
for (const f of fs.readdirSync(dir).filter((n) => n.endsWith('.ansi')).sort()) {
  if (only && !f.includes(only)) continue;
  const [theme, screen, size] = f.replace('.ansi', '').split('-');
  const [cols, rows] = size.split('x').map(Number);
  const rowsText = fs.readFileSync(path.join(dir, f), 'utf8').replace(/\n$/, '').split('\n');
  // the page colour stands for the terminal's own background: the system theme paints none
  const page = theme === 'system' ? '#16181d' : '#0b0c0f';
  const css = `body{margin:0;background:${page}}span{display:inline-block;height:${LH}em;vertical-align:top;white-space:pre}pre{margin:0;padding:8px;color:#c4cad6;font:${FONT}px/${LH} monospace;font-variant-ligatures:none;white-space:pre}`;
  const html = `<!doctype html><meta charset="utf-8"><style>${css}</style><pre>${rowsText.map(rowToHtml).join('\n')}</pre>`;
  const w = Math.ceil(cols * FONT * 0.602) + 16, h = Math.ceil(rows * FONT * LH) + 16;
  const png = path.join(out, f.replace('.ansi', '.png'));
  shot(html, png, w, h);
  if (proto && theme === 'synthesis') {
    const ref = { mission: 'view1.png', chat: 'view2.png' }[screen];
    const b64 = (p) => 'data:image/png;base64,' + fs.readFileSync(p).toString('base64');
    const pair = `<!doctype html><meta charset="utf-8"><style>body{margin:0;background:#000;display:flex;gap:10px;align-items:flex-start}div{color:#888;font:11px sans-serif}img{display:block;height:${Math.max(h, 520)}px}</style>
<div>prototype (owner)<img src="${b64(path.join(proto, ref))}"></div><div>Synthesis TUI ${cols}x${rows} (Chrome, generic monospace)<img src="${b64(png)}"></div>`;
    shot(pair, path.join(out, `side-${screen}-${cols}x${rows}.png`), Math.ceil(Math.max(h, 520) * 1672 / 1041) + w * Math.max(h, 520) / h + 30, Math.max(h, 520) + 30);
  }
}
