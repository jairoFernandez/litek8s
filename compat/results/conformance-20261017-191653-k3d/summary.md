# Conformance: k3d

**416/419 passed** (0 skipped), 39 min wall clock

| SIG | passed | ran | skipped |
|---|---|---|---|
| api-machinery | 93 | 94 | 0 |
| apps | 59 | 60 | 0 |
| architecture | 1 | 1 | 0 |
| auth | 10 | 10 | 0 |
| cli | 17 | 17 | 0 |
| instrumentation | 4 | 4 | 0 |
| network | 43 | 43 | 0 |
| node | 89 | 89 | 0 |
| scheduling | 10 | 11 | 0 |
| storage | 90 | 90 | 0 |

## Failing (3)

- [It] [sig-api-machinery] Namespaces [Serial] should patch a Namespace [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-apps] Daemon set [Serial] should run and stop complex daemon [Conformance]  
  `[FAILED] context deadline exceeded`
- [It] [sig-scheduling] SchedulerPreemption [Serial] validates pod disruption condition is added to the preempted pod [Conformance]  
  `[FAILED] listing schedulable nodes error: error: Get "https://0.0.0.0:58467/api/v1/nodes?fieldSelector=spec.unschedulable%3Dfalse": EOF. Non-retryable failure or timed out while listing nodes for e2e `
