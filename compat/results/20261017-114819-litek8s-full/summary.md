# Compatibility: litek8s-full

| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 25 | 25 | 100 % |
| L2 | 14 | 15 | 93 % |
| L3 | 6 | 7 | 86 % |
| L4 | 20 | 20 | 100 % |
| **all** | **72** | **74** | **97 %** |

## Not passing

- `port_forward` (L2, fail): timed out after 20s waiting for: curl -fsS http://127.0.0.1:20717/
- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
