# Conformance: litek8s-r4

**388/419 passed** (0 skipped), 40 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 81 | 94 | 0 |
| apps | 57 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 16 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 35 | 43 | 0 |
| node | 87 | 89 | 0 |
| scheduling | 7 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (31)

- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should mutate custom resource with pruning [Conformance]  
  `[FAILED] `
- [It] [sig-api-machinery] Aggregator Should be able to support the 1.17 Sample API Server using the current Aggregator [Conformance]  
  `[FAILED] creating apiservice v1alpha1.wardle.example.com: no protobuf schema for apiregistration.k8s.io/v1 APIService`
- [It] [sig-api-machinery] CustomResourceFieldSelectors [Privileged:ClusterAdmin] CustomResourceFieldSelectors MUST list and watch custom resources matching the field selector [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] removes definition from spec when one version gets changed to not be served [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-to-single-ver.v5.e2e-test-crd-publish-openapi-1471-crd" to be served with the right OpenAPI schema: failed to wait fo`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] updates the published spec when one version gets renamed [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-650-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI s`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD with validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-7352-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD without validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-empty.v1.e2e-test-crd-publish-openapi-1069-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of different groups [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-2823-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group and version but different kinds [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-common-group.v6.e2e-test-crd-publish-openapi-3848-crd" to be served with the right OpenAPI schema: failed to wait for OpenA`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group but different versions [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-5469-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI `
- [It] [sig-api-machinery] Garbage collector should orphan pods created by rc if delete options say so [Serial] [Conformance]  
  `[FAILED] expect 100 pods, got 90 pods`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a pod. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-api-machinery] Watchers should be able to start watching from a specific resource version [Conformance]  
  `[FAILED] Timed out waiting for expected watch notification: {MODIFIED &ConfigMap{ObjectMeta:{e2e-watch-test-resource-version  watch-9732  74f47187-5689-4579-8809-ff38cb835a41 10882 0 2026-10-18 12:20:`
- [It] [sig-apps] Daemon set [Serial] should rollback without unnecessary restarts [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] DisruptionController should block an eviction until the PDB is updated to allow it [Conformance]  
  `[FAILED] Expected`
- [It] [sig-cli] Kubectl client Kubectl describe should check if kubectl describe prints relevant information for rc and pods [Conformance]  
  `[FAILED] error running /usr/local/bin/kubectl --server=http://127.0.0.1:6443 --kubeconfig=/Users/jairo/workspace/personal/litek8s/.claude/worktrees/adoring-darwin-f8f6bd/compat/.work/litek8s.kubeconfi`
- [It] [sig-network] DNS should provide DNS for ExternalName services [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] Unable to read agnhost_udp@dns-querier-2.dns-test-service-2.dns-413.svc.cluster.local from pod dns-413/dns-test-5103245f-e739-413f-9a8b-4216f8ce5822: client rate limiter Wait returned an erro`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] Unable to read agnhost_tcp@dns-test-service.dns-2847.svc.cluster.local from pod dns-2847/dns-test-5a3bbeb1-c89d-482f-beaa-fe2489a0b2d8: client rate limiter Wait returned an error: context dea`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] Unable to read agnhost_tcp@_http._tcp.dns-test-service.dns-4198.svc from pod dns-4198/dns-test-1d42db0f-4869-458c-bc8d-3b90d535b7cb: client rate limiter Wait returned an error: rate: Wait(n=1`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] Failed to connect to exposed host ports`
- [It] [sig-network] Proxy version v1 should proxy through a service and a pod [Conformance]  
  `[FAILED] 0 (400; 2.930709ms): path /api/v1/namespaces/proxy-2559/pods/https:proxy-service-rkp7h-8648545f7c-wvvkg:462/proxy/ gave status error: {TypeMeta:{Kind: APIVersion:} ListMeta:{SelfLink: Resourc`
- [It] [sig-network] Services should serve a basic endpoint from pods [Conformance]  
  `[FAILED] error waithing for service endpoint-test2 in namespace services-3819 to expose endpoints map[pod1:[80]]: %!v(MISSING): timed out waiting for the condition`
- [It] [sig-node] Container Runtime blackbox test when starting a container that exits should run with the expected status [NodeConformance] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] Sysctls [LinuxOnly] [NodeConformance] should support sysctls [MinimumKubeletVersion:1.21] [Environment:NotInUserNS] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PreemptionExecutionPath runs ReplicaSets to verify preemption running path [Conformance]  
  `[FAILED] failed pod observation expectations: context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates basic preemption works [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-3264  ab17cfe6-41a2-46db-a28d-00cddfb96ec1 13152 1 2026-10-18 12:33:47 -0`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates lower priority pod preemption by critical pod [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-7919  1e47bd7e-bc65-4898-8548-24da3fa86e8d 14161 1 2026-10-18 12:37:16 -0`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] Timed out after 300.001s.`
