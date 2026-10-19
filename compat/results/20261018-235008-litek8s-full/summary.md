# Compatibility: litek8s-full

| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 43 | 43 | 100 % |
| L2 | 15 | 15 | 100 % |
| L3 | 5 | 7 | 71 % |
| L4 | 46 | 47 | 98 % |
| **all** | **116** | **119** | **97 %** |

## Not passing

- `svc_dns` (L3, fail): timed out after 30s waiting for: kubectl -n compat-svc-dns exec client -- wget -qO- -T 3 http://web/
- `svc_headless` (L3, fail): timed out after 30s waiting for: bash -c [[ $(kubectl -n compat-svc-headless exec client -- nslookup web.compat-svc-headless.svc.cluster.local 2>/dev/null | grep -c '^Address: ') -ge 2 ]]
- `helm_loki` (L4, fail): context deadline exceeded
