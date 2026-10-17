# Compatibility: litek8s-crun

| tier | passed | total | rate |
|---|---|---|---|
| L2 | 15 | 15 | 100 % |
| L3 | 6 | 7 | 86 % |
| L4 | 20 | 20 | 100 % |
| **all** | **41** | **42** | **98 %** |

## Not passing

- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
