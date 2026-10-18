# Conformance: litek8s-r2

**271/419 passed** (0 skipped), 77 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 48 | 94 | 0 |
| apps | 32 | 60 | 0 |
| architecture | 0 | 1 | 0 |
| auth | 5 | 10 | 0 |
| cli | 15 | 17 | 0 |
| instrumentation | 3 | 4 | 0 |
| network | 15 | 43 | 0 |
| node | 66 | 89 | 0 |
| scheduling | 4 | 11 | 0 |
| storage | 83 | 90 | 0 |

## Failing (148)

- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should be able to deny attaching pod [Conformance]  
  `[FAILED] unexpected 'kubectl attach' error message. expected to contain "attaching to pod 'to-be-attached-pod' is not allowed", got "error running /usr/local/bin/kubectl --server=http://127.0.0.1:6443`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should be able to deny custom resource creation, update and deletion [Conformance]  
  `[FAILED] expect error contains "the custom resource contains unwanted data", got "admission webhook \"deny-unwanted-custom-resource-data.k8s.io\" denied the request: denied"`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should be able to deny pod and configmap creation [Conformance]  
  `[FAILED] expect error contains "the configmap contains unwanted key and value", got "admission webhook \"deny-unwanted-configmap-data.k8s.io\" denied the request: denied"`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should honor timeout [Conformance]  
  `[FAILED] expect an HTTP/dial timeout error querying the slow webhook, got: "Internal error occurred: failed calling webhook \"allow-configmap-with-delay-webhook.k8s.io\": webhook timed out: deadline h`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should mutate custom resource with pruning [Conformance]  
  `[FAILED] `
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should mutate everything except 'skip-me' configmaps [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should mutate pod and apply defaults after mutation [Conformance]  
  `[FAILED] expect the init terminationMessagePolicy to be default to "File", got ""`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should reject mutating webhook configurations with invalid match conditions [Conformance]  
  `[FAILED] create mutatingwebhookconfiguration should have been denied by the api-server`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should reject validating webhook configurations with invalid match conditions [Conformance]  
  `[FAILED] create validatingwebhookconfiguration should have been denied by the api-server`
- [It] [sig-api-machinery] AdmissionWebhook [Privileged:ClusterAdmin] should unconditionally reject operations on fail closed webhook [Conformance]  
  `[FAILED] create configmap in namespace: fail-closed-namespace-3346 should be unconditionally rejected by the webhook`
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
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-to-single-ver.v5.e2e-test-crd-publish-openapi-8812-crd" to be served with the right OpenAPI schema: failed to wait fo`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] updates the published spec when one version gets renamed [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-4860-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI `
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD preserving unknown fields at the schema root [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-unknown-at-root.v1.e2e-test-crd-publish-openapi-5839-crd" to be served with the right OpenAPI schema: failed to wait for Op`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD preserving unknown fields in an embedded object [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-unknown-in-nested.v1.e2e-test-crd-publish-openapi-8968-crd" to be served with the right OpenAPI schema: failed to wait for `
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD with validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-9708-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for CRD without validation schema [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-empty.v1.e2e-test-crd-publish-openapi-1837-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of different groups [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-foo.v1.e2e-test-crd-publish-openapi-5443-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI spec v`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group and version but different kinds [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-common-group.v6.e2e-test-crd-publish-openapi-5557-crd" to be served with the right OpenAPI schema: failed to wait for OpenA`
- [It] [sig-api-machinery] CustomResourcePublishOpenAPI [Privileged:ClusterAdmin] works for multiple CRDs of same group but different versions [Conformance]  
  `[FAILED] failed to wait for definition "com.example.crd-publish-openapi-test-multi-ver.v2.e2e-test-crd-publish-openapi-6876-crd" to be served with the right OpenAPI schema: failed to wait for OpenAPI `
- [It] [sig-api-machinery] FieldValidation should create/apply an invalid CR with extra properties for CRD with validation schema [Conformance]  
  `[FAILED] error missing unknown field: <nil>:`
- [It] [sig-api-machinery] FieldValidation should detect duplicates in a CR when preserving unknown fields [Conformance]  
  `[FAILED] error missing duplicate field: <nil>:`
- [It] [sig-api-machinery] FieldValidation should detect unknown and duplicate fields of a typed object [Conformance]  
  `[PANICKED] Test Panicked`
- [It] [sig-api-machinery] FieldValidation should detect unknown metadata fields in both the root and embedded object of a CR [Conformance]  
  `[FAILED] error missing duplicate field: <nil>:`
- [It] [sig-api-machinery] FieldValidation should detect unknown metadata fields of a typed object [Conformance]  
  `[PANICKED] Test Panicked`
- [It] [sig-api-machinery] Garbage collector should not delete dependents that have both valid owner and owner that's waiting for dependents to be deleted [Serial] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] Garbage collector should orphan pods created by rc if delete options say so [Serial] [Conformance]  
  `[FAILED] expect 50 pods, got 48 pods`
- [It] [sig-api-machinery] Namespaces [Serial] should apply changes to a namespace status [Conformance]  
  `[FAILED] patched object should have the applied annotation`
- [It] [sig-api-machinery] Namespaces [Serial] should ensure that all pods are removed when a namespace is deleted [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should apply changes to a resourcequota status [Conformance]  
  `[FAILED] Error waiting for ResourceQuota "e2e-rq-status-fpjxl" to reset its Status: context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a configMap. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a pod. [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a replica set. [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a replication controller. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a secret. [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and capture the life of a service. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-api-machinery] ResourceQuota should create a ResourceQuota and ensure its status is promptly calculated. [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should verify ResourceQuota with best effort scope. [Conformance]  
  `[FAILED] client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-api-machinery] ResourceQuota should verify ResourceQuota with terminating scopes. [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-api-machinery] Servers with support for API chunking should support continue listing from the last key if the original version has been compacted away, though the list is inconsistent [Slow] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-api-machinery] Servers with support for Table transformation should return a 406 for a backend which does not implement metadata [Conformance]  
  `[FAILED] failed to return error when posting self subject access review: &SelfSubjectAccessReview{ObjectMeta:{      0 0001-01-01 00:00:00 +0000 UTC <nil> <nil> map[] map[] [] [] []},Spec:SelfSubjectAc`
- [It] [sig-api-machinery] Watchers should observe an object deletion if it stops meeting the requirements of the selector [Conformance]  
  `[FAILED] Timed out waiting for expected watch notification: {DELETED <nil>}`
- [It] [sig-apps] ControllerRevision [Serial] should manage the lifecycle of a ControllerRevision [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-apps] CronJob should support CronJob API operations [Conformance]  
  `[FAILED] patched object should have the applied annotation`
- [It] [sig-apps] Daemon set [Serial] should rollback without unnecessary restarts [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]  
  `[FAILED] error waiting for controllerrevisions to be created: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-apps] Daemon set [Serial] should verify changes to a daemon set status [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-apps] Deployment RollingUpdateDeployment should delete old pods and create new ones [Conformance]  
  `[FAILED] error waiting for deployment "test-rolling-update-deployment" (got 1 / registry.k8s.io/e2e-test-images/agnhost:2.53) and new replica set "test-rolling-update-deployment-7d56fc7fd5" (got 1 / r`
- [It] [sig-apps] Deployment deployment should support proportional scaling [Conformance]  
  `[FAILED] replicaset "webserver-deployment-747979b4c5" never had desired number of .spec.replicas`
- [It] [sig-apps] Deployment deployment should support rollover [Conformance]  
  `[FAILED] error waiting for deployment "test-rollover-deployment" status to match expectation: total pods available: 0, less than the min required: 1`
- [It] [sig-apps] DisruptionController Listing PodDisruptionBudgets for all namespaces should list and delete a collection of PodDisruptionBudgets [Conformance]  
  `[FAILED] Waiting for the pdb to be processed in namespace disruption-3405: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-apps] DisruptionController should block an eviction until the PDB is updated to allow it [Conformance]  
  `[FAILED] Waiting for the pdb to be processed in namespace disruption-8277: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-apps] DisruptionController should create a PodDisruptionBudget [Conformance]  
  `[FAILED] Waiting for the pdb to be processed in namespace disruption-7754: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-apps] DisruptionController should observe PodDisruptionBudget status updated [Conformance]  
  `[FAILED] Waiting for the pdb to be processed in namespace disruption-5781: context deadline exceeded`
- [It] [sig-apps] DisruptionController should update/patch PodDisruptionBudget status [Conformance]  
  `[FAILED] Waiting for the pdb to be processed in namespace disruption-1309: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-apps] Job should adopt matching orphans and release non-matching pods [Conformance]  
  `[FAILED] wait for pod "adopt-release-ms8td" to be readopted`
- [It] [sig-apps] Job should allow to use the pod failure policy on exit code to fail the job early [Conformance]  
  `[FAILED] failed to ensure job failure in namespace: job-406: Told to stop trying after 16.023s.`
- [It] [sig-apps] Job should apply changes to a job status [Conformance]  
  `[FAILED] patched object should have the applied annotation`
- [It] [sig-apps] Job should execute all indexes despite some failing when using backoffLimitPerIndex [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Job should mark indexes as failed when the FailIndex action is matched in podFailurePolicy [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Job should terminate job execution when the number of failed indexes exceeds maxFailedIndexes [Conformance]  
  `[FAILED] Expected`
- [It] [sig-apps] Job with successPolicy should succeeded when all indexes succeeded [Conformance]  
  `[FAILED] failed to ensure that job has SuccessCriteriaMet with SuccessPolicy reason condition: Timed out after 900.001s.`
- [It] [sig-apps] Job with successPolicy succeededCount rule should succeeded even when some indexes remain pending [Conformance]  
  `[FAILED] failed to ensure that the job has SuccessCriteriaMet condition with SuccessPolicy rule: Timed out after 900.001s.`
- [It] [sig-apps] Job with successPolicy succeededIndexes rule should succeeded even when some indexes remain pending [Conformance]  
  `[FAILED] failed to ensure that job has SuccessCriteriaMet with SuccessPolicy reason condition: Timed out after 900.000s.`
- [It] [sig-apps] ReplicaSet Replace and Patch tests [Conformance]  
  `[FAILED] failed to see replicas of test-rs in namespace replicaset-8318 scale to requested amount of 3: timed out waiting for the condition`
- [It] [sig-apps] ReplicationController should surface a failure condition on a common issue like exceeded quota [Conformance]  
  `[FAILED] resource quota "condition-test" never synced`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] Scaling should happen in predictable order and halt if any stateful pod is unhealthy [Slow] [Conformance]  
  `[FAILED] StatefulSet ss scaled unexpectedly scaled to 3 -> 0 replicas`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] Should recreate evicted statefulset [Conformance]  
  `[FAILED] Pod ss-0 expected to be re-created at least once`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should list, patch and delete a collection of StatefulSets [Conformance]  
  `[FAILED] statefulset not using ssPatchImage. Is using registry.k8s.io/e2e-test-images/httpd:2.4.38-4`
- [It] [sig-apps] StatefulSet Basic StatefulSet functionality [StatefulSetBasic] should perform canary updates and phased rolling updates of template modifications [Conformance]  
  `[FAILED] Pod statefulset-9419/ss2-0 has image registry.k8s.io/e2e-test-images/httpd:2.4.39-4 not equal to current image registry.k8s.io/e2e-test-images/httpd:2.4.38-4`
- [It] [sig-architecture] Conformance Tests should have at least two untainted nodes [Conformance]  
  `[FAILED] Conformance requires at least two nodes`
- [It] [sig-auth] Certificates API [Privileged:ClusterAdmin] should support CSR API operations [Conformance]  
  `[FAILED] expected certificatesigningrequests/approval, got []v1.APIResource{v1.APIResource{Name:"certificatesigningrequests", SingularName:"certificatesigningrequest", Namespaced:false, Group:"", Vers`
- [It] [sig-auth] ServiceAccounts ServiceAccountIssuerDiscovery should support OIDC discovery of service account issuer [Conformance]  
  `[FAILED] Told to stop trying after 2.004s.`
- [It] [sig-auth] ServiceAccounts should create a serviceAccountToken and ensure a successful TokenReview [Conformance]  
  `[FAILED] Unable to create serviceAccountToken: the token subresource of serviceaccounts is not supported by this server yet`
- [It] [sig-auth] ServiceAccounts should guarantee kube-root-ca.crt exist in any namespace [Conformance]  
  `[FAILED] timed out waiting for the condition`
- [It] [sig-auth] ServiceAccounts should mount an API token into pods [Conformance]  
  `[FAILED] expected single authentication.kubernetes.io/credential-id extra info item starting with 'JTI=', got []`
- [It] [sig-cli] Kubectl client Guestbook application should create and stop a working application [Conformance]  
  `[FAILED] Frontend service did not start serving content in 600 seconds.`
- [It] [sig-cli] Kubectl client Kubectl describe should check if kubectl describe prints relevant information for rc and pods [Conformance]  
  `[FAILED] error running /usr/local/bin/kubectl --server=http://127.0.0.1:6443 --kubeconfig=/Users/jairo/workspace/personal/litek8s/.claude/worktrees/adoring-darwin-f8f6bd/compat/.work/litek8s.kubeconfi`
- [It] [sig-instrumentation] Events API should ensure that an event can be fetched, patched, deleted, and listed [Conformance]  
  `[FAILED] expected single event, got []v1.Event{}`
- [It] [sig-network] DNS should provide /etc/hosts entries for the cluster [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for ExternalName services [Conformance]  
  `[FAILED] unexpected error getting pod client logs for webserver: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] Unable to read agnhost_udp@dns-querier-2.dns-test-service-2.dns-982.svc.cluster.local from pod dns-982/dns-test-49eb24ee-a8fb-424d-aa32-75cdb2b0cdbb: client rate limiter Wait returned an erro`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] unexpected error getting pod client logs for webserver: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] Unable to read agnhost_udp@kubernetes.default.svc.cluster.local from pod dns-8037/dns-test-4545e170-b95e-4923-982a-362f3be08b60: client rate limiter Wait returned an error: rate: Wait(n=1) wo`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] Unable to read agnhost_tcp@dns-test-service from pod dns-7932/dns-test-f231f714-9fcb-4ec1-86d3-31d06bc81305: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-network] DNS should support configurable pod DNS nameservers [Conformance]  
  `[FAILED] customized DNS server not found in configured in pod, expected to contain: 1.1.1.1, got: 127.0.0.11`
- [It] [sig-network] EndpointSlice should create Endpoints and EndpointSlices for Pods matching a Service [Conformance]  
  `[FAILED] Error fetching EndpointSlice for Service endpointslice-2438/example-named-port: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] EndpointSlice should have Endpoints and EndpointSlices pointing to API Server [Conformance]  
  `[FAILED] error obtaining API server "kubernetes" Endpoint resource on "default" namespace: endpoints "kubernetes" not found`
- [It] [sig-network] EndpointSliceMirroring should mirror a custom Endpoints resource through create update and delete [Conformance]  
  `[FAILED] Did not find matching EndpointSlice for endpointslicemirroring-7013/example-custom-endpoints: timed out waiting for the condition`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] wait for pod pod2 timeout, err:Timed out after 300.001s.`
- [It] [sig-network] Ingress API should support creating Ingress API operations [Conformance]  
  `[FAILED] patched object should have the applied annotation`
- [It] [sig-network] Proxy version v1 A set of valid responses are returned for both pod and service Proxy [Conformance]  
  `[FAILED] The resp.StatusCode returned: 200`
- [It] [sig-network] Proxy version v1 should proxy through a service and a pod [Conformance]  
  `[FAILED] 0: path /api/v1/namespaces/proxy-7986/pods/proxy-service-m4xxt-7c974d9c6d-h92gb:1080/proxy/: wanted <a href="/api/v1/namespaces/proxy-7986/pods/proxy-service-m4xxt-7c974d9c6d-h92gb:1080/proxy`
