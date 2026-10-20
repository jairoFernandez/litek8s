#!/usr/bin/env node
// Renders the raster brand assets from src/assets/logo.svg: the Apple touch
// icon, a 32 px PNG favicon, a 512 px mark and the social card (og.png).
// The PNGs are committed; re-run after changing the logo:
//   node scripts/brand.mjs
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import sharp from 'sharp';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const logo = fs.readFileSync(path.join(web, 'src/assets/logo.svg'), 'utf8');
const pub = (f) => path.join(web, 'public', f);
fs.mkdirSync(pub('brand'), { recursive: true });

const mark = (size) => sharp(Buffer.from(logo), { density: (72 * size) / 64 * 2 }).resize(size, size).png();

await mark(32).toFile(pub('favicon-32.png'));
await mark(512).toFile(pub('brand/litek8s-mark-512.png'));
fs.copyFileSync(path.join(web, 'src/assets/logo.svg'), pub('brand/litek8s-mark.svg'));

// Apple touch icon: the mark on the site's dark background, iOS rounds it.
await sharp({ create: { width: 180, height: 180, channels: 4, background: '#0c0b12' } })
  .composite([{ input: await mark(132).toBuffer(), left: 24, top: 24 }])
  .png()
  .toFile(pub('apple-touch-icon.png'));

// Social card, 1200x630. Text is plain SVG text in a system font (librsvg
// has no access to the web fonts).
const inner = logo.replace(/^<svg[^>]*>/, '').replace(/<\/svg>\s*$/, '');
const og = `<svg xmlns="http://www.w3.org/2000/svg" width="1200" height="630" viewBox="0 0 1200 630">
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
  <g fill="none" transform="translate(88 92) scale(1.75)">${inner}</g>
  <text x="222" y="170" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="68" font-weight="700" fill="#f4f3f8" letter-spacing="-2">litek8s</text>
  <text x="88" y="318" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="64" font-weight="700" fill="#f4f3f8" letter-spacing="-2">A lightweight Kubernetes</text>
  <text x="88" y="394" font-family="Helvetica Neue, Helvetica, Arial, sans-serif" font-size="64" font-weight="700" fill="#b9a4ff" letter-spacing="-2">in one binary.</text>
  <rect x="88" y="462" width="1024" height="76" rx="14" fill="#1e1e2e" stroke="#ffffff" stroke-opacity="0.1"/>
  <text x="118" y="510" font-family="Menlo, Monaco, monospace" font-size="26" fill="#8b90a5">$ <tspan fill="#f4f3f8">litek8s dev up</tspan></text>
  <text x="420" y="510" font-family="Menlo, Monaco, monospace" font-size="26" fill="#4ade80">litek8s-dev ready in 1.28s <tspan fill="#8b90a5">· macOS</tspan></text>
</svg>`;
await sharp(Buffer.from(og)).png().toFile(pub('og.png'));
console.log('brand: favicon-32.png, apple-touch-icon.png, brand/litek8s-mark{.svg,-512.png}, og.png');
