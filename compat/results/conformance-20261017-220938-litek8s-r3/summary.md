# Conformance: litek8s-r3

**346/419 passed** (0 skipped), 59 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 76 | 94 | 0 |
| apps | 46 | 60 | 0 |
| architecture | 0 | 1 | 0 |
| auth | 6 | 10 | 0 |
| cli | 16 | 17 | 0 |
| instrumentation | 3 | 4 | 0 |
| network | 30 | 43 | 0 |
| node | 78 | 89 | 0 |
| scheduling | 4 | 11 | 0 |
| storage | 87 | 90 | 0 |

## Failing (73)

- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should be able to deny custom resource creation, update and deletion [Conformance]  
  `[FAILED] deleting custom resource cr-instance-2 in namespace: webhook-6476 should be denied`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should mutate custom resource with pruning [Conformance]  
  `[FAILED] `
- [It] [sig-api-machinery] Aggregator Should be able to support the 1.17 Sample API Server using the current Aggregator [Conformance]  
  `[FAILED] creating apiservice v1alpha1.wardle.example.com: no protobuf schema for apiregistration.k8s.io/v1 APIService`
- [It] [sig-api-machinery] CustomResourceConversionWebhook [Privileged:ClusterAdmin] should be able to convert a non homogeneous list of CRs [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] CustomResourceConversionWebhook [Privileged:ClusterAdmin] should be able to convert from CR v1 to CR v2 [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] CustomResourceDefinition resources [Privileged:ClusterAdmin] custom resource defaulting for requests and from storage works [Conformance]  
  `[FAILED] waiting for CR to be defaulted on read: timed out waiting for the condition`
- [It] [sig-api-machinery] CustomResourceFieldSelectors [Privileged:ClusterAdmin] CustomResourceFieldSelectors MUST list and watch custom resources matching the field selector [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] removes definition from spec when one version gets changed to not be served [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-to-single-ver.v5.e2e-test-crd-publish-openapi-8403-crd" to be served with the right OpenAPI schema: failed to wait fo`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] updates the published spec when one version gets renamed [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-5112-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI `
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD with validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-7771-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD without validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-empty.v1.e2e-test-crd-publish-openapi-8407-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of different groups [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-6632-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group and version but different kinds [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-common-group.v6.e2e-test-crd-publish-openapi-6918-crd" to be served with the right OpenAPI schema: failed to wait for OpenA`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group but different versions [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-2558-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI `
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a pod. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-api-machinery] Servers with support for API chunking should return chunks of results for list calls [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] Servers with support for API chunking should support continue listing from the last key if the original version has been compacted away, though the list is inconsistent [Slow] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] Watchers should be able to start watching from a specific resource version [Conformance]  
  `[FAILED] Timed out waiting for expected watch notification: {MODIFIED &ConfigMap{ObjectMeta:{e2e-watch-test-resource-version  watch-4593  3c42197c-b4b0-4428-a72a-6127c273e564 2900 0 2026-10-17 22:12:4`
- [It] [sig-apps] ControllerRevision [Serial] should manage the lifecycle of a ControllerRevision [Conformance]  
  `[FAILED] Failed to find any controllerRevisions`
- [It] [sig-apps] Daemon set [Serial] should rollback without unnecessary restarts [Conformance]  
  `[FAILED] Conformance test suite needs a cluster with at least 2 nodes.`
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]  
  `[FAILED] error waiting for controllerrevisions to be created: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-apps] Deployment RollingUpdateDeployment should delete old pods and create new ones [Conformance]  
  `[FAILED] error waiting for deployment "test-rolling-update-deployment" (got 1 / registry.k8s.io/e2e-test-images/agnhost:2.53) and new replica set "test-rolling-update-deployment-7d56fc7fd5" (got 1 / r`
- [It] [sig-apps] Deployment deployment should support proportional scaling [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-apps] Deployment deployment should support rollover [Conformance]  
  `[FAILED] error waiting for deployment "test-rollover-deployment" status to match expectation: total pods available: 0, less than the min required: 1`
- [It] [sig-apps] DisruptionController should block an eviction until the PDB is updated to allow it [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Job with successPolicy should succeeded when all indexes succeeded [Conformance]  
  `[FAILED] failed to ensure that job has SuccessCriteriaMet with SuccessPolicy reason condition: Timed out after 900.002s.`
- [It] [sig-apps] ReplicaSet Replace and Patch tests [Conformance]  
  `[FAILED] failed to see replicas of test-rs in namespace replicaset-59 scale to requested amount of 3: timed out waiting for the condition`
- [It] [sig-apps] ReplicationController should surface a failure condition on a common issue like exceeded quota [Conformance]  
  `[FAILED] couldn't apply the provided updated to rc "condition-test": Operation cannot be fulfilled on replicationcontrollers "condition-test": the object has been modified; please apply your changes t`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] Scaling should happen in predictable order and halt if any stateful pod is unhealthy [Slow] [Conformance]  
  `[FAILED] StatefulSet ss scaled unexpectedly scaled to 3 -> 2 replicas`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] Should recreate evicted statefulset [Conformance]  
  `[FAILED] Pod ss-0 expected to be re-created at least once`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should list, patch and delete a collection of StatefulSets [Conformance]  
  `[FAILED] statefulset not using ssPatchImage. Is using registry.k8s.io/e2e-test-images/httpd:2.4.38-4`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should perform canary updates and phased rolling updates of template modifications [Conformance]  
  `[FAILED] Pod statefulset-1296/ss2-0 has image registry.k8s.io/e2e-test-images/httpd:2.4.39-4 not equal to current image registry.k8s.io/e2e-test-images/httpd:2.4.38-4`
