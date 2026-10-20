| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 43 | 43 | 100 % |
| L2 | 15 | 15 | 100 % |
| L3 | 6 | 7 | 86 % |
| L4 | 10 | 47 | 21 % |
| **all** | **81** | **119** | **68 %** |

## Not passing

- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
- `helm_traefik` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
## Not passing

- `api_from_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-api-from-pod exec client -- sh -c wget -qO- -T 3 --no-check-certificate --header "Authorization: Bearer $(cat /var/run/secrets/kubernetes.io/serviceaccount/token)" https://kubernetes.default.svc/version
- `helm_traefik` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_argo_cd` (L4, fail): 	* Post "https://127.0.0.1:6443/api/v1/namespaces/compat-helm-argo-cd/serviceaccounts?fieldManager=helm": EOF
- `helm_cnpg` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-cnpg)
- `helm_headlamp` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-headlamp)
- `helm_nats` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-nats)
- `helm_vault` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-vault)
- `helm_minio_operator` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-minio-operator)
- `helm_external_dns` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-external-dns)
- `helm_gitea` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-gitea)
- `helm_loki` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-loki)
- `helm_kps` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-kps)
- `helm_vm_single` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-vm-single)
- `helm_alloy` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-alloy)
- `helm_haproxy` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-haproxy)
- `helm_envoy_gateway` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-envoy-gateway)
- `helm_nginx_ingress` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-nginx-ingress)
- `helm_metallb` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-metallb)
- `helm_openebs_local` (L4, fail): Error from server (ServiceUnavailable): the server is currently unable to handle the request (get namespaces compat-helm-openebs-local)
