---
title: "Falling behind: why a Deployment waited 4.7 seconds for its controller"
description: "Under a burst of writes, litek8s's controllers fell behind the change feed and spent seconds starting over. How we caught up from history instead."
date: 2026-11-14
series: speed
tags: [performance, controllers, api-server, rust]
docs: [numbers, contributing]
translation: es/quedarse-atras
---

The [second part](/blog/the-copy-hidden-in-json/) of this series ended
with a loose thread: during the benchmark's `create` scenario, the log
filled with one line, over and over:

```text
controllers: fell behind the change feed, resyncing
```

Every request in that scenario succeeded, and throughput looked fine. This
post is about what that line cost, which the benchmark was not measuring.

## Where it stood

`cargo bench --bench api_load` on main, on a laptop: 32 clients against an
in-process API server with its WAL on disk and its controllers running.

| scenario | ops/s | p99 | max |
|---|---|---|---|
| get | 104,673 | 0.41 ms | 4.3 ms |
| patch | 26,809 | 2.06 ms | 16.4 ms |
| create | 26,384 | 2.42 ms | 81.5 ms |

And 326 of those log lines in one run, 304 of them during `patch`. The max
of `create`, 81.5 ms against a p99 of 2.4 ms, was the other loose end.

## How controllers follow the store

Every write to litek8s's store goes to a change feed: a tokio broadcast
channel that keeps the last 256 events. Watches and the controllers read
it. A reader that is more than 256 events behind gets `Lagged` and loses
its place.

The store also keeps a history, the last 4,096 changes, so that a watch
can resume from a resourceVersion. Watches already used it when they
lagged: they caught up from the history and carried on. The controllers
did not. Their loop did this:

```rust
Some(Err(RecvError::Lagged(_))) => break, // ...and start over
```

Starting over meant a resync: queue every object a controller owns, then
two passes of garbage collection, one for objects whose owners are gone
and one for deletions left half done. Both parsed **every object in the
store** into a `serde_json::Value`, just to read its uid, ownerReferences
and finalizers.

## Timing the resync

We added a timer around it and ran the `create` scenario, which grows the
store to about 100,000 ConfigMaps:

```text
TMPDBG resync 280.99ms
TMPDBG resync 318.97ms
TMPDBG resync 435.56ms
TMPDBG resync 647.73ms
TMPDBG resync 875.47ms
TMPDBG resync 1.02s
TMPDBG resync 1.33s
TMPDBG resync 1.91s
```

At 26,000 writes a second, the 4,096 changes in the history cover about
150 ms. A resync took 0.3 to 1.9 seconds. So by the time it finished, the
controllers were behind again, with no history left to catch up from, and
started another one. They spent the burst resyncing.

What that means for a user: while a resync runs, controllers react to
nothing. To measure it, we added a scenario to the benchmark,
`rollout_under_load`: while 32 clients create ConfigMaps, create a
Deployment and time how long until its ReplicaSet exists. On main:

```text
rollout_under_load    (ReplicaSet after 4754 ms, median of 3; worst 5579 ms)
```

**4.7 seconds** for a step that takes a few milliseconds on an idle
cluster. The resync also ran on a tokio worker thread, holding it the
whole time, and every request scheduled on that worker waited too.

## Catching up instead of starting over

The first change is the one watches already had. The loop remembers the
last resourceVersion it saw, and on `Lagged` asks the store for what came
after it:

```rust
if lagged {
    let Ok(caught_up) = manager.api.store().subscribe(Some(last_rv)) else { break 'feed };
    live = caught_up.live;
    for event in &caught_up.backlog {
        manager.observe_at(event, &mut last_rv);
    }
}
```

Only when the history no longer reaches back (`subscribe` returns
`Gone`) does it take a full resync. Full resyncs per run went from 326 to
25.

## Reading only what garbage collection needs

The remaining resyncs still took too long. Garbage collection only needs
four things from each object, so it now reads only those, with narrow
serde types that skip the rest of the JSON without building it:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    uid: Option<String>,
    owner_references: Option<Vec<Reference>>, // only their uid
    finalizers: Option<Vec<String>>,
    deletion_timestamp: Option<serde::de::IgnoredAny>,
}
```

The two passes became one, which also feeds the garbage collection that
runs after every deletion. That pass used to parse a whole namespace, and
scanned the store once per namespace when a cluster-scoped owner went
away. A resync over 100,000 objects went from 0.3–1.9 s to 0.03–0.25 s.

That is still longer than the history covers at 26,000 writes a second.
So the resync's garbage collection now runs on a blocking thread
(`spawn_blocking`), and the loop keeps reading the feed while it works.
When it finishes, the loop queues the objects it found waiting for
finalization.

## The 81 ms that was not the controllers

The `create` max did not move with these changes. A timer on the WAL's
compaction found it:

```text
TMPDBG compaction 101744 objects 83.78ms
```

The store's WAL is a file of every change, rewritten as a snapshot every
~20,000 writes. The rewrite wrote and `fsync`ed every object while holding
the store's lock, and every request needs that lock. With 1,000 objects it
took 6 ms. With 100,000 objects it took 84 ms, and
every request waited for it.

Now, under the lock, compaction only takes the entries: each key and a
pointer to the bytes the store already keeps (`Arc<[u8]>`, no copy). A
thread writes and syncs the snapshot. Meanwhile, writes keep going to the
old WAL, which stays complete if the process dies, and are also kept in a
buffer. The first write after the snapshot is done appends that buffer to
it and renames it over the old WAL. With 101,744 objects, the lock is
held for about 11 ms to take the entries and 2 ms for the swap. A new test
writes during a compaction, restarts, and checks that every write is
there.

## The numbers

Main and this change, alternated three times on the same laptop with the
same benchmark:

| | main | now |
|---|---|---|
| `rollout_under_load`, median | 4,712–4,889 ms | **75–174 ms** |
| full resyncs per run | 285–328 | 41–46 |
| `create`, max | 83–139 ms | **26–30 ms** |
| `create`, p99.9 | 14.9–17.4 ms | 14.6–17.1 ms |
| get, patch, create, lists | | same ops/s, within noise |

Throughput did not change, and was never the problem. What changed is the
time controllers spend not reacting, 27 to 65 times less, and the longest
pause of a request. All 773 tests pass. It ships in the next release.

## What's next

- **41–46 full resyncs remain** per run, during the heaviest bursts. Each
  is now cheap and runs off the loop, but the history could be sized by
  write rate instead of by count.
- **A burst of 26,000 writes a second is not what a home lab sees.** It is
  what a Helm install of a large chart, or an operator's resync, looks like
  for a few seconds. And on a Raspberry Pi with an SD card, an `fsync` can
  take far longer than on a laptop's SSD. Measuring there is next.

The lesson: a benchmark that measures requests sees the API server. It
does not see whether the controllers behind it are still doing their work.
That needed a scenario of its own.
