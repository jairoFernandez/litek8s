// Which repository markdown becomes which docs page. Read by
// scripts/sync-docs.mjs (to generate src/content/docs/) and by
// astro.config.mjs (for the sidebar), so the two never disagree.
//
// `source` is a path relative to the repository root. README.md is the
// project's landing page and is not synced; a page can still take some of
// its sections with `source: 'README.md', sections: ['## heading', ...]`
// ("" is the text before the first heading).
//
// Translations: a page can set `translations: { es: { source, label, title?,
// seoTitle?, description? } }` with a translated source under docs/es/; it is
// served at /es/docs/<slug>/. Pages without one fall back to English there
// (Starlight shows a notice). Groups can set `translations: { es: 'label' }`.
//
// For search results and link previews, a page can set `seoTitle` (the
// <title> and og:title, in place of the page's short title) and
// `description` (in place of its first paragraph). Keep titles under ~60
// characters with " | litek8s" added, and descriptions under ~160.

export const REPO = 'https://github.com/jairoFernandez/litek8s';
export const BRANCH = 'main';

export const groups = [
  {
    label: 'Start here', translations: { es: 'Empieza aquí' },
    pages: [
      { slug: '', label: 'Introduction', title: 'Introduction', seoTitle: 'litek8s: a lightweight Kubernetes in one binary', source: 'docs/overview.md', translations: { es: { source: 'docs/es/overview.md', label: "Introducción", seoTitle: "litek8s: un Kubernetes liviano en un solo binario" } } },
      { slug: 'alongside', label: 'Next to your clusters', source: 'docs/alongside.md', translations: { es: { source: 'docs/es/alongside.md', label: "Junto a tus clusters" } } },
      {
        slug: 'install', label: 'Install', seoTitle: 'Install litek8s on macOS and Linux', source: 'docs/install.md',
        description: 'Install litek8s on macOS (Apple silicon) or Linux (x86_64, aarch64) with Homebrew or the install script, then start a local Kubernetes cluster.', translations: { es: { source: 'docs/es/install.md', label: "Instalación", seoTitle: "Instalar litek8s en macOS y Linux", description: "Instala litek8s en macOS (Apple silicon) o Linux (x86_64, aarch64) con Homebrew o el script de instalación, y arranca un cluster de Kubernetes local." } } },
      {
        slug: 'quickstart', label: 'Quickstart', seoTitle: 'Quickstart: a local Kubernetes cluster in a second', source: 'docs/quickstart.md',
        description: 'Start a local Kubernetes cluster with litek8s dev up, deploy nginx with kubectl, use the same cluster as a Docker engine, and add multi-node clusters.', translations: { es: { source: 'docs/es/quickstart.md', label: "Inicio rápido", seoTitle: "Inicio rápido: un cluster de Kubernetes local en un segundo", description: "Arranca un cluster de Kubernetes local con litek8s dev up, despliega nginx con kubectl, usa el mismo cluster como motor Docker y agrega clusters de varios nodos." } } },
      {
        slug: 'numbers', label: 'Numbers', seoTitle: 'Benchmarks: litek8s vs k3s, kind and k3d', source: 'docs/numbers.md',
        description: 'Memory, startup time and CI numbers for litek8s against k3s, kind and k3d, measured with scripts in the repo: a 23 MiB control plane, 0.2 s dev clusters.', translations: { es: { source: 'docs/es/numbers.md', label: "Números", seoTitle: "Benchmarks: litek8s frente a k3s, kind y k3d", description: "Memoria, tiempo de arranque y números de CI de litek8s frente a k3s, kind y k3d, medidos con scripts del repo: un control plane de 23 MiB, clusters de 0.2 s." } } },
      {
        slug: 'platforms', label: 'Platforms', seoTitle: 'Supported platforms: macOS, Linux, Raspberry Pi', source: 'docs/platforms.md',
        description: 'Where litek8s runs: dev clusters on macOS (Apple silicon) and rootless Linux, servers and nodes on Linux x86_64 and aarch64 with crun.', translations: { es: { source: 'docs/es/platforms.md', label: "Plataformas", seoTitle: "Plataformas soportadas: macOS, Linux, Raspberry Pi", description: "Dónde corre litek8s: clusters de desarrollo en macOS (Apple silicon) y Linux rootless, servidores y nodos en Linux x86_64 y aarch64 con crun." } } },
      { slug: 'multi-node', label: 'Multi-node and HA', seoTitle: 'Multi-node Kubernetes and HA with litek8s', source: 'docs/multi-node.md', translations: { es: { source: 'docs/es/multi-node.md', label: "Varios nodos y HA", seoTitle: "Kubernetes de varios nodos y HA con litek8s" } } },
      {
        slug: 'limitations', label: 'Limitations', seoTitle: 'Limitations of litek8s', source: 'docs/limitations.md',
        description: 'What litek8s does not do yet: simpler authentication and RBAC than upstream, young on-disk formats, kubelet behaviors and admission features still missing.', translations: { es: { source: 'docs/es/limitations.md', label: "Limitaciones", seoTitle: "Limitaciones de litek8s", description: "Lo que litek8s todavía no hace: formatos en disco jóvenes, comportamientos del kubelet y funciones de admisión que aún faltan." } } },
    ],
  },
  {
    label: 'Guides', translations: { es: 'Guías' },
    pages: [
      { slug: 'dev', label: 'Dev clusters', source: 'docs/dev.md', translations: { es: { source: 'docs/es/dev.md', label: "Clusters de desarrollo" } } },
      { slug: 'docker', label: 'Docker API', source: 'docs/docker.md', translations: { es: { source: 'docs/es/docker.md', label: "API de Docker" } } },
      { slug: 'ci', label: 'CI (GitHub Actions)', source: 'docs/ci.md', translations: { es: { source: 'docs/es/ci.md', label: "CI (GitHub Actions)" } } },
      { slug: 'ui', label: 'Terminal UI', source: 'docs/ui.md', translations: { es: { source: 'docs/es/ui.md', label: "Interfaz de terminal" } } },
      { slug: 'observability', label: 'Metrics, logs, alerts', source: 'docs/observability.md', translations: { es: { source: 'docs/es/observability.md', label: "Métricas, logs y alertas" } } },
      { slug: 'clusters', label: 'Clusters over SSH', source: 'docs/clusters.md', translations: { es: { source: 'docs/es/clusters.md', label: "Clusters por SSH" } } },
      {
        slug: 'auth', label: 'Authentication and RBAC', seoTitle: 'Authentication and RBAC on litek8s servers', source: 'docs/auth.md',
        description: 'How litek8s servers authenticate clients and enforce RBAC, the RBAC-audit mode, and how to fix a cluster made with an earlier release (401, 403).',
        translations: {
          es: {
            source: 'docs/es/auth.md', label: 'Autenticación y RBAC', seoTitle: 'Autenticación y RBAC en servidores litek8s',
            description: 'Cómo los servidores litek8s autentican clientes y aplican RBAC, el modo RBAC-audit y cómo arreglar un cluster creado con una versión anterior (401, 403).',
          },
        },
      },
      { slug: 'registries', label: 'Private registries and mirrors', source: 'docs/registries.md', translations: { es: { source: 'docs/es/registries.md', label: "Registros privados y mirrors" } } },
      { slug: 'networking', label: 'Networking across NATs', source: 'docs/networking.md', translations: { es: { source: 'docs/es/networking.md', label: "Red a través de NATs" } } },
      { slug: 'ha', label: 'HA control plane', source: 'docs/ha.md', translations: { es: { source: 'docs/es/ha.md', label: "Control plane en HA" } } },
      { slug: 'crds', label: 'CRDs and server-side apply', source: 'docs/crds.md', translations: { es: { source: 'docs/es/crds.md', label: "CRDs y server-side apply" } } },
      { slug: 'agents', label: 'MCP and LLM agents', source: 'docs/agents.md', translations: { es: { source: 'docs/es/agents.md', label: "MCP y agentes LLM" } } },
      { slug: 'llm-cluster', label: 'One LLM across machines', source: 'docs/llm-cluster.md', translations: { es: { source: 'docs/es/llm-cluster.md', label: "Un LLM entre varias máquinas" } } },
    ],
  },
  {
    label: 'Ecosystem', translations: { es: 'Ecosistema' },
    pages: [
      {
        slug: 'litecd', label: 'GitOps with litecd', seoTitle: 'GitOps with litecd: Argo CD manifests in one small pod', source: 'docs/litecd.md',
        description: 'Run Argo CD Applications on litek8s with litecd: the same manifests and status in one Rust pod of 7 MiB instead of Argo CD\'s 310 MiB.',
        translations: { es: { source: 'docs/es/litecd.md', label: 'GitOps con litecd', seoTitle: 'GitOps con litecd: manifiestos de Argo CD en un pod pequeño', description: 'Corre Applications de Argo CD en litek8s con litecd: los mismos manifiestos y status en un pod de Rust de 7 MiB en vez de los 310 MiB de Argo CD.' } },
      },
    ],
  },
  {
    label: 'Contributing', translations: { es: 'Contribuir' },
    pages: [
      { slug: 'contributing', label: 'Building and testing', source: 'CONTRIBUTING.md', translations: { es: { source: 'docs/es/contributing.md', label: "Compilar y probar" } } },
      { slug: 'develop-without-docker', label: 'Developing without Docker', source: 'docs/develop-without-docker.md', translations: { es: { source: 'docs/es/develop-without-docker.md', label: "Desarrollar sin Docker" } } },
    ],
  },
];

export const pages = groups.flatMap((g) => g.pages);

/** The site path of a docs page. */
export const route = (slug) => (slug ? `/docs/${slug}/` : '/docs/');

/** Languages other than English, with their URL prefix. */
export const LOCALES = { es: { label: 'Español', lang: 'es' } };

/** The site path of a docs page in a language ('en' or a LOCALES key). */
export const localRoute = (slug, lang = 'en') => (lang === 'en' ? route(slug) : `/${lang}${route(slug)}`);
