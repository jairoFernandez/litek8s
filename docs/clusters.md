# Clusters of machines over SSH

Set up and keep a cluster of real machines (a Raspberry Pi, an old laptop, a
cheap cloud VM) from your laptop, without logging in to each one:

- `litek8s install server|agent --ssh user@host` installs one machine;
- `litek8s cluster apply -f cluster.yaml` installs or updates every machine
  of a file, and does nothing when nothing changed.

Both run your own `ssh`, so `~/.ssh/config` (Host aliases, `ProxyJump`,
ports, keys), the SSH agent and `known_hosts` apply as for any `ssh` you
type. The machines need Linux (x86_64 or aarch64) with systemd, and root or
sudo. Nothing is needed on them beforehand: litek8s, crun and iptables are
installed when missing.

## One machine

```bash
# a server (it runs pods too); adds the kubeconfig context and prints the join command
litek8s install server --ssh pi@192.168.1.10

# a node
litek8s install agent --ssh me@old-laptop --join https://192.168.1.10:6443 --token K10<hash>::<secret>

litek8s uninstall --ssh me@old-laptop [--purge]
```

What happens, one `ssh` command per step, its output streamed back:

1. **Probe**: OS and architecture (`uname -s -m`), whether sudo needs a
   password, systemd, and which litek8s (if any) is installed.
2. **Binary**: kept if `/usr/local/bin/litek8s` there is already the one
   wanted. Otherwise, in this order: `--binary PATH` (a Linux build, checked
   to match the machine's architecture) is uploaded; this litek8s itself is
   uploaded when it is a static Linux build of the same architecture; else
   the machine downloads the GitHub release archive of this version with its
   own `curl` and checks it against the release's `SHA256SUMS`
   (`--release v0.2.0` picks another release; `LITEK8S_DOWNLOAD_URL` points
   at a mirror). A development build (`litek8s --version` says `DEV`) has no
   release, so it needs `--binary` (`bench/build-linux.sh arm64` builds one).
3. **Packages**: crun and iptables from the machine's package manager when
   they are missing and the node will run pods with crun.
4. **Install**: `sudo litek8s install server|agent <flags>` on the machine,
   exactly as [the local install](multi-node.md) would run
   it: the same defaults, checks and systemd unit. The join token goes in a
   0600 file (then `/etc/litek8s/<role>.env`), never on a command line.
5. **Kubeconfig** (server): the context is added to your kubeconfig
   (`$KUBECONFIG` or `~/.kube/config`) and made current, with the cluster's
   CA (checked against the join token) and `tls-server-name:
   kubernetes.default.svc`, so the API is reached over verified TLS at the
   machine's own address.

Re-running with the same binary and flags leaves the service running
(`... already runs this binary with these flags: nothing to change`); other
flags or another binary rewrite the unit and restart it, and pods keep
running across the restart.

The flags for the SSH side may go anywhere after `server`/`agent`; all the
others are the service's (`litek8s apiserver --help`, `litek8s agent
--help`):

| Flag | |
|---|---|
| `--ssh USER@HOST` | the machine (or a Host of `~/.ssh/config`) |
| `-i FILE`, `--ssh-port N`, `--ssh-option K=V` | passed to `ssh` (`-i`, `-p`, `-o`) |
| `--binary PATH`, `--release TAG` | where the binary comes from (above) |
| `--context NAME` | server: the kubeconfig context (default: its node name) |
| `--api-address HOST[:PORT]` | server: the address for the kubeconfig (default: the host `ssh` connects to, after `~/.ssh/config`; else `--advertise-address`) |

**sudo with a password**: litek8s asks for it once per machine, without
echo, and hands it to `sudo -S` over the SSH connection. Without a terminal
(CI, a pipe) it stops with the line to add to `/etc/sudoers.d/` for
passwordless sudo instead of hanging.

## A cluster file

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                        # the kubeconfig context
servers:                            # 1, or 3 or 5 for an HA control plane
  - ssh: pi@192.168.1.10
agents:
  - ssh: me@old-laptop
    labels: {disk: ssd}
  - ssh: ubuntu@ec2-host
    name: ec2                       # the Node's name (default: its hostname)
    nodeIp: 10.0.0.7                # the address other nodes reach it at
    taints: ["gpu=true:NoSchedule"] # or {key, value, effect}
    args: [--node-grace-period, 10m]
```

```bash
litek8s cluster apply -f cluster.yaml           # install / update, idempotent
litek8s cluster status -f cluster.yaml          # service, version, node, readiness, pods per machine
litek8s cluster apply -f cluster.yaml --prune   # also remove agents taken out of the file
litek8s cluster delete -f cluster.yaml --yes [--purge]
```

`apply`:

1. probes every machine (and asks for sudo passwords, one at a time);
2. installs the servers in order. With several, the first gets
   `--cluster-init` and the others `--server <first> --token <its token>`
   ([docs/ha.md](ha.md));
3. adds the kubeconfig context `<metadata.name>`;
4. installs the agents, all at once, with `--join <first server>` and its
   token;
5. sets each Node's labels and taints through the API, and annotates the
   Node with the cluster, its role and its SSH destination
   (`cluster.litek8s.dev/*`);
6. reports what changed, or `cluster home is up to date: nothing changed`.

Labels and taints that `apply` did not set (by hand, by the node lifecycle
controller) are left alone; ones it set and that left the file are removed.
An agent removed from the file is reported; with `--prune` it is cordoned,
its pods are deleted (but DaemonSets') and rescheduled elsewhere, litek8s is
uninstalled from it (`--purge` also deletes its data) and its Node deleted.
`apply` never removes servers: take one out of an HA control plane with
`litek8s server remove` first ([docs/ha.md](ha.md)).

All the fields:

| Field | |
|---|---|
| `servers[]`, `agents[]` | `ssh` (required), `name`, `nodeIp`, `labels`, `taints`, `args`, and `sshPort`, `identityFile`, `sshOptions` for that machine |
| `serverArgs`, `agentArgs` | flags every server / agent gets, before its own `args` |
| `sshPort`, `identityFile`, `sshOptions` | for every machine (`sshOptions: [StrictHostKeyChecking=accept-new]`) |
| `apiAddress` | where this machine reaches the API (`host[:port]`; default: the first server's SSH host) |
| `joinUrl` | where agents and other servers join (default: the first server's node IP, `https://<ip>:6443`) |
| `binary`, `release` | the binary to install (as `--binary` / `--release`, which override them) |

`args` may not set `--token`, `--join`, `--server`, `--cluster-init`,
`--node-name` or `--node-ip`: `apply` sets them from the fields. Unknown
fields are errors, and `apply` lists every problem of a file at once.

### Networks

The laptop needs SSH to every machine and HTTPS to the first server's port
6443 (behind NAT or a jump host: forward it and set `apiAddress`). Agents
need the first server's `joinUrl`. Pods on different nodes talk over the
WireGuard overlay (UDP 51820 between nodes). Remember the API has no client
authentication: keep 6443 on a trusted network.

## Testing

`bench/cluster-e2e.sh` boots three Ubuntu 24.04 VMs (`bench/vm.sh`) and
runs the whole cycle from the Mac: apply (binary downloaded as a release
from a local HTTP server), a re-apply that restarts nothing, pods on all
three nodes and across the overlay, `status`, an agent removed with
`--prune`, and `delete --yes --purge`.
