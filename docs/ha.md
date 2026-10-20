# High availability: several API servers

`litek8s apiserver` can run as a cluster of 3 (or 5) servers, like k3s with
embedded etcd: any one server can die without losing writes or availability.
Measurements are in [bench/HA.md](../bench/HA.md).

## Running it

```bash
# The first server starts the cluster (a single server's objects move into it).
# Servers reach each other on the API port and the Raft port (API port + 1).
litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 \
  --node-ip 192.168.1.10 --cluster-init

# Two more join it with the token the first prints (also in <data-dir>/join-token).
litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 \
  --node-ip 192.168.1.11 --server https://192.168.1.10:6443 --token K10...::...

# Agents join any server and learn the others.
litek8s agent --join https://192.168.1.10:6443 --token K10...::...

litek8s server list --server http://192.168.1.10:6443
litek8s server remove lk3 --server https://192.168.1.10:6443 --token K10...::...
```

Flags are only needed the first time: a data directory that belongs to a
cluster restarts as that member (`--cluster-init` and `--server` are ignored
then). Remove a server (`litek8s server remove`) before reinstalling it with
an empty data directory; a server that comes back with a fresh data
directory under the same name or address replaces its old identity.

**One address for clients.** kubectl takes one server per cluster. Run the
built-in load balancer where the clients are (a laptop, or each agent):

```bash
litek8s lb --listen 127.0.0.1:6443 --server http://192.168.1.10:6443
kubectl --server http://127.0.0.1:6443 get nodes
```

It learns every server from the one given, checks each one's `/readyz`
every second and passes whole TCP connections (HTTP or TLS) to a ready one,
staying with the same server while it is ready. A virtual IP (keepalived)
or a DNS name with every server's address also work: every server serves
the whole API, and `/readyz` says whether it may be used.

Agents need nothing of that: they get the server list when they join and on
every connection, keep it in `<node-dir>/servers.json`, probe the server in
use every 2 s and move to the next one when it is unreachable or not ready.
Pods reach the API at the `kubernetes` Service, whose endpoints are every
live server (each renews a Lease `kube-system/litek8s-apiserver-<name>`; the
leader publishes the endpoints from the fresh ones, as kube-apiserver's
lease endpoint reconciler does).

## How it works

```
 client ─┐                       ┌──────── leader ───────┐
 kubectl │  write  ┌─ follower ─┐│ HTTP handler          │
         └───────► │ forward ───┼┼► admission, defaults  │
                   │            ││   store.update ──► Raft log ──► followers
                   │ wait until ││                       │          (append,
                   │ applied ◄──┼┼── answer + log index  │           fsync)
                   └────────────┘└───────────────────────┘
```

- **Replicated store.** The object store (`src/kube/store.rs`) is unchanged
  in single-server mode (WAL on disk, same performance and format). In HA mode
  every mutation becomes a self-contained [`Mutation`] (create / update /
  delete, the object, the resourceVersion precondition) appended to the Raft
  log (openraft). Each server's state machine applies the log to its
  in-memory store, checking the precondition there, so every server makes the
  same decision, holds the same objects with the same resourceVersions and
  records the same watch history. List, get and watch semantics, the
  resourceVersion format and the 410/resume behaviour are the same as on a
  single server.
- **resourceVersion** stays the store's counter of applied mutations, which
  is a deterministic function of the log: cluster-wide monotonic, and watches
  resume from any server with the same number. (Using the Raft log index
  instead would have left gaps for failed preconditions, blank and
  membership entries, and changed the single-server numbering.)
