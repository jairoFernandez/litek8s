#!/usr/bin/env node
// Renders a social card (Open Graph / Twitter image, 1200x630) for every
// blog post: public/og/blog/<post id>.png, e.g. og/blog/es/el-nodo-demasiado-lleno.png.
// The PNGs are committed, as scripts/brand.mjs's are (librsvg has no
// access to the web fonts, so the text is in a system font and the result
// depends on the machine). Run it after adding or retitling a post:
//   node scripts/og-blog.mjs            only posts without a card
//   node scripts/og-blog.mjs --all      every post again
// src/routeData.ts and the blog's PostPage use the card when it exists,
// else public/og.png.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import sharp from 'sharp';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const blogDir = path.join(web, 'src/content/blog');
const outDir = path.join(web, 'public/og/blog');
const all = process.argv.includes('--all');
const logo = fs.readFileSync(path.join(web, 'src/assets/logo.svg'), 'utf8');
const inner = logo.replace(/^<svg[^>]*>/, '').replace(/<\/svg>\s*$/, '');

const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** Front matter fields we need, without a YAML dependency. */
function frontMatter(text) {
  const m = text.match(/^---\n([\s\S]*?)\n---/);
  if (!m) return {};
  const get = (k) => {
    const v = m[1].match(new RegExp(`^${k}:\\s*(.*)$`, 'm'))?.[1]?.trim();
    if (!v) return undefined;
    if (v.startsWith('"')) return JSON.parse(v);
    if (v.startsWith("'")) return v.slice(1, -1).replace(/''/g, "'");
    return v;
  };
  const tags = (get('tags') ?? '').replace(/^\[|\]$/g, '').split(',').map((t) => t.trim()).filter(Boolean);
  return { title: get('title'), lang: get('lang') ?? 'en', tags, draft: get('draft') === 'true' };
}

/** Greedy word wrap by an average glyph width (bold Helvetica ~0.5 em). */
function wrap(text, size, width) {
  const max = Math.floor(width / (size * 0.5));
  const lines = [];
  let line = '';
  for (const w of text.split(/\s+/)) {
    if (line && (line + ' ' + w).length > max) { lines.push(line); line = w; } else line = line ? `${line} ${w}` : w;
  }
  if (line) lines.push(line);
  return lines;
}

/** The title at the largest size that fits in 4 lines. */
function fitTitle(title) {
  for (const size of [68, 62, 56, 50, 46]) {
    const lines = wrap(title, size, 1024);
    if (lines.length <= 4) return { size, lines };
  }
  const lines = wrap(title, 46, 1024).slice(0, 4);
  lines[3] = lines[3].replace(/\s*\S*$/, '') + '…';
  return { size: 46, lines };
}

function card({ title, lang, tags }) {
  // A title "A: B" gets its part after the colon in the accent color.
  const { size, lines } = fitTitle(title);
  const lh = Math.round(size * 1.12);
  const top = 250 + Math.round((4 - lines.length) * lh * 0.35);
  const label = lang === 'es' ? 'Blog' : 'Blog';
  let accent = false;
  const titleSvg = lines.map((l, i) => {
    let out = '';
    const c = l.indexOf(':');
    if (!accent && c >= 0) {
      out = `<tspan fill="#f4f3f8">${esc(l.slice(0, c + 1))}</tspan><tspan fill="#b9a4ff">${esc(l.slice(c + 1))}</tspan>`;
      accent = true;
    } else out = `<tspan fill="${accent ? '#b9a4ff' : '#f4f3f8'}">${esc(l)}</tspan>`;
    return `<text x="88" y="${top + i * lh}" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="${size}" font-weight="700" letter-spacing="-1.5" xml:space="preserve">${out}</text>`;
  }).join('\n  ');
  const tagLine = tags.slice(0, 4).map((t) => `#${t}`).join("  ");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630">
  <defs>
    <radialGradient id="glow" cx="0" cy="0" r="1" gradientUnits="userSpaceOnUse" gradientTransform="matrix(620 0 0 420 980 80)">
      <stop stop-color="#7c3aed" stop-opacity="0.45"/><stop offset="1" stop-color="#7c3aed" stop-opacity="0"/>
    </radialGradient>
    <pattern id="grid" width="48" height="48" patternUnits="userSpaceOnUse">
      <path d="M48 0H0V48" fill="none" stroke="#ffffff" stroke-opacity="0.05"/>
    </pattern>
  </defs>
  <rect width="1200" height="630" fill="#0c0b12"/>
  <rect width="1200" height="630" fill="url(#grid)"/>
  <rect width="1200" height="630" fill="url(#glow)"/>
  <g fill="none" transform="translate(88 64) scale(1.0)">${inner}</g>
  <text x="168" y="112" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="40" font-weight="700" fill="#f4f3f8" letter-spacing="-1">litek8s</text>
  <text x="318" y="112" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="40" fill="#8b90a5">· ${label}</text>
  ${titleSvg}
  <text x="88" y="566" font-family="Menlo, Monaco, monospace" font-size="24" fill="#a78bfa" xml:space="preserve">${esc(tagLine)}</text>
  <text x="1112" y="566" text-anchor="end" font-family="Menlo, Monaco, monospace" font-size="24" fill="#8b90a5">litek8s.dev</text>
</svg>`;
}

const files = [];
(function walk(dir) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p);
    else if (e.name.endsWith('.md')) files.push(p);
  }
})(blogDir);

let made = 0;
for (const f of files.sort()) {
  const id = path.relative(blogDir, f).replace(/\\/g, '/').replace(/\.md$/, '');
  const out = path.join(outDir, `${id}.png`);
  if (!all && fs.existsSync(out)) continue;
  const fm = frontMatter(fs.readFileSync(f, 'utf8'));
  if (!fm.title || fm.draft) continue;
  fs.mkdirSync(path.dirname(out), { recursive: true });
  await sharp(Buffer.from(card(fm))).png({ compressionLevel: 9 }).toFile(out);
  made += 1;
  console.log(`og-blog: ${path.relative(web, out)}`);
}
console.log(`og-blog: ${made} card(s) written, ${files.length} posts`);
