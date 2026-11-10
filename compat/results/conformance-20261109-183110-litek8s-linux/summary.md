# Conformance: litek8s-linux

**411/419 passed** (0 skipped), 21 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 94 | 94 | 0 |
| apps | 60 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 16 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 43 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 11 | 11 | 0 |
| storage | 83 | 90 | 0 |

## Failing (8)

- [It] [sig-cli] Kubectl client Kubectl describe should check if kubectl describe prints relevant information for rc and pods [Conformance]  
  `[FAILED] failed to find Kube-Proxy Version: in Name:               litek8s-conf`
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
