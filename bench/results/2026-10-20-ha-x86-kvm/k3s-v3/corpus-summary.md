| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 43 | 43 | 100 % |
| L2 | 15 | 15 | 100 % |
| L3 | 6 | 7 | 86 % |
| L4 | 43 | 47 | 91 % |
| **all** | **114** | **119** | **96 %** |

## Not passing

- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
- `helm_traefik` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
## Not passing

- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
- `helm_traefik` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_argo_cd` (L4, fail): Error: INSTALLATION FAILED: cannot re-use a name that is still in use
- `helm_nginx_ingress` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_trivy_operator` (L4, fail): Error: INSTALLATION FAILED: apiserver not ready
