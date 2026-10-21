#!/usr/bin/env node
// Generates the docs pages from the repository's own markdown (README.md,
// docs/*.md, CONTRIBUTING.md) so the site never holds a second copy:
//
//   - pages and their sources come from docs.manifest.mjs;
//   - README.md is split by `## ` section into several pages;
//   - each file's `# ` heading becomes the page title (Starlight renders it);
//   - links to synced files point at their pages (README anchors resolve to
//     the page that holds the heading), images under docs/ are copied to
//     public/docs-assets/, and every other repository path (../bench/...,
//     ../compat/..., examples) points at GitHub;
//   - the README's Mermaid diagrams are replaced by the site's SVG versions
//     (src/diagrams/*.svg), the same ones the landing page shows.
//
// Output (git-ignored): src/content/docs/docs/*.md, public/docs-assets/.
// Runs before `astro dev` and `astro build`.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import GithubSlugger from 'github-slugger';
import { BRANCH, REPO, pages, route } from '../docs.manifest.mjs';

const web = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const root = path.resolve(web, '..');
const outDir = path.join(web, 'src/content/docs/docs');
const assetsDir = path.join(web, 'public/docs-assets');
const diagramsDir = path.join(web, 'src/diagrams');

const warnings = [];
const warn = (msg) => warnings.push(msg);

