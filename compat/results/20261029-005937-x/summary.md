# Compatibility: x

| tier | passed | total | rate |
|---|---|---|---|
| M1 | 0 | 1 | 0 % |
| **all** | **0** | **1** | **0 %** |

## Not passing

- `multi_service_spreads` (M1, fail): timed out after 60s waiting for: bash -c kubectl -n compat-multi-service-spreads exec pa -- sh -c 'for i in $(seq 1 20); do wget -qO- -T 3 http://who/; done' | sort -u | tr '\n' ' ' | grep -q 'pa pb'
