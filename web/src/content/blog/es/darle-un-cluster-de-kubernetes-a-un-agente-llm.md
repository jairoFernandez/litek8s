---
title: "Darle un cluster de Kubernetes a un agente LLM sin miedo"
description: "litek8s mcp le da a un agente diagnose_pod, logs, PromQL y GitOps, y rechaza clusters que no son litek8s. Las barreras, y lo que no cubren."
date: 2026-10-30
tags: [llm, agentes, mcp, kubernetes, seguridad]
docs: [agents, observability, numbers, litecd]
lang: es
translation: giving-an-llm-agent-a-kubernetes-cluster
---

Darle `kubectl` a un agente de código es fácil. El problema es todo lo que
viene con eso: tu kubeconfig, que seguramente también tiene un cluster de
staging, quizá producción, quizá un login de la nube que ejecuta un exec
plugin la primera vez que se usa. Un `--context` equivocado, o ninguno, y
"limpia los pods que fallan" termina pasando donde no querías.

`litek8s mcp` es nuestra respuesta para clusters locales y de home lab: un
servidor MCP dentro del mismo binario de litek8s, con herramientas que
responden en JSON estructurado y barreras que viven en el código y no en un
prompt. Este post cuenta qué le da a un agente, qué lo frena y los huecos que
conocemos.

## Configuración

Para Claude Code:

```bash
claude mcp add litek8s -- litek8s mcp                       # all tools
claude mcp add litek8s -- litek8s mcp --context litek8s-dev # pin a default cluster
claude mcp add litek8s-ro -- litek8s mcp --read-only        # no apply/delete/cluster changes
litek8s skill --install ~/.claude/skills                    # the skill, for every project
```

Cualquier otro cliente MCP también sirve: el servidor habla MCP por stdio
(JSON-RPC, un mensaje por línea), versiones de protocolo 2025-06-18,
2025-03-26 y 2024-11-05, y se arranca con
`{"command": "litek8s", "args": ["mcp"]}`.

## Lo que recibe el agente

| necesita | herramientas |
|---|---|
| clusters donde trabajar | `clusters_list`, `cluster_status`, `cluster_up`, `cluster_down`, `cluster_delete`, `contexts_list` |
| objetos | `get` (una tabla, o un objeto con sus eventos y su cadena de dueños), `describe`, `events` |
| por qué este pod está roto | `diagnose_pod` |
| cambiar cosas | `apply` (server-side apply, `dry_run`), `delete` |
| logs | `pod_logs` (`previous`), `logs_search` (entre pods y nodos, incluidos contenedores borrados) |
| métricas y alertas | `query`, `query_range` (PromQL sobre las métricas incluidas), `alerts_list`, `alert_silence`, `alert_unsilence` |
| GitOps | `applications_list`, `application_status`, `application_diff`, `application_sync`, `application_refresh` |
| Docker en un cluster de desarrollo | `docker_ps`, `docker_images`, `docker_logs` |

La skill (`skills/litek8s/`, que imprime `litek8s skill`) le dice al agente
cuándo usar estas herramientas y cuándo basta con kubectl, con flujos de
trabajo, recetas de PromQL, en qué se diferencia litek8s del Kubernetes
upstream y cómo diagnosticar problemas. El servidor también la expone, junto
con la documentación, como recursos MCP compilados dentro del binario.

## Una llamada en vez de seis

La forma típica en que un agente depura un pod es un ciclo: `get pods`,
`describe`, `logs`, `logs --previous`, `get events`, `get deployment`, y cada
paso le vuelca texto en el contexto. `diagnose_pod` hace todo eso en una
llamada y pone la conclusión primero. Este es el ejemplo de la documentación,
recortado:

```text
> diagnose_pod {"context": "litek8s-demo", "namespace": "default", "selector": "app=web"}
{"pod": "default/web-7c9f-abcde", "ready": false, "phase": "Running",
 "findings": [{"severity": "error", "reason": "CrashLoopBackOff",
   "message": "container web keeps exiting (4 restarts); last exit code 1 (Error: application error (see its logs))",
   "next_step": "Read previous_logs below: the reason it exited is usually in its last lines."}],
 "containers": [...], "owners": [{"kind": "ReplicaSet", ...}, {"kind": "Deployment", "status": "ready 0/1"}],
 "events": [...], "logs": {"web": {"previous_logs": ["Error: DATABASE_URL is not set"]}}}
```

`findings[0]` trae la causa y el siguiente paso; el resto es la evidencia:
condiciones, estado de los contenedores, última salida, probes, si el nodo
está listo, eventos, dueños, logs actuales y anteriores. Un detalle que vale
la pena saber: el agente de nodo de litek8s no registra los eventos
`Unhealthy`, `BackOff` ni `Failed` del kubelet, así que el diagnóstico lee el
estado de los contenedores, las últimas terminaciones y la definición de las
probes en lugar de los eventos.

El repositorio prueba esto con un agente real. `compat/agents-e2e.sh` crea un
cluster de desarrollo desechable con un Deployment que se reinicia en bucle y
otro que nunca queda listo, corre Claude Code sin interfaz (`claude -p`) con
solo las herramientas MCP de litek8s permitidas y la skill instalada, y
verifica que su respuesta nombre las dos causas: `DATABASE_URL`, sacado de
los logs anteriores, y el puerto 8080 de la probe.

## Las barreras