/** Lines of a markdown file, each tagged with whether it is inside a code fence. */
function scan(text) {
  let fence = null;
  return text.split('\n').map((line) => {
    const m = line.match(/^\s*(`{3,}|~{3,})(.*)$/);
    if (m) {
      if (!fence) {
        fence = m[1];
        return { line, code: true, open: m[2].trim() };
      }
      if (m[1].startsWith(fence) && !m[2].trim()) {
        fence = null;
        return { line, code: true, close: true };
      }
    }
    return { line, code: !!fence };
  });
}

/** The `#`..`######` headings outside code fences, with GitHub's anchor slugs. */
function headings(text) {
  const slugger = new GithubSlugger();
  const out = [];
  for (const l of scan(text)) {
    if (l.code) continue;
    const m = l.line.match(/^(#{1,6})\s+(.+?)\s*#*\s*$/);
    if (m) out.push({ depth: m[1].length, text: m[2], slug: slugger.slug(plain(m[2])) });
  }
  return out;
}

/** Heading or paragraph markdown as plain text. */
function plain(md) {
  return md
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/[`*_]/g, '')
    .replace(/<[^>]+>/g, '')
    .trim();
}

/** README.md split into { name, slug, body } by `## ` heading; name "" is the preamble. */
function splitReadme(text) {
  const slugger = new GithubSlugger();
  const sections = [{ name: '', slug: '', lines: [] }];
  for (const l of scan(text)) {
    const m = !l.code && l.line.match(/^##\s+(.+?)\s*$/);
    if (m) sections.push({ name: m[1], slug: slugger.slug(plain(m[1])), lines: [] });
    else sections.at(-1).lines.push(l.line);
  }
  return sections.map((s) => ({ ...s, body: s.lines.join('\n').trim() }));
}

const posix = (p) => p.split(path.sep).join('/');
const read = (rel) => fs.readFileSync(path.join(root, rel), 'utf8');

// --- 1. Page bodies -------------------------------------------------------

const readme = splitReadme(read('README.md'));
const built = pages.map((page) => {
  if (page.source === 'README.md') {
    const parts = page.sections.map((name) => {
      const s = readme.find((r) => r.name === name);
      if (!s) throw new Error(`README.md has no section "## ${name}" (docs.manifest.mjs, page ${page.slug || 'index'})`);
      return s;
    });
    const body = parts
      .map((s) => {
        if (s.name === '') return s.body.replace(/^#\s+.+\n+/, '');
        return parts.length === 1 ? s.body : `## ${s.name}\n\n${s.body}`;
      })
      .join('\n\n');
    return { page, title: page.title ?? parts[0].name, body };
  }
  const text = read(page.source);
  const h1 = text.match(/^#\s+(.+?)\s*$/m);
  if (!h1) throw new Error(`${page.source} has no "# " title`);
  return { page, title: page.title ?? plain(h1[1]), body: text.replace(h1[0], '').trim() };
});

// --- 2. Where every synced file and anchor lives on the site ----------------

/** repo path -> (anchor | "") -> site URL */
const targets = new Map();
for (const { page, body } of built) {
  const map = targets.get(page.source) ?? new Map();
  targets.set(page.source, map);
  if (!map.has('')) map.set('', route(page.slug));
  if (page.source !== 'README.md') for (const h of headings(body)) map.set(h.slug, `${route(page.slug)}#${h.slug}`);
}
// README anchors are GitHub's slugs over the whole file, in order.
{
  const owner = new Map(); // README section name -> page
  for (const { page } of built) if (page.source === 'README.md') for (const s of page.sections) owner.set(s, page);
  const map = targets.get('README.md');
  const slugger = new GithubSlugger();
  let section = '';
  for (const l of scan(read('README.md'))) {
    if (l.code) continue;
    const m = l.line.match(/^(#{1,6})\s+(.+?)\s*$/);
    if (!m) continue;
    const slug = slugger.slug(plain(m[2]));
    if (m[1] === '##') section = m[2];
    const page = owner.get(m[1] === '#' ? '' : section);
    if (!page) continue; // a section the site leaves out
    const isPageTitle = m[1] === '#' || (m[1] === '##' && page.sections.length === 1);
    map.set(slug, isPageTitle ? route(page.slug) : `${route(page.slug)}#${slug}`);
  }
}

// --- 3. Rewriting links ------------------------------------------------------

function githubUrl(repoPath, hash) {
  const abs = path.join(root, repoPath);
  let kind = 'blob';
  if (fs.existsSync(abs)) {
    if (fs.statSync(abs).isDirectory()) kind = 'tree';
  } else {
    warn(`link to missing repository path ${repoPath}`);
  }
  return `${REPO}/${kind}/${BRANCH}/${repoPath}${hash ? `#${hash}` : ''}`;
}

const copied = new Set();
function rewrite(target, source) {
  if (/^[a-z][a-z0-9+.-]*:/i.test(target)) return target; // https:, mailto:, ...
  const [p, hash = ''] = target.split('#');
  const repoPath = p ? posix(path.normalize(path.join(path.dirname(source), p))).replace(/\/+$/, '') : source;
  if (repoPath.startsWith('..')) {
    warn(`${source}: link outside the repository: ${target}`);
    return target;
  }
  const page = targets.get(repoPath);
  if (page) {
    const url = page.get(hash) ?? (hash ? null : page.get(''));
    if (url) return url;
    warn(`${source}: no heading #${hash} in ${repoPath}`);
    return `${page.get('')}#${hash}`;
  }
  if (repoPath.startsWith('docs/') && /\.(svg|png|jpe?g|gif|webp)$/i.test(repoPath)) {
    const rel = repoPath.slice('docs/'.length);
    if (!copied.has(rel)) {
      fs.mkdirSync(path.dirname(path.join(assetsDir, rel)), { recursive: true });
      fs.copyFileSync(path.join(root, repoPath), path.join(assetsDir, rel));
      copied.add(rel);
    }
    return `/docs-assets/${rel}`;
  }
  return githubUrl(repoPath, hash);
}

/** Rewrites `[x](t)`, `![x](t)` and `[x]: t` outside code fences and inline code. */
function rewriteLinks(body, source) {
  return scan(body)
    .map((l) => {
      if (l.code) return l.line;
      const ref = l.line.match(/^(\s*\[[^\]]+\]:\s*)(\S+)(.*)$/);
      if (ref) return ref[1] + rewrite(ref[2], source) + ref[3];
      // Split on inline code spans so `[a](b)` inside backticks is left alone.
      return l.line
        .split(/(`+[^`]*`+)/)
        .map((part, i) => (i % 2 ? part : part.replace(/(\]\()([^)\s]+)((?:\s+"[^"]*")?\))/g, (_, a, t, b) => a + rewrite(t, source) + b)))
        .join('');
    })
    .join('\n');
}

// --- 4. Diagrams ---------------------------------------------------------------

// A README Mermaid block -> the SVG that replaces it, picked by what it draws.
const diagramFor = [
  [/subgraph cloud/, 'contexts'],
  [/subgraph env/, 'local'],
  [/subgraph bin/, 'architecture'],
  [/WireGuard/, 'multinode'],
  [/Raft/, 'ha'],
];

function replaceMermaid(body, source) {
  const out = [];
  let block = null;
  for (const l of scan(body)) {
    if (block) {
      if (l.close) {
        const code = block.join('\n');
        const key = diagramFor.find(([re]) => re.test(code))?.[1];
        const file = key && path.join(diagramsDir, `${key}.svg`);
        if (file && fs.existsSync(file)) {
          // One HTML block: markdown ends raw HTML at the first blank line.
          const svg = fs.readFileSync(file, 'utf8').replace(/<\?xml[^>]*>\s*/, '').replace(/\n\s*\n/g, '\n').trim();
          out.push(`<figure class="lk-diagram not-content" data-diagram="${key}">${svg}</figure>`);
        } else {
          warn(`${source}: a Mermaid diagram with no SVG version, kept as code`);
          out.push('```mermaid', code, '```');
        }
        block = null;
      } else block.push(l.line);
      continue;
    }
    if (l.open === 'mermaid') block = [];
    else out.push(l.line);
  }
  return out.join('\n');
}

// --- 5. Write -----------------------------------------------------------------------

function description(body) {
  for (const para of body.split(/\n\s*\n/)) {
    const t = para.trim();
    if (!t || /^(#|```|~~~|\||<|>|[-*+]\s|\d+\.\s)/.test(t)) continue;
    const text = plain(t.replace(/\s+/g, ' '));
    return text.length > 200 ? `${text.slice(0, 197).replace(/\s+\S*$/, '')}…` : text;
  }
  return '';
}

fs.rmSync(outDir, { recursive: true, force: true });
fs.rmSync(assetsDir, { recursive: true, force: true });
fs.mkdirSync(outDir, { recursive: true });

for (const { page, title, body } of built) {
  const md = replaceMermaid(rewriteLinks(body, page.source), page.source);
  const front = [
    '---',
    `title: ${JSON.stringify(title)}`,
    `description: ${JSON.stringify(description(body))}`,
    `editUrl: ${JSON.stringify(`${REPO}/edit/${BRANCH}/${page.source}`)}`,
    '---',
    `<!-- Generated by web/scripts/sync-docs.mjs from ${page.source}; edit that file instead. -->`,
    '',
  ].join('\n');
  fs.writeFileSync(path.join(outDir, `${page.slug || 'index'}.md`), `${front}\n${md}\n`);
}

// The terminal UI screenshots, also shown on the landing page.
for (const f of fs.readdirSync(path.join(root, 'docs/ui'))) {
  if (!f.endsWith('.svg') || copied.has(`ui/${f}`)) continue;
  fs.mkdirSync(path.join(assetsDir, 'ui'), { recursive: true });
  fs.copyFileSync(path.join(root, 'docs/ui', f), path.join(assetsDir, 'ui', f));
  copied.add(`ui/${f}`);
}

// Every docs/*.md should be on the site; say so when a new one is not.
const mapped = new Set(pages.map((p) => p.source));
for (const f of fs.readdirSync(path.join(root, 'docs'))) {
  if (f.endsWith('.md') && !mapped.has(`docs/${f}`)) warn(`docs/${f} is not in web/docs.manifest.mjs`);
}

for (const w of warnings) console.warn(`sync-docs: warning: ${w}`);
console.log(`sync-docs: ${built.length} pages, ${copied.size} images -> ${posix(path.relative(web, outDir))}`);
