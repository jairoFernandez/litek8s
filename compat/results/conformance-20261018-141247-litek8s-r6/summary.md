# Conformance: litek8s-r6

**412/419 passed** (0 skipped), 27 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 93 | 94 | 0 |
| apps | 60 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 37 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 11 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (7)

- [It] [sig-api-machinery] Aggregator Should be able to support the 1.17 Sample API Server using the current Aggregator [Conformance]  
  `[FAILED] creating apiservice v1alpha1.wardle.example.com: no protobuf schema for apiregistration.k8s.io/v1 APIService`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] unexpected error getting pod client logs for webserver: client rate limiter Wait returned an error: rate: Wait(n=1) would exceed context deadline`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] Unable to read agnhost_udp@kubernetes.default.svc.cluster.local from pod dns-4209/dns-test-1d02e34b-e7b1-4b77-b455-d9447e3a63d9: client rate limiter Wait returned an error: rate: Wait(n=1) wo`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] EndpointSlice should create Endpoints and EndpointSlices for Pods matching a Service [Conformance]  
  `[FAILED] Error fetching EndpointSlice for Service endpointslice-1333/example-named-port: client rate limiter Wait returned an error: context deadline exceeded`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] Failed to connect to exposed host ports`
