// llms.txt (https://llmstxt.org) and llms-full.txt, built from the same
// manifest as the docs sidebar: every page, its description, and a link to
// its markdown source, so agents find the docs without crawling HTML.
import fs from 'node:fs';
import path from 'node:path';
import { REPO, BRANCH, groups, route } from '../../docs.manifest.mjs';

// `npm run build` runs in web/; the docs live in the repository around it.
const root = path.resolve(process.cwd(), '..');
const raw = (source: string) => `${REPO.replace('github.com', 'raw.githubusercontent.com')}/${BRANCH}/${source}`;

const SUMMARY = `> A whole Kubernetes in one Rust binary: a local cluster in about a second,
> spare machines as one cluster, a control plane that idles at 23 MiB. It
> serves the real Kubernetes API (kubectl, Helm, Argo CD work unchanged), plus
> a Docker Engine API, built-in metrics/logs/alerts, a terminal UI and an MCP
> server (\`litek8s mcp\`).

Agents: prefer \`litek8s mcp\` (tools for dev clusters, get/apply/logs/PromQL,
diagnose_pod) and its resources \`litek8s://skill/...\` and \`litek8s://docs/...\`;
\`litek8s skill\` prints the same guide. Servers authenticate clients and
enforce RBAC after 0.2.11: a 401 or 403 on a cluster made with an earlier
release is fixed as in "Authentication and RBAC" below.`;

export function llmsTxt(site: URL): string {
  const out = ['# litek8s', '', SUMMARY, ''];
  for (const g of groups) {
    out.push(`## ${g.label}`, '');
    for (const p of g.pages) {
      const desc = p.description ?? p.seoTitle ?? p.title ?? '';
      out.push(`- [${p.label}](${raw(p.source)}): ${desc} (page: ${new URL(route(p.slug), site).href})`.replace(/: \s*\(/, ' ('));
    }
    out.push('');
  }
  out.push('## Optional', '', `- [Changelog](${raw('CHANGELOG.md')}): what changed in each release, breaking changes first`, `- [Agent skill](${raw('skills/litek8s/SKILL.md')}): the guide \`litek8s mcp\` serves to agents`, '');
  return out.join('\n');
}

export function llmsFullTxt(): string {
  const out = ['# litek8s', '', SUMMARY, ''];
  for (const g of groups) {
    for (const p of g.pages) {
      const file = path.join(root, p.source);
      if (!fs.existsSync(file)) continue;
      out.push(`\n---\n\n<!-- ${p.source} -->\n`, fs.readFileSync(file, 'utf8').trim(), '');
    }
  }
  return out.join('\n');
}
