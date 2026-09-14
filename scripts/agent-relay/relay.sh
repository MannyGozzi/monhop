#!/bin/zsh
# Mac client for the agent relay (PROTOCOL.md): send <file|-> | wait [timeout] | log [since] | health
set -eu
here=${0:A:h}
token=$(cat "$here/token.txt")
base="http://$(cat "$here/host.txt"):24880/$token"
since_file="$here/since.txt"
show() { python3 -c '
import json, sys
items = json.load(sys.stdin)
last = 0
for m in items:
    last = max(last, int(m["seq"]))
    if m.get("from") != "mac":
        sys.stdout.write("WIN relay #%d %s\n%s\n\n" % (m["seq"], m["at"], m["text"]))
sys.stderr.write(str(last))
'; }
case "${1:-}" in
  health) curl -s --max-time 10 "$base/health"; echo ;;
  send) curl -s --max-time 30 -X POST "$base/send?from=mac" -H 'Content-Type: text/plain; charset=utf-8' --data-binary @"${2:--}"; echo ;;
  log) curl -s --max-time 30 "$base/log?since=${2:-0}" | show 2>/dev/null ;;
  wait)
    timeout=${2:-120}; since=$(cat "$since_file" 2>/dev/null || echo 0); deadline=$(( $(date +%s) + timeout ))
    while :; do
      left=$(( deadline - $(date +%s) )); [ "$left" -gt 0 ] || { echo "no new message within $timeout s (since $since)"; break; }
      out=$(curl -s --max-time $(( left + 30 )) "$base/wait?since=$since&timeout=$left")
      shown=$(printf '%s' "$out" | show 2>/tmp/relay-last.$$); last=$(cat /tmp/relay-last.$$); rm -f /tmp/relay-last.$$
      [ -n "$shown" ] && printf '%s\n' "$shown"
      [ "${last:-0}" -gt "$since" ] && since=$last && printf '%s' "$since" > "$since_file"
      [ -n "$shown" ] && break
    done ;;
  *) echo "usage: relay.sh send <file|-> | wait [timeout] | log [since] | health" >&2; exit 2 ;;
esac
