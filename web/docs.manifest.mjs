// Which repository markdown becomes which docs page. Read by
// scripts/sync-docs.mjs (to generate src/content/docs/) and by
// astro.config.mjs (for the sidebar), so the two never disagree.
//
// `source` is a path relative to the repository root. README.md is the
// project's landing page and is not synced; a page can still take some of
// its sections with `source: 'README.md', sections: ['## heading', ...]`
// ("" is the text before the first heading).
//
// For search results and link previews, a page can set `seoTitle` (the
// <title> and og:title, in place of the page's short title) and
// `description` (in place of its first paragraph). Keep titles under ~60
// characters with " | litek8s" added, and descriptions under ~160.

export const REPO = 'https://github.com/jairoFernandez/litek8s';
export const BRANCH = 'main';

export const groups = [
  {
    label: 'Start here',
    pages: [
      { slug: '', label: 'Introduction', title: 'Introduction', seoTitle: 'litek8s: a lightweight Kubernetes in one binary', source: 'docs/overview.md' },
      { slug: 'alongside', label: 'Next to your clusters', source: 'docs/alongside.md' },
      {
        slug: 'install', label: 'Install', seoTitle: 'Install litek8s on macOS and Linux', source: 'docs/install.md',
        description: 'Install litek8s on macOS (Apple silicon) or Linux (x86_64, aarch64) with Homebrew or the install script, then start a local Kubernetes cluster.',
      },
      {
        slug: 'quickstart', label: 'Quickstart', seoTitle: 'Quickstart: a local Kubernetes cluster in a second', source: 'docs/quickstart.md',
        description: 'Start a local Kubernetes cluster with litek8s dev up, deploy nginx with kubectl, use the same cluster as a Docker engine, and add multi-node clusters.',
      },
      {
        slug: 'numbers', label: 'Numbers', seoTitle: 'Benchmarks: litek8s vs k3s, kind and k3d', source: 'docs/numbers.md',
        description: 'Memory, startup time and CI numbers for litek8s against k3s, kind and k3d, measured with scripts in the repo: a 23 MiB control plane, 0.2 s dev clusters.',
      },
      {
        slug: 'platforms', label: 'Platforms', seoTitle: 'Supported platforms: macOS, Linux, Raspberry Pi', source: 'docs/platforms.md',
        description: 'Where litek8s runs: dev clusters on macOS (Apple silicon) and rootless Linux, servers and nodes on Linux x86_64 and aarch64 with crun.',
      },
      { slug: 'multi-node', label: 'Multi-node and HA', seoTitle: 'Multi-node Kubernetes and HA with litek8s', source: 'docs/multi-node.md' },
      {
        slug: 'limitations', label: 'Limitations', seoTitle: 'Limitations of litek8s', source: 'docs/limitations.md',
        description: 'What litek8s does not do yet: no client authentication or RBAC enforcement, young on-disk formats, kubelet behaviors and admission features still missing.',
      },
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
