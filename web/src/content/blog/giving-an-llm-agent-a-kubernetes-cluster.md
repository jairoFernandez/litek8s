---
title: "Giving an LLM agent a Kubernetes cluster without fear"
description: "litek8s mcp gives an agent diagnose_pod, logs, PromQL and GitOps tools, and refuses clusters that are not litek8s. The guardrails, and what they don't cover."
date: 2026-10-30
tags: [llm, agents, mcp, kubernetes, safety]
docs: [agents, observability, numbers, litecd]
translation: es/darle-un-cluster-de-kubernetes-a-un-agente-llm
---

Handing a coding agent `kubectl` is easy. The problem is everything else
that comes with it: your kubeconfig, which probably also holds a staging
cluster, maybe production, maybe a cloud login that runs an exec plugin on
first use. One wrong `--context`, or none, and "clean up the failing pods"
happens somewhere you did not mean.

`litek8s mcp` is our answer for local and home-lab clusters: an MCP server
in the same litek8s binary, with tools that answer in structured JSON and
guardrails that live in code rather than in a prompt. This post is what it
gives an agent, what stops it, and the holes we know about.

## Setup

For Claude Code:

```bash
claude mcp add litek8s -- litek8s mcp                       # all tools
claude mcp add litek8s -- litek8s mcp --context litek8s-dev # pin a default cluster
claude mcp add litek8s-ro -- litek8s mcp --read-only        # no apply/delete/cluster changes
litek8s skill --install ~/.claude/skills                    # the skill, for every project
```

Any other MCP client works too: the server speaks MCP over stdio (JSON-RPC,
one message per line), protocol versions 2025-06-18, 2025-03-26 and
2024-11-05, started as `{"command": "litek8s", "args": ["mcp"]}`.

## What the agent gets

| need | tools |
|---|---|
| clusters to work in | `clusters_list`, `cluster_status`, `cluster_up`, `cluster_down`, `cluster_delete`, `contexts_list` |
| objects | `get` (a table, or one object with its events and owner chain), `describe`, `events` |
| why is this pod broken | `diagnose_pod` |
| change things | `apply` (server-side apply, `dry_run`), `delete` |
| logs | `pod_logs` (`previous`), `logs_search` (across pods and nodes, deleted containers included) |
| metrics and alerts | `query`, `query_range` (PromQL on the built-in metrics), `alerts_list`, `alert_silence`, `alert_unsilence` |
| GitOps | `applications_list`, `application_status`, `application_diff`, `application_sync`, `application_refresh` |
| Docker on a dev cluster | `docker_ps`, `docker_images`, `docker_logs` |

The skill (`skills/litek8s/`, printed by `litek8s skill`) tells the agent
when to use these and when plain kubectl is fine, with workflows, PromQL
recipes, where litek8s differs from upstream, and troubleshooting. The
server also exposes it, and the docs, as MCP resources compiled into the
binary.

## One call instead of six

The usual way an agent debugs a pod is a loop: `get pods`, `describe`,
`logs`, `logs --previous`, `get events`, `get deployment`, each one dumping
text into its context. `diagnose_pod` does that in one call and puts the
conclusion first. This is the example from the docs, trimmed:

```text
> diagnose_pod {"context": "litek8s-demo", "namespace": "default", "selector": "app=web"}
{"pod": "default/web-7c9f-abcde", "ready": false, "phase": "Running",
 "findings": [{"severity": "error", "reason": "CrashLoopBackOff",
   "message": "container web keeps exiting (4 restarts); last exit code 1 (Error: application error (see its logs))",
   "next_step": "Read previous_logs below: the reason it exited is usually in its last lines."}],
 "containers": [...], "owners": [{"kind": "ReplicaSet", ...}, {"kind": "Deployment", "status": "ready 0/1"}],
 "events": [...], "logs": {"web": {"previous_logs": ["Error: DATABASE_URL is not set"]}}}
```

`findings[0]` is cause and next step; the rest is the evidence: conditions,
container states, last exit, probes, node readiness, events, owners, current
and previous logs. One detail worth knowing: litek8s's node agent does not
record kubelet's `Unhealthy`, `BackOff` or `Failed` events, so the diagnosis
reads container states, last terminations and probe specs instead of
events.

The repository tests this with a real agent. `compat/agents-e2e.sh` makes a
throwaway dev cluster with a crash-looping and a never-ready Deployment,
runs Claude Code headless (`claude -p`) with only the litek8s MCP tools
allowed and the skill installed, and checks that its answer names both
causes: `DATABASE_URL` from the previous logs, and the probe's port 8080.

