#!/usr/bin/env bash
# Drives the Windows development PC from the Mac over SSH. Protocol and rules: README.md next to this file.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
conf="$here/pc.conf"
if [[ ! -f $conf ]]; then
  echo "Create $conf with two lines: host=<ssh alias from ~/.ssh/config> and checkout=<PC checkout path>." >&2
  exit 2
fi
host=$(sed -n 's/^host=//p' "$conf")
checkout=$(sed -n 's/^checkout=//p' "$conf")
mail='.claude\state\mail'
app='$env:LOCALAPPDATA\Programs\MonHop'

# PowerShell from stdin, run in the PC checkout. -EncodedCommand sidesteps every quoting layer.
ps() {
  local b64
  b64=$({
    printf '%s\n' "\$ProgressPreference = 'SilentlyContinue'" "Set-Location -LiteralPath '$checkout'"
    cat
  } | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')
  ssh -n -o BatchMode=yes "$host" "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $b64" | tr -d '\r'
}

# The SFTP form of a checkout-relative path.
remote_path() {
  printf '%s/%s' "${checkout//\\//}" "${1//\\//}"
}

sync() {
  local commit=${1:?usage: pc.sh sync <commit>}
  ps <<EOF
\$ErrorActionPreference = 'Stop'
if (git status --porcelain) { throw 'The PC checkout has local changes; not syncing.' }
git fetch --quiet origin
git switch --quiet main
git merge --ff-only --quiet origin/main
if (\$LASTEXITCODE -ne 0) { throw 'main cannot fast-forward to origin/main.' }
\$head = git rev-parse HEAD
if (-not \$head.StartsWith('$commit')) { throw "The PC is at \$head, not $commit. Push first." }
"synced \$head"
EOF
}

launch() {
  ps <<EOF
\$ErrorActionPreference = 'Stop'
\$task = 'MonHop dev launch'
\$action = New-ScheduledTaskAction -Execute "$app\monhop-desktop.exe"
\$principal = New-ScheduledTaskPrincipal -UserId \$env:USERNAME -LogonType Interactive
Register-ScheduledTask -TaskName \$task -Action \$action -Principal \$principal -Force | Out-Null
try { Start-ScheduledTask -TaskName \$task; Start-Sleep -Seconds 3 } finally { Unregister-ScheduledTask -TaskName \$task -Confirm:\$false }
Get-Process monhop-desktop -ErrorAction SilentlyContinue | ForEach-Object { "running pid=\$(\$_.Id) session=\$(\$_.SessionId)" }
EOF
}

case "${1:-}" in
  run)
    if [[ $# -ge 2 ]]; then ps < "$2"; else ps; fi
    ;;
  sync)
    sync "${2:-}"
    ;;
  build)
    ps <<'EOF'
. .\scripts\env.ps1
$ErrorActionPreference = 'Continue'
"building $(git rev-parse --short=9 HEAD)"
cargo build --workspace --release --locked 2>&1 | Where-Object { "$_" -notmatch '^\s*(Compiling|Downloaded|Downloading)' } | ForEach-Object { "$_" }
if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
EOF
    ;;
  install)
    ps <<EOF
\$ErrorActionPreference = 'Stop'
\$head = git rev-parse --short=9 HEAD
Get-Process monhop-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500
foreach (\$exe in 'monhop-desktop.exe', 'monhop.exe') {
  Copy-Item -LiteralPath "target\release\\\$exe" -Destination "$app\\\$exe" -Force
  \$built = (Get-FileHash "target\release\\\$exe").Hash
  if ((Get-FileHash "$app\\\$exe").Hash -ne \$built) { throw "\$exe did not install intact." }
  "installed \$exe \$(\$built.Substring(0, 16)) from \$head"
}
EOF
    launch
    ;;
  launch)
    launch
    ;;
  log)
    ps <<EOF
Get-Content -LiteralPath "\$env:LOCALAPPDATA\com.manuelgozzi.monhop\logs\monhop.log" -Tail ${2:-40}
EOF
    ;;
  ask)
    commit=${2:?usage: pc.sh ask <commit> <prompt-file> [model]}
    prompt=${3:?usage: pc.sh ask <commit> <prompt-file> [model]}
    model=${4:-claude-sonnet-5}
    sync "$commit"
    name="ask-$(date -u +%Y%m%dT%H%M%SZ).md"
    staged=$(mktemp)
    {
      printf 'You are the Windows agent, run headless over SSH by the Mac agent at commit %s. ' "$commit"
      printf 'You are a guest worker: do not invoke claude, codex, or grok, and never commit, push, pull or edit tracked files. '
      printf 'Follow scripts/pc/README.md. Answer in your final output.\n\n'
      cat "$prompt"
    } > "$staged"
    ps <<< "New-Item -ItemType Directory -Force -Path '$mail' | Out-Null"
    scp -q "$staged" "$host:$(remote_path "$mail\\$name")"
    rm -f "$staged"
    ps <<EOF
\$OutputEncoding = [Text.UTF8Encoding]::new(\$false)
Get-Content -Raw -LiteralPath '$mail\\$name' | claude -p --model $model --effort xhigh --permission-mode bypassPermissions 2>&1
"ASK_EXIT=\$LASTEXITCODE"
EOF
    ;;
  tell)
    b64=$(cat "${2:--}" | base64 | tr -d '\n')
    ps <<EOF
\$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force -Path '$mail' | Out-Null
\$box = Join-Path (Get-Location) '$mail\to-win.jsonl'
\$seq = if (Test-Path \$box) { @(Get-Content \$box).Count + 1 } else { 1 }
\$text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$b64')).Trim()
\$line = [ordered]@{ seq = \$seq; at = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'); from = 'mac'; text = \$text } | ConvertTo-Json -Compress
[IO.File]::AppendAllText(\$box, \$line + "\`n", [Text.UTF8Encoding]::new(\$false))
"left message \$seq"
EOF
    ;;
  listen)
    interval=${2:-10}
    seen_file="$here/.seen"
    source=$(remote_path "$mail\\to-mac.jsonl")
    ps <<< "New-Item -ItemType Directory -Force -Path '$mail' | Out-Null; if (-not (Test-Path '$mail\\to-mac.jsonl')) { New-Item -ItemType File -Path '$mail\\to-mac.jsonl' | Out-Null }"
    misses=0
    while :; do
      copy=$(mktemp)
      # The mailbox file always exists, so a failed copy means the PC is unreachable; say so once.
      if ! scp -q "$host:$source" "$copy" 2>/dev/null; then
        misses=$((misses + 1))
        [[ $misses -eq 3 ]] && echo "listener: cannot reach the PC (3 polls in a row)"
      else
        [[ $misses -ge 3 ]] && echo "listener: reached the PC again"
        misses=0
        python3 - "$copy" "$seen_file" <<'PY'
import json, sys
path, seen_path = sys.argv[1], sys.argv[2]
try:
    seen = int(open(seen_path).read().strip() or 0)
except (FileNotFoundError, ValueError):
    seen = 0
newest = seen
for raw in open(path, encoding="utf-8-sig"):
    if not raw.strip():
        continue
    message = json.loads(raw)
    if message["seq"] <= seen:
        continue
    print(f"[windows #{message['seq']} {message['at']} @{message.get('commit', '?')}] {message['text']}", flush=True)
    newest = max(newest, message["seq"])
if newest != seen:
    open(seen_path, "w").write(str(newest))
PY
      fi
      rm -f "$copy"
      sleep "$interval"
    done
    ;;
  *)
    sed -n '/^| Command/,/^$/p' "$here/README.md" >&2
    exit 2
    ;;
esac
