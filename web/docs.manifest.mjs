// Which repository markdown becomes which docs page. Read by
// scripts/sync-docs.mjs (to generate src/content/docs/) and by
// astro.config.mjs (for the sidebar), so the two never disagree.
//
// `source` is a path relative to the repository root. README.md is the
// project's landing page and is not synced; a page can still take some of
// its sections with `source: 'README.md', sections: ['## heading', ...]`
// ("" is the text before the first heading).

export const REPO = 'https://github.com/jairoFernandez/litek8s';
export const BRANCH = 'main';

export const groups = [
  {
    label: 'Start here',
    pages: [
      { slug: '', label: 'Introduction', title: 'Introduction', source: 'docs/overview.md' },
      { slug: 'install', label: 'Install', source: 'docs/install.md' },
      { slug: 'quickstart', label: 'Quickstart', source: 'docs/quickstart.md' },
      { slug: 'numbers', label: 'Numbers', source: 'docs/numbers.md' },
      { slug: 'platforms', label: 'Platforms', source: 'docs/platforms.md' },
      { slug: 'multi-node', label: 'Multi-node and HA', source: 'docs/multi-node.md' },
      { slug: 'limitations', label: 'Limitations', source: 'docs/limitations.md' },
    ],
  },
  {
    label: 'Guides',
    pages: [
      { slug: 'dev', label: 'Dev clusters', source: 'docs/dev.md' },
      { slug: 'docker', label: 'Docker API', source: 'docs/docker.md' },
      { slug: 'ci', label: 'CI (GitHub Actions)', source: 'docs/ci.md' },
      { slug: 'ui', label: 'Terminal UI', source: 'docs/ui.md' },
      { slug: 'observability', label: 'Metrics, logs, alerts', source: 'docs/observability.md' },
      { slug: 'clusters', label: 'Clusters over SSH', source: 'docs/clusters.md' },
      { slug: 'registries', label: 'Private registries and mirrors', source: 'docs/registries.md' },
      { slug: 'networking', label: 'Networking across NATs', source: 'docs/networking.md' },
      { slug: 'ha', label: 'HA control plane', source: 'docs/ha.md' },
      { slug: 'crds', label: 'CRDs and server-side apply', source: 'docs/crds.md' },
      { slug: 'agents', label: 'MCP and LLM agents', source: 'docs/agents.md' },
      { slug: 'llm-cluster', label: 'One LLM across machines', source: 'docs/llm-cluster.md' },
    ],
  },
  {
    label: 'Contributing',
    pages: [
      { slug: 'contributing', label: 'Building and testing', source: 'CONTRIBUTING.md' },
      { slug: 'develop-without-docker', label: 'Developing without Docker', source: 'docs/develop-without-docker.md' },
    ],
  },
];

export const pages = groups.flatMap((g) => g.pages);

/** The site path of a docs page. */
export const route = (slug) => (slug ? `/docs/${slug}/` : '/docs/');
