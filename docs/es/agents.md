# litek8s para agentes LLM

Un agente que conoce Kubernetes ya conoce la mayor parte de litek8s: sirve la
API real, así que kubectl y Helm funcionan. Lo que no puede adivinar es la
superficie propia de litek8s (clusters de desarrollo, el socket de Docker, las
métricas/logs/alertas integradas, HA y la unión de nodos) ni en qué se diferencia
litek8s de upstream. Tres piezas cubren eso:

1. **`litek8s mcp`**: un servidor MCP en el mismo binario, con herramientas que
   responden en JSON estructurado y con salvaguardas.
2. **La skill para agentes** (`skills/litek8s/`, que imprime `litek8s skill`):
   cuándo usar litek8s y cuándo kubectl, flujos de trabajo, recetas de PromQL,
   diferencias con upstream, resolución de problemas.
3. **`-o json`** en los comandos propios de litek8s: `dev status`, `dev list`,
   `query`, `alerts` (y sus subcomandos), `logs`.


El sitio web sirve [`/llms.txt`](https://litek8s.dev/llms.txt) (cada página de
la documentación con su descripción y un enlace a su markdown) y
[`/llms-full.txt`](https://litek8s.dev/llms-full.txt) (todas ellas en un solo
archivo), para agentes que buscan documentación en la web en lugar de correr el
servidor MCP.

## Claude Code

```bash
claude mcp add litek8s -- litek8s mcp                       # all tools
claude mcp add litek8s -- litek8s mcp --context litek8s-dev # pin a default cluster
claude mcp add litek8s-ro -- litek8s mcp --read-only        # no apply/delete/cluster changes
litek8s skill --install ~/.claude/skills                    # the skill, for every project
litek8s skill --install .claude/skills                      # or for this project only
```

Otros clientes MCP: el servidor habla MCP por stdio (JSON-RPC, un mensaje por
línea), versiones de protocolo 2025-06-18, 2025-03-26 y 2024-11-05:
`{"command": "litek8s", "args": ["mcp"]}`.

## Herramientas

| herramienta | qué hace | cambia cosas |
|---|---|---|
| `clusters_list`, `cluster_status` | clusters de desarrollo: en ejecución, servidor, contexto, socket de Docker, nodos, pods, memoria | |
| `cluster_up` | arranca/crea un cluster de desarrollo (`nodes`, o un worker detenido con `node`) y espera a que esté Ready; agrega el contexto `litek8s-<name>` sin cambiar de contexto | sí |
| `cluster_down`, `cluster_delete` | detiene / elimina un cluster de desarrollo (o detiene un worker) | sí, `confirm` |
| `contexts_list` | contextos del kubeconfig; `probe: true` indica cuáles son de litek8s | |
| `get` | una tabla como la de `kubectl get` (columnas del servidor), o un objeto con sus eventos y su cadena de owners | |
| `describe` | el texto de `kubectl describe` más la cadena de owners | |
| `events` | eventos de un namespace o de un objeto, el más reciente al final | |
| `diagnose_pod` | por qué un pod no está Ready: hallazgos con causa y siguiente paso, condiciones, estados de los contenedores, última salida, probes, disponibilidad del nodo, eventos, owners, logs actuales y anteriores | |
| `apply` | server-side apply de YAML/JSON (texto o archivo), `dry_run` | sí |
| `delete` | elimina un objeto | sí, `confirm` |
| `pod_logs` | las últimas líneas de un contenedor (`previous`) | |
| `logs_search` | la búsqueda de logs integrada en pods y nodos, incluidos los contenedores eliminados | |
| `query`, `query_range` | PromQL sobre las métricas integradas | |
| `alerts_list`, `alert_silence`, `alert_unsilence` | alertas y silencios | silencios |
| `applications_list`, `application_status` | Applications de Argo CD (`argoproj.io`, sin importar qué controlador las reconcilie): sync, health, revisión, última operación, condiciones; los recursos de una app que están fuera de sincronía, no están sanos o se van a podar, con sus mensajes de health (leídos de los objetos en vivo cuando el status no los tiene, como con Argo CD v3: `health_from: live object`), los recursos fallidos de su operación, el historial y los eventos | |
| `application_diff` | qué difiere de git, por recurso: un diff unificado (`-` en vivo, `+` git) de los campos que define git, a partir del Secret `<app>.diff` que publica su controlador (ver docs/ui.md); con Argo CD, que no publica ninguno, lo indica | |
| `application_sync` | escribe la operación de sync que escribe la CLI argocd (las opciones de sync de la app, `prune`, `revision`), `wait_seconds` para esperar su resultado; sin `confirm` lista lo que está fuera de sincronía y lo que se podaría | sí, `confirm` |
| `application_refresh` | la anotación de refresh (también `hard`); responde una vez que el controlador la tomó | sí |
| `docker_ps`, `docker_images`, `docker_logs` | la API de Docker de un cluster de desarrollo | |

