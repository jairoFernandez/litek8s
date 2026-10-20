# Compatibility: litek8s-dev-cells-macos-vm

| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 43 | 43 | 100 % |
| L2 | 15 | 15 | 100 % |
| L3 | 7 | 7 | 100 % |
| L4 | 46 | 47 | 98 % |
| **all** | **118** | **119** | **99 %** |

## Not passing

- `helm_vault` (L4, fail): Error: INSTALLATION FAILED: looks like "https://helm.releases.hashicorp.com" is not a valid chart repository or cannot be reached: Get "https://helm.releases.hashicorp.com/index.yaml": context deadline exceeded (Client.Timeout exceeded while awaiting headers)
