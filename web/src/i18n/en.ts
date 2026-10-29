// Landing page copy and data, English. Another language: copy this file
// (e.g. es.ts), translate the strings, point its internal links at that
// language's pages, and add src/pages/<lang>/index.astro that renders
// <Landing t={dict} />. Commands, numbers and URLs stay as they are.
// Every number here comes from README.md and the bench/compat files it links.

const GH = 'https://github.com/jairoFernandez/litek8s';
const blob = (p: string) => `${GH}/blob/main/${p}`;

export const en = {
  lang: 'en',
  meta: {
    title: 'litek8s · a lightweight Kubernetes in one binary',
    description:
      'The real Kubernetes API in one Rust binary: local dev clusters in a second, a Docker-compatible API, built-in metrics and logs, HA. Idles at 23 MiB.',
    // Alt text of the social card (public/og.png).
    imageAlt: 'litek8s: a lightweight Kubernetes in one binary',
  },
  links: {
    github: GH,
    releases: `${GH}/releases`,
    crates: 'https://crates.io/crates/litek8s',
    docs: '/docs/',
    rss: '/blog/rss.xml',
    issues: `${GH}/issues`,
    license: blob('LICENSE'),
  },
  nav: {
    skip: 'Skip to content',
    items: [
      { label: 'Features', href: '#features' },
      { label: 'Numbers', href: '#numbers' },
      { label: 'How it works', href: '#how' },
      { label: 'Install', href: '#install' },
      { label: 'Docs', href: '/docs/' },
      { label: 'Blog', href: '/blog/' },
    ],
    github: 'GitHub',
    // The other language's landing page (the nav's language link, hreflang).
    lang: { label: 'Español', short: 'ES', lang: 'es', href: '/es/' },
    home: '/',
    theme: 'Switch between dark and light',
    menu: 'Menu',
  },
  hero: {
    badge: 'v0.2 · early preview · Apache-2.0',
    lines: ['A whole Kubernetes', 'in one binary,', 'idling at 23 MiB.'],
    lead:
      'litek8s serves the real Kubernetes API, so kubectl, Helm and Argo CD work unchanged. The control plane, scheduler, controllers, DNS, service proxy and node agent are one Rust process. For local development, home labs and the edge.',
    primary: 'Install',
    secondary: 'Read the docs',
    copy: 'Copy',
    copied: 'Copied',
    install: 'brew install jairoFernandez/tap/litek8s',
  },
  demo: {
    label: 'Terminal demo',
    replay: 'Replay',
    caption: '`dev up` to node Ready: 1.28 s on macOS, 0.17 s on Linux.',
    captionSource: { label: 'bench/RESULTS.md', href: blob('bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18') },
    // Steps: `$` a command (typed), `>` output, `~` a wait in ms, `*` the
    // dev up spinner (ms), `ui` the real `litek8s ui` frame.
    scenes: [
      {
        id: 'up',
        tab: 'dev up',
        steps: [
          ['$', 'litek8s dev up'],
          ['*', 1280],
          ['>', '<span class="ok">litek8s-dev ready in 1.28s</span> <span class="dim">(vm, http://127.0.0.1:6443, kubectl context litek8s-dev)</span>'],
          ['~', 500],
          ['$', 'kubectl get nodes'],
          ['>', '<span class="dim">NAME          STATUS   ROLES           AGE   VERSION</span>'],
          ['>', 'litek8s-dev   <span class="ok">Ready</span>    control-plane   3s    v1.33.0+litek8s'],
        ],
      },
      {
        id: 'deploy',
        tab: 'deploy',
        steps: [
          ['$', 'kubectl create deployment web --image=nginx'],
          ['>', 'deployment.apps/web created'],
          ['$', 'kubectl expose deployment web --type=NodePort --port 80'],
          ['>', 'service/web exposed'],
          ['$', "curl -s localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}') | grep title"],
          ['~', 300],
          ['>', '&lt;title&gt;<span class="ok">Welcome to nginx!</span>&lt;/title&gt;'],
        ],
      },
      {
        id: 'docker',
        tab: 'docker',
        steps: [
          ['$', 'export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock'],
          ['$', 'docker run --rm hello-world'],
          ['~', 400],
          ['>', '<span class="ok">Hello from Docker!</span>'],
          ['>', '<span class="dim">This message shows that your installation appears to be working correctly.</span>'],
          ['$', 'docker compose up -d --build'],
        ],
      },
      {
        id: 'ui',
        tab: 'litek8s ui',
        steps: [
          ['$', 'litek8s ui'],
          ['ui', 0],
        ],
      },
    ] as Array<{ id: string; tab: string; steps: Array<[string, string | number]> }>,
  },
  strip: {
    label: 'Works with the tools you already use',
    more: { label: 'Next to the clusters you already run, not instead of them', href: '/docs/alongside/' },
    items: ['kubectl', 'Helm', 'Argo CD', 'docker', 'docker compose', 'Dev Containers', 'Testcontainers', 'Grafana', 'Claude Code (MCP)'],
  },
  tagline: {
    eyebrow: 'One process',
    text:
      'One process does what a Kubernetes distribution splits across etcd, the API server, the controller manager, the scheduler, CoreDNS, kube-proxy, the kubelet and containerd, plus a Docker-compatible API and a metrics, logs and alerts stack.',
  },
  numbers: {
    eyebrow: 'Numbers',
    title: 'Measured, with the scripts in the repo',
    lead: 'Against k3s, kind and k3d. Every row links to the file with the setup, raw data and caveats.',
    source: 'source',
    smaller: 'Shorter is better.',
    rows: [
      {
        label: 'Whole orchestrator, 5 nginx pods',
        note: 'PSS, v0.2.1; idle, no pods: 23.2 vs 567',
        unit: 'MiB',
        bars: [
          { name: 'litek8s', value: 33.5, us: true },
          { name: 'k3s minimal', value: 579 },
        ],
        href: blob('bench/RESULTS.md#footprint-of-the-v021-release-2026-10-21-x86_64kvm'),
        src: 'bench/RESULTS.md',
      },
      {
        label: 'Argo CD + one synced app',
        note: 'orchestrator memory',
        unit: 'MiB',
        bars: [
          { name: 'litek8s', value: 90, us: true },
          { name: 'k3s minimal', value: 900 },
        ],
        href: blob('bench/RESULTS.md#argo-cd-on-k3s-and-on-litek8s-2026-10-19-x86_64kvm'),
        src: 'bench/RESULTS.md',
      },
      {
        label: '`dev up` to node Ready, Linux',
        note: 'local dev cluster',
        unit: 's',
        bars: [
          { name: 'litek8s', value: 0.17, us: true },
          { name: 'k3d', value: 11.1 },
          { name: 'kind', value: 31.6 },
        ],
        href: blob('bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18'),
        src: 'bench/RESULTS.md',
      },
      {
        label: '`dev up` to node Ready, macOS',
        note: 'local dev cluster',
        unit: 's',
        bars: [
          { name: 'litek8s', value: 1.28, us: true },
          { name: 'k3d', value: 10.4 },
          { name: 'kind', value: 34.3 },
        ],
        href: blob('bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18'),
        src: 'bench/RESULTS.md',
      },
      {
        label: 'Dev cluster idle memory, Linux',
        note: 'local dev cluster',
        unit: 'MiB',
        bars: [
          { name: 'litek8s', value: 47, us: true },
          { name: 'k3d', value: 398 },
          { name: 'kind', value: 526 },
        ],
        href: blob('bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18'),
        src: 'bench/RESULTS.md',
      },
      {
        label: 'Metrics, logs and alerts, added memory',
        note: 'vs kube-prometheus-stack + Loki',
        unit: 'MiB',
        prefix: '+',
        bars: [
          { name: 'litek8s', value: 1.8, us: true },
          { name: 'kube-prometheus-stack + Loki', value: 1371 },
        ],
        href: blob('bench/RESULTS.md#observability-built-in-vs-kube-prometheus-stack--loki-2026-10-20-x86_64kvm'),
        src: 'bench/RESULTS.md',
      },
    ],
    scores: [
      {
        label: 'Compatibility corpus',
        note: 'kubectl, CRDs, workloads, networking, 47 Helm charts',
        value: 119,
        of: 119,
        ref: 'k3s: 119/119',
        href: blob('compat/RESULTS.md#without-docker-2026-10-19-119-cases'),
        src: 'compat/RESULTS.md',
      },
      {
        label: 'Kubernetes v1.33 conformance',
        note: '419 specs',
        value: 413,
        of: 419,
        ref: 'k3s: 416/419',
        href: blob('compat/CONFORMANCE.md'),
        src: 'compat/CONFORMANCE.md',
      },
    ],
    footnote:
      'On macOS a dev cluster runs in a small Linux VM, and Activity Monitor shows the whole VM: about 240 MiB fresh, ~380 MiB after pulling and running 5 nginx pods, as macOS keeps every page the guest has touched until the VM stops. The 6 conformance failures: 5 are macOS-only (TCP DNS on port 53, a hostPort binding) and pass on Linux; 1 was a watch fixed since. A 3-server HA control plane stays linearizable under leader kills and partitions, and a 3-node cluster ran 21 h under churn with no leak.',
    footLinks: [
      { label: 'Memory on macOS', href: '/docs/dev/#memory-on-macos' },
      { label: 'bench/HA.md', href: blob('bench/HA.md') },
      { label: 'bench/SOAK.md', href: blob('bench/SOAK.md') },
    ],
  },
  features: {
    eyebrow: "What's in the binary",
    title: 'Everything a cluster needs, nothing to assemble',
    more: 'Read more',
    items: [
      {
        id: 'dev',
        title: 'Dev clusters',
        body: 'kind, k3d or minikube without Docker. As many clusters as you like, each with as many nodes, in one rootless sandbox on Linux or one small Virtualization.framework VM on macOS that grows its memory on demand. NodePorts on localhost, your home directory mounted at the same path.',
        href: '/docs/dev/',
        code: [
          '<span class="c">$</span> litek8s dev up --name ci --nodes 3',
          '<span class="c">$</span> litek8s dev load myapp:dev    <span class="dim"># a local image</span>',
          '<span class="c">$</span> litek8s dev list             <span class="dim"># state, API port, nodes, memory</span>',
          '<span class="c">$</span> litek8s dev down             <span class="dim"># objects and images are kept</span>',
        ],
      },
      {
        id: 'docker',
        title: 'Docker Engine API',
        body: 'docker, docker compose, docker build, Dev Containers and Testcontainers run on the dev cluster, next to (not instead of) Docker or OrbStack. Compose projects can get CPU and memory budgets.',
        href: '/docs/docker/',
        code: [
          '<span class="c">$</span> export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock',
          '<span class="c">$</span> docker compose up -d --build',
          '<span class="c">$</span> docker run --rm -it alpine sh',
          '<span class="dim"># the same cluster, the same images</span>',
          '<span class="c">$</span> kubectl get pods -A',
        ],
      },
      {
        id: 'ci',
        title: 'Kubernetes in CI',
        body: 'If your pipeline needs a cluster, this is the setup step to use: Ready in ~2.5 s instead of kind\'s ~45 s on GitHub\'s runners, Helm and multi-node jobs 3–5× shorter with 26–37× less memory, and 1.8× faster than kind in Docker in Docker on GitLab-style container jobs.',
        href: '/docs/ci/',
        code: [
          '<span class="dim"># .github/workflows/e2e.yml</span>',
          '- uses: jairoFernandez/litek8s/.github/actions/setup-litek8s@v0.2.1',
          '- run: helm install hello ./chart --wait && helm test hello',
          '<span class="dim"># cluster Ready in ~2.5 s, job 21 s (kind: 79 s)</span>',
        ],
      },
      {
        id: 'ui',
        title: 'Terminal UI',
        body: 'litek8s ui: cluster state, a k9s-style Kubernetes view, containers by compose project with logs and shells, images, volumes, disk cleanup. Keyboard-driven, works over SSH.',
        href: '/docs/ui/',
        image: 'kube',
      },
      {
        id: 'obs',
        title: 'Built-in observability',
        body: 'An embedded time-series store with a Prometheus-compatible API (Grafana works), log search across every pod and node, alert rules to a webhook, ntfy or Telegram, and kubectl top without metrics-server.',
        href: '/docs/observability/',
        code: [
          '<span class="c">$</span> litek8s logs -l app=web --since 1h --grep ERROR',
          "<span class=\"c\">$</span> litek8s query 'topk(5, sum by (pod) (rate(container_cpu_usage_seconds_total[5m])))'",
          '<span class="c">$</span> litek8s alerts',
          '<span class="c">$</span> kubectl top pods -A --containers',
        ],
      },
      {
        id: 'nodes',
        title: 'Real servers and nodes',
        body: 'litek8s apiserver on a box; more machines join with a k3s-style token that pins the CA. Pods talk across nodes over a WireGuard mesh litek8s sets up itself, and run on crun directly: no containerd, no pause containers.',
        href: '/docs/multi-node/',
        code: [
          '<span class="dim"># the server; prints the join command</span>',
          '<span class="c">$</span> curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh -s -- server',
          '<span class="dim"># every other machine</span>',
          '<span class="c">$</span> curl -fsSL …/install.sh | sh -s -- agent --join https://192.168.1.10:6443 --token K10&lt;hash&gt;::&lt;secret&gt;',
        ],
      },
      {
        id: 'ha',
        title: 'HA control plane',
        body: '3 or 5 API servers share the store through Raft, no etcd. Any one server can die without losing writes; litek8s lb gives kubectl one address in front of all three.',
        href: '/docs/ha/',
        code: [
          '<span class="c">$</span> install.sh | sh -s -- server --cluster-init',
          '<span class="c">$</span> install.sh | sh -s -- server --server https://&lt;first&gt;:6443 --token &lt;token&gt;',
          '<span class="c">$</span> litek8s lb --listen 127.0.0.1:6443 --server http://192.168.1.10:6443',
        ],
      },
      {
        id: 'agents',
        title: 'For LLM agents',
        body: 'litek8s mcp is an MCP server (dev clusters, objects, pod diagnosis, logs, PromQL, alerts) with guardrails: destructive tools need confirmation, a read-only mode, output limits. litek8s skill installs an agent skill.',
        href: '/docs/agents/',
        code: [
          '<span class="c">$</span> claude mcp add litek8s -- litek8s mcp',
          '<span class="c">$</span> claude mcp add litek8s-ro -- litek8s mcp --read-only',
          '<span class="c">$</span> litek8s skill --install ~/.claude/skills',
        ],
      },
      {
        id: 'crds',
        title: 'CRDs done properly',
        body: 'OpenAPI schema validation, CEL rules type-checked and cost-estimated at CRD create, ratcheting, and server-side apply with managedFields, conflicts and pruning, like a Kubernetes 1.33 API server.',
        href: '/docs/crds/',
        code: [
          '<span class="dim">x-kubernetes-validations:</span>',
          '  - rule: <span class="ok">"self.replicas &lt;= self.maxReplicas"</span>',
          "    message: <span class=\"ok\">\"replicas must not exceed maxReplicas\"</span>",
          '<span class="dim"># a typo is rejected when the CRD is created:</span>',
          "<span class=\"err\">ERROR: &lt;input&gt;:1:5: undefined field 'replica'</span>",
        ],
      },
    ],
  },
  how: {
    eyebrow: 'How it works',
    title: 'One process, from laptop to rack',
    tabs: [
      {
        id: 'architecture',
        label: 'Architecture',
        text: 'kubectl, Helm and Argo CD reach the Kubernetes API server on :6443; docker and compose reach the Docker Engine API on docker.sock. Both sit in one process with the object store, the controllers and scheduler, DNS and the service proxy, metrics, logs and alerts, and the node agent, and both run containers on crun.',
      },
      {
        id: 'local',
        label: 'Local clusters',
        text: 'Every cluster, and every node of a multi-node cluster, runs in one small VM on macOS or one rootless sandbox on Linux, started with the first cluster and stopped with the last. Each cluster gets its own API port, kubectl context and Docker socket.',
      },
      {
        id: 'multinode',
        label: 'Joining machines',
        text: 'A server runs the whole control plane and its own pods; any Linux machine (a Raspberry Pi, an old laptop, a cheap cloud VM) joins it as a node with the token the server prints. Every node gets a /24 of 10.42.0.0/16; Services and DNS work cluster-wide.',
      },
      {
        id: 'ha',
        label: 'HA',
        text: 'Three servers make the control plane highly available: the object store is replicated with Raft, no etcd. Any one server can die without losing writes; nodes fail over to another server, and litek8s lb gives kubectl one address.',
      },
    ],
    docs: 'Read the docs',
  },
  install: {
    eyebrow: 'Install',
    title: 'Two commands to a cluster',
    lead: 'macOS (Apple silicon) and Linux (x86_64, aarch64). macOS needs nothing else: no Docker Desktop, no Lima, no sudo.',
    methods: [
      { id: 'brew', label: 'Homebrew', code: 'brew install jairoFernandez/tap/litek8s' },
      { id: 'script', label: 'Install script', code: 'curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh' },
      { id: 'cargo', label: 'cargo', code: 'cargo install litek8s' },
    ],
    notes: [
      'The script downloads the latest release for your OS and architecture, checks its SHA-256 and installs litek8s to ~/.local/bin.',
      'On Linux, dev clusters run rootless and need crun, uidmap, passt (or slirp4netns), iproute2 and iptables: `litek8s dev setup` checks everything and says what is missing.',
      'From source with cargo: Rust ≥ 1.90; macOS also needs the VM guest.',
    ],
    quickstart: 'Quickstart',
    quick: [
      ['litek8s dev up', ''],
      ['kubectl create deployment web --image=nginx', ''],
      ['kubectl expose deployment web --type=NodePort --port 80', ''],
      ["curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')", ''],
      ['litek8s ui', 'browse it all'],
      ['export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock', ''],
      ['docker run --rm hello-world', 'the same cluster speaks the Docker API'],
      ['litek8s dev up --name ci --nodes 3', 'another cluster, three nodes'],
      ['litek8s dev down', 'stop; objects and images are kept'],
    ],
    servers: 'On Linux servers',
    serversText: 'One command per machine, like k3s’s installer: a systemd unit, the token in /etc/litek8s, pods that keep running across restarts and upgrades.',
    serversLink: 'Multi-node and HA',
  },
  platforms: {
    eyebrow: 'Platforms',
    title: 'Where it runs',
    head: ['', 'dev clusters', 'server / node', 'release binary'],
    rows: [
      ['macOS, Apple silicon (13+)', 'yes, in a VM (Virtualization.framework)', 'for development only (Docker runtime)', 'darwin-arm64, VM guest included'],
      ['Linux x86_64 / aarch64, rootless', 'yes, user-namespace sandbox (kernel ≥ 5.11, cgroup v2)', '–', 'linux-x86_64, linux-aarch64 (static)'],
      ['Linux, root with crun', 'yes', 'yes: the intended server and node setup', 'same'],
      ['macOS Intel, Windows', 'no (WSL2 untested)', 'no', '–'],
    ],
    note: 'Tested on Ubuntu 24.04 (arm64 and x86_64) and recent macOS on Apple silicon. Ubuntu ≥ 23.10 needs `sudo litek8s dev setup` once.',
  },
  limits: {
    eyebrow: 'Limitations',
    title: 'The honest list',
    lead: 'Early preview, built and tested by one person: expect rough edges.',
    items: [
      { title: 'Authentication and RBAC are simpler than upstream.', text: 'Servers authenticate clients and enforce RBAC since 0.2.12, but there is no OIDC or webhook login and no certificate revocation. Dev clusters stay open on 127.0.0.1; on a server, keep port 6443 (and the Raft port, 6444) off untrusted networks anyway.', strong: true },
      { title: 'Weeks old.', text: 'APIs, flags and on-disk formats may change between 0.x releases without migration.' },
      { title: 'Not every kubelet behavior.', text: 'No Unhealthy/BackOff events; a memory emptyDir is one tmpfs per container mount; ValidatingAdmissionPolicy and webhook matchConditions are accepted but not enforced.' },
      { title: 'Metrics stop at the platform.', text: 'Containers, nodes and cluster state are covered; application /metrics endpoints are not scraped.' },
      { title: 'The macOS VM keeps memory it touched.', text: 'A Virtualization.framework limit; litek8s keeps the guest small instead.' },
      { title: 'macOS servers are for development.', text: 'They use Docker for pods; run real servers and nodes on Linux.' },
    ],
    more: 'More in the differences reference',
    moreHref: blob('skills/litek8s/references/differences.md'),
  },
  cta: {
    title: ['Start a cluster', 'in about a second.'],
    text: 'Free and open source, Apache-2.0. Feedback and issues are very welcome.',
    primary: 'Read the docs',
    star: 'Star on GitHub',
  },
  footer: {
    tagline: 'A lightweight Kubernetes in one binary.',
    cols: [
      {
        title: 'Project',
        links: [
          { label: 'GitHub', href: GH },
          { label: 'Releases', href: `${GH}/releases` },
          { label: 'crates.io', href: 'https://crates.io/crates/litek8s' },
          { label: 'Issues', href: `${GH}/issues` },
          { label: 'Blog', href: '/blog/' },
        ],
      },
      {
        title: 'Docs',
        links: [
          { label: 'Introduction', href: '/docs/' },
          { label: 'Next to your clusters', href: '/docs/alongside/' },
          { label: 'Dev clusters', href: '/docs/dev/' },
          { label: 'Docker API', href: '/docs/docker/' },
          { label: 'Terminal UI', href: '/docs/ui/' },
        ],
      },
      {
        title: 'More',
        links: [
          { label: 'Observability', href: '/docs/observability/' },
          { label: 'HA control plane', href: '/docs/ha/' },
          { label: 'MCP and agents', href: '/docs/agents/' },
          { label: 'Security', href: blob('SECURITY.md') },
        ],
      },
    ],
    legal: 'Apache-2.0. Kubernetes is a registered trademark of The Linux Foundation; litek8s is not affiliated with it.',
  },
};

export type Dict = typeof en;
