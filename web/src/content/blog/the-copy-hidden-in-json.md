---
title: "kubectl get 2.4× faster: the copy hidden in json!"
description: "A profile of kubectl get's Table showed half the time going to copying values. The cause was one serde_json macro, and the fix was moving instead."
date: 2026-10-31
series: speed
tags: [performance, profiling, api-server, rust]
docs: [numbers, contributing]
translation: es/la-copia-escondida-en-json
---

The [first part](/blog/small-is-not-enough/) of this series made lists of
built-in resources 13× faster by sending the bytes the store already keeps.
One list was left out on purpose: the **Table**, what `kubectl get` asks
for. Its rows need fields read from each object, so it still parsed every
object and built a new answer. It got 23% faster and was the next target.

## Where it stood

`api_load` on 0.2.19, 1,000 ConfigMaps, 32 clients:

| scenario | ops/s | p50 |
|---|---|---|
| list_all | 5,938 | 1.3 ms |
| list_table | **364** | **20.8 ms** |

Sixteen times slower than the plain list. Parsing explains part of that:
about 1.1 µs per object, so about 1 ms for 1,000. But at 364 lists per
second on 14 cores, each Table cost tens of milliseconds of CPU. Something
else was doing most of the work.

## Looking instead of guessing

macOS ships `sample`, a sampling profiler that needs no setup. The
benchmark runs one scenario when given its name, so we left `list_table`
running for 15 seconds and sampled the process for 5:

```bash
PERF_SECS=15 PERF_RUNS=1 target/release/deps/api_load-<hash> --bench list_table &
/usr/bin/sample $! 5 -file sample.txt
```

Summing the samples under each function of the request handler:

| where | share of the handler |
|---|---|
| `table::build` | **52%** |
| `Store::list` (parsing the objects) | 21% |
| serializing the response | 6% |

The top of the stacks was `malloc`, `free`, `BTreeMap::insert` and
`Value::serialize`. Building a Table allocated and freed far more than a
table of 1,000 rows should.

## The copy you don't see

`table::build` built each row like this:

```rust
let cells: Vec<Value> = extractors.iter().map(|f| f(obj)).collect();
let mut row = json!({"cells": cells});
row["object"] = json!({
    "kind": "PartialObjectMetadata",
    "apiVersion": "meta.k8s.io/v1",
    "metadata": obj.get("metadata").cloned().unwrap_or(json!({})),
});
```

and then the table with `json!({..., "rows": rows})`.

It reads well. The catch is in serde_json's `json!` macro: a literal goes
into the value as is, but **any expression** goes through
`serde_json::to_value(&expr)`. That takes a reference, so it cannot move the
value. It serializes it into a new `Value` tree, map entry by map entry,
and the original is dropped right after. So each row's cells and metadata
were copied once, and then `"rows": rows` copied every row again, all to
build something serialized to bytes a moment later.

That is the `Value::serialize` and `BTreeMap::insert` at the top of the
profile, and most of the `free` too.

## Moving instead

The objects were already owned by the caller, so `table::build` now takes
them by value. Each row's metadata is taken out of its object (no clone),
and the rows and the table are built as maps directly:

```rust
let mut row = Map::with_capacity(2);
row.insert("cells".into(), Value::Array(cells));
// ...the object's metadata, moved out with take()
table.insert("rows".into(), Value::Array(rows));
```

serde_json's map is a `BTreeMap` (we don't enable `preserve_order`), so the
keys come out sorted however they are inserted, and the response is the
same byte for byte. The general list path had the same pattern,
`json!({"items": items})`, for custom resources and PartialObjectMetadata.
It now goes through a small `list_value` helper that moves the items in.

| scenario | before | after |
|---|---|---|
| list_table | 364 ops/s, p50 20.8 ms | **864 ops/s, p50 7.6 ms** (×2.4) |

All 700 unit and API server tests pass unchanged. It ships in 0.2.20.

## What's next

- **A Table is still about 8× slower than a plain list.** The next cost is
  the 21% that parses each whole object, `managedFields` included, when the
  columns read a handful of fields. A partial parse, as label selectors
  already do, should close more of the gap.
- **Controllers under write load** still log "fell behind the change feed,
  resyncing" during the create scenario.

The lesson is small: `json!` is for literals. When a value is already
built and owned, move it into the map yourself.
