# Compatibility: k3d-multi

| tier | passed | total | rate |
|---|---|---|---|
| M1 | 4 | 5 | 80 % |
| **all** | **4** | **5** | **80 %** |

## Not passing

- `multi_service_spreads` (M1, fail): error: failed to create ClusterIP service: services "who" is forbidden: unable to create new content in namespace compat-multi-service-spreads because it is being terminated
