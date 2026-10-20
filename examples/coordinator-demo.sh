#!/usr/bin/env bash
# In-cluster coordinator demo: node-0 splits a matmul across its peers over the
# private LAN and reports each peer's round-trip time. Run from your machine —
# it SSHes the request to node-0, which coordinates the rest internally.
#
#   ./examples/coordinator-demo.sh
#
# Edit COORDINATOR / PEERS below if your node IPs differ (see `terraform output`
# for public SSH IPs; the private IPs are the 10.42.1.x the agents listen on).
set -euo pipefail

KEY=~/.ssh/id_ed25519
COORDINATOR="${COORDINATOR:-ubuntu@203.0.113.10}"   # public IP we SSH into (placeholder: set yours)
PEERS='["10.42.1.135","10.42.1.184","10.42.1.99","10.42.1.133","10.42.1.216"]'  # private IPs of the workers

# A 60x64 . 64x64 matmul (small, so the demo is fast; the point is the LAN
# round-trip timing, not the compute).
REQ=$(python3 -c "
import json
n,k,m=60,64,64
left=[[((i*k+j)%7)*0.1 for j in range(k)] for i in range(n)]
right=[[((i*m+j)%5)*0.1 for j in range(m)] for i in range(k)]
print(json.dumps({'CoordinateMatmul':{'left':left,'right':right,'peer_ips':$PEERS}}))
")

echo "Sending CoordinateMatmul to node-0 ($COORDINATOR)..."
echo "node-0 will split the rows across ${PEERS} over the private LAN."
echo

printf '%s\n' "$REQ" | ssh -o BatchMode=yes -i "$KEY" -o IdentitiesOnly=yes "$COORDINATOR" \
  '~/.local/bin/litek8s agent' \
  | python3 -c "
import sys, json
r = json.load(sys.stdin)
d = r.get('CoordinatedMatmul')
if not d:
    print('Unexpected response:', str(r)[:300]); sys.exit(1)
print('Result shape:', len(d['result']), 'x', len(d['result'][0]))
print()
print('Per-peer LAN round-trip:')
for ip, ms in d['peer_ms']:
    print(f'  {ip:<16} {ms} ms')
print()
print('=> each round-trip is single-digit ms over the cluster LAN,')
print('   vs ~600ms if the Mac drove each one over the internet.')
"
