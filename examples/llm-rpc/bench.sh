#!/bin/sh
# Prompt and generation speed of the llama-server behind URL, from the
# server's own timings: one warm-up request, then RUNS identical ones.
#   examples/llm-rpc/bench.sh http://localhost:30900 [runs] [max_tokens]
set -eu
url="${1:?usage: bench.sh URL [runs] [max_tokens]}"
runs="${2:-3}"
max="${3:-128}"
body='{"messages":[{"role":"user","content":"Write a short story about a lighthouse keeper who finds a message in a bottle."}],"max_tokens":'"$max"',"temperature":0,"cache_prompt":false}'
req() { curl -fsS "$url/v1/chat/completions" -H 'Content-Type: application/json' -d "$body"; }
req >/dev/null
i=0
while [ "$i" -lt "$runs" ]; do
  req | python3 -c '
import json, sys
t = json.load(sys.stdin)["timings"]
print("prompt %4d tok %7.1f tok/s   generation %4d tok %6.1f tok/s" % (
    t["prompt_n"], t["prompt_per_second"], t["predicted_n"], t["predicted_per_second"]))'
  i=$((i + 1))
done
