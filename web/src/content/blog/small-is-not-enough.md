---
title: "Small is not enough: measuring litek8s's speed, and a 13× faster list"
description: "We had numbers for memory and disk, none for speed. A load test for the API server, a list made 13× faster, and how CI now catches regressions."
date: 2026-10-30
series: speed
tags: [performance, benchmarks, api-server, ci, rust]
docs: [numbers, contributing]
translation: es/pequeno-no-basta
---

Every number we had published about litek8s was about **size**: 128 MiB of
memory with an empty cluster against k3s's 572, a 22 MiB binary, 285 MiB of
disk. Those came from `bench/run.sh`, which boots a fresh 1 GiB VM per
system and watches it idle. Nothing in the repository said how **fast** the
API server answers when kubectl, Helm, Argo CD and a few controllers all
talk to it at once.

So we set a goal: litek8s should be fast, not only small. The first step
was a way to measure it.

## Two benchmarks, no VM

Both run on a laptop with `cargo bench`, without Docker or a VM.

**`benches/micro.rs`** uses criterion on the hot paths: the object store
(create, get, update, list, WAL write and replay, a write fanned out to 100
watchers), merge and strategic patches, and JSON.

**`benches/api_load.rs`** starts a whole `apiserver` in-process, with the
WAL on disk and every controller running, and drives it over real HTTP/1.1
keep-alive connections, 32 clients at a time. Each scenario reports ops/s
and p50/p90/p99:

| scenario | what it does |
|---|---|
| `get`, `create`, `patch` | one ConfigMap per request |
| `list_all` | 1,000 ConfigMaps |
| `list_selector` | `labelSelector`: 100 of the 1,000 |
| `list_table` | what `kubectl get` asks for (a server-side Table) |
| `watch_latency` | a create until its watch event arrives |
| `schedule` | a Deployment of 100 replicas until every Pod is bound to a Node |

The first baseline, on an Apple Silicon laptop:

| scenario | ops/s | p99 |
|---|---|---|
| get | 73,600 | 0.57 ms |
| create | 23,300 | 3.8 ms |
| patch | 19,100 | 4.7 ms |
| list_selector | 865 | 91 ms |
| list_all | **507** | **33 ms** |
| list_table | 335 | 41 ms |
| watch (create → event) | | 0.25 ms |
| 100 replicas → all bound | | 275 ms total |

Writes and single reads looked good. Lists did not: 507 lists of 1,000
small objects per second, and a label selector that returns a tenth of the
objects was only twice as fast as returning all of them. Both hinted that
the cost did not depend on what was sent back.

## Where a list spent its time

The micro-benchmarks told the story. Listing from the store cost about
**1.1 µs per object**, the same at 100, 1,000 or 10,000 objects, and about
what parsing a small object from JSON costs. The store keeps every object
as compact JSON bytes. On every list the server:

1. copied the bytes out of the store,
2. **parsed each object** into a `serde_json::Value`,
3. filtered by the selectors,
4. built the List,
5. and **serialized everything back** into the JSON it started as.

Steps 2 and 5 are a round trip that ends where it began. For a built-in
resource, the stored JSON is exactly what goes on the wire.

## The fix: send the bytes you already have

Lists of built-in resources now take a separate path, `list_stored`. It
covers every list that does not ask for a Table or PartialObjectMetadata
and is not a custom resource or one of the two certificate types served in
two versions. It copies the stored bytes straight into the response:

```rust
out.extend_from_slice(br#"{"apiVersion":"#);
serde_json::to_writer(&mut out, &rt.api_version())?;
out.extend_from_slice(br#","items":["#);
for (i, (_, json)) in items.iter().enumerate() {
    if i > 0 { out.push(b','); }
    out.extend_from_slice(json);
}
// ...then "kind" and "metadata", in the order serde_json writes them
```

Selectors still need to read something. A label selector alone parses only
`metadata.labels`, into a small struct; serde skips everything else
without allocating. Field selectors, rarer, still parse the whole object.
Pagination (`limit`, `continue`) works on the store's keys and is shared
with the general path. The response is byte for byte what it was before.

| scenario | before | after |
|---|---|---|
| list_all | 507 ops/s, p99 33 ms | **6,616 ops/s, p99 2.1 ms** (×13) |
| list_selector | 865 ops/s, p99 91 ms | **5,667 ops/s, p99 11 ms** (×6.5) |
| list_table | 335 ops/s | 412 ops/s (+23%) |

`kubectl get` (the Table) still parses every object, because building its
rows needs the fields. It is the next target.

## Catching regressions without fooling ourselves

`api_load` compares each run with `bench/perf-baseline.json` and exits 1 if
a scenario loses more than 15% of its ops/s or its p99 grows by as much.
The first runs taught us three things about that comparison.

**One run is noise.** The same code, three runs in a row: patch did 22,397,
then 20,139, then 11,535 ops/s. A single run would raise false alarms, so
each scenario now runs three times and keeps the median of each figure on
its own. The tail is noisier still: on a busy machine the p99 of identical
code doubled between runs, so p99 gets twice the margin of throughput.

**The machine matters more than the code.** One comparison showed every
scenario 40% slower, including code we had not touched. The load average
was 25: other compilers were running. `api_load` now warns when the load
is above half the CPUs and says not to save a baseline.

**A baseline only holds on its own machine.** A JSON file recorded on a
laptop says nothing about a shared CI runner. So CI compares a pull
request with its base **on the same runner, in the same job**:
`bench/perf-compare.sh` checks the base out in a git worktree, runs its
`api_load` into a temporary baseline, then runs the pull request against
it. A regression beyond 20% is measured again, base then head, and the job
fails only if **the same scenario** regressed both times: our first version
failed on "any regression in each attempt", and comparing identical code it
flagged `list_selector` once and `list_all` the next time. The tables go to the job summary. The same script
runs locally:

```bash
bench/perf-compare.sh main
```

Comparing the code with itself was the best test of the comparison, and it
found a bug in the benchmark. With three runs per scenario, the second
`create` run reused the first run's names: tens of thousands of 409
AlreadyExists, counted quietly as errors next to numbers that looked
plausible. Names are now unique across runs, and any failed request counts
as a regression.

## What's next

- **`kubectl get` (Tables)**: build the rows from a partial parse, not the
  whole object.
- **Controllers under write load**: during `create`, the controllers log
  "fell behind the change feed, resyncing". A resync is a full pass over
  the store; a bigger feed buffer or cheaper events should keep them in
  step.
- **A larger store**: the scenarios run with about a thousand objects per
  namespace; the ones that scan should also run at 100,000.

Small was the first promise. Fast is now measured, and checked on every
pull request.