- [It] [sig-architecture] Conformance Tests should have at least two untainted nodes [Conformance]  
  `[FAILED] Conformance requires at least two nodes`
- [It] [sig-auth] ServiceAccounts ServiceAccountIssuerDiscovery should support OIDC discovery of service account issuer [Conformance]  
  `[FAILED] Told to stop trying after 2.002s.`
- [It] [sig-auth] ServiceAccounts should create a serviceAccountToken and ensure a successful TokenReview [Conformance]  
  `[FAILED] Unable to create serviceAccountToken: the token subresource of serviceaccounts is not supported by this server yet`
- [It] [sig-auth] ServiceAccounts should guarantee kube-root-ca.crt exist in any namespace [Conformance]  
  `[FAILED] timed out waiting for the condition`
- [It] [sig-auth] ServiceAccounts should mount an API token into pods [Conformance]  
  `[FAILED] expected single authentication.kubernetes.io/credential-id extra info item starting with 'JTI=', got []`
- [It] [sig-cli] Kubectl client Kubectl describe should check if kubectl describe prints relevant information for rc and pods [Conformance]  
  `[FAILED] error running /usr/local/bin/kubectl --server=http://127.0.0.1:6443 --kubeconfig=/Users/jairo/workspace/personal/litek8s/.claude/worktrees/adoring-darwin-f8f6bd/compat/.work/litek8s.kubeconfi`
- [It] [sig-instrumentation] Events API should ensure that an event can be fetched, patched, deleted, and listed [Conformance]  
  `[FAILED] expected single event, got []v1.Event{}`
- [It] [sig-network] DNS should provide DNS for ExternalName services [Conformance]  
  `[FAILED] unexpected error getting pod client logs for webserver: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] Unable to read agnhost_tcp@dns-querier-2.dns-test-service-2.dns-5546.svc.cluster.local from pod dns-5546/dns-test-bd872d28-5d12-4b19-9008-774adfe5b57d: client rate limiter Wait returned an er`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] unexpected error getting pod client logs for webserver: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] EndpointSlice should create Endpoints and EndpointSlices for Pods matching a Service [Conformance]  
  `[FAILED] Expected EndpointSlice to have 0 ports, got 1`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] wait for pod pod2 timeout, err:Timed out after 300.001s.`
- [It] [sig-network] Proxy version v1 should proxy through a service and a pod [Conformance]  
  `[FAILED] 0: path /api/v1/namespaces/proxy-3233/pods/proxy-service-gjzlh-6b8f4f4f8b-fnd8d/proxy/: wanted <a href="/api/v1/namespaces/proxy-3233/pods/proxy-service-gjzlh-6b8f4f4f8b-fnd8d/proxy/rewriteme`
- [It] [sig-network] Services should be able to switch session affinity for NodePort service [LinuxOnly] [Conformance]  
  `[FAILED] Affinity should hold but didn't.`
