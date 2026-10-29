# Compatibility: litek8s-vms

| tier | passed | total | rate |
|---|---|---|---|
| L3 | 10 | 11 | 91 % |
| M1 | 2 | 5 | 40 % |
| **all** | **12** | **16** | **75 %** |

## Not passing

- `netpol_namespaces` (L3, fail): compat/run.sh: line 51: peer: unbound variable
- `multi_service_spreads` (M1, fail): timed out after 60s waiting for: bash -c kubectl -n compat-multi-service-spreads exec pa -- sh -c 'for i in $(seq 1 20); do wget -qO- -T 3 http://who/; done' | sort -u | tr '\n' ' ' | grep -q 'pa pb'
- `multi_node_kill_deployment` (M1, fail): set COMPAT_NODE_KILL and COMPAT_NODE_START to power nodes off and on
- `multi_node_short_outage` (M1, fail): set COMPAT_NODE_KILL and COMPAT_NODE_START to power nodes off and on
