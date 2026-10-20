# Conformance: litek8s-ha

**406/419 passed** (0 skipped), 25 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 93 | 94 | 0 |
| apps | 59 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 9 | 10 | 0 |
| cli | 16 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 41 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 11 | 11 | 0 |
| storage | 83 | 90 | 0 |

## Failing (13)

- [It] [sig-api-machinery] Aggregator Should be able to support the 1.17 Sample API Server using the current Aggregator [Conformance]  
  `[FAILED] could not find group version resource for dynamic client and wardle/flunders (discovery error: <nil>, discovery results: map[schema.GroupVersionResource]struct {}{schema.GroupVersionResource{`
- [It] [sig-apps] Daemon set [Serial] should update pod when spec was updated and update strategy is RollingUpdate [Conformance]  
  `[FAILED] Expected`
- [It] [sig-auth] ServiceAccounts ServiceAccountIssuerDiscovery should support OIDC discovery of service account issuer [Conformance]  
  `[FAILED] Unexpected error getting pod logs: timed out waiting for the condition`
- [It] [sig-cli] Kubectl client Guestbook application should create and stop a working application [Conformance]  
  `[FAILED] Frontend service did not start serving content in 600 seconds.`
- [It] [sig-network] Proxy version v1 A set of valid responses are returned for both pod and service Proxy [Conformance]  
  `[FAILED] Service didn't start within time out period. timed out waiting for the condition: timed out waiting for the condition`
- [It] [sig-network] Proxy version v1 should proxy through a service and a pod [Conformance]  
  `[FAILED] 0 (503; 5.684959ms): path /api/v1/namespaces/proxy-2073/services/http:proxy-service-8ffp2:portname2/proxy/ gave status error: {TypeMeta:{Kind: APIVersion:} ListMeta:{SelfLink: ResourceVersion`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0644,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0666,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (non-root,0777,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (root,0644,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (root,0666,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes should support (root,0777,tmpfs) [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
- [It] [sig-storage] EmptyDir volumes volume on tmpfs should have the correct mode [LinuxOnly] [NodeConformance] [Conformance]  
  `[FAILED] expected "mount type of \"/test-volume\": tmpfs" in container output: Expected`
