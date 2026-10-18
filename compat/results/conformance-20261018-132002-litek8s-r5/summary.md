# Conformance: litek8s-r5

**405/419 passed** (0 skipped), 47 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 90 | 94 | 0 |
| apps | 59 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 38 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 7 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (14)

- [It] [sig-api-machinery] Aggregator Should be able to support the 1.17 Sample API Server using the current Aggregator [Conformance]  
  `[FAILED] creating apiservice v1alpha1.wardle.example.com: no protobuf schema for apiregistration.k8s.io/v1 APIService`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] removes definition from spec when one version gets changed to not be served [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-to-single-ver.v6alpha1.e2e-test-crd-publish-openapi-2098-crd" not to be served anymore: failed to wait for OpenAPI sp`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD without validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-empty.v1.e2e-test-crd-publish-openapi-3625-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a pod. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should have a working scale subresource [Conformance]  
  `[FAILED] Failed to patch stateful set: Operation cannot be fulfilled on statefulsets.apps "ss": the object has been modified; please apply your changes to the latest version and try again: Operation c`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] Unable to read agnhost_udp@dns-test-service.dns-3752.svc.cluster.local from pod dns-3752/dns-test-226c63af-4467-485c-ae14-a966a56242c3: client rate limiter Wait returned an error: context dea`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] Unable to read agnhost_udp@kubernetes.default.svc.cluster.local from pod dns-9701/dns-test-62765f09-b312-4810-93d1-8262edc64a3f: client rate limiter Wait returned an error: context deadline e`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] Unable to read agnhost_udp@dns-test-service from pod dns-7328/dns-test-68e7ca29-603d-4759-bad6-e6d343cf34d4: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] Failed to connect to exposed host ports`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PreemptionExecutionPath runs ReplicaSets to verify preemption running path [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates basic preemption works [Conformance]  
  `[FAILED] Timed out after 300.001s.`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates lower priority pod preemption by critical pod [Conformance]  
  `[FAILED] Timed out after 300.002s.`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] Timed out after 300.001s.`
