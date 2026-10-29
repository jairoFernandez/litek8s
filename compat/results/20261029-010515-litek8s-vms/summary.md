# Compatibility: litek8s-vms

| tier | passed | total | rate |
|---|---|---|---|
| L3 | 10 | 11 | 91 % |
| M1 | 3 | 5 | 60 % |
| **all** | **13** | **16** | **81 %** |

## Not passing

- `netpol_namespaces` (L3, fail): Error from server (AlreadyExists): namespaces "compat-netpol-namespaces-peer" already exists
- `multi_node_kill_deployment` (M1, fail): set COMPAT_NODE_KILL and COMPAT_NODE_START to power nodes off and on
- `multi_node_short_outage` (M1, fail): set COMPAT_NODE_KILL and COMPAT_NODE_START to power nodes off and on
