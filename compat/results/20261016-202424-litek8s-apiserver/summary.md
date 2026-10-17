# Compatibility: litek8s-apiserver

| tier | passed | total | rate |
|---|---|---|---|
| L0 | 7 | 7 | 100 % |
| L1 | 25 | 25 | 100 % |
| L2 | 0 | 15 | 0 % |
| L3 | 0 | 7 | 0 % |
| L4 | 0 | 20 | 0 % |
| **all** | **32** | **74** | **43 %** |

## Not passing

- `deploy_ready` (L2, timeout): 
- `deploy_scale` (L2, timeout): 
- `deploy_rollout_undo` (L2, timeout): 
- `pod_logs` (L2, timeout): 
- `pod_exec` (L2, timeout): 
- `port_forward` (L2, timeout): 
- `job_complete` (L2, timeout): 
- `cronjob_schedules` (L2, timeout): 
- `statefulset_ordinals` (L2, fail): error: rollout status is only available for RollingUpdate strategy type
- `daemonset_all_nodes` (L2, fail): error: rollout status is only available for RollingUpdate strategy type
- `multi_container_pod` (L2, timeout): 
- `config_injection` (L2, timeout): 
- `http_readiness` (L2, timeout): 
- `restart_on_crash` (L2, timeout): 
- `pvc_local` (L2, timeout): 
- `svc_clusterip` (L3, timeout): 
- `svc_dns` (L3, timeout): 
- `svc_endpoints` (L3, timeout): 
- `svc_headless` (L3, timeout): 
- `svc_nodeport` (L3, timeout): 
- `pod_to_pod` (L3, timeout): 
- `api_from_pod` (L3, timeout): 
- `helm_podinfo` (L4, fail): context deadline exceeded
- `helm_traefik` (L4, fail): context deadline exceeded
- `helm_cert_manager` (L4, fail): context deadline exceeded
- `helm_metrics_server` (L4, fail): context deadline exceeded
- `helm_node_exporter` (L4, fail): context deadline exceeded
- `helm_ksm` (L4, fail): context deadline exceeded
- `helm_grafana` (L4, timeout): Error: INSTALLATION FAILED: context canceled
- `helm_kyverno` (L4, fail): context deadline exceeded
- `helm_ext_secrets` (L4, fail): context deadline exceeded
- `helm_reloader` (L4, fail): context deadline exceeded
- `helm_keda` (L4, fail): context deadline exceeded
- `helm_argo_cd` (L4, fail): context deadline exceeded
- `helm_cnpg` (L4, fail): context deadline exceeded
- `helm_headlamp` (L4, fail): context deadline exceeded
- `helm_nats` (L4, fail): context deadline exceeded
- `helm_vault` (L4, fail): context deadline exceeded
- `helm_minio_operator` (L4, fail): context deadline exceeded
- `helm_external_dns` (L4, fail): context deadline exceeded
- `helm_gitea` (L4, fail): context deadline exceeded
- `helm_loki` (L4, fail): context deadline exceeded
