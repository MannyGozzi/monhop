#!/bin/zsh
# Streams every Windows message as one event block for the Mac agent's monitor; the Mac's own are skipped.
set -u
here=${0:A:h}
token=$(cat "$here/token.txt")
host=$(cat "$here/host.txt")
since=$(python3 -c 'import json,sys; print(max([json.loads(l)["seq"] for l in open(sys.argv[1]) if l.strip()] or [0]))' "$here/log.jsonl" 2>/dev/null || echo 0)
while true; do
  out=$(curl -s --max-time 1560 "http://$host:24880/$token/wait?since=$since&timeout=1500") || { echo "relay unreachable at $(date -u +%H:%M:%SZ), retrying"; sleep 15; continue; }
  next=$(printf '%s' "$out" | python3 -c '
import json, sys
since = int(sys.argv[1])
try:
    items = json.load(sys.stdin)
except Exception:
    items = []
for m in items:
    since = max(since, int(m["seq"]))
    if m.get("from") == "win":
        sys.stdout.write("WIN relay #%d %s\n%s\n" % (m["seq"], m["at"], m["text"]))
sys.stderr.write("%d" % since)
' "$since" 2>/tmp/relay-watch-seq.$$)
  [ -n "$next" ] && printf '%s\n' "$next"
  new=$(cat /tmp/relay-watch-seq.$$ 2>/dev/null); rm -f /tmp/relay-watch-seq.$$
  if [ -n "$new" ] && [ "$new" -ge "$since" ] 2>/dev/null; then since=$new; else sleep 5; fi
done