Toda herramienta que habla con un cluster recibe `context` (por defecto:
`--context`, si no, como `litek8s ui`: el contexto actual de `$KUBECONFIG`, y si
no, el cluster de desarrollo por defecto en ejecución). Las herramientas de
Docker reciben `cluster`.

Recursos: `litek8s://skill/SKILL.md`, `litek8s://skill/references/*.md` y
`litek8s://docs/*.md` (este archivo, dev, docker, observability, ha, crds, ui,
CONFORMANCE), compilados dentro del binario.

## Salvaguardas

- **Solo litek8s.** Antes de la primera llamada a un API server (y de nuevo
  cada minuto) el servidor lee `/version` *sin credenciales* y se niega a
  continuar a menos que `gitVersion` incluya `+litek8s`. Por lo tanto, el plugin
  exec de un kubeconfig (logins de la nube) nunca se ejecuta para un servidor que
  no es litek8s, y un contexto que apunta a un cluster de producción se rechaza
  con un mensaje que dice que uses kubectl. Las herramientas de Docker rechazan
  un engine cuya plataforma en `/version` no sea `litek8s`.
- **Confirmación.** `delete`, `application_sync`, `cluster_down` y
  `cluster_delete` no cambian nada sin `confirm: true`; sin eso responden qué
  pasaría (`would_delete`, el status del cluster). Las herramientas llevan
  anotaciones MCP (`readOnlyHint`, `destructiveHint`) para que los clientes
  puedan preguntarle al usuario.
- **`--read-only`** deja fuera todas las herramientas que cambian algo.
- **Tamaño.** Un resultado de más de `--max-output` caracteres (20000) se
  recorta: las listas conservan el principio, los logs y los eventos sus
  entradas más recientes, y `_truncated` dice cómo acotar la petición.
- **Nada se filtra a stdout.** Los comandos de clusters de desarrollo corren
  como procesos hijos con su salida capturada en el resultado.
- El servidor nunca cambia el contexto de kubectl ni el contexto de docker.

## Ejemplo: un pod roto

```text
> diagnose_pod {"context": "litek8s-demo", "namespace": "default", "selector": "app=web"}
{"pod": "default/web-7c9f-abcde", "ready": false, "phase": "Running",
 "findings": [{"severity": "error", "reason": "CrashLoopBackOff",
   "message": "container web keeps exiting (4 restarts); last exit code 1 (Error: application error (see its logs))",
   "next_step": "Read previous_logs below: the reason it exited is usually in its last lines."}],
 "containers": [...], "owners": [{"kind": "ReplicaSet", ...}, {"kind": "Deployment", "status": "ready 0/1"}],
 "events": [...], "logs": {"web": {"previous_logs": ["Error: DATABASE_URL is not set"]}}}
```

El agente de nodo de litek8s registra los eventos Pulled/Created/Started, pero
no los `Unhealthy`, `BackOff` o `Failed` del kubelet, así que el diagnóstico lee
los estados de los contenedores, las últimas terminaciones y las especificaciones
de los probes en lugar de los eventos.

## `-o json`

```bash
litek8s dev list -o json            # {"clusters": [...], "environment": {...}}
litek8s dev status --name ci -o json
litek8s query 'up' -o json          # the Prometheus API's data object
litek8s alerts -o json              # active alerts (--all for resolved too)
litek8s alerts silences -o json
litek8s logs -l app=web --since 1h -o json   # one JSON object per line
```

## Pruebas

Pruebas unitarias junto al código (`src/mcp/*`: esquemas, salvaguardas,
recorte, reglas de diagnóstico); `tests/mcp.rs` maneja `litek8s mcp` por stdio
contra un API server dentro del mismo proceso (sin Docker), incluido el rechazo
de un servidor que no es litek8s. `compat/agents-e2e.sh` es el ciclo completo
con un agente real: un cluster de desarrollo desechable con un Deployment en
crash loop y otro que nunca está listo, Claude Code en modo headless
(`claude -p`, con solo las herramientas MCP de litek8s permitidas y la skill
instalada), y una verificación de que su respuesta nombra ambas causas
(`DATABASE_URL` en los logs anteriores, el puerto 8080 del probe).
