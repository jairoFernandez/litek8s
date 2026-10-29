// Landing page copy and data, Spanish (src/pages/es/index.astro). Same shape
// as en.ts; commands, numbers and external URLs as there; internal links point
// at the Spanish pages (/es/docs/..., which fall back to English where a page
// is not translated, and /es/blog/).

import { en, type Dict } from './en';

const GH = 'https://github.com/jairoFernandez/litek8s';
const blob = (p: string) => `${GH}/blob/main/${p}`;
const docs = (slug = '') => (slug ? `/es/docs/${slug}/` : '/es/docs/');

export const es: Dict = {
  ...en,
  lang: 'es',
  meta: {
    title: 'litek8s · un Kubernetes ligero en un solo binario',
    description:
      'La API real de Kubernetes en un binario de Rust: clusters de desarrollo en un segundo, una API compatible con Docker, métricas y logs integrados, HA. En reposo usa 23 MiB.',
    imageAlt: 'litek8s: un Kubernetes ligero en un solo binario',
  },
  links: { ...en.links, docs: docs(), rss: '/es/blog/rss.xml' },
  nav: {
    skip: 'Saltar al contenido',
    items: [
      { label: 'Funciones', href: '#features' },
      { label: 'Números', href: '#numbers' },
      { label: 'Cómo funciona', href: '#how' },
      { label: 'Instalar', href: '#install' },
      { label: 'Docs', href: docs() },
      { label: 'Blog', href: '/es/blog/' },
    ],
    github: 'GitHub',
    lang: { label: 'English', short: 'EN', lang: 'en', href: '/' },
    home: '/es/',
    theme: 'Cambiar entre modo oscuro y claro',
    menu: 'Menú',
  },
  hero: {
    ...en.hero,
    badge: 'v0.2 · vista previa · Apache-2.0',
    lines: ['Un Kubernetes completo', 'en un solo binario,', 'en reposo con 23 MiB.'],
    lead:
      'litek8s sirve la API real de Kubernetes, así que kubectl, Helm y Argo CD funcionan sin cambios. El plano de control, el scheduler, los controladores, el DNS, el proxy de servicios y el agente de nodo son un solo proceso de Rust. Para desarrollo local, home labs y el edge.',
    primary: 'Instalar',
    secondary: 'Leer la documentación',
    copy: 'Copiar',
    copied: 'Copiado',
  },
  demo: {
    ...en.demo,
    label: 'Demo en terminal',
    replay: 'Repetir',
    caption: 'De `dev up` a nodo Ready: 1.28 s en macOS, 0.17 s en Linux.',
  },
  strip: {
    label: 'Funciona con las herramientas que ya usas',
    more: { label: 'Junto a los clusters que ya tienes, no en su lugar', href: docs('alongside') },
    items: en.strip.items,
  },
  tagline: {
    eyebrow: 'Un proceso',
    text:
      'Un proceso hace lo que una distribución de Kubernetes reparte entre etcd, el API server, el controller manager, el scheduler, CoreDNS, kube-proxy, el kubelet y containerd, más una API compatible con Docker y una pila de métricas, logs y alertas.',
  },
  numbers: {
    ...en.numbers,
    eyebrow: 'Números',
    title: 'Medido, con los scripts del repositorio',
    lead: 'Contra k3s, kind y k3d. Cada fila enlaza al archivo con la configuración, los datos crudos y las salvedades.',
    source: 'fuente',
    smaller: 'Más corto es mejor.',
    rows: en.numbers.rows.map((r, i) => ({
      ...r,
      ...[
        { label: 'Orquestador completo, 5 pods de nginx', note: 'PSS, v0.2.1; en reposo, sin pods: 23.2 vs 567' },
        { label: 'Argo CD + una app sincronizada', note: 'memoria del orquestador' },
        { label: '`dev up` a nodo Ready, Linux', note: 'cluster de desarrollo local' },
        { label: '`dev up` a nodo Ready, macOS', note: 'cluster de desarrollo local' },
        { label: 'Memoria en reposo del cluster de desarrollo, Linux', note: 'cluster de desarrollo local' },
        { label: 'Métricas, logs y alertas, memoria añadida', note: 'vs kube-prometheus-stack + Loki' },
      ][i],
    })),
    scores: en.numbers.scores.map((s, i) => ({
      ...s,
      ...[
        { label: 'Corpus de compatibilidad', note: 'kubectl, CRDs, workloads, red, 47 charts de Helm' },
        { label: 'Conformance de Kubernetes v1.33', note: '419 specs' },
      ][i],
    })),
    footnote:
      'En macOS un cluster de desarrollo corre en una pequeña VM Linux, y el Monitor de Actividad muestra la VM entera: unos 240 MiB recién iniciada, ~380 MiB después de descargar y correr 5 pods de nginx, porque macOS conserva cada página que el guest tocó hasta que la VM se detiene. Los 6 fallos de conformance: 5 son solo de macOS (DNS por TCP en el puerto 53, un binding de hostPort) y pasan en Linux; 1 era un watch, ya corregido. Un plano de control HA de 3 servidores sigue siendo linealizable ante caídas del líder y particiones, y un cluster de 3 nodos corrió 21 h bajo carga sin fugas.',
    footLinks: [
      { label: 'Memoria en macOS', href: `${docs('dev')}#memory-on-macos` },
      { label: 'bench/HA.md', href: blob('bench/HA.md') },
      { label: 'bench/SOAK.md', href: blob('bench/SOAK.md') },
    ],
  },
  features: {
    eyebrow: 'Qué trae el binario',
    title: 'Todo lo que un cluster necesita, nada que armar',
    more: 'Leer más',
    items: en.features.items.map((f) => ({
      ...f,
      href: docs(f.href.replace(/^\/docs\/|\/$/g, '')),
      ...{
        dev: {
          title: 'Clusters de desarrollo',
          body: 'kind, k3d o minikube sin Docker. Tantos clusters como quieras, cada uno con tantos nodos como quieras, en un sandbox sin root en Linux o en una pequeña VM de Virtualization.framework en macOS que crece su memoria a demanda. NodePorts en localhost, tu directorio home montado en la misma ruta.',
        },
        docker: {
          title: 'API de Docker Engine',
          body: 'docker, docker compose, docker build, Dev Containers y Testcontainers corren sobre el cluster de desarrollo, junto a (no en lugar de) Docker u OrbStack. Los proyectos de Compose pueden tener presupuestos de CPU y memoria.',
        },
        ci: {
          title: 'Kubernetes en CI',
          body: 'Si tu pipeline necesita un cluster, este es el paso de setup: Ready en ~2.5 s en lugar de los ~45 s de kind en los runners de GitHub, jobs de Helm y multi-nodo 3–5× más cortos con 26–37× menos memoria, y 1.8× más rápido que kind en Docker in Docker en jobs de contenedor estilo GitLab.',
        },
        ui: {
          title: 'UI de terminal',
          body: 'litek8s ui: estado del cluster, una vista de Kubernetes al estilo k9s, contenedores por proyecto de compose con logs y shells, imágenes, volúmenes, limpieza de disco. Se maneja con el teclado y funciona por SSH.',
        },
        obs: {
          title: 'Observabilidad integrada',
          body: 'Un almacén de series de tiempo embebido con una API compatible con Prometheus (Grafana funciona), búsqueda de logs en todos los pods y nodos, reglas de alerta hacia un webhook, ntfy o Telegram, y kubectl top sin metrics-server.',
        },
        nodes: {
          title: 'Servidores y nodos reales',
          body: 'litek8s apiserver en una máquina; las demás se unen con un token al estilo k3s que fija la CA. Los pods hablan entre nodos por una malla WireGuard que litek8s arma solo, y corren directamente sobre crun: sin containerd, sin contenedores pause.',
        },
        ha: {
          title: 'Plano de control HA',
          body: '3 o 5 API servers comparten el almacén con Raft, sin etcd. Cualquier servidor puede caer sin perder escrituras; litek8s lb le da a kubectl una sola dirección delante de los tres.',
        },
        agents: {
          title: 'Para agentes LLM',
          body: 'litek8s mcp es un servidor MCP (clusters de desarrollo, objetos, diagnóstico de pods, logs, PromQL, alertas) con protecciones: las herramientas destructivas piden confirmación, hay modo de solo lectura y límites de salida. litek8s skill instala un skill para agentes.',
        },
        crds: {
          title: 'CRDs bien hechas',
          body: 'Validación de esquemas OpenAPI, reglas CEL con chequeo de tipos y costo estimado al crear la CRD, ratcheting, y server-side apply con managedFields, conflictos y pruning, como un API server de Kubernetes 1.33.',
        },
      }[f.id],
    })),
  },
  how: {
    eyebrow: 'Cómo funciona',
    title: 'Un proceso, de la laptop al rack',
    tabs: [
      {
        id: 'architecture',
        label: 'Arquitectura',
        text: 'kubectl, Helm y Argo CD llegan al API server de Kubernetes en :6443; docker y compose llegan a la API de Docker Engine en docker.sock. Ambos viven en un proceso con el almacén de objetos, los controladores y el scheduler, el DNS y el proxy de servicios, las métricas, logs y alertas, y el agente de nodo, y ambos corren contenedores sobre crun.',
      },
      {
        id: 'local',
        label: 'Clusters locales',
        text: 'Cada cluster, y cada nodo de un cluster multi-nodo, corre en una pequeña VM en macOS o en un sandbox sin root en Linux, que arranca con el primer cluster y se detiene con el último. Cada cluster tiene su propio puerto de API, contexto de kubectl y socket de Docker.',
      },
      {
        id: 'multinode',
        label: 'Sumar máquinas',
        text: 'Un servidor corre todo el plano de control y sus propios pods; cualquier máquina Linux (una Raspberry Pi, una laptop vieja, una VM barata en la nube) se le une como nodo con el token que imprime el servidor. Cada nodo recibe un /24 de 10.42.0.0/16; los Services y el DNS funcionan en todo el cluster.',
      },
      {
        id: 'ha',
        label: 'HA',
        text: 'Tres servidores hacen al plano de control altamente disponible: el almacén de objetos se replica con Raft, sin etcd. Cualquier servidor puede caer sin perder escrituras; los nodos pasan a otro servidor, y litek8s lb le da a kubectl una sola dirección.',
      },
    ],
    docs: 'Leer la documentación',
  },
  install: {
    ...en.install,
    eyebrow: 'Instalar',
    title: 'Dos comandos hasta un cluster',
    lead: 'macOS (Apple silicon) y Linux (x86_64, aarch64). macOS no necesita nada más: ni Docker Desktop, ni Lima, ni sudo.',
    methods: en.install.methods.map((m) => (m.id === 'script' ? { ...m, label: 'Script de instalación' } : m)),
    notes: [
      'El script descarga el último release para tu sistema y arquitectura, verifica su SHA-256 e instala litek8s en ~/.local/bin.',
      'En Linux, los clusters de desarrollo corren sin root y necesitan crun, uidmap, passt (o slirp4netns), iproute2 e iptables: `litek8s dev setup` revisa todo y dice qué falta.',
      'Desde el código con cargo: Rust ≥ 1.90; macOS además necesita el guest de la VM.',
    ],
    quickstart: 'Inicio rápido',
    quick: [
      ['litek8s dev up', ''],
      ['kubectl create deployment web --image=nginx', ''],
      ['kubectl expose deployment web --type=NodePort --port 80', ''],
      ["curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')", ''],
      ['litek8s ui', 'explóralo todo'],
      ['export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock', ''],
      ['docker run --rm hello-world', 'el mismo cluster habla la API de Docker'],
      ['litek8s dev up --name ci --nodes 3', 'otro cluster, tres nodos'],
      ['litek8s dev down', 'detener; los objetos y las imágenes se conservan'],
    ],
    servers: 'En servidores Linux',
    serversText: 'Un comando por máquina, como el instalador de k3s: una unidad de systemd, el token en /etc/litek8s, pods que siguen corriendo entre reinicios y actualizaciones.',
    serversLink: 'Multi-nodo y HA',
  },
  platforms: {
    eyebrow: 'Plataformas',
    title: 'Dónde corre',
    head: ['', 'clusters de desarrollo', 'servidor / nodo', 'binario del release'],
    rows: [
      ['macOS, Apple silicon (13+)', 'sí, en una VM (Virtualization.framework)', 'solo para desarrollo (runtime Docker)', 'darwin-arm64, guest de la VM incluido'],
      ['Linux x86_64 / aarch64, sin root', 'sí, sandbox de user namespaces (kernel ≥ 5.11, cgroup v2)', '–', 'linux-x86_64, linux-aarch64 (estáticos)'],
      ['Linux, root con crun', 'sí', 'sí: la configuración prevista para servidores y nodos', 'los mismos'],
      ['macOS Intel, Windows', 'no (WSL2 sin probar)', 'no', '–'],
    ],
    note: 'Probado en Ubuntu 24.04 (arm64 y x86_64) y macOS reciente en Apple silicon. Ubuntu ≥ 23.10 necesita `sudo litek8s dev setup` una vez.',
  },
  limits: {
    eyebrow: 'Limitaciones',
    title: 'La lista honesta',
    lead: 'Vista previa temprana, hecha y probada por una persona: espera asperezas.',
    items: [
      { title: 'La autenticación y RBAC son más simples que en upstream.', text: 'Los servidores autentican clientes y aplican RBAC desde la 0.2.12, pero no hay login OIDC ni webhook ni revocación de certificados. Los clusters de desarrollo siguen abiertos en 127.0.0.1; en un servidor, igual mantén el puerto 6443 (y el de Raft, 6444) fuera de redes no confiables.', strong: true },
      { title: 'Tiene semanas.', text: 'Las APIs, flags y formatos en disco pueden cambiar entre versiones 0.x sin migración.' },
      { title: 'No todo el comportamiento del kubelet.', text: 'Sin eventos Unhealthy/BackOff; un emptyDir en memoria es un tmpfs por montaje de contenedor; ValidatingAdmissionPolicy y los matchConditions de webhooks se aceptan pero no se aplican.' },
      { title: 'Las métricas llegan hasta la plataforma.', text: 'Cubren contenedores, nodos y estado del cluster; los endpoints /metrics de las aplicaciones no se scrapean.' },
      { title: 'La VM de macOS conserva la memoria que tocó.', text: 'Un límite de Virtualization.framework; litek8s mantiene pequeño el guest a cambio.' },
      { title: 'Los servidores macOS son para desarrollo.', text: 'Usan Docker para los pods; corre servidores y nodos reales en Linux.' },
    ],
    more: 'Más en la referencia de diferencias',
    moreHref: blob('skills/litek8s/references/differences.md'),
  },
  cta: {
    title: ['Arranca un cluster', 'en un segundo.'],
    text: 'Libre y de código abierto, Apache-2.0. Comentarios e issues son muy bienvenidos.',
    primary: 'Leer la documentación',
    star: 'Estrella en GitHub',
  },
  footer: {
    tagline: 'Un Kubernetes ligero en un solo binario.',
    cols: [
      {
        title: 'Proyecto',
        links: [
          { label: 'GitHub', href: GH },
          { label: 'Releases', href: `${GH}/releases` },
          { label: 'crates.io', href: 'https://crates.io/crates/litek8s' },
          { label: 'Issues', href: `${GH}/issues` },
          { label: 'Blog', href: '/es/blog/' },
        ],
      },
      {
        title: 'Docs',
        links: [
          { label: 'Introducción', href: docs() },
          { label: 'Junto a tus clusters', href: docs('alongside') },
          { label: 'Clusters de desarrollo', href: docs('dev') },
          { label: 'API de Docker', href: docs('docker') },
          { label: 'UI de terminal', href: docs('ui') },
        ],
      },
      {
        title: 'Más',
        links: [
          { label: 'Observabilidad', href: docs('observability') },
          { label: 'Plano de control HA', href: docs('ha') },
          { label: 'MCP y agentes', href: docs('agents') },
          { label: 'Seguridad', href: blob('SECURITY.md') },
        ],
      },
    ],
    legal: 'Apache-2.0. Kubernetes es una marca registrada de The Linux Foundation; litek8s no está afiliado a ella.',
  },
};
