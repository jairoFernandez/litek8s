# Conformance: litek8s-r8-parallel

**385/390 passed** (29 skipped)

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 85 | 85 | 9 |
| apps | 52 | 52 | 8 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 38 | 43 | 0 |
| node | 87 | 87 | 2 |
| scheduling | 2 | 2 | 9 |
| storage | 89 | 89 | 1 |

## Failing (5)

- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] Unable to read jessie_udp@dns-test-service.dns-9498.svc.cluster.local from pod dns-9498/dns-test-7069c962-1cef-4a2c-a618-c1b5a2436559: Get "http://127.0.0.1:6443/api/v1/namespaces/dns-9498/po`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] Unable to read agnhost_udp@dns-test-service from pod dns-6209/dns-test-63acd4a6-88d8-496b-b3f5-d1e0f609582d: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context d`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] Failed to connect to exposed host ports`

## Skipped (29)

- [It] [sig-api-machinery] Garbage collector should keep the rc around until all its pods are deleted if the deleteOptions says so [Serial] [Conformance]
- [It] [sig-api-machinery] Garbage collector should not delete dependents that have both valid owner and owner that's waiting for dependents to be deleted [Serial] [Conformance]
- [It] [sig-api-machinery] Garbage collector should orphan pods created by rc if delete options say so [Serial] [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should apply a finalizer to a Namespace [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should apply an update to a Namespace [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should apply changes to a namespace status [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should ensure that all pods are removed when a namespace is deleted [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should ensure that all services are removed when a namespace is deleted [Conformance]
- [It] [sig-api-machinery] Namespaces [Serial] should patch a Namespace [Conformance]
- [It] [sig-apps] ControllerRevision [Serial] should manage the lifecycle of a ControllerRevision [Conformance]
- [It] [sig-apps] Daemon set [Serial] should list and delete a collection of DaemonSets [Conformance]
- [It] [sig-apps] Daemon set [Serial] should retry creating failed daemon pods [Conformance]
- [It] [sig-apps] Daemon set [Serial] should rollback without unnecessary restarts [Conformance]
- [It] [sig-apps] Daemon set [Serial] should run and stop complex daemon [Conformance]
- [It] [sig-apps] Daemon set [Serial] should run and stop simple daemon [Conformance]
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]
- [It] [sig-apps] Daemon set [Serial] should verify changes to a daemon set status [Conformance]
- [It] [sig-node] NoExecuteTaintManager Multiple Pods [Serial] evicts pods with minTolerationSeconds [Disruptive] [Conformance]
- [It] [sig-node] NoExecuteTaintManager Single Pod [Serial] removing taint cancels eviction [Disruptive] [Conformance]
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates resource limits of pods that are allowed to run [Conformance]
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates that NodeSelector is respected if matching [Conformance]
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates that NodeSelector is respected if not matching [Conformance]
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates that there exists conflict between pods with same hostPort and protocol but one using 0.0.0.0 hostIP [Conformance]
- [It] [sig-scheduling] SchedulerPreemption [Serial] PreemptionExecutionPath runs ReplicaSets to verify preemption running path [Conformance]
- [It] [sig-scheduling] SchedulerPreemption [Serial] PriorityClass endpoints verify PriorityClass endpoints can be operated with different HTTP methods [Conformance]
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates basic preemption works [Conformance]
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates lower priority pod preemption by critical pod [Conformance]
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]
- [It] [sig-storage] EmptyDir wrapper volumes should not cause race condition when used for configmaps [Serial] [Conformance]
