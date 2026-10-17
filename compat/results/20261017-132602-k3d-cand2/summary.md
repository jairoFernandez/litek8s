# Compatibility: k3d-cand2

| tier | passed | total | rate |
|---|---|---|---|
| L4 | 9 | 10 | 90 % |
| **all** | **9** | **10** | **90 %** |

## Not passing

- `helm_immich` (L4, fail): Error: INSTALLATION FAILED: execution error at (immich/templates/checks.yaml:1:64): .Values.immich.persistence.library.existingClaim is required.
