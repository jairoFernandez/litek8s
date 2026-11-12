# GPUs and other devices (CDI)

A node with GPUs (or any other device a vendor describes with the
[Container Device Interface](https://github.com/cncf-tags/container-device-interface))
offers them to pods as extended resources, with no device plugin, operator or
DaemonSet: the node agent reads the node's CDI specs, advertises one unit per
device, and gives each container the devices it asked for. Linux nodes on the
crun runtime (servers and agents); not the Docker runtime of macOS servers.

## On the node

Install the vendor's driver and its CDI tool, and write the spec. NVIDIA:

```bash
# the driver, then the NVIDIA Container Toolkit (nvidia-ctk)
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
nvidia-ctk cdi list        # nvidia.com/gpu=0, nvidia.com/gpu=1, nvidia.com/gpu=all, ...
```

AMD (`amd-ctk cdi generate`) and Intel write theirs the same way. Then
(re)start the node's litek8s (`sudo systemctl restart litek8s-agent`, or
`litek8s-server` on a server): it logs `2 nvidia.com/gpu device(s) from CDI
specs` and the Node shows them:

```console
$ kubectl describe node gpu1 | grep -A6 Capacity
Capacity:
  cpu:             16
  memory:          65536000Ki
  nvidia.com/gpu:  2
```

Every spec's `kind` (`nvidia.com/gpu`, `amd.com/gpu`, `intel.com/gpu`...)
becomes a resource with one unit per named device; `all` is not counted.
Specs are read from `/etc/cdi` and `/var/run/cdi` (later files win for a
device defined twice); `LITEK8S_CDI_SPEC_DIRS=/a:/b` in the service's
environment reads others instead, and an empty value turns CDI off.

## In a pod

Ask in `limits`, as for any device plugin's resource:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: cuda-check
spec:
  restartPolicy: Never
  containers:
    - name: cuda
      image: nvcr.io/nvidia/cuda:12.6.3-base-ubuntu24.04
      command: ["nvidia-smi", "-L"]
      resources:
        limits:
          nvidia.com/gpu: 1
```

The scheduler puts the pod on a node with a free GPU (or reports
`Insufficient nvidia.com/gpu`, and preempts lower-priority pods for one).
On the node the container gets its devices' CDI edits: the device nodes
(allowed in its device cgroup), the driver's libraries mounted from the node,
the environment and the vendor's hooks (`nvidia-ctk hook ...`). Nothing in
the image has to know about litek8s.

- **Exclusive.** A device belongs to one pod at a time. Containers of a pod
  get disjoint devices; init containers reuse the pod's.
- **Sticky.** Which pod holds which device is kept in
  `<node dir>/cdi-allocations.json`, so restarting the agent hands every
  running pod the same devices; a pod's devices are free again when it is
  gone.
- **Read at container start.** The edits come from the specs as they are
  when the container starts, so regenerating the spec after a driver upgrade
  needs no restart; adding or removing devices does (the capacity is read
  at startup).

To keep other pods off GPU nodes, taint them (`gpu=true:NoSchedule`, in a
[cluster file](clusters.md#a-cluster-file) too) and tolerate the taint in
the GPU pods.

## Not supported

- Time-slicing, MIG partitions and other sharing of one device by several
  pods (MIG devices that `nvidia-ctk` lists as their own CDI devices are
  separate units, as any other device).
- Dynamic Resource Allocation (`resource.k8s.io` ResourceClaims) and device
  plugins: CDI specs are the only source of devices.
- Choosing devices by topology (NUMA, NVLink): a pod gets the free devices
  in the spec's order.
- GPU metrics: run NVIDIA's DCGM exporter and point a Prometheus at it.