- [It] [sig-network] Services should be able to switch session affinity for service with type clusterIP [LinuxOnly] [Conformance]  
  `[FAILED] Affinity should hold but didn't.`
- [It] [sig-network] Services should have session affinity work for NodePort service [LinuxOnly] [Conformance]  
  `[FAILED] Affinity should hold but didn't.`
- [It] [sig-network] Services should have session affinity work for service with type clusterIP [LinuxOnly] [Conformance]  
  `[FAILED] Affinity should hold but didn't.`
- [It] [sig-network] Services should serve a basic endpoint from pods [Conformance]  
  `[FAILED] error waithing for service endpoint-test2 in namespace services-6263 to expose endpoints map[pod1:[80]]: %!v(MISSING): timed out waiting for the condition`
- [It] [sig-node] Container Runtime blackbox test when starting a container that exits should run with the expected status [NodeConformance] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] InitContainer [NodeConformance] should invoke init containers on a RestartAlways pod [Conformance]  
  `[FAILED] init container init1 should be in Ready status`
- [It] [sig-node] InitContainer [NodeConformance] should invoke init containers on a RestartNever pod [Conformance]  
  `[FAILED] init container init1 should be in Ready status`
- [It] [sig-node] NoExecuteTaintManager Multiple Pods [Serial] evicts pods with minTolerationSeconds [Disruptive] [Conformance]  
  `[FAILED] Failed to evict all Pods. 2 pod(s) is not evicted.`
- [It] [sig-node] Pods should allow activeDeadlineSeconds to be updated [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 300.002s.`
- [It] [sig-node] Sysctls [LinuxOnly] [NodeConformance] should support sysctls [MinimumKubeletVersion:1.21] [Environment:NotInUserNS] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-node] Variable Expansion should allow substituting values in a volume subpath [Conformance]  
  `[FAILED] expected pod "var-expansion-b2bba1bf-a008-49df-ba73-3c8ded46a3e4" success: Timed out after 300.001s.`
- [It] [sig-node] Variable Expansion should fail substituting values in a volume subpath with absolute path [Conformance]  
  `[FAILED] while waiting for the pod container to fail: Timed out after 120.002s.`
- [It] [sig-node] Variable Expansion should fail substituting values in a volume subpath with backticks [Conformance]  
  `[FAILED] while waiting for the pod container to fail: Timed out after 120.001s.`
- [It] [sig-node] Variable Expansion should succeed in writing subpaths in container [Conformance]  
  `[FAILED] while waiting for pod to be running: Timed out after 120.003s.`
- [It] [sig-node] Variable Expansion should verify that a failing subpath expansion can be modified during the lifecycle of a container [Slow] [Conformance]  
  `[FAILED] while waiting for pod to be running: Timed out after 120.001s.`
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates resource limits of pods that are allowed to run [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates that NodeSelector is respected if not matching [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PreemptionExecutionPath runs ReplicaSets to verify preemption running path [Conformance]  
  `[FAILED] failed pod observation expectations: context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PriorityClass endpoints verify PriorityClass endpoints can be operated with different HTTP methods [Conformance]  
  `[FAILED] expect a patch error on an immutable field`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates basic preemption works [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-3000  29c66853-d96f-49ff-a344-065d4e096ce5 476691 1 2026-10-17 22:44:01 -`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates lower priority pod preemption by critical pod [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-1313  b4207e8b-fc78-4da0-9abc-bd7158e3ff31 477865 1 2026-10-17 23:01:37 -`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] Timed out after 300.002s.`
- [It] [sig-storage] PersistentVolumes CSI Conformance should apply changes to a pv/pvc status [Conformance]  
  `[FAILED] Checking that the PV status has been read`
- [It] [sig-storage] Projected secret should be consumable from pods in volume as non-root with defaultMode and fsGroup set [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "(mode of file \"/etc/projected-secret-volume/data-1\": -r--r-----\|mode of Windows file \"/etc/projected-secret-volume/data-1\": -r--r-----)" in container output: Expected`
- [It] [sig-storage] Secrets should be consumable from pods in volume as non-root with defaultMode and fsGroup set [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "(mode of file \"/etc/secret-volume/data-1\": -r--r-----\|mode of Windows file \"/etc/secret-volume/data-1\": -r--r-----)" in container output: Expected`
