// Which repository markdown becomes which docs page. Read by
// scripts/sync-docs.mjs (to generate src/content/docs/) and by
// astro.config.mjs (for the sidebar), so the two never disagree.
//
// `source` is a path relative to the repository root. For README.md,
// `sections` lists the `## ` headings that go on the page ("" is the text
// before the first heading); every other README section is left out.

export const REPO = 'https://github.com/jairoFernandez/litek8s';
export const BRANCH = 'main';

export const groups = [
  {
    label: 'Start here',
    pages: [
      { slug: '', label: 'Introduction', title: 'Introduction', source: 'README.md', sections: ['', "What's in the binary", 'How it works', 'The legacy daemon', 'License'] },
      { slug: 'install', label: 'Install', source: 'README.md', sections: ['Install'] },
      { slug: 'quickstart', label: 'Quickstart', source: 'README.md', sections: ['Quickstart'] },
      { slug: 'numbers', label: 'Numbers', source: 'README.md', sections: ['Numbers'] },
      { slug: 'platforms', label: 'Platforms', source: 'README.md', sections: ['Platforms'] },
      { slug: 'multi-node', label: 'Multi-node and HA', source: 'README.md', sections: ['Multi-node and HA'] },
      { slug: 'limitations', label: 'Limitations', source: 'README.md', sections: ['Limitations'] },
    ],
  },
  {
    label: 'Guides',
    pages: [
      { slug: 'dev', label: 'Dev clusters', source: 'docs/dev.md' },
      { slug: 'docker', label: 'Docker API', source: 'docs/docker.md' },
      { slug: 'ui', label: 'Terminal UI', source: 'docs/ui.md' },
      { slug: 'observability', label: 'Metrics, logs, alerts', source: 'docs/observability.md' },
      { slug: 'networking', label: 'Networking across NATs', source: 'docs/networking.md' },
      { slug: 'ha', label: 'HA control plane', source: 'docs/ha.md' },
      { slug: 'crds', label: 'CRDs and server-side apply', source: 'docs/crds.md' },
      { slug: 'agents', label: 'MCP and LLM agents', source: 'docs/agents.md' },
      { slug: 'llm-cluster', label: 'One LLM across machines', source: 'docs/llm-cluster.md' },
      { slug: 'llm', label: 'Built-in LLM experiment', source: 'docs/llm.md', badge: 'experimental' },
    ],
  },
  {
    label: 'Contributing',
    pages: [
      { slug: 'contributing', label: 'Building and testing', source: 'CONTRIBUTING.md' },
      { slug: 'develop-without-docker', label: 'Developing without Docker', source: 'docs/develop-without-docker.md' },
    ],
  },
  {
    label: 'Legacy daemon',
    pages: [
      { slug: 'legacy-daemon', label: 'The legacy daemon', source: 'docs/legacy-daemon.md' },
      { slug: 'legacy-daemon-demo', label: 'Demo guide', source: 'docs/legacy-daemon-demo.md' },
    ],
  },
];

export const pages = groups.flatMap((g) => g.pages);

/** The site path of a docs page. */
export const route = (slug) => (slug ? `/docs/${slug}/` : '/docs/');