- [It] [sig-network] ServiceCIDR and IPAddress API should support ServiceCIDR API operations [Conformance]  
  `[FAILED] unexpected error getting default ServiceCIDR: servicecidrs.networking.k8s.io "kubernetes" not found`
- [It] [sig-network] Services should be able to change the type from ClusterIP to ExternalName [Conformance]  
  `[FAILED] unexpected Spec.ClusterIP (10.43.0.13) for ExternalName service, expected empty`
- [It] [sig-network] Services should be able to change the type from ExternalName to ClusterIP [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint externalname-service:80 over TCP protocol`
- [It] [sig-network] Services should be able to change the type from ExternalName to NodePort [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint externalname-service:80 over TCP protocol`
- [It] [sig-network] Services should be able to change the type from NodePort to ExternalName [Conformance]  
  `[FAILED] unexpected Spec.ClusterIP (10.43.0.13) for ExternalName service, expected empty`
- [It] [sig-network] Services should be able to create a functioning NodePort service [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint nodeport-test:80 over TCP protocol`
- [It] [sig-network] Services should be able to switch session affinity for NodePort service [LinuxOnly] [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint affinity-nodeport-transition:80 over TCP protocol`
- [It] [sig-network] Services should be able to switch session affinity for service with type clusterIP [LinuxOnly] [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint affinity-clusterip-transition:80 over TCP protocol`
- [It] [sig-network] Services should complete a service status lifecycle [Conformance]  
  `[FAILED] failed to locate Service test-service-zlm2h in namespace services-3324: timed out waiting for the condition`
- [It] [sig-network] Services should have session affinity work for NodePort service [LinuxOnly] [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint affinity-nodeport:80 over TCP protocol`
- [It] [sig-network] Services should have session affinity work for service with type clusterIP [LinuxOnly] [Conformance]  
  `[FAILED] service is not reachable within 2m0s timeout on endpoint affinity-clusterip:80 over TCP protocol`
- [It] [sig-network] Services should serve a basic endpoint from pods [Conformance]  
  `[FAILED] error waithing for service endpoint-test2 in namespace services-3293 to expose endpoints map[pod1:[80]]: %!v(MISSING): timed out waiting for the condition`
- [It] [sig-network] Services should serve endpoints on same port and different protocols [Conformance]  
  `[FAILED] Failed to connect to Service TCP port: service is not reachable within 30s timeout on endpoint 10.43.0.11 80 over TCP protocol`
- [It] [sig-network] Services should serve multiport endpoints from pods [Conformance]  
  `[FAILED] error waithing for service multi-endpoint-test in namespace services-6670 to expose endpoints map[pod1:[100] pod2:[101]]: %!v(MISSING): timed out waiting for the condition`
- [It] [sig-node] ConfigMap should fail to create ConfigMap with empty key [Conformance]  
  `[FAILED] created configMap "configmap-test-emptyKey-99b3ab77-8330-4c85-989c-9a26abe90f5e" with empty key in namespace "configmap-8101"`
- [It] [sig-node] Container Runtime blackbox test on terminated container should report termination message from file when pod succeeds and TerminationMessagePolicy FallbackToLogsOnError is set [NodeConformance] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] Container Runtime blackbox test on terminated container should report termination message from log output if TerminationMessagePolicy FallbackToLogsOnError is set [NodeConformance] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] Container Runtime blackbox test on terminated container should report termination message if TerminationMessagePath is set as non-root user and at a non-default path [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 300.001s.`
- [It] [sig-node] Container Runtime blackbox test when starting a container that exits should run with the expected status [NodeConformance] [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] Ephemeral Containers [NodeConformance] should update the ephemeral containers in an existing pod [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] Ephemeral Containers [NodeConformance] will start an ephemeral container in an existing pod [Conformance]  
  `[FAILED] Expected`
- [It] [sig-node] InitContainer [NodeConformance] should invoke init containers on a RestartAlways pod [Conformance]  
  `[FAILED] init container init1 should be in Ready status`
- [It] [sig-node] InitContainer [NodeConformance] should invoke init containers on a RestartNever pod [Conformance]  
  `[FAILED] init container init1 should be in Ready status`
- [It] [sig-node] KubeletManagedEtcHosts should test kubelet managed /etc/hosts file [NodeConformance] [Conformance]  
  `[FAILED] /etc/hosts file should be kubelet managed (name: busybox-1, retries: 193). /etc/hosts contains "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\nfe00::\tip6-localnet\nff00::\t`
- [It] [sig-node] NoExecuteTaintManager Multiple Pods [Serial] evicts pods with minTolerationSeconds [Disruptive] [Conformance]  
  `[FAILED] Failed to evict all Pods. 2 pod(s) is not evicted.`
- [It] [sig-node] NoExecuteTaintManager Single Pod [Serial] removing taint cancels eviction [Disruptive] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-node] Pods should allow activeDeadlineSeconds to be updated [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 300.000s.`
- [It] [sig-node] Pods should contain environment variables for services [NodeConformance] [Conformance]  
  `[FAILED] Container should have service environment variables set`
- [It] [sig-node] Pods should support remote command execution over websockets [NodeConformance] [Conformance]  
  `[FAILED] Failed to open websocket to ws://127.0.0.1:6443/api/v1/namespaces/pods-3573/pods/pod-exec-websocket-96567e45-823a-495b-b656-bdebb1651d7d/exec?command=echo&command=remote+execution+test&contai`
- [It] [sig-node] Pods should support retrieving logs from the container over websockets [NodeConformance] [Conformance]  
  `[FAILED] Failed to open websocket to ws://127.0.0.1:6443/api/v1/namespaces/pods-5169/pods/pod-logs-websocket-5747edb6-65b7-4ea2-837f-ce49a0807f09/log?container=main: websocket.Dial ws://127.0.0.1:6443`
- [It] [sig-node] Secrets should fail to create secret due to empty secret key [Conformance]  
  `[FAILED] created secret "secret-emptykey-test-cd68b28f-e115-431b-a37d-9d8377eb6e2e" with empty key in namespace "secrets-809"`
- [It] [sig-node] Sysctls [LinuxOnly] [NodeConformance] should support sysctls [MinimumKubeletVersion:1.21] [Environment:NotInUserNS] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-node] Variable Expansion should allow substituting values in a volume subpath [Conformance]  
  `[FAILED] expected pod "var-expansion-85b0c449-1760-4bcc-ac2e-5d5c78cd8682" success: Timed out after 300.001s.`
- [It] [sig-node] Variable Expansion should fail substituting values in a volume subpath with absolute path [Conformance]  
  `[FAILED] while waiting for the pod container to fail: Timed out after 120.000s.`
- [It] [sig-node] Variable Expansion should fail substituting values in a volume subpath with backticks [Conformance]  
  `[FAILED] while waiting for the pod container to fail: Timed out after 120.001s.`
- [It] [sig-node] Variable Expansion should succeed in writing subpaths in container [Conformance]  
  `[FAILED] while waiting for pod to be running: Timed out after 120.007s.`
- [It] [sig-node] Variable Expansion should verify that a failing subpath expansion can be modified during the lifecycle of a container [Slow] [Conformance]  
  `[FAILED] while waiting for pod to be running: Timed out after 120.001s.`
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates resource limits of pods that are allowed to run [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPredicates [Serial] validates that NodeSelector is respected if not matching [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PreemptionExecutionPath runs ReplicaSets to verify preemption running path [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] PriorityClass endpoints verify PriorityClass endpoints can be operated with different HTTP methods [Conformance]  
  `[FAILED] expect a patch error on an immutable field`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates basic preemption works [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-4103  d970e8e2-71d2-4b4b-b7df-da518a4cd28a 12640 2 2026-10-17 21:58:51 -0`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates lower priority pod preemption by critical pod [Conformance]  
  `[FAILED] expected pod to be preempted, instead got pod &Pod{ObjectMeta:{pod0-0-sched-preemption-low-priority  sched-preemption-7624  ba40b3dc-68e8-47a3-8ee6-54c2b49937dd 11903 2 2026-10-17 21:45:39 -0`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] listing schedulable nodes error: error: Get "http://127.0.0.1:6443/api/v1/nodes?fieldSelector=spec.unschedulable%3Dfalse": dial tcp 127.0.0.1:6443: connect: connection refused. Non-retryable `
- [It] [sig-storage] Downward API volume should update annotations on modification [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 180.000s.`
- [It] [sig-storage] Downward API volume should update labels on modification [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 180.001s.`
- [It] [sig-storage] PersistentVolumes CSI Conformance should apply changes to a pv/pvc status [Conformance]  
  `[FAILED] Checking that the PV status has been read`
- [It] [sig-storage] Projected downwardAPI should update annotations on modification [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 120.001s.`
- [It] [sig-storage] Projected downwardAPI should update labels on modification [NodeConformance] [Conformance]  
  `[FAILED] Timed out after 120.007s.`
- [It] [sig-storage] Projected secret should be consumable from pods in volume as non-root with defaultMode and fsGroup set [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "(mode of file \"/etc/projected-secret-volume/data-1\": -r--r-----\|mode of Windows file \"/etc/projected-secret-volume/data-1\": -r--r-----)" in container output: Expected`
- [It] [sig-storage] Secrets should be consumable from pods in volume as non-root with defaultMode and fsGroup set [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "(mode of file \"/etc/secret-volume/data-1\": -r--r-----\|mode of Windows file \"/etc/secret-volume/data-1\": -r--r-----)" in container output: Expected`
