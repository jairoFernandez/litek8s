#!/usr/bin/env node
// Checks every internal link and #anchor in dist/ (run after `npm run build`):
// the page must exist and, with a #fragment, have an element with that id.
// External links are listed by host, not fetched.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../dist');
const pages = new Map(); // site path -> html

(function walk(dir) {
  for (const f of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, f.name);
    if (f.isDirectory()) walk(p);
    else if (f.name.endsWith('.html')) {
      const rel = '/' + path.relative(dist, p).split(path.sep).join('/');
      pages.set(rel.replace(/index\.html$/, ''), fs.readFileSync(p, 'utf8'));
    }
  }
})(dist);

const ids = new Map();
const idsOf = (page) => {
  if (!ids.has(page)) ids.set(page, new Set([...pages.get(page).matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])));
  return ids.get(page);
};

let bad = 0;
const external = new Map();
for (const [from, html] of pages) {
  if (from.startsWith('/_astro') || from.startsWith('/pagefind')) continue;
  for (const [, raw] of html.matchAll(/<a\s[^>]*href="([^"]+)"/g)) {
    const href = raw.replace(/&amp;/g, '&');
    if (/^(https?:)?\/\//.test(href)) {
      const host = new URL(href, 'https://x').host;
      external.set(host, (external.get(host) ?? 0) + 1);
      continue;
    }
    if (/^(mailto|tel):/.test(href)) continue;
    const url = new URL(href, `https://site${from}`);
    let target = decodeURIComponent(url.pathname);
    if (!pages.has(target)) {
      const file = path.join(dist, target);
      if (!fs.existsSync(file)) {
        console.error(`broken: ${from} -> ${href}`);
        bad++;
        continue;
      }
      target = null; // an asset, no anchors to check
    }
    const frag = decodeURIComponent(url.hash.slice(1));
    if (frag && target !== null && frag !== '_top' && !idsOf(target).has(frag)) {
      console.error(`missing anchor: ${from} -> ${href}`);
      bad++;
    }
  }
}
console.log(`check-links: ${pages.size} pages, ${bad} problems; external hosts: ${[...external].map(([h, n]) => `${h} (${n})`).join(', ')}`);
process.exit(bad ? 1 : 0);
