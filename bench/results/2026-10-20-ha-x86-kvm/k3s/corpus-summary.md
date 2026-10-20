| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 43 | 43 | 100 % |
| L2 | 15 | 15 | 100 % |
| L3 | 2 | 7 | 29 % |
| L4 | 37 | 47 | 79 % |
| **all** | **104** | **119** | **87 %** |

## Not passing

- `svc_dns` (L3, fail): timed out after 30s waiting for: kubectl -n compat-svc-dns exec client -- wget -qO- -T 3 http://web/
- `svc_headless` (L3, fail): timed out after 30s waiting for: bash -c [[ $(kubectl -n compat-svc-headless exec client -- nslookup web.compat-svc-headless.svc.cluster.local 2>/dev/null | grep -c '^Address: ') -ge 2 ]]
## Not passing

- `svc_dns` (L3, fail): timed out after 30s waiting for: kubectl -n compat-svc-dns exec client -- wget -qO- -T 3 http://web/
- `svc_headless` (L3, fail): timed out after 30s waiting for: bash -c [[ $(kubectl -n compat-svc-headless exec client -- nslookup web.compat-svc-headless.svc.cluster.local 2>/dev/null | grep -c '^Address: ') -ge 2 ]]
- `svc_nodeport` (L3, fail): timed out after 30s waiting for: kubectl -n compat-svc-nodeport exec client -- wget -qO- -T 3 http://192.168.77.11:32219/
- `pod_to_pod` (L3, fail): timed out after 30s waiting for: kubectl -n compat-pod-to-pod exec client -- wget -qO- -T 3 http://10.42.1.15/
- `api_from_pod` (L3, fail): Error from server (Forbidden): pods "client" is forbidden: error looking up service account compat-api-from-pod/default: serviceaccount "default" not found
- `helm_traefik` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_loki` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_nginx_ingress` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_redis_operator` (L4, fail): 	* etcdserver: request timed out
- `helm_home_assistant` (L4, fail): Error: INSTALLATION FAILED: context deadline exceeded
- `helm_jellyfin` (L4, fail): Unable to connect to the server: net/http: TLS handshake timeout
- `helm_nextcloud` (L4, fail): Unable to connect to the server: net/http: TLS handshake timeout
- `helm_uptime_kuma` (L4, fail): Unable to connect to the server: net/http: TLS handshake timeout
- `helm_vaultwarden` (L4, fail): Unable to connect to the server: net/http: TLS handshake timeout
- `helm_argo_wf` (L4, fail): 	* timed out waiting for the condition