- **Log and snapshots.** The log is on disk in CRC-checked segment files,
  appended and made durable by a flusher thread that syncs every append that
  queued up meanwhile with one `fdatasync` (group commit); nothing is
  acknowledged before it is durable on a majority. Every 20 000 entries a
  snapshot (the WAL's record format with a header) replaces the log before it
  (2 000 entries are kept so a briefly absent follower catches up from the
  log). On start a server loads its newest snapshot and replays the log.
  `--cluster-init` on a data directory with a single server's `objects.wal`
  moves those objects into the first snapshot.
- **Writes on a follower.** Client write requests (POST/PUT/PATCH/DELETE) are
  forwarded whole to the leader, so the in-memory allocators (ClusterIPs,
  NodePorts, pod CIDRs) and admission run in one place. Writes a follower
  makes itself (node status from its agents, leases) are forwarded as
  proposals. Either way the follower answers only once it has applied the
  entry (the leader returns its applied index), so a client that writes and
  then reads through the same server sees its write. Proposals carry a
  (session, seq) id and the state machine remembers outcomes until the
  proposer acknowledges them, so a proposal retried after the leader died
  between committing and answering is applied once.
- **Reads.** Gets, lists and watches are served from the local store. With
  `resourceVersion=""` (the default, "the most recent state") they pass a
  read barrier first: the leader confirms with a heartbeat round that it
  still leads and names its commit index (ReadIndex), and the server waits
  until it has applied that far. That is the linearizable read kube-apiserver
  gets from etcd (`--consistent-list-from-cache` does the same thing:
  quorum-read the revision, then serve from the cache). Concurrent readers
  share one heartbeat round. `resourceVersion="0"` reads whatever the server
  has (no barrier), and `resourceVersion=N` waits up to 3 s for the server to
  reach N (else `504 Too large resource version`, as kube-apiserver).
  Write handlers read the current object first (to answer 404, merge a
  patch), so a write handled by a server also passes the barrier: without it
  a deposed leader cut off from the others answered 404 from a stale state
  (the linearizability check found that).
- **Singletons.** The controllers, the scheduler, node lifecycle, Event
  expiry, the bootstrap objects and the API endpoints reconciler run only on
  the leader, started after it confirmed its term and applied everything
  before it, and stopped when it loses leadership. Their writes are never
  forwarded: if the server is no longer the leader they fail, so a deposed
  leader's controllers cannot act on stale state. Every server runs the
  discovery side of API aggregation (each needs the aggregated types in its
  registry); only the leader writes APIService status.
- **Streams.** `logs`, `exec`, `attach`, `port-forward` and the pod proxy
  are relayed to the server the pod's node is connected to (the node's Lease
  names it); webhooks and aggregated API servers on such nodes are reached
  through a server-to-server tunnel to that node's agent connection.
- **Transport.** Servers talk HTTPS pinned to the cluster CA and
  authenticated with the join secret (the token): Raft (RPCs, proposals,
  read indexes) on a listener of its own, the API port + 1 by default
  (`--raft-port`), forwarded requests on the API port. Raft, its
  connections and its listener run on two threads of their own, so a burst
  of API work on a small server never delays heartbeats; a Raft request
  openraft stopped waiting for still completes in the background so its
  connection is reused rather than dialed again. A joining server
  fetches the CA (checked against the token's hash), the serving certificate
  and the service account key from the server it joins. As in k3s by
  default, the agent token is the server token: anyone holding it can join
  either.
- **Timers.** Heartbeats every 300 ms; a follower calls an election after
  the leader lease (3 s) plus a random 1.5–3 s without one, so a dead leader
  is replaced in about 5 s (bench/HA.md: 4.8–5.6 s of write unavailability
  per leader kill). openraft 0.9 also gives every AppendEntries one
  heartbeat interval to complete (requests are capped at 512 KiB so big
  objects go a few at a time). On fast dedicated disks and a wired network
  `LITEK8S_RAFT_HEARTBEAT_MS=100` on every server gives ~2 s failovers (see
  Disks below for why that is not the default).

## Consistency model

| Request | Served by | Guarantee |
|---|---|---|
| create / update / patch / delete | the leader (forwarded) | linearizable; a 2xx means committed on a majority |
| get / list / watch, `resourceVersion=""` | any server, after the read barrier | linearizable |
| get / list, `resourceVersion="0"` | any server | may be stale (as kube-apiserver's watch cache) |
| get / list / watch, `resourceVersion=N` | any server, once it reached N | not older than N |
| the same client, write then read through one server | that server | read-your-writes |

A request that fails with `504 Timeout` (no leader for 15 s, or the server
was cut off) may or may not have been applied, as with etcd timeouts.

## Disks

Every server writes the Raft log with `fdatasync` before it acknowledges an
entry, like etcd, and openraft 0.9's core waits for that flush before it
handles the next message, heartbeats included. A follower whose disk takes
seconds to flush (an SD card, or one disk shared with image pulls that write
gigabytes) cannot answer heartbeats meanwhile, and the others elect a new
leader. To keep flushes short the log is preallocated (16 MiB segments
written full of zeros, as etcd's WAL), so an append is an overwrite with no
metadata for the filesystem to journal: on ext4 a journal commit would wait
for every other dirty file on the disk. That is why the default timers
tolerate ~4.5 s without a heartbeat; with fast dedicated disks they can be
shortened (`LITEK8S_RAFT_HEARTBEAT_MS=100`); bench/HA.md measures both.

## Limits

- A server cut off from the majority keeps serving `resourceVersion="0"`
  reads and live watches from its last state; its `/readyz` turns false within
  3 s so load balancers and agents move away.
- Membership changes one server at a time; 3 or 5 voters. Learners (non-voting
  servers) are only used while a server joins.
- No authentication for API clients yet (as on a single server): keep the
  port on a trusted network.
