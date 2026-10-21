#!/usr/bin/env node
// Writes docs/readme/demo.svg: the README's animated terminal
// (litek8s dev up -> kubectl get nodes -> litek8s ui, the last one the real
// TUI frame from docs/ui/home-dark.svg). Pure CSS keyframes inside the SVG,
// so it animates where READMEs are shown as <img> (GitHub, crates.io), with
// no script and no external resources. Re-run after changing it:
//   node scripts/readme-demo.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const out = path.join(root, 'docs/readme/demo.svg');

const W = 880, H = 440, BAR = 38, CYCLE = 13; // seconds per loop
const X0 = 24, Y0 = 74, LH = 27, CW = 9; // text origin, line height, char width
const BG = '#1e1e2e', FG = '#e4e4ec', DIM = '#8b90a5', OK = '#4ade80', ACC = '#a78bfa';
const OFF = 12.4; // everything fades out here, then the loop starts again

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
const pct = (t) => `${((t / CYCLE) * 100).toFixed(3)}%`;
let css = '';
let body = '';
let n = 0;

/** A class visible from `on` to `off` seconds. */
function show(on, off = OFF) {
  const k = `v${n++}`;
  const fade = off === OFF ? 0.3 : 0.01;
  css += `@keyframes ${k}{0%,${pct(on - 0.01)}{opacity:0}${pct(on)},${pct(off)}{opacity:1}${pct(off + fade)},100%{opacity:0}}.${k}{opacity:0;animation:${k} ${CYCLE}s infinite}`;
  return k;
}

/** A text run at column `col` of `line`. */
const run = (line, col, text, fill, extra = '') =>
  `<text x="${X0 + col * CW}" y="${Y0 + line * LH}" fill="${fill}" textLength="${text.length * CW}" lengthAdjust="spacingAndGlyphs" xml:space="preserve"${extra}>${esc(text)}</text>`;

/** A command typed from `start`, one char per `per` seconds; caret until `caretOff`. */
function command(line, text, start, per, caretOff) {
  const vis = show(start - 0.35);
  const end = start + text.length * per;
  const k = `t${n++}`;
  const w = text.length * CW;
  css += `@keyframes ${k}{0%,${pct(start)}{transform:translateX(0);animation-timing-function:steps(${text.length},end)}${pct(end)},100%{transform:translateX(${w}px)}}.${k}{animation:${k} ${CYCLE}s infinite}`;
  const c = `c${n++}`;
  css += `@keyframes ${c}{0%,${pct(caretOff - 0.01)}{opacity:1}${pct(caretOff)},100%{opacity:0}}.${c}{animation:${c} ${CYCLE}s infinite}`;
  const x = X0 + 2 * CW;
  body += `<g class="${vis}">${run(line, 0, '$', ACC)}${run(line, 2, text, FG)}
<g class="${k}"><rect x="${x}" y="${Y0 + line * LH - 19}" width="${w + 12}" height="26" fill="${BG}"/><rect class="${c}" x="${x}" y="${Y0 + line * LH - 16}" width="9" height="20" fill="${ACC}"/></g></g>\n`;
  return end;
}

/** Output runs [[col, text, fill], ...] on `line`, from `on`. */
function output(line, on, runs) {
  body += `<g class="${show(on)}">${runs.map(([col, text, fill]) => run(line, col, text, fill)).join('')}</g>\n`;
}

// the script
let t = 0.6;
t = command(0, 'litek8s dev up', t, 0.06, 2.65);
// dev up waits for the node to be Ready: 1.28 s on macOS (bench/RESULTS.md)
const ready = t + 1.28;
const waiting = show(t + 0.05, ready);
const done = show(ready);
output(1, ready, [[0, 'litek8s-dev ready in 1.28s', OK], [27, '(vm, http://127.0.0.1:6443, kubectl context litek8s-dev)', DIM]]);
t = command(2, 'kubectl get nodes', ready + 0.7, 0.05, ready + 2.05);
output(3, t + 0.3, [[0, 'NAME          STATUS   ROLES           AGE   VERSION', DIM]]);
output(4, t + 0.3, [[0, 'litek8s-dev', FG], [14, 'Ready', OK], [23, 'control-plane   3s    v1.33.0+litek8s', FG]]);
t = command(5, 'litek8s ui', t + 1.1, 0.06, OFF);
const tuiOn = t + 0.45;

// The real TUI frame, scaled into the terminal body.
const tui = fs.readFileSync(path.join(root, 'docs/ui/home-dark.svg'), 'utf8')
  .replace(/<\?xml[^>]*>\s*/, '')
  .replace(/^<svg\b[^>]*>/, '')
  .replace(/<\/svg>\s*$/, '');
const th = H - BAR - 14, tw = Math.round((th * 1028) / 700);
const tk = `u${n++}`;
css += `@keyframes ${tk}{0%,${pct(tuiOn)}{opacity:0;transform:translateY(10px) scale(.985)}${pct(tuiOn + 0.5)},${pct(OFF)}{opacity:1;transform:none}${pct(OFF + 0.3)},100%{opacity:0}}.${tk}{opacity:0;transform-origin:center;animation:${tk} ${CYCLE}s infinite}`;
const textOff = `h${n++}`;
css += `@keyframes ${textOff}{0%,${pct(tuiOn)}{opacity:1}${pct(tuiOn + 0.3)},100%{opacity:0}}.${textOff}{animation:${textOff} ${CYCLE}s infinite}`;

const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${W} ${H}" width="${W}" height="${H}" role="img" aria-label="litek8s dev up starts a cluster in 1.28 seconds, kubectl get nodes shows the node Ready, litek8s ui opens the terminal UI">
<style>
text{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'DejaVu Sans Mono',monospace;font-size:15px}
${css}
@media (prefers-reduced-motion:reduce){*{animation:none!important}}
</style>
<rect width="${W}" height="${H}" rx="14" fill="${BG}"/>
<rect x=".5" y=".5" width="${W - 1}" height="${H - 1}" rx="13.5" fill="none" stroke="#ffffff" stroke-opacity=".08"/>
<circle cx="22" cy="19" r="6" fill="#ff5f57"/><circle cx="42" cy="19" r="6" fill="#febc2e"/><circle cx="62" cy="19" r="6" fill="#28c840"/>
<text x="${W / 2}" y="24" fill="${DIM}" text-anchor="middle" style="font-size:13px">litek8s</text>
<text class="${waiting}" x="${W - 24}" y="24" fill="${DIM}" text-anchor="end" style="font-size:13px">waiting for the node…</text>
<text class="${done}" x="${W - 24}" y="24" fill="${OK}" text-anchor="end" style="font-size:13px">node Ready in 1.28 s</text>
<line x1="0" y1="${BAR}" x2="${W}" y2="${BAR}" stroke="#ffffff" stroke-opacity=".07"/>
<g class="${textOff}">
${body}</g>
<g class="${tk}"><svg x="${(W - tw) / 2}" y="${BAR + 6}" width="${tw}" height="${th}" viewBox="0 0 1028 700">${tui}</svg></g>
</svg>
`;
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, svg);
console.log(`readme-demo: ${path.relative(root, out)} (${(svg.length / 1024).toFixed(1)} KiB, ${CYCLE} s loop)`);