**Solo litek8s.** Antes de la primera llamada a un API server, y de nuevo
cada minuto, el servidor lee `/version` *sin credenciales* y se niega a
seguir si `gitVersion` no lleva `+litek8s`. De ahí salen dos cosas. El exec
plugin de un kubeconfig (un login de la nube) nunca se ejecuta para un
servidor que no es litek8s. Y un contexto que apunta a otro cluster recibe
un error, no una acción. El mensaje, tal como está en el código:

```text
refusing: context prod (…) is not a litek8s API server (gitVersion …). These tools only operate litek8s clusters; use kubectl for other clusters.
```

Las herramientas de Docker hacen lo mismo: rechazan un motor cuya plataforma
en `/version` no sea `litek8s`.

**Lo destructivo son dos llamadas.** `delete`, `application_sync`,
`cluster_down` y `cluster_delete` no cambian nada sin `confirm: true`. Sin
eso responden qué pasaría. Un `delete` ilustrativo de un Deployment sin
`confirm`, con los campos que devuelve el código:

```text
> delete {"context": "litek8s-demo", "kind": "deployments", "name": "web"}
{"would_delete": "Deployment/web", "context": "litek8s-demo", "namespace": "default",
 "note": "its 1 replicas (and their pods) go with it", "confirmed": false,
 "hint": "Nothing was changed. This is destructive: call again with confirm: true once the user agrees."}
```

`application_sync` sin `confirm` lista lo que está desincronizado y lo que se
borraría (prune). Además, las herramientas llevan anotaciones MCP
(`readOnlyHint`, `destructiveHint`), para que el cliente le pregunte al
usuario antes de ejecutarlas.

**`--read-only`** deja fuera todas las herramientas que cambian algo. El
agente no puede llamar lo que no ve.

**Límite de tamaño.** Un resultado de más de `--max-output` caracteres
(20000) se corta: las listas conservan el principio, los logs y eventos lo
más reciente, y `_truncated` explica cómo acotar la consulta. Un pod con un
mega de logs no se come la ventana de contexto del agente.

**Sin canales laterales.** Los comandos de clusters de desarrollo corren como
procesos hijos con su salida capturada en el resultado, y el servidor nunca
cambia el contexto de kubectl ni el de Docker a tus espaldas.

## Clusters desechables, tan baratos que son lo normal

La regla de la skill para experimentar es: hazlo en un cluster aparte.
`cluster_up name=try nodes=1`, luego `apply`, `get` o `diagnose_pod`, y al
final `cluster_delete name=try confirm=true`. Eso solo tiene sentido porque un
cluster de desarrollo es pequeño y rápido:

| | litek8s | referencia |
|---|---|---|
| `dev up` → nodo Ready, Linux / macOS | 0.17 s / 1.28 s | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s |

Medido con los scripts de benchmark del repositorio; el detalle está en
[Números](/es/docs/numbers/). El contexto es `litek8s-<nombre>`, y
`cluster_up` lo agrega sin cambiar el actual.

## GitOps y métricas sin montar nada nuevo

El cluster ya trae métricas, logs y alertas, así que el agente responde "¿está
lento, grande, reiniciándose?" con PromQL en lugar de adivinar. Una receta de
la skill:

```text
topk(10, sum by (namespace, pod) (container_memory_working_set_bytes{container!=""}))
```

Para las Applications de Argo CD (reconciliadas por Argo CD o por litecd),
`application_status` lista los recursos que necesitan atención con sus
mensajes de salud, y `application_diff` muestra un diff unificado por recurso
entre lo vivo y git, a partir del Secret `<app>.diff` que publica su
controlador. Con Argo CD, que no publica ninguno, lo dice.

## De qué no te protege

Las barreras en código son mejores que las barreras en un prompt, pero tienen
bordes. Estos son los nuestros:

- **Solo litek8s no es solo desarrollo.** Un servidor litek8s que instalaste
  en serio con `litek8s install server` también dice `+litek8s`. Si un
  cluster así está en tu kubeconfig, las herramientas lo van a operar.
  Registra un servidor `--read-only` aparte para él, o fija `--context` al
  cluster de desarrollo.
- **La verificación no es autorización.** Una vez que un servidor pasa, las
  herramientas usan las credenciales del kubeconfig. En un servidor, lo que
  las limita es RBAC; los clusters de desarrollo siguen en AlwaysAllow sobre
  `127.0.0.1`.
- **Un agente con shell igual puede correr kubectl.** La skill le dice que no
  apunte kubectl a otros contextos y que siempre nombre el contexto que
  quiere usar. Eso es una regla en un prompt, no un muro. Si te importa,
  permite solo las herramientas MCP, como hace la prueba de punta a punta.
- **`confirm: true` está a un argumento de distancia.** La vista previa hace
  que el agente vea la consecuencia primero, y las anotaciones le permiten al
  cliente preguntarte, pero un cliente que lo aprueba todo automáticamente
  también va a aprobar esto.

## Pruébalo

```bash
brew install jairoFernandez/tap/litek8s
claude mcp add litek8s -- litek8s mcp
litek8s skill --install ~/.claude/skills
```

Después pídele a tu agente que cree un cluster, que rompa algo en él y que te
diga por qué. La lista completa de herramientas, las barreras y los comandos
con `-o json` están en [MCP y agentes LLM](/es/docs/agents/); las métricas que
puede consultar, en [Métricas, logs y alertas](/es/docs/observability/).
