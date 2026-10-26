# litek8s for LLM agents

An agent that knows Kubernetes already knows most of litek8s: it serves the
real API, so kubectl and Helm work. What it cannot guess is litek8s's own
surface (dev clusters, the Docker socket, built-in metrics/logs/alerts, HA
and node join) and where litek8s differs from upstream. Three pieces cover
that:

1. **`litek8s mcp`**: an MCP server in the same binary, with tools that
   answer in structured JSON and guardrails.
2. **The agent skill** (`skills/litek8s/`, printed by `litek8s skill`):
   when to use litek8s vs kubectl, workflows, PromQL recipes, differences
   from upstream, troubleshooting.
3. **`-o json`** on the litek8s-specific commands: `dev status`, `dev list`,
   `query`, `alerts` (and its subcommands), `logs`.

## Claude Code

```bash
claude mcp add litek8s -- litek8s mcp                       # all tools
claude mcp add litek8s -- litek8s mcp --context litek8s-dev # pin a default cluster
claude mcp add litek8s-ro -- litek8s mcp --read-only        # no apply/delete/cluster changes
litek8s skill --install ~/.claude/skills                    # the skill, for every project
litek8s skill --install .claude/skills                      # or for this project only
```

Other MCP clients: the server speaks MCP over stdio (JSON-RPC, one message
per line), protocol versions 2025-06-18, 2025-03-26 and 2024-11-05:
`{"command": "litek8s", "args": ["mcp"]}`.

## Tools

| tool | what | changes things |
|---|---|---|
| `clusters_list`, `cluster_status` | dev clusters: running, server, context, Docker socket, nodes, pods, memory | |
| `cluster_up` | start/create a dev cluster (`nodes`, or one stopped worker with `node`), wait for Ready; adds context `litek8s-<name>` without switching | yes |
| `cluster_down`, `cluster_delete` | stop / delete a dev cluster (or stop one worker) | yes, `confirm` |
| `contexts_list` | kubeconfig contexts; `probe: true` tells which are litek8s | |
| `get` | a table like `kubectl get` (server columns), or one object with its events and owner chain | |
| `describe` | `kubectl describe` text plus the owner chain | |
| `events` | events of a namespace or object, newest last | |
| `diagnose_pod` | why a pod is not Ready: findings with cause and next step, conditions, container states, last exit, probes, node readiness, events, owners, current and previous logs | |
| `apply` | server-side apply of YAML/JSON (text or file), `dry_run` | yes |
| `delete` | delete one object | yes, `confirm` |
| `pod_logs` | one container's last lines (`previous`) | |
| `logs_search` | the built-in log search across pods and nodes, including deleted containers | |
| `query`, `query_range` | PromQL on the built-in metrics | |
| `alerts_list`, `alert_silence`, `alert_unsilence` | alerts and silences | silences |
| `applications_list`, `application_status` | Argo CD Applications (`argoproj.io`, whichever controller reconciles them): sync, health, revision, last operation, conditions; one app's resources that are out of sync, not healthy or to be pruned, with health messages (read from the live objects where the status has none, as with Argo CD v3: `health_from: live object`), its operation's failed resources, history and events | |
| `application_diff` | what differs from git, per resource: a unified diff (`-` live, `+` git) of the fields git sets, from the Secret `<app>.diff` its controller publishes (see docs/ui.md); with Argo CD, which publishes none, it says so | |
| `application_sync` | write the sync operation the argocd CLI writes (the app's sync options, `prune`, `revision`), `wait_seconds` for its result; without `confirm` it lists what is out of sync and what would be pruned | yes, `confirm` |
| `application_refresh` | the refresh annotation (`hard` too); answers once the controller has taken it | yes |
| `docker_ps`, `docker_images`, `docker_logs` | a dev cluster's Docker API | |

Every tool that talks to a cluster takes `context` (default: `--context`,
else like `litek8s ui`: `$KUBECONFIG`'s current context, else the running
default dev cluster). Docker tools take `cluster`.

Resources: `litek8s://skill/SKILL.md`, `litek8s://skill/references/*.md`
and `litek8s://docs/*.md` (this file, dev, docker, observability, ha, crds,
ui, CONFORMANCE), compiled into the binary.

## Guardrails

- **litek8s only.** Before the first call to an API server (and again
  every minute) the server reads `/version` *without credentials* and
  refuses unless `gitVersion` carries `+litek8s`. A kubeconfig's exec
  plugin (cloud logins) therefore never runs for a server that is not
  litek8s, and a context pointing at a production cluster is refused with
  a message saying to use kubectl. Docker tools refuse an engine whose
  `/version` platform is not `litek8s`.
- **Confirmation.** `delete`, `application_sync`, `cluster_down` and `cluster_delete` change
  nothing without `confirm: true`; without it they answer what would happen
  (`would_delete`, the cluster's status). The tools carry MCP annotations
  (`readOnlyHint`, `destructiveHint`) so clients can ask the user.
- **`--read-only`** leaves out every tool that changes something.
- **Size.** A result over `--max-output` characters (20000) is cut: lists
  keep their head, logs and events their newest entries, and `_truncated`
  says how to narrow the request.
- **No stdout leaks.** Dev cluster commands run as child processes with
  their output captured into the result.
- The server never switches the kubectl context or the docker context.

## Example: a broken pod

```text
> diagnose_pod {"context": "litek8s-demo", "namespace": "default", "selector": "app=web"}
{"pod": "default/web-7c9f-abcde", "ready": false, "phase": "Running",
 "findings": [{"severity": "error", "reason": "CrashLoopBackOff",
   "message": "container web keeps exiting (4 restarts); last exit code 1 (Error: application error (see its logs))",
   "next_step": "Read previous_logs below: the reason it exited is usually in its last lines."}],
 "containers": [...], "owners": [{"kind": "ReplicaSet", ...}, {"kind": "Deployment", "status": "ready 0/1"}],
 "events": [...], "logs": {"web": {"previous_logs": ["Error: DATABASE_URL is not set"]}}}
```

litek8s's node agent records Pulled/Created/Started events but not
kubelet's `Unhealthy`, `BackOff` or `Failed`, so the diagnosis reads
container states, last terminations and probe specs rather than events.

## `-o json`

```bash
litek8s dev list -o json            # {"clusters": [...], "environment": {...}}
litek8s dev status --name ci -o json
litek8s query 'up' -o json          # the Prometheus API's data object
litek8s alerts -o json              # active alerts (--all for resolved too)
litek8s alerts silences -o json
litek8s logs -l app=web --since 1h -o json   # one JSON object per line
```

## Tests

Unit tests next to the code (`src/mcp/*`: schemas, guardrails, truncation,
diagnosis rules); `tests/mcp.rs` drives `litek8s mcp` over stdio against an
in-process API server (no Docker), including the refusal of a non-litek8s
server. `compat/agents-e2e.sh` is the whole loop with a real agent: a
throwaway dev cluster with a crash-looping and a never-ready Deployment,
Claude Code headless (`claude -p`, only the litek8s MCP tools allowed, the
skill installed), and a check that its answer names both causes
(`DATABASE_URL` from the previous logs, the probe's port 8080).
