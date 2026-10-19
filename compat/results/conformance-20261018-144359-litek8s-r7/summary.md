# Conformance: litek8s-r7

**413/419 passed** (0 skipped), 29 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 93 | 94 | 0 |
| apps | 60 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 38 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 11 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (6)

- [It] [sig-api-machinery] Watchers should observe add, update, and delete watch notifications on configmaps [Conformance]  
  `[FAILED] Watch closed unexpectedly`
- [It] [sig-network] DNS should provide DNS for pods for Subdomain [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for services [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should provide DNS for the cluster [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] DNS should resolve DNS of partial qualified names for services [LinuxOnly] [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-network] HostPort validates that there is no conflict between pods with same hostPort but different hostIP and protocol [LinuxOnly] [Conformance]  
  `[FAILED] Failed to connect to exposed host ports`
