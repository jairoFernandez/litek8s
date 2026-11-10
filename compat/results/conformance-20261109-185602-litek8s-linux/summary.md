# Conformance: litek8s-linux

**418/419 passed** (0 skipped), 20 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 94 | 94 | 0 |
| apps | 60 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 43 | 43 | 0 |
| node | 88 | 89 | 0 |
| scheduling | 11 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (1)

- [It] [sig-node] InitContainer [NodeConformance] should not start app containers and fail the pod if init containers fail on a RestartNever pod [Conformance]  
  `[FAILED] second init container should have reason PodInitializing: {"name":"init1","state":{"waiting":{"reason":"ContainerCreating","message":"Pulling image \"registry.k8s.io/e2e-test-images/busybox:1`