## The guardrails

**litek8s only.** Before the first call to an API server, and again every
minute, the server reads `/version` *without credentials* and refuses
unless `gitVersion` carries `+litek8s`. Two things follow. A kubeconfig's
exec plugin (a cloud login) never runs for a server that is not litek8s.
And a context pointing at another cluster gets an error, not an action. The
message, from the code:

```text
refusing: context prod (…) is not a litek8s API server (gitVersion …). These tools only operate litek8s clusters; use kubectl for other clusters.
```

The Docker tools do the same: they refuse an engine whose `/version`
platform is not `litek8s`.

**Destructive means two calls.** `delete`, `application_sync`,
`cluster_down` and `cluster_delete` change nothing without `confirm: true`.
Without it they answer what would happen. An illustrative `delete` of a
Deployment without `confirm`, with the fields the code returns:

```text
> delete {"context": "litek8s-demo", "kind": "deployments", "name": "web"}
{"would_delete": "Deployment/web", "context": "litek8s-demo", "namespace": "default",
 "note": "its 1 replicas (and their pods) go with it", "confirmed": false,
 "hint": "Nothing was changed. This is destructive: call again with confirm: true once the user agrees."}
```

`application_sync` without `confirm` lists what is out of sync and what
would be pruned. The tools also carry MCP annotations (`readOnlyHint`,
`destructiveHint`), so clients can ask the user before running them.

**`--read-only`** leaves out every tool that changes something. The agent
cannot call what it cannot see.

**Output caps.** A result over `--max-output` characters (20000) is cut:
lists keep their head, logs and events their newest entries, and
`_truncated` says how to narrow the request. A pod with a megabyte of logs
does not eat the agent's context window.

**No side channels.** Dev cluster commands run as child processes with
their output captured into the result, and the server never switches the
kubectl context or the Docker context behind your back.

## Scratch clusters are cheap enough to be the default

The skill's rule for experiments is: do them in their own cluster.
`cluster_up name=try nodes=1`, then `apply`, `get` or `diagnose_pod`, then
`cluster_delete name=try confirm=true`. That is only reasonable because a
dev cluster is small and fast:

| | litek8s | reference |
|---|---|---|
| `dev up` → node Ready, Linux / macOS | 0.17 s / 1.28 s | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s |

Measured with the repository's bench scripts; setup in
[Numbers](/docs/numbers/). The context is `litek8s-<name>`, and
`cluster_up` adds it without switching the current one.

## GitOps and metrics without new plumbing

The cluster already has metrics, logs and alerts built in, so the agent
answers "is it slow, big, restarting?" with PromQL instead of guessing. A
recipe from the skill:

```text
topk(10, sum by (namespace, pod) (container_memory_working_set_bytes{container!=""}))
```

For Argo CD Applications (reconciled by Argo CD or by litecd),
`application_status` lists the resources that need attention with their
health messages, and `application_diff` shows a unified diff per resource
of live against git, from the Secret `<app>.diff` its controller publishes.
With Argo CD, which publishes none, it says so.

## What it does not protect you from

Guardrails in code are better than guardrails in a prompt, but they have
edges. These are ours:

- **litek8s-only is not dev-only.** A litek8s server you installed for real
  with `litek8s install server` also says `+litek8s`. If such a cluster is
  in your kubeconfig, the tools will operate it. Register a separate
  `--read-only` server for it, or pin `--context` to the dev cluster.
- **The check is not authorization.** Once a server passes, the tools use
  the kubeconfig's credentials. On a server, RBAC is what limits them; dev
  clusters stay AlwaysAllow on `127.0.0.1`.
- **An agent with a shell can still run kubectl.** The skill tells it not to
  point kubectl at other contexts and to always name the context it means.
  That is a rule in a prompt, not a wall. If that matters, allow only the
  MCP tools, as the end-to-end test does.
- **`confirm: true` is one argument away.** The preview makes the agent see
  the consequence first, and the annotations let the client ask you, but a
  client that auto-approves everything will approve this too.

## Try it

```bash
brew install jairoFernandez/tap/litek8s
claude mcp add litek8s -- litek8s mcp
litek8s skill --install ~/.claude/skills
```

Then ask your agent to create a cluster, break something on it, and tell
you why. The full tool list, guardrails and `-o json` commands are in
[MCP and LLM agents](/docs/agents/); the metrics it can query in
[Metrics, logs, alerts](/docs/observability/).
