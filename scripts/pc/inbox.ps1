# Prints messages the Mac agent left for the Windows agent that were not shown yet (see README.md).
param([switch]$Follow)
$ErrorActionPreference = 'Stop'
$checkout = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$mail = Join-Path $checkout '.claude\state\mail'
$box = Join-Path $mail 'to-win.jsonl'
$seenFile = Join-Path $mail 'to-win.seen'
do {
    $seen = if (Test-Path $seenFile) { [int](Get-Content -Raw $seenFile) } else { 0 }
    if (Test-Path $box) {
        foreach ($raw in [IO.File]::ReadAllLines($box, [Text.UTF8Encoding]::new($false))) {
            if (-not $raw.Trim()) { continue }
            $message = $raw | ConvertFrom-Json
            if ($message.seq -le $seen) { continue }
            "[mac #$($message.seq) $($message.at)] $($message.text)"
            $seen = $message.seq
            Set-Content -LiteralPath $seenFile -Value $seen -NoNewline
        }
    }
    if ($Follow) { Start-Sleep -Seconds 5 }
} while ($Follow)
