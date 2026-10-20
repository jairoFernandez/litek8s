# Conformance: litek8s-ha

**390/419 passed** (0 skipped), 32 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 93 | 94 | 0 |
| apps | 55 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 16 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 40 | 43 | 0 |
| node | 80 | 89 | 0 |
| scheduling | 10 | 11 | 0 |
| storage | 81 | 90 | 0 |

## Failing (29)

- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should unconditionally reject operations on fail closed webhook [Conformance]  
  `[FAILED] expect an internal error, got &url.Error{Op:"Post", URL:"http://127.0.0.1:6443/api/v1/namespaces/fail-closed-namespace-6784/configmaps", Err:(*net.OpError)(0xc010c1ae60)}`
- [It] [sig-apps] CronJob should not schedule jobs when suspended [Slow] [Conformance]  
  `[FAILED] Failed after 195.077s.`
- [It] [sig-apps] CronJob should not schedule new jobs when ForbidConcurrent [Slow] [Conformance]  
  `[FAILED] Failed to schedule CronJob forbid: cannot confirm the latest state with the cluster: failed to connect to 192.168.77.12:6444: Connection refused (os error 111)`
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Deployment deployment should delete old replica sets [Conformance]  
  `[FAILED] checking for ready nodes: cannot confirm the latest state with the cluster: not enough for a quorum, cluster: {voters:[{3467408461920483837:ServerNode { name: "lk1", addr: "192.168.77.11:6443`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should perform rolling updates and roll backs of template modifications [Conformance]  
  `[FAILED] Failed waiting for state update: cannot confirm the latest state with the cluster: not enough for a quorum, cluster: {voters:[{3467408461920483837:ServerNode { name: "lk1", addr: "192.168.77.`
- [It] [sig-cli] Kubectl client Guestbook application should create and stop a working application [Conformance]  
  `[FAILED] Frontend service did not start serving content in 600 seconds.`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] DeferCleanup callback returned error: cannot confirm the latest state with the cluster: not enough for a quorum, cluster: {voters:[{3467408461920483837:ServerNode { name: "lk1", addr: "192.16`
- [It] [sig-network] Proxy version v1 should proxy through a service and a pod [Conformance]  
  `[FAILED] 0 (503; 4.079149ms): path /api/v1/namespaces/proxy-350/services/http:proxy-service-zwg2g:portname1/proxy/ gave status error: {TypeMeta:{Kind: APIVersion:} ListMeta:{SelfLink: ResourceVersion:`
- [It] [sig-network] Services should be able to change the type from ClusterIP to ExternalName [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-node] Downward API should provide host IP as an env var [NodeConformance] [Conformance]  
  `[FAILED] failed to get logs from downward-api-ec318b6d-9193-48e6-8450-e96efcacd724 for dapi-container: the server was unable to return a response in the time allotted, but may still be processing the `
- [It] [sig-node] Kubelet when scheduling a read only busybox container should not write to root filesystem [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 64.011s.`
- [It] [sig-node] PodTemplates should delete a collection of pod templates [Conformance]  
  `[FAILED] failed to create pod template: cannot confirm the latest state with the cluster: not enough for a quorum, cluster: {voters:[{3467408461920483837:ServerNode { name: "lk1", addr: "192.168.77.11`
- [It] [sig-node] PreStop should call prestop when killing a pod [Conformance]  
  `[FAILED] Error validating prestop: Get "http://127.0.0.1:6443/api/v1/namespaces/prestop-3807/pods/server/proxy/read": context deadline exceeded`
- [It] [sig-node] Probing container should *not* be restarted with a /healthz http liveness probe [NodeConformance] [Conformance]  
  `[FAILED] getting pod : cannot confirm the latest state with the cluster: failed to connect to 192.168.77.12:6444: Connection refused (os error 111)`
- [It] [sig-node] Probing container should *not* be restarted with a GRPC liveness probe [NodeConformance] [Conformance]  
  `[FAILED] getting pod : cannot confirm the latest state with the cluster: failed to connect to 192.168.77.11:6444: Connection refused (os error 111)`
- [It] [sig-node] Probing container should *not* be restarted with a exec "cat /tmp/health" liveness probe [NodeConformance] [Conformance]  
  `[FAILED] getting pod : cannot confirm the latest state with the cluster: failed to connect to 192.168.77.11:6444: Connection refused (os error 111)`
- [It] [sig-node] Probing container should *not* be restarted with a tcp:8080 liveness probe [NodeConformance] [Conformance]  
  `[FAILED] getting pod : cannot confirm the latest state with the cluster: failed to connect to 192.168.77.12:6444: Connection refused (os error 111)`
- [It] [sig-node] Security Context should support pod.Spec.SecurityContext.RunAsUser And pod.Spec.SecurityContext.RunAsGroup [LinuxOnly] [Conformance]  
  `[FAILED] failed to get logs from security-context-5cf01f19-ea3b-40ee-9ffd-b3900d944c0b for test-container: the server was unable to return a response in the time allotted, but may still be processing `
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] listing schedulable nodes error: error: cannot confirm the latest state with the cluster: failed to connect to 192.168.77.12:6444: Connection refused (os error 111). Non-retryable failure or `
- [It] [sig-storage] Downward API volume should provide container's memory limit [NodeConformance] [Conformance]  
  `[FAILED] failed to get logs from downwardapi-volume-9f63ebb2-4e89-4258-bd33-8ba5b9c60f21 for client-container: the server was unable to return a response in the time allotted, but may still be process`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0644,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0666,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0777,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (root,0644,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (root,0666,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] failed to get logs from pod-84b4b69b-97a5-4d9a-9f29-f99c2919cec4 for test-container: the server was unable to return a response in the time allotted, but may still be processing the request (`
- [It] [sig-storage] EmptyDir volumes should support (root,0777,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes volume on tmpfs should have the correct mode [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] Projected downwardAPI should update labels on modification [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 136.309s.`
